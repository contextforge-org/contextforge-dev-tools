//! Scoped official client conformance for the external dataplane.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::conformance::DEFAULT_MCP_SPEC_VERSION;
use crate::infrastructure::process::{CommandSpec, ProcessRunner, SystemProcessRunner};
use crate::mcp::GatewayTopology;
use crate::mcp::gateway::{GatewayClient, GatewayRequest};

pub(crate) const INTERNAL_CLIENT_COMMAND: &str = "__client-conformance";
pub(crate) const CLIENT_COMPOSE_ARGS_ENV: &str = "CF_CLIENT_CONFORMANCE_COMPOSE_ARGS";
pub(crate) const CLIENT_BASE_URL_ENV: &str = "CF_CLIENT_CONFORMANCE_BASE_URL";
pub(crate) const CLIENT_SERVER_ID_ENV: &str = "CF_CLIENT_CONFORMANCE_SERVER_ID";
pub(crate) const CLIENT_TOKEN_ENV: &str = "MCP_CONFORMANCE_TOKEN";
const SCENARIO_ENV: &str = "MCP_CONFORMANCE_SCENARIO";
const PROTOCOL_VERSION_ENV: &str = "MCP_CONFORMANCE_PROTOCOL_VERSION";
const CONTEXT_ENV: &str = "MCP_CONFORMANCE_CONTEXT";
const CONFIG_WRITER: &str = "/opt/contextforge-conformance/write_client_config.py";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCall {
    name: String,
    arguments: Value,
}

/// Returns whether the raw process arguments select the private client driver.
#[must_use]
pub(crate) fn is_internal_client_invocation(arguments: &[OsString]) -> bool {
    arguments.get(1).map(OsString::as_os_str) == Some(OsStr::new(INTERNAL_CLIENT_COMMAND))
}

/// Runs one official client scenario selected through the runner environment.
pub(crate) async fn run_internal_client(arguments: &[OsString]) -> Result<()> {
    if arguments.len() != 3 {
        bail!("internal client conformance requires exactly one scenario-server URL");
    }
    let scenario_server_url = arguments[2]
        .to_str()
        .context("client conformance scenario-server URL is not UTF-8")?;
    let scenario = required_environment(SCENARIO_ENV)?;
    let protocol_version = required_environment(PROTOCOL_VERSION_ENV)?;
    if protocol_version != DEFAULT_MCP_SPEC_VERSION {
        bail!(
            "external dataplane client conformance supports protocol {DEFAULT_MCP_SPEC_VERSION}, not {protocol_version}"
        );
    }
    let server_id = required_environment(CLIENT_SERVER_ID_ENV)?;
    let token = required_environment(CLIENT_TOKEN_ENV)?;
    let base_url = required_environment(CLIENT_BASE_URL_ENV)?;
    let tool_calls = scenario_tool_calls(&scenario)?;
    let backend_url = container_backend_url(scenario_server_url)?;
    publish_scenario_config(&backend_url, &server_id, &tool_calls)?;

    let mut client =
        GatewayClient::builder(GatewayTopology::Dataplane, &base_url, &server_id, &token)
            .protocol_version(&protocol_version)
            .build()
            .context("failed to construct the client-conformance gateway")?;
    for (index, tool_call) in tool_calls.into_iter().enumerate() {
        let response = client
            .send(GatewayRequest::probe(json!({
                "jsonrpc": "2.0",
                "id": index + 1,
                "method": "tools/call",
                "params": {
                    "name": tool_call.name,
                    "arguments": tool_call.arguments,
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": protocol_version,
                        "io.modelcontextprotocol/clientInfo": {
                            "name": "dataplane-client-conformance-driver",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                        "io.modelcontextprotocol/clientCapabilities": {},
                    },
                },
            })))
            .await
            .context("external dataplane rejected a client-conformance tool call")?;
        let valid_response = response.status() == 200
            && response
                .message()
                .is_some_and(|message| message.get("result").is_some_and(|value| !value.is_null()))
            && response
                .message()
                .is_none_or(|message| message.get("error").is_none_or(Value::is_null));
        if !valid_response {
            bail!(
                "external dataplane returned an invalid client-conformance response for tool {:?}: HTTP {}; body={}",
                tool_call.name,
                response.status(),
                response.body()
            );
        }
    }
    Ok(())
}

