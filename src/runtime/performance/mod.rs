//! Locust performance workflow orchestration.

use super::*;

impl<R: ProcessRunner> RuntimeContext<R> {
    pub(super) async fn run_load(&self, args: ResolvedLoadArgs) -> AppResult<()> {
        let server_id = self.default_server_id().to_owned();
        let operation_server_id = server_id.clone();
        self.with_managed_authenticated_target(args.topology, &server_id, |token| async move {
            let settings =
                LoadSettings::resolve(&self.config, &args.request).map_err(AppFailure::from)?;
            let command = LocustCommand::new_with_protocol_version(
                &self.config,
                args.topology,
                &settings,
                &token,
                (args.topology == StackMode::Dataplane).then_some(operation_server_id.as_str()),
                args.protocol_version.as_str(),
            )
            .map_err(AppFailure::from)?;
            let process_result = self
                .runner
                .run(&self.compose_environment(command.command().clone(), args.topology, true)?)
                .map_err(AppFailure::from);
            finalize_locust_run(process_result, command.report_dir(), &token)
        })
        .await
    }
}

fn finalize_locust_run(
    process_result: AppResult<()>,
    report_dir: &Path,
    bearer_token: &str,
) -> AppResult<()> {
    audit_locust_reports(report_dir, bearer_token).map_err(AppFailure::from)?;
    process_result
}
