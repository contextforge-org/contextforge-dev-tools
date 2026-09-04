//! Public MCP route protocol probe.

use std::fmt;
use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};
use url::Url;

use crate::mcp::GatewayTopology;
use crate::{OutputStyle, TestStatus};

use crate::mcp::backend_identity::BackendIdentity;
use crate::mcp::gateway::{GatewayClient, GatewayRequest, HeaderOverride};
use crate::mcp::protocol::{
    initialize_with_id_and_version, is_stateless_protocol, jsonrpc_with_id,
    stateless_jsonrpc_with_id, tool_call_args,
};

const REDACTED: &str = "<redacted>";
const INITIALIZE_ID: u64 = 1;
const TOOLS_LIST_ID: u64 = 2;
const TOOL_CALL_ID: u64 = 3;
const MIN_RETRY_INTERVAL: Duration = Duration::from_millis(10);

#[cfg(test)]
#[path = "probe_tests.rs"]
mod tests;

/// Runtime values needed by the public MCP probe.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ProbeConfig {
    /// Stack topology whose public route is under test.
    pub(crate) mode: GatewayTopology,
    /// Base URL of the public nginx endpoint.
    pub(crate) base_url: String,
    /// Virtual server identifier used in the public MCP route.
    pub(crate) server_id: String,
    /// Bearer token sent by authenticated probe steps.
    pub(crate) bearer_token: String,
    /// Maximum time spent waiting for the dataplane publisher configuration.
    pub(crate) config_timeout: Duration,
    /// Delay between authenticated initialize attempts.
    pub(crate) retry_interval: Duration,
    /// Maximum time allowed for any individual transport request.
    pub(crate) request_timeout: Duration,
    /// MCP protocol revision requested during initialization.
    pub(crate) protocol_version: String,
    /// Tool names supplied by standalone mocked configuration.
    ///
    /// A nonempty value bypasses `tools/list`, which the external dataplane
    /// deliberately does not implement, while still exercising routed calls.
    pub(crate) tool_names: Vec<String>,
    /// Terminal-aware styling for human-readable probe results.
    pub(crate) output_style: OutputStyle,
}

impl fmt::Debug for ProbeConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProbeConfig")
            .field("mode", &self.mode)
            .field("base_url", &self.base_url)
            .field("server_id", &self.server_id)
            .field("bearer_token", &REDACTED)
            .field("config_timeout", &self.config_timeout)
            .field("retry_interval", &self.retry_interval)
            .field("request_timeout", &self.request_timeout)
            .field("protocol_version", &self.protocol_version)
            .field("tool_name_count", &self.tool_names.len())
            .finish()
    }
}

/// Transport-neutral MCP POST request issued by the probe flow.
#[derive(Clone, PartialEq)]
pub(crate) struct ProbeRequest {
    /// Fully resolved public MCP URL.
    pub(crate) url: String,
    /// JSON-RPC request payload.
    pub(crate) payload: Value,
    /// Optional bearer token; omitted for the negative authentication check.
    pub(crate) bearer_token: Option<String>,
    /// Optional MCP session identifier.
    pub(crate) session_id: Option<String>,
    /// MCP protocol revision sent in the transport header, when applicable.
    ///
    /// Initialize requests omit this header. Requests after initialization use
    /// the version negotiated by the server.
    pub(crate) protocol_version: Option<String>,
}

impl fmt::Debug for ProbeRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProbeRequest")
            .field("url", &self.url)
            .field("payload", &REDACTED)
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| REDACTED),
            )
            .field("session_id", &self.session_id.as_ref().map(|_| REDACTED))
            .field("protocol_version", &self.protocol_version)
            .finish()
    }
}

/// Parsed response returned by a [`ProbeTransport`].
#[derive(Clone, PartialEq)]
pub(crate) struct ProbeResponse {
    /// HTTP response status.
    pub(crate) status: u16,
    /// MCP session identifier response header, when present.
    pub(crate) session_id: Option<String>,
    /// Parsed JSON-RPC response from either JSON or SSE.
    pub(crate) message: Option<Value>,
    /// Harness backend identity parsed without retaining untrusted values.
    pub(crate) backend_identity: BackendIdentity,
}

impl fmt::Debug for ProbeResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProbeResponse")
            .field("status", &self.status)
            .field("session_id", &self.session_id.as_ref().map(|_| REDACTED))
            .field("message", &self.message.as_ref().map(|_| REDACTED))
            .field("backend_identity", &self.backend_identity)
            .finish()
    }
}

