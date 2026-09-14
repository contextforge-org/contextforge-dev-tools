//! Locust performance workflow orchestration.

use super::*;

impl<R: ProcessRunner> RuntimeContext<R> {
    pub(super) async fn start_standalone_fast_time(&self, observability: bool) -> AppResult<()> {
        let command = self.standalone_dataplane_project(observability).command([
            "up",
            "-d",
            "--wait",
            "fast_time_server",
        ]);
        let command = self.standalone_dataplane_environment(command, true)?;
        self.runner
            .run_async(&command)
            .await
            .map_err(AppFailure::from)
    }

    pub(super) fn publish_standalone_fast_time_config(
        &self,
        server_id: &str,
        protocol_version: &str,
        token: &str,
        observability: bool,
    ) -> AppResult<Vec<String>> {
        let progress = Activity::spinner("Publish Fast Time routing configuration");
        let command = self.standalone_dataplane_project(observability).command([
            "run",
            "--quiet-build",
            "--rm",
            "--no-deps",
            "-e",
            CLIENT_TOKEN_ENV,
            "config_writer",
            "fixture",
            server_id,
            "http://fast_time_server:9080/mcp",
            protocol_version,
        ]);
        let command = self
            .standalone_dataplane_environment(command, true)?
            .env(CLIENT_TOKEN_ENV, token);
        let result = self.capture_text(&command).and_then(|output| {
            let names: Vec<String> = serde_json::from_str(&output)
                .context("Fast Time config helper returned invalid tool names")
                .map_err(AppFailure::from)?;
            if !names.iter().any(|name| name == "echo") {
                return Err(AppFailure::from(anyhow!(
                    "Fast Time backend does not advertise echo; load was not started"
                )));
            }
            Ok(vec!["echo".to_owned()])
        });
        progress.finish(result.is_ok());
        result
    }

    pub(super) async fn run_load(&self, args: ResolvedLoadArgs) -> AppResult<()> {
        let settings =
            LoadSettings::resolve(&self.config, &args.request).map_err(AppFailure::from)?;
        let server_id = self.default_server_id().to_owned();
        let operation_server_id = server_id.clone();
        let preparation = Activity::spinner("Preparing performance stack");
        self.with_managed_authenticated_target(
            args.topology,
            &server_id,
            args.standalone,
            args.observability,
            session::StandaloneBackend::FastTime(args.client_era),
            |token, standalone_tool_names| async move {
                let project = if args.standalone {
                    self.standalone_dataplane_project(args.observability)
                        .with_profiles(["performance"])
                } else {
                    self.performance_compose_project(args.topology, args.observability)
                };
                let command = LocustCommand::new(
                    &self.config,
                    project,
                    args.topology,
                    &settings,
                    &token,
                    (args.topology == StackMode::Dataplane).then_some(operation_server_id.as_str()),
                    args.client_era,
                )
                .map_err(AppFailure::from)?;
                let mut command_spec = self.target_environment(
                    command.command().clone(),
                    args.topology,
                    args.standalone,
                )?;
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
                        &format!("load::{}::{}", args.topology.lane_label(), args.client_era,),
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