fn scenario_tool_calls(scenario: &str) -> Result<Vec<ToolCall>> {
    let calls = match scenario {
        "tools_call" => json!([{"name": "add_numbers", "arguments": {"a": 2, "b": 3}}]),
        "request-metadata" => json!([{"name": "metadata_probe", "arguments": {}}]),
        "http-standard-headers" => json!([{"name": "test_headers", "arguments": {}}]),
        "http-custom-headers" => {
            let context = required_environment(CONTEXT_ENV)?;
            serde_json::from_str::<Value>(&context)
                .context("MCP_CONFORMANCE_CONTEXT is not valid JSON")?
                .get("toolCalls")
                .cloned()
                .context("MCP_CONFORMANCE_CONTEXT has no toolCalls array")?
        }
        _ => bail!("unsupported external dataplane client conformance scenario {scenario:?}"),
    };
    let calls: Vec<ToolCall> =
        serde_json::from_value(calls).context("client conformance tool calls are malformed")?;
    if calls.is_empty() || calls.iter().any(|call| call.name.is_empty()) {
        bail!("client conformance requires at least one named tool call");
    }
    Ok(calls)
}

fn container_backend_url(value: &str) -> Result<String> {
    let mut url = url::Url::parse(value).context("scenario-server URL is invalid")?;
    let loopback = match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if !matches!(url.scheme(), "http" | "https") || !loopback {
        bail!("scenario-server URL must be an absolute loopback HTTP(S) URL");
    }
    url.set_host(Some("host.docker.internal")).map_err(|_| {
        anyhow!("failed to address the scenario server from the dataplane container")
    })?;
    Ok(url.into())
}

fn publish_scenario_config(
    backend_url: &str,
    server_id: &str,
    tool_calls: &[ToolCall],
) -> Result<()> {
    let serialized_args = required_environment(CLIENT_COMPOSE_ARGS_ENV)?;
    let compose_args: Vec<String> = serde_json::from_str(&serialized_args)
        .context("CF_CLIENT_CONFORMANCE_COMPOSE_ARGS is not a JSON string array")?;
    if compose_args.first().map(String::as_str) != Some("compose") {
        bail!("client conformance Compose arguments must begin with compose");
    }
    let tool_names = tool_calls
        .iter()
        .map(|call| call.name.as_str())
        .collect::<BTreeSet<_>>();
    let tool_names = serde_json::to_string(&tool_names)
        .context("failed to serialize client conformance tool names")?;
    let command = CommandSpec::new("docker").args(compose_args).args([
        "run",
        "--rm",
        "--no-deps",
        "-e",
        CLIENT_TOKEN_ENV,
        "--entrypoint",
        "python3",
        "gateway",
        CONFIG_WRITER,
        server_id,
        backend_url,
        &tool_names,
    ]);
    SystemProcessRunner
        .run(&command)
        .context("failed to publish the client-conformance dataplane configuration")
}

fn required_environment(name: &str) -> Result<String> {
    std::env::var(name)
        .with_context(|| format!("{name} is required for internal client conformance"))
        .and_then(|value| {
            if value.is_empty() {
                bail!("{name} must not be empty for internal client conformance");
            }
            Ok(value)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_scenario_url_is_rewritten_for_the_dataplane_container() {
        assert_eq!(
            container_backend_url("http://127.0.0.1:43123/mcp?scenario=tools")
                .expect("loopback URL should be accepted"),
            "http://host.docker.internal:43123/mcp?scenario=tools"
        );
        assert!(container_backend_url("https://example.com/mcp").is_err());
    }

    #[test]
    fn fixed_client_scenarios_map_to_the_external_driver_tools() {
        assert_eq!(
            scenario_tool_calls("tools_call").expect("tools_call should be supported")[0].name,
            "add_numbers"
        );
        assert_eq!(
            scenario_tool_calls("request-metadata").expect("request-metadata should be supported")
                [0]
            .name,
            "metadata_probe"
        );
        assert_eq!(
            scenario_tool_calls("http-standard-headers")
                .expect("http-standard-headers should be supported")[0]
                .name,
            "test_headers"
        );
        assert!(scenario_tool_calls("initialize").is_err());
    }
}
