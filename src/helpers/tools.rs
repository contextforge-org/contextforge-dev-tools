//! Upstream tools run only inside the image containing their pinned packages.

use std::ffi::OsString;
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use url::Url;

use crate::conformance::client::{CLIENT_TOKEN_ENV, INTERNAL_CLIENT_COMMAND};
use crate::conformance::results::{
    DEFAULT_CONFORMANCE_SUITE, official_client_command, official_server_command,
};
use crate::error::AppFailure;
use crate::infrastructure::process::{CommandSpec, ProcessRunner, SystemProcessRunner};
use crate::mcp::auth_proxy::AuthProxy;
use crate::mcp::backend_identity::is_dataplane_endpoint;

#[derive(Parser)]
struct ToolArgs {
    #[command(subcommand)]
    command: Tool,
}

#[derive(Subcommand)]
enum Tool {
    Server {
        endpoint: Url,
        spec_version: String,
        #[arg(long)]
        proxy: Option<ProxyMode>,
    },
    Client {
        scenario: String,
        spec_version: String,
    },
    Inspect {
        endpoint: Url,
        spec_version: String,
        method: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ProxyMode {
    Builtin,
    External,
}

/// Uses Compose DNS for local gateways; remote endpoints retain their address.
fn gateway_destination(endpoint: &Url) -> Result<Url> {
    let loopback = match endpoint.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    let mut destination = endpoint.clone();
    if loopback {
        destination.set_host(Some("nginx"))?;
        destination
            .set_port(None)
            .map_err(|()| anyhow::anyhow!("invalid gateway port"))?;
        destination
            .set_scheme("http")
            .map_err(|()| anyhow::anyhow!("invalid gateway scheme"))?;
    }
    Ok(destination)
}

async fn proxy(endpoint: Url, version: Option<&str>, require_dataplane: bool) -> Result<AuthProxy> {
    let token = std::env::var(CLIENT_TOKEN_ENV).context("MCP_CONFORMANCE_TOKEN is required")?;
    Ok(AuthProxy::start_routed(
        endpoint.clone(),
        gateway_destination(&endpoint)?,
        &token,
        version,
        require_dataplane,
    )
    .await?)
}

pub(crate) async fn run(arguments: &[OsString]) -> Result<(), AppFailure> {
    let tool = ToolArgs::try_parse_from(arguments)
        .map_err(anyhow::Error::from)?
        .command;
    let expected = Path::new("/artifacts/expected-failures.yml");
    let results = Path::new("/artifacts/official");
    let (command, proxy) = match tool {
        Tool::Server {
            endpoint,
            spec_version,
            proxy: mode,
        } => {
            let proxy = match mode {
                Some(mode) => {
                    Some(proxy(endpoint.clone(), None, matches!(mode, ProxyMode::External)).await?)
                }
                None => None,
            };
            let url = proxy.as_ref().map_or(&endpoint, AuthProxy::url);
            (
                official_server_command(
                    url.as_str(),
                    DEFAULT_CONFORMANCE_SUITE,
                    &spec_version,
                    expected,
                    results,
                ),
                proxy,
            )
        }
        Tool::Client {
            scenario,
            spec_version,
        } => (
            official_client_command(
                &format!("cf-integration {INTERNAL_CLIENT_COMMAND}"),
                &scenario,
                &spec_version,
                expected,
                results,
            ),
            None,
        ),
        Tool::Inspect {
            endpoint,
            spec_version,
            method,
        } => {
            let require_dataplane = is_dataplane_endpoint(&endpoint);
            let proxy = proxy(endpoint, Some(&spec_version), require_dataplane).await?;
            let command = CommandSpec::new("mcp-inspector").args([
                "--cli",
                proxy.url().as_str(),
                "--transport",
                "http",
                "--method",
                &method,
            ]);
            (command, Some(proxy))
        }
    };
    let mut command = command.clear_environment();
    for key in [
        "PATH",
        "HOME",
        CLIENT_TOKEN_ENV,
        "CF_CONFIG_REDIS_URL",
        "CF_CLIENT_CONFORMANCE_BASE_URL",
        "CF_CLIENT_CONFORMANCE_SERVER_ID",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command = command.env(key, value);
        }
    }
    let result = SystemProcessRunner
        .run_async(&command)
        .await
        .map_err(AppFailure::from);
    if let Some(proxy) = proxy {
        let cleanup = proxy.shutdown().await.map_err(anyhow::Error::from);
        if result.is_ok() {
            cleanup?;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_gateway_routing_preserves_paths_and_remote_endpoints() {
        for host in ["127.0.0.1", "localhost", "[::1]"] {
            let public =
                Url::parse(&format!("http://{host}:8080/servers/id/mcp?key=value")).expect("URL");
            assert_eq!(
                gateway_destination(&public)
                    .expect("Docker destination")
                    .as_str(),
                "http://nginx/servers/id/mcp?key=value"
            );
        }
        let remote = Url::parse("https://gateway.example/servers/id/mcp").expect("URL");
        assert_eq!(
            gateway_destination(&remote).expect("remote destination"),
            remote
        );
    }
}
