//! Official MCP Inspector composition.

use super::*;

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
            true,
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
                let project = if standalone {
                    self.standalone_conformance_compose_project(true)
                } else {
                    self.compose_project(mode)
                }
                .with_tools(self.config.asset_root());
                let compose = self
                    .target_environment(
                        project.command(std::iter::empty::<&str>()),
                        mode,
                        standalone,
                    )?
                    .env(CLIENT_TOKEN_ENV, token);
                let arguments = [
                    "inspect",
                    endpoint.as_str(),
                    protocol_version.wire_version(),
                    method,
                ]
                .map(OsString::from);
                let (sender, receiver) = tokio::sync::watch::channel(false);
                let run = self.run_tool(compose, &arguments, None, None, receiver);
                tokio::pin!(run);
                tokio::select! {
                    result = &mut run => result,
                    _ = tokio::signal::ctrl_c() => {
                        sender.send_replace(true);
                        run.await
                    }
                }
            },
        )
        .await
    }
}