impl ProbeResponse {
    /// Creates a parsed probe response.
    #[must_use]
    pub(crate) fn new(status: u16, session_id: Option<String>, message: Option<Value>) -> Self {
        Self {
            status,
            session_id,
            message,
            backend_identity: BackendIdentity::Missing,
        }
    }

    /// Assigns the parsed harness backend identity.
    #[must_use]
    pub(crate) fn with_backend_identity(mut self, backend_identity: BackendIdentity) -> Self {
        self.backend_identity = backend_identity;
        self
    }
}

/// Async boundary used to send probe requests.
pub(crate) trait ProbeTransport: Send + Sync {
    /// Sends one MCP request and returns its parsed response.
    ///
    /// # Errors
    ///
    /// Returns an error when the request cannot be sent or its response cannot
    /// be converted to [`ProbeResponse`].
    async fn post(&self, request: ProbeRequest) -> Result<ProbeResponse>;
}

impl ProbeTransport for GatewayClient {
    async fn post(&self, request: ProbeRequest) -> Result<ProbeResponse> {
        if request.url != self.endpoint().as_str() {
            bail!("probe request endpoint does not match the MCP client endpoint");
        }
        let authorization = request.bearer_token.map_or(HeaderOverride::Omit, |token| {
            HeaderOverride::Value(format!("Bearer {token}"))
        });
        let protocol_version = request
            .protocol_version
            .map_or(HeaderOverride::Omit, HeaderOverride::Value);
        let session = request
            .session_id
            .map_or(HeaderOverride::Omit, HeaderOverride::Value);
        let gateway_request = GatewayRequest::probe(request.payload)
            .authorization(authorization)
            .protocol_version(protocol_version)
            .session(session);
        let mut client = self.clone();
        let exchange = client.send(gateway_request).await?;
        if (200..300).contains(&exchange.status())
            && !exchange.body().is_empty()
            && exchange.message().is_none()
        {
            bail!("unsupported MCP response content type");
        }
        let backend_identity = if exchange.mode().requires_dataplane() {
            BackendIdentity::Dataplane
        } else {
            BackendIdentity::Missing
        };
        Ok(ProbeResponse::new(
            exchange.status(),
            exchange.session_id().map(str::to_owned),
            exchange.message().cloned(),
        )
        .with_backend_identity(backend_identity))
    }
}

