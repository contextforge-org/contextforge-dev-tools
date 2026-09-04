//! MCP probe workflow orchestration.

use super::*;

impl<R: ProcessRunner> RuntimeContext<R> {
    pub(super) async fn run_probe(
        &self,
        topology: StackMode,
        standalone: bool,
        protocol_version: &ProtocolVersion,
    ) -> AppResult<()> {
        let server_id = self.default_server_id().to_owned();
        self.with_managed_authenticated_target(
            topology,
            &server_id,
            standalone,
            true,
            protocol_version,
            |token, tool_names| async {
                let config = ProbeConfig {
                    mode: gateway_topology(topology),
                    base_url: self.base_url()?.to_owned(),
                    server_id: server_id.clone(),
                    bearer_token: token,
                    config_timeout: Duration::from_secs(
                        self.environment_u64("CF_PROBE_CONFIG_TIMEOUT", 120)?,
                    ),
                    retry_interval: Duration::from_secs(5),
                    request_timeout: Duration::from_secs(
                        self.environment_u64("CF_PROBE_REQUEST_TIMEOUT", 30)?,
                    ),
                    protocol_version: protocol_version.wire_version().to_owned(),
                    tool_names,
                    output_style: OutputStyle::stdout(),
                };
                let transport = GatewayClient::builder(
                    config.mode,
                    &config.base_url,
                    &config.server_id,
                    &config.bearer_token,
                )
                .protocol_version(config.protocol_version.clone())
                .build()
                .map_err(|error| AppFailure::from(anyhow!(error)))?;
                let stdout = std::io::stdout();
                let mut output = stdout.lock();
                run_probe(&transport, &config, &mut output)
                    .await
                    .map_err(AppFailure::from)
            },
        )
        .await
    }
}
