//! Locust performance workflow orchestration.

use super::*;

impl<R: ProcessRunner> RuntimeContext<R> {
    pub(super) async fn run_load(&self, args: ResolvedLoadArgs) -> AppResult<()> {
        let settings =
            LoadSettings::resolve(&self.config, &args.request).map_err(AppFailure::from)?;
        let server_id = self.default_server_id().to_owned();
        let operation_server_id = server_id.clone();
        let preparation = Activity::spinner("Preparing performance stack");
        self.with_managed_performance_target(
            args.topology,
            &server_id,
            args.standalone,
            args.observability,
            |token, standalone_tool_names| async move {
                let command = LocustCommand::new_with_protocol_version(
                    &self.config,
                    args.topology,
                    &settings,
                    &token,
                    (args.topology == StackMode::Dataplane).then_some(operation_server_id.as_str()),
                    args.protocol_version.wire_version(),
                )
                .map_err(AppFailure::from)?;
                let mut command_spec =
                    self.compose_environment(command.command().clone(), args.topology, true)?;
                if args.standalone {
                    command_spec = command_spec
                        .env("MCP_TOOL_NAMES", standalone_tool_names.join(","))
                        .env("MCP_SKIP_TOOL_LIST", "true");
                }
                let output_log = command.report_dir().join("locust.log");
                fs::write(&output_log, [])
                    .with_context(|| format!("failed to clear Locust output log {output_log:?}"))
                    .map_err(AppFailure::from)?;
                preparation.finish(true);

                let description = format!(
                    "Running load test ({} users, {}/s, {})",
                    settings.users(),
                    settings.spawn_rate(),
                    settings.run_time(),
                );
                let activity = Activity::spinner(description);
                let started = std::time::Instant::now();
                let process_result = self
                    .runner
                    .run_to_log(&command_spec, &output_log)
                    .map_err(AppFailure::from);
                let result = finalize_locust_run(process_result, command.report_dir(), &token);
                let elapsed = started.elapsed();
                activity.finish(result.is_ok());

                let status = if result.is_ok() {
                    TestStatus::Pass
                } else {
                    TestStatus::Fail
                };
                println!(
                    "{}",
                    OutputStyle::stdout().test_result(
                        status,
                        &format!("performance::{}", args.topology.lane_label()),
                        Some(elapsed),
                        None,
                    )
                );
                if result.is_ok() {
                    println!(
                        "{}",
                        OutputStyle::stdout().info(&format!(
                            "Report: {}",
                            command.report_dir().join("locust_report.html").display()
                        ))
                    );
                } else if output_log.is_file() {
                    eprintln!(
                        "{}",
                        OutputStyle::stderr()
                            .failure(&format!("Load output: {}", output_log.display()))
                    );
                }
                result
            },
        )
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