/// Runs the end-to-end protocol probe against the public MCP route.
///
/// # Errors
///
/// Returns an error on transport failures, unexpected HTTP or JSON-RPC
/// responses, a missing MCP session ID, an empty tool list, output failures, or
/// a tool response whose `isError` field is true.
pub(crate) async fn run_probe<T: ProbeTransport, W: Write>(
    transport: &T,
    config: &ProbeConfig,
    output: &mut W,
) -> Result<()> {
    let url = probe_url(&config.base_url, config.mode, &config.server_id)?;
    write_line(
        output,
        &format!("probe url: {}", sanitize_for_output(&url)),
        "failed to write probe URL",
    )?;

    if is_stateless_protocol(&config.protocol_version) {
        return run_stateless_probe(transport, config, output, url).await;
    }

    let initialize_payload =
        initialize_with_id_and_version(json!(INITIALIZE_ID), &config.protocol_version);
    let unauthenticated = post_with_timeout(
        transport,
        ProbeRequest {
            url: url.clone(),
            payload: initialize_payload.clone(),
            bearer_token: None,
            session_id: None,
            protocol_version: None,
        },
        config.request_timeout,
        "auth_negative",
        config.mode,
    )
    .await?;
    if !matches!(unauthenticated.status, 401 | 403) {
        bail!(
            "auth_negative=FAIL expected 401 or 403 without Authorization, got {}",
            unauthenticated.status
        );
    }
    write_probe_result(
        output,
        config,
        TestStatus::Pass,
        "auth_negative",
        &format!("status={}", unauthenticated.status),
        "failed to write negative authentication result",
    )?;

    let started = Instant::now();
    let authenticated = loop {
        let attempt_timeout = if config.config_timeout.is_zero() {
            config.request_timeout
        } else {
            config
                .request_timeout
                .min(config.config_timeout.saturating_sub(started.elapsed()))
        };
        let response = post_with_timeout(
            transport,
            ProbeRequest {
                url: url.clone(),
                payload: initialize_payload.clone(),
                bearer_token: Some(config.bearer_token.clone()),
                session_id: None,
                protocol_version: None,
            },
            attempt_timeout,
            "initialize",
            config.mode,
        )
        .await?;
        if response.status == 200
            || config.config_timeout.is_zero()
            || started.elapsed() >= config.config_timeout
        {
            break response;
        }
        let remaining = config.config_timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break response;
        }
        tokio::time::sleep(config.retry_interval.max(MIN_RETRY_INTERVAL).min(remaining)).await;
        if started.elapsed() >= config.config_timeout {
            break response;
        }
    };

    let initialize_result = result_of("initialize", &authenticated, INITIALIZE_ID)?;
    let negotiated_version = initialize_result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .filter(|version| !version.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("initialize=FAIL missing negotiated protocolVersion"))?;
    let session_id = authenticated
        .session_id
        .filter(|session_id| !session_id.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("initialize=FAIL no Mcp-Session-Id header in response"))?;
    write_probe_result(
        output,
        config,
        TestStatus::Pass,
        "initialize",
        &format!("status={} session=present", authenticated.status),
        "failed to write initialize result",
    )?;

    let initialized_response = post_with_timeout(
        transport,
        ProbeRequest {
            url: url.clone(),
            payload: json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }),
            bearer_token: Some(config.bearer_token.clone()),
            session_id: Some(session_id.clone()),
            protocol_version: Some(negotiated_version.clone()),
        },
        config.request_timeout,
        "initialized",
        config.mode,
    )
    .await?;
    accepted_empty("initialized", &initialized_response)?;
    write_probe_result(
        output,
        config,
        TestStatus::Pass,
        "initialized",
        "status=202",
        "failed to write initialized notification result",
    )?;

    let tool_names = if config.tool_names.is_empty() {
        let tools_response = post_with_timeout(
            transport,
            ProbeRequest {
                url: url.clone(),
                payload: jsonrpc_with_id("tools/list", Some(json!({})), json!(TOOLS_LIST_ID)),
                bearer_token: Some(config.bearer_token.clone()),
                session_id: Some(session_id.clone()),
                protocol_version: Some(negotiated_version.clone()),
            },
            config.request_timeout,
            "tools_list",
            config.mode,
        )
        .await?;
        listed_tool_names(&tools_response, output, config)?
    } else {
        write_mocked_tool_catalog(output, config)?;
        config.tool_names.clone()
    };
    let callable = tool_names
        .iter()
        .find_map(|name| tool_call_args(name).map(|arguments| (name.as_str(), arguments)));
    let Some((tool_name, arguments)) = callable else {
        write_probe_result(
            output,
            config,
            TestStatus::Skip,
            "tool_call",
            "no echo/get_system_time tool available",
            "failed to write tool call skip result",
        )?;
        return Ok(());
    };

    let call_response = post_with_timeout(
        transport,
        ProbeRequest {
            url,
            payload: jsonrpc_with_id(
                "tools/call",
                Some(json!({"name": tool_name, "arguments": arguments})),
                json!(TOOL_CALL_ID),
            ),
            bearer_token: Some(config.bearer_token.clone()),
            session_id: Some(session_id),
            protocol_version: Some(negotiated_version),
        },
        config.request_timeout,
        "tool_call",
        config.mode,
    )
    .await?;
    let call_result = result_of("tool_call", &call_response, TOOL_CALL_ID)?;
    if !matches!(call_result.get("content"), Some(Value::Array(_))) {
        bail!("tool_call=FAIL result must contain a content array");
    }
    let is_error = match call_result.get("isError") {
        None => false,
        Some(Value::Bool(is_error)) => *is_error,
        Some(_) => bail!("tool_call=FAIL isError must be a boolean when present"),
    };
    if is_error {
        bail!("tool_call=FAIL tool returned error");
    }
    write_probe_result(
        output,
        config,
        TestStatus::Pass,
        "tool_call",
        &format!("tool={}", sanitize_for_output(tool_name)),
        "failed to write tool call result",
    )?;

    Ok(())
}

