//! Official MCP Inspector composition.

use super::*;

const INSPECTOR_PACKAGE: &str = "@modelcontextprotocol/inspector@2.2.0";
pub(super) const NPM_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "TMP",
    "TEMP",
    "XDG_CACHE_HOME",
    "NPM_CONFIG_CACHE",
    "npm_config_cache",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
];

impl<R: ProcessRunner> RuntimeContext<R> {
    pub(super) async fn inspect(
        &self,
        mode: StackMode,
        standalone: bool,
        protocol_version: &ProtocolVersion,
        method: &str,
        server_id: Option<&str>,
    ) -> AppResult<()> {
        let server_id = server_id
            .unwrap_or_else(|| self.default_server_id())
            .to_owned();
        let operation_server_id = server_id.clone();
        self.with_managed_authenticated_target(
            mode,
            &server_id,
            standalone,
            protocol_version,
            |token, _| async move {
                let endpoint = GatewayClient::new(
                    gateway_topology(mode),
                    self.base_url()?,
                    &operation_server_id,
                    &token,
                )
                .context("failed to construct the Inspector gateway endpoint")
                .map_err(AppFailure::from)?
                .endpoint()
                .clone();
                let proxy = AuthProxy::start_with_protocol_version(
                    endpoint,
                    &token,
                    Some(protocol_version.wire_version()),
                )
                .await
                .context("failed to start the Inspector authentication proxy")
                .map_err(AppFailure::from)?;
                let command = allowlisted_npx_environment(
                    inspector_command(proxy.url().as_str(), method).cwd(self.config.root()),
                );
                let process_result = self
                    .runner
                    .run_async(&command)
                    .await
                    .map_err(AppFailure::from);
                let shutdown_result = proxy
                    .shutdown()
                    .await
                    .context("failed to stop the Inspector authentication proxy")
                    .map_err(AppFailure::from);
                finish_with_cleanup(process_result.err(), shutdown_result)
            },
        )
        .await
    }
}

pub(super) fn inspector_command(endpoint: &str, method: &str) -> CommandSpec {
    CommandSpec::new("npx").clear_environment().args([
        "-y",
        INSPECTOR_PACKAGE,
        "--cli",
        endpoint,
        "--transport",
        "http",
        "--method",
        method,
    ])
}

pub(super) fn allowlisted_npx_environment(mut command: CommandSpec) -> CommandSpec {
    command = command.clear_environment();
    for key in NPM_ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            command = command.env(key, value);
        }
    }
    command
}