async fn run_stateless_probe<T: ProbeTransport, W: Write>(
    transport: &T,
    config: &ProbeConfig,
    output: &mut W,
    url: String,
) -> Result<()> {
    let discover_payload = stateless_jsonrpc_with_id(
        "server/discover",
        None,
        json!(INITIALIZE_ID),
        &config.protocol_version,
    );
    let unauthenticated = post_with_timeout(
        transport,
        ProbeRequest {
            url: url.clone(),
            payload: discover_payload.clone(),
            bearer_token: None,
            session_id: None,
            protocol_version: Some(config.protocol_version.clone()),
        },
        config.request_timeout,
        "auth_negative",
        config.mode,
    )
    .await?;
    if unauthenticated.status != 401 {
        bail!(
            "auth_negative=FAIL expected 401 without Authorization, got {}",
            unauthenticated.status
        );
    }
    write_probe_result(
        output,
        config,
        TestStatus::Pass,
        "auth_negative",
        "status=401",
        "failed to write negative authentication result",
    )?;

    let started = Instant::now();
    let authenticated = loop {
        let attempt_timeout = if config.config_timeout.is_zero() {
            config.request_timeout
        } else {
            config
                .request_timeout
                .min(config.config_timeout.saturating_sub(started.elapsed()))
        };
        let response = post_with_timeout(
            transport,
            ProbeRequest {
                url: url.clone(),
                payload: discover_payload.clone(),
                bearer_token: Some(config.bearer_token.clone()),
                session_id: None,
                protocol_version: Some(config.protocol_version.clone()),
            },
            attempt_timeout,
            "server_discover",
            config.mode,
        )
        .await?;
        if response.status == 200
            || config.config_timeout.is_zero()
            || started.elapsed() >= config.config_timeout
        {
            break response;
        }
        let remaining = config.config_timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break response;
        }
        tokio::time::sleep(config.retry_interval.max(MIN_RETRY_INTERVAL).min(remaining)).await;
        if started.elapsed() >= config.config_timeout {
            break response;
        }
    };

    let discovery = result_of("server_discover", &authenticated, INITIALIZE_ID)?;
    let supports_version = discovery
        .get("supportedVersions")
        .and_then(Value::as_array)
        .is_some_and(|versions| {
            versions
                .iter()
                .any(|version| version.as_str() == Some(config.protocol_version.as_str()))
        });
    if !supports_version {
        bail!("server_discover=FAIL requested protocol version is not advertised by the server");
    }
    if discovery
        .get("capabilities")
        .and_then(Value::as_object)
        .is_none()
        || discovery
            .get("resultType")
            .and_then(Value::as_str)
            .is_none()
        || discovery
            .get("cacheScope")
            .and_then(Value::as_str)
            .is_none()
        || discovery.get("ttlMs").and_then(Value::as_u64).is_none()
    {
        bail!("server_discover=FAIL response is missing required discovery fields");
    }
    write_probe_result(
        output,
        config,
        TestStatus::Pass,
        "server_discover",
        "status=200 lifecycle=stateless",
        "failed to write server discovery result",
    )?;

    let tool_names = if config.tool_names.is_empty() {
        let tools_response = post_with_timeout(
            transport,
            ProbeRequest {
                url: url.clone(),
                payload: stateless_jsonrpc_with_id(
                    "tools/list",
                    Some(json!({})),
                    json!(TOOLS_LIST_ID),
                    &config.protocol_version,
                ),
                bearer_token: Some(config.bearer_token.clone()),
                session_id: None,
                protocol_version: Some(config.protocol_version.clone()),
            },
            config.request_timeout,
            "tools_list",
            config.mode,
        )
        .await?;
        listed_tool_names(&tools_response, output, config)?
    } else {
        write_mocked_tool_catalog(output, config)?;
        config.tool_names.clone()
    };
    let callable = tool_names
        .iter()
        .find_map(|name| tool_call_args(name).map(|arguments| (name.as_str(), arguments)));
    let Some((tool_name, arguments)) = callable else {
        write_probe_result(
            output,
            config,
            TestStatus::Skip,
            "tool_call",
            "no echo/get_system_time tool available",
            "failed to write tool call skip result",
        )?;
        return Ok(());
    };
    let call_response = post_with_timeout(
        transport,
        ProbeRequest {
            url,
            payload: stateless_jsonrpc_with_id(
                "tools/call",
                Some(json!({"name": tool_name, "arguments": arguments})),
                json!(TOOL_CALL_ID),
                &config.protocol_version,
            ),
            bearer_token: Some(config.bearer_token.clone()),
            session_id: None,
            protocol_version: Some(config.protocol_version.clone()),
        },
        config.request_timeout,
        "tool_call",
        config.mode,
    )
    .await?;
    let call_result = result_of("tool_call", &call_response, TOOL_CALL_ID)?;
    if !matches!(call_result.get("content"), Some(Value::Array(_))) {
        bail!("tool_call=FAIL result must contain a content array");
    }
    if call_result
        .get("isError")
        .is_some_and(|value| value != &Value::Bool(false))
    {
        bail!("tool_call=FAIL tool returned error or a malformed isError value");
    }
    write_probe_result(
        output,
        config,
        TestStatus::Pass,
        "tool_call",
        &format!("tool={}", sanitize_for_output(tool_name)),
        "failed to write tool call result",
    )?;
    Ok(())
}

fn listed_tool_names<W: Write>(
    response: &ProbeResponse,
    output: &mut W,
    config: &ProbeConfig,
) -> Result<Vec<String>> {
    let tools_result = result_of("tools_list", response, TOOLS_LIST_ID)?;
    let tools = tools_result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("tools_list=FAIL unexpected response: missing tools array"))?;
    if tools.is_empty() {
        bail!("tools_list=FAIL no tools returned");
    }
    let tool_names = tools
        .iter()
        .map(|tool| {
            tool.as_object()
                .and_then(|tool| tool.get("name"))
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("tools_list=FAIL every tool must have a nonempty name"))
        })
        .collect::<Result<Vec<_>>>()?;
    write_probe_result(
        output,
        config,
        TestStatus::Pass,
        "tools_list",
        &format!("count={}", tool_names.len()),
        "failed to write tools list result",
    )?;
    Ok(tool_names)
}

fn write_mocked_tool_catalog<W: Write>(output: &mut W, config: &ProbeConfig) -> Result<()> {
    write_probe_result(
        output,
        config,
        TestStatus::Pass,
        "tools_catalog",
        &format!("count={} source=mocked-redis", config.tool_names.len()),
        "failed to write mocked tool catalog result",
    )
}

async fn post_with_timeout<T: ProbeTransport>(
    transport: &T,
    request: ProbeRequest,
    timeout: Duration,
    step: &'static str,
    mode: GatewayTopology,
) -> Result<ProbeResponse> {
    match tokio::time::timeout(timeout, transport.post(request)).await {
        Ok(Ok(response)) => {
            if mode.requires_dataplane()
                && let Some(message) = response.backend_identity.dataplane_error()
            {
                bail!("{step}=FAIL {message}");
            }
            Ok(response)
        }
        Ok(Err(_)) => bail!("{step} request failed"),
        Err(_) => bail!("{step} request timed out"),
    }
}

fn accepted_empty(step: &str, response: &ProbeResponse) -> Result<()> {
    if response.status != 202 {
        bail!("{step}=FAIL status={}", response.status);
    }
    if response.message.is_some() {
        bail!("{step}=FAIL expected an empty response");
    }
    Ok(())
}

fn result_of<'a>(
    step: &str,
    response: &'a ProbeResponse,
    expected_id: u64,
) -> Result<&'a Map<String, Value>> {
    if response.status != 200 {
        bail!("{step}=FAIL status={}", response.status);
    }
    let Some(message) = response.message.as_ref() else {
        bail!("{step}=FAIL unexpected response: no JSON-RPC message");
    };
    let Some(message_object) = message.as_object() else {
        bail!("{step}=FAIL response must be a JSON-RPC object");
    };
    if message_object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        bail!("{step}=FAIL invalid JSON-RPC version");
    }
    if message_object.get("id").and_then(Value::as_u64) != Some(expected_id) {
        bail!("{step}=FAIL response ID mismatch");
    };
    if message_object.contains_key("error") {
        bail!("{step}=FAIL JSON-RPC error");
    }
    message_object
        .get("result")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("{step}=FAIL response result must be an object"))
}

fn probe_url(base_url: &str, mode: GatewayTopology, server_id: &str) -> Result<String> {
    let mut url = Url::parse(base_url).map_err(|_| anyhow!("invalid probe base URL"))?;
    url.set_query(None);
    url.set_fragment(None);
    let mut segments = url
        .path_segments_mut()
        .map_err(|()| anyhow!("probe base URL must be hierarchical"))?;
    segments.pop_if_empty();
    if mode.requires_dataplane() {
        segments.push("servers");
        segments.push(server_id);
    }
    segments.push("mcp");
    drop(segments);
    Ok(url.into())
}

fn sanitize_for_output(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => sanitized.push_str("\\n"),
            '\r' => sanitized.push_str("\\r"),
            '\t' => sanitized.push_str("\\t"),
            character if character.is_control() => {
                sanitized.push_str(&format!("\\u{{{:04x}}}", character as u32));
            }
            character => sanitized.push(character),
        }
    }
    sanitized
}

fn write_probe_result<W: Write>(
    output: &mut W,
    config: &ProbeConfig,
    status: TestStatus,
    step: &str,
    detail: &str,
    error: &'static str,
) -> Result<()> {
    let name = if detail.is_empty() {
        step.to_owned()
    } else {
        format!("{step} {detail}")
    };
    write_line(
        output,
        &config.output_style.test_result(status, &name, None, None),
        error,
    )
}

fn write_line<W: Write>(output: &mut W, line: &str, error: &'static str) -> Result<()> {
    writeln!(output, "{line}").map_err(|_| anyhow!(error))
}
