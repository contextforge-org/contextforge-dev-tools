//! Official conformance orchestration.

use super::*;
use std::fmt::Write as _;
use std::io::{IsTerminal, Write as IoWrite};
use std::time::Instant;

use crate::compliance::conformance::{DEFAULT_CONFORMANCE_SUITE, ScenarioOutcome};

const CONFORMANCE_SERVER_ERA_ENV: &str = "CF_CONFORMANCE_SERVER_ERA";

struct ConformanceProgress {
    task: Option<tokio::task::JoinHandle<()>>,
    terminal: bool,
}

impl ConformanceProgress {
    fn start(description: impl Into<String>) -> Self {
        let description = description.into();
        let terminal = std::io::stderr().is_terminal();
        if !terminal {
            return Self {
                task: None,
                terminal,
            };
        }

        let task = tokio::spawn(async move {
            const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut frame = 0;
            loop {
                {
                    let mut stderr = std::io::stderr().lock();
                    let _ = write!(
                        stderr,
                        "\r\x1b[2KConformance {} {description}",
                        FRAMES[frame % FRAMES.len()]
                    );
                    let _ = stderr.flush();
                }
                frame += 1;
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        });
        Self {
            task: Some(task),
            terminal,
        }
    }
}

impl Drop for ConformanceProgress {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if self.terminal {
            let mut stderr = std::io::stderr().lock();
            let _ = write!(stderr, "\r\x1b[2K");
            let _ = stderr.flush();
        }
    }
}

impl<R: ProcessRunner> RuntimeContext<R> {
    fn require_loopback_fixture_base_url(&self) -> AppResult<()> {
        let base_url = self.base_url()?;
        let url = url::Url::parse(base_url)
            .context("MCP_CLI_BASE_URL is not a valid URL")
            .map_err(AppFailure::from)?;
        let is_loopback = match url.host() {
            Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            None => false,
        };
        if !is_loopback {
            return Err(AppFailure::from(anyhow!(
                "official conformance requires a loopback MCP_CLI_BASE_URL"
            )));
        }
        Ok(())
    }

    pub(super) async fn start_conformance_service(
        &self,
        topology: StackMode,
        server_era: ConformanceServerEra,
    ) -> AppResult<()> {
        let project = self.conformance_compose_project(topology);
        let build = project.command(["build", OFFICIAL_CONFORMANCE_SERVICE]);
        let build = self
            .compose_environment(build, topology, true)?
            .env(CONFORMANCE_SERVER_ERA_ENV, server_era.label());
        self.runner.run_async(&build).await?;

        let up = project.command([
            "up",
            "-d",
            "--wait",
            OFFICIAL_CONFORMANCE_SERVICE,
            OFFICIAL_CONFORMANCE_PROXY_SERVICE,
        ]);
        let up = self
            .compose_environment(up, topology, true)?
            .env(CONFORMANCE_SERVER_ERA_ENV, server_era.label());
        Ok(self.runner.run_async(&up).await?)
    }

    pub(super) fn conformance_fixture_endpoint(&self, topology: StackMode) -> AppResult<url::Url> {
        let command = self.conformance_compose_project(topology).command([
            "port",
            OFFICIAL_CONFORMANCE_SERVICE,
            "3000",
        ]);
        let command = self.compose_environment(command, topology, true)?;
        let output = self.runner.capture_stdout(&command)?;
        parse_conformance_fixture_endpoint(&output).map_err(AppFailure::from)
    }

    async fn stop_conformance_service(&self, topology: StackMode) -> AppResult<()> {
        let remove = self.conformance_compose_project(topology).command([
            "rm",
            "--stop",
            "--force",
            OFFICIAL_CONFORMANCE_PROXY_SERVICE,
            OFFICIAL_CONFORMANCE_SERVICE,
        ]);
        let remove = self.compose_environment(remove, topology, true)?;
        self.runner
            .run_async(&remove)
            .await
            .map_err(AppFailure::from)
    }

    pub(super) async fn execute_conformance(&self, action: ConformanceAction) -> AppResult<()> {
        match action {
            ConformanceAction::Run {
                lanes,
                spec_version,
                server_era,
                results_dir,
            } => {
                let artifact_root = results_dir
                    .as_deref()
                    .unwrap_or_else(|| self.config.integration_dir());
                let setup_log = artifact_root.join("conformance/setup.log");
                if let Some(parent) = setup_log.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| {
                            format!("failed to create conformance log directory {parent:?}")
                        })
                        .map_err(AppFailure::from)?;
                }
                fs::write(&setup_log, [])
                    .with_context(|| format!("failed to clear conformance log {setup_log:?}"))
                    .map_err(AppFailure::from)?;
                let quiet_runner = LoggingProcessRunner::new(&self.runner, &setup_log);
                let executor = RuntimeContext::new(self.config.clone(), quiet_runner);
                let result = executor
                    .run_conformance(&lanes, &spec_version, server_era, results_dir.as_deref())
                    .await;
                println!(
                    "{} {}",
                    OutputStyle::stdout().info("  Setup output"),
                    setup_log.display()
                );
                result
            }
            ConformanceAction::Report {
                results_dir,
                output_dir,
            } => self.regenerate_conformance_report(results_dir.as_deref(), output_dir.as_deref()),
        }
    }

    async fn run_conformance(
        &self,
        lanes: &[ConformanceTarget],
        spec_version: &str,
        server_era: ConformanceServerEra,
        results_dir: Option<&Path>,
    ) -> AppResult<()> {
        self.run_conformance_with_interrupt(lanes, spec_version, server_era, results_dir, async {
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await;
            }
        })
        .await
    }

    async fn run_conformance_with_interrupt<I>(
        &self,
        lanes: &[ConformanceTarget],
        spec_version: &str,
        server_era: ConformanceServerEra,
        results_dir: Option<&Path>,
        interrupt: I,
    ) -> AppResult<()>
    where
        I: Future<Output = ()>,
    {
        if lanes.is_empty() {
            return Err(AppFailure::from(anyhow!(
                "at least one conformance lane must be selected"
            )));
        }
        expected_server_scenarios(DEFAULT_CONFORMANCE_SUITE, spec_version)
            .map_err(AppFailure::from)?;
        self.require_loopback_fixture_base_url()?;

        let paths = CompliancePaths::new(
            results_dir.unwrap_or_else(|| self.config.integration_dir()),
            self.config.root().join("reports"),
        );
        paths.clear_conformance()?;

        let topologies = conformance_topologies(lanes);
        let run_direct = lanes.contains(&ConformanceTarget::FixtureDirect);
        let mut direct_complete = false;
        let mut failures = Vec::new();
        let mut interrupted = false;
        tokio::pin!(interrupt);
        let (cancellation_sender, cancellation_receiver) = tokio::sync::watch::channel(false);

        let cleanup_progress = ConformanceProgress::start("clearing prior integration stacks");
        self.cleanup(TopologySelection::All, CleanupKind::Reset)?;
        drop(cleanup_progress);

        for topology in topologies {
            let target = conformance_target(topology);
            let run_routed = lanes.contains(&target);
            let stack_progress = ConformanceProgress::start(format!(
                "preparing {}",
                conformance_topology_label(topology)
            ));
            let mut topology_failure = self.stack_up_for_conformance(topology, true).await.err();
            drop(stack_progress);
            let mut fixture_state = None;
            let mut fixture_metadata = None;
            let mut fixture_endpoint = None;
            let mut service_started = false;
            let mut managed_token = None;

            if topology_failure.is_none() {
                let fixture_progress = ConformanceProgress::start(format!(
                    "starting the official fixture for {}",
                    conformance_topology_label(topology)
                ));
                let (start_result, start_interrupted) = finish_phase_after_interrupt(
                    self.start_conformance_service(topology, server_era),
                    interrupt.as_mut(),
                )
                .await;
                drop(fixture_progress);
                interrupted |= start_interrupted;
                match start_result {
                    Ok(()) => {
                        service_started = true;
                        if interrupted {
                            topology_failure = Some(interrupted_conformance_failure());
                        } else {
                            match self.conformance_fixture_endpoint(topology) {
                                Ok(endpoint) => {
                                    fixture_endpoint = Some(endpoint);
                                    fixture_metadata = Some(ConformanceFixtureMetadata {
                                        repository: OFFICIAL_CONFORMANCE_REPOSITORY.to_owned(),
                                        revision: OFFICIAL_CONFORMANCE_REVISION.to_owned(),
                                        server_id: OFFICIAL_CONFORMANCE_SERVER_ID.to_owned(),
                                    });
                                }
                                Err(error) => topology_failure = Some(error),
                            }
                        }
                    }
                    Err(error) => {
                        topology_failure = Some(if interrupted {
                            interrupted_conformance_failure()
                        } else {
                            error
                        });
                    }
                }
            }

            if topology_failure.is_none() && run_direct && !direct_complete {
                let run_inputs = fixture_endpoint.as_ref().zip(fixture_metadata.as_ref());
                match run_inputs {
                    Some((endpoint, metadata)) => {
                        let run = DirectConformanceRun {
                            endpoint,
                            spec_version,
                            server_era,
                            fixture: metadata,
                            cancellation: cancellation_receiver.clone(),
                        };
                        let direct = self.run_official_conformance_direct(&run, &paths);
                        tokio::pin!(direct);
                        tokio::select! {
                            result = &mut direct => {
                                direct_complete = true;
                                if let Err(error) = result {
                                    let failure = format!("fixture direct: {error}");
                                    eprintln!(
                                        "{}",
                                        OutputStyle::stderr().failure(
                                            &format!("Conformance failure: {failure}")
                                        )
                                    );
                                    failures.push(failure);
                                }
                            }
                            () = interrupt.as_mut() => {
                                interrupted = true;
                                cancellation_sender.send_replace(true);
                                let _ = direct.await;
                                direct_complete = true;
                                topology_failure = Some(interrupted_conformance_failure());
                            }
                        }
                    }
                    None => {
                        topology_failure = Some(AppFailure::from(anyhow!(
                            "successful fixture startup did not retain its direct endpoint"
                        )));
                    }
                }
            }

            if topology_failure.is_none() && run_routed {
                match self.admin_session_token().await.and_then(|token| {
                    ConformanceFixtureClient::builder(self.base_url()?, token)
                        .build()
                        .map_err(AppFailure::from)
                }) {
                    Ok(client) => {
                        let provision_progress = ConformanceProgress::start(format!(
                            "registering the official fixture for {}",
                            conformance_topology_label(topology)
                        ));
                        let (provision_result, provision_interrupted) =
                            finish_phase_after_interrupt(
                                client.provision(OFFICIAL_CONFORMANCE_BACKEND_URL),
                                interrupt.as_mut(),
                            )
                            .await;
                        drop(provision_progress);
                        interrupted |= provision_interrupted;
                        match provision_result {
                            Ok(fixture) => {
                                if interrupted {
                                    topology_failure = Some(interrupted_conformance_failure());
                                } else if topology == StackMode::Dataplane
                                    && let Err(error) = {
                                        let publisher_progress = ConformanceProgress::start(
                                            "waiting for the external data-plane configuration",
                                        );
                                        let result = self
                                            .wait_for_publisher_snapshot_quiet(&fixture.server_id)
                                            .await;
                                        drop(publisher_progress);
                                        result
                                    }
                                {
                                    topology_failure = Some(error);
                                }
                                fixture_state = Some((client, fixture));
                            }
                            Err(error) => {
                                topology_failure = Some(if interrupted {
                                    interrupted_conformance_failure()
                                } else {
                                    AppFailure::from(error)
                                });
                            }
                        }
                    }
                    Err(error) => topology_failure = Some(error),
                }
            }

            if topology_failure.is_none() && run_routed {
                let run_inputs = fixture_state
                    .as_ref()
                    .map(|(_, fixture)| fixture)
                    .zip(fixture_metadata.as_ref());
                match run_inputs {
                    Some((fixture, metadata)) => match self.issue_conformance_token().await {
                        Ok(token) => {
                            managed_token = Some(token);
                            let token = managed_token
                                .as_ref()
                                .expect("managed token was just stored");
                            let tests = async {
                                self.run_official_conformance_mode(
                                    &OfficialConformanceRun {
                                        topology,
                                        server_id: &fixture.server_id,
                                        token: &token.value,
                                        spec_version,
                                        server_era,
                                        fixture: metadata,
                                        cancellation: cancellation_receiver.clone(),
                                    },
                                    &paths,
                                )
                                .await
                                .err()
                            };
                            tokio::pin!(tests);
                            tokio::select! {
                                failure = &mut tests => topology_failure = failure,
                                () = interrupt.as_mut() => {
                                    interrupted = true;
                                    cancellation_sender.send_replace(true);
                                    let _ = tests.await;
                                    topology_failure = Some(interrupted_conformance_failure());
                                }
                            }
                        }
                        Err(error) => topology_failure = Some(error),
                    },
                    None => {
                        topology_failure = Some(AppFailure::from(anyhow!(
                            "successful fixture setup did not retain its runtime state"
                        )));
                    }
                }
            }

            if let Some(token) = managed_token.as_ref() {
                topology_failure =
                    finish_with_cleanup(topology_failure, self.revoke_managed_token(token).await)
                        .err();
            }

            if let Some((client, fixture)) = fixture_state {
                let api_cleanup = client
                    .cleanup(Some(&fixture))
                    .await
                    .map_err(AppFailure::from);
                let service_cleanup = self.stop_conformance_service(topology).await;
                topology_failure = finish_with_cleanup(
                    topology_failure,
                    combine_cleanup_results(api_cleanup, service_cleanup),
                )
                .err();
            } else if service_started {
                topology_failure = finish_with_cleanup(
                    topology_failure,
                    self.stop_conformance_service(topology).await,
                )
                .err();
            }

            topology_failure = finish_with_cleanup(
                topology_failure,
                self.cleanup(topology_selection(topology), CleanupKind::Down),
            )
            .err();
            if let Some(error) = topology_failure {
                let failure = format!("{} topology: {error}", conformance_topology_label(topology));
                eprintln!(
                    "{}",
                    OutputStyle::stderr().failure(&format!("Conformance failure: {failure}"))
                );
                failures.push(failure);
            }
            if interrupted {
                cancellation_sender.send_replace(true);
                break;
            }
        }

        if !interrupted {
            match self.write_comparison_from_artifacts(
                &paths,
                Some((spec_version, server_era, DEFAULT_CONFORMANCE_SUITE)),
            ) {
                Ok(path) => println!(
                    "{} {}",
                    OutputStyle::stdout().info("Conformance comparison:"),
                    path.display()
                ),
                Err(error) => {
                    let failure = format!("comparison report: {error}");
                    eprintln!(
                        "{}",
                        OutputStyle::stderr().failure(&format!("Conformance failure: {failure}"))
                    );
                    failures.push(failure);
                }
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(AppFailure::from(anyhow!(
                "conformance run completed with failures:\n- {}",
                failures.join("\n- ")
            )))
        }
    }

    async fn run_official_conformance_mode(
        &self,
        run: &OfficialConformanceRun<'_>,
        paths: &CompliancePaths,
    ) -> AppResult<()> {
        let target = conformance_target(run.topology);
        let endpoint = GatewayClient::builder(
            GatewayTopology::Dataplane,
            self.base_url()?,
            run.server_id,
            run.token,
        )
        .protocol_version(run.spec_version)
        .build()
        .context("failed to construct the conformance gateway endpoint")
        .map_err(AppFailure::from)?
        .endpoint()
        .clone();
        let proxy = match run.topology {
            StackMode::Controlplane => {
                AuthProxy::start_builtin_data_plane(endpoint, run.token).await
            }
            StackMode::Dataplane => AuthProxy::start(endpoint, run.token).await,
        }
        .context("failed to start the conformance authentication proxy")
        .map_err(AppFailure::from)?;
        let result = self
            .run_official_conformance_target(
                &ConformanceTargetRun {
                    target,
                    endpoint: proxy.url(),
                    spec_version: run.spec_version,
                    server_era: run.server_era,
                    fixture: run.fixture,
                    cancellation: run.cancellation.clone(),
                },
                paths,
            )
            .await;
        let shutdown = proxy
            .shutdown()
            .await
            .context("failed to stop the conformance authentication proxy")
            .map_err(AppFailure::from);
        finish_with_cleanup(result.err(), shutdown)
    }

    async fn run_official_conformance_direct(
        &self,
        run: &DirectConformanceRun<'_>,
        paths: &CompliancePaths,
    ) -> AppResult<()> {
        self.run_official_conformance_target(
            &ConformanceTargetRun {
                target: ConformanceTarget::FixtureDirect,
                endpoint: run.endpoint,
                spec_version: run.spec_version,
                server_era: run.server_era,
                fixture: run.fixture,
                cancellation: run.cancellation.clone(),
            },
            paths,
        )
        .await
    }

    async fn run_official_conformance_target(
        &self,
        run: &ConformanceTargetRun<'_>,
        paths: &CompliancePaths,
    ) -> AppResult<()> {
        let expected_scenarios =
            expected_server_scenarios(DEFAULT_CONFORMANCE_SUITE, run.spec_version)
                .map_err(AppFailure::from)?;
        let lane_paths = paths.conformance_lane(run.target);
        remove_file_if_exists(&lane_paths.completion)?;
        recreate_directory(&lane_paths.official_results)?;
        fs::create_dir_all(&lane_paths.root)
            .with_context(|| {
                format!(
                    "failed to create conformance artifact directory {:?}",
                    lane_paths.root
                )
            })
            .map_err(AppFailure::from)?;
        fs::write(&lane_paths.expected_failures, "server: []\n")
            .with_context(|| {
                format!(
                    "failed to write empty expected-failure file {:?}",
                    lane_paths.expected_failures
                )
            })
            .map_err(AppFailure::from)?;
        write_run_metadata(
            &lane_paths.metadata,
            &ConformanceRunMetadata {
                oracle: crate::compliance::conformance::OFFICIAL_CONFORMANCE_PACKAGE.to_owned(),
                target: run.target.label().to_owned(),
                spec_version: run.spec_version.to_owned(),
                server_era: run.server_era,
                suite: DEFAULT_CONFORMANCE_SUITE.to_owned(),
                fixture: Some(run.fixture.clone()),
            },
        )?;

        let command = allowlisted_npx_environment(
            official_server_command(
                run.endpoint.as_str(),
                DEFAULT_CONFORMANCE_SUITE,
                run.spec_version,
                &lane_paths.expected_failures,
                &lane_paths.official_results,
            )
            .cwd(self.config.root()),
        );
        let style = OutputStyle::stdout();
        println!(
            "{}",
            render_conformance_lane_header(
                run.target,
                expected_scenarios.len(),
                run.spec_version,
                run.server_era,
                style,
            )
        );
        let started = Instant::now();
        let runner_progress = ConformanceProgress::start(format!(
            "running {} scenarios for {}",
            expected_scenarios.len(),
            run.target
        ));
        let process_result = self
            .runner
            .run_async_cancellable_to_log(
                &command,
                run.cancellation.clone(),
                &lane_paths.runner_log,
            )
            .await
            .map_err(AppFailure::from);
        drop(runner_progress);

        let results = load_server_results(&lane_paths.official_results).map_err(AppFailure::from);
        if !conformance_process_completed(&process_result) {
            return process_result;
        }
        let results = results?;
        mark_conformance_complete(
            &process_result,
            &results,
            run.target,
            DEFAULT_CONFORMANCE_SUITE,
            run.spec_version,
            &lane_paths.completion,
        )?;
        println!(
            "{}",
            render_conformance_lane_results(run.target, &results, started.elapsed(), style)
        );
        println!(
            "{} {}",
            style.info("   Artifacts"),
            lane_paths.root.display()
        );
        println!(
            "{} {}",
            style.info(" Full output"),
            lane_paths.runner_log.display()
        );
        process_result?;
        Ok(())
    }
}

fn render_conformance_lane_header(
    target: ConformanceTarget,
    scenario_count: usize,
    spec_version: &str,
    server_era: ConformanceServerEra,
    style: OutputStyle,
) -> String {
    let divider = style.info("────────────");
    let lane = style.heading(&format!(" MCP conformance lane: {target}"));
    format!(
        "{divider}\n{lane}\n    Starting {scenario_count} scenarios with {} (client {spec_version}, server {server_era})",
        crate::compliance::conformance::OFFICIAL_CONFORMANCE_PACKAGE
    )
}

fn render_conformance_lane_results(
    target: ConformanceTarget,
    results: &ConformanceResults,
    elapsed: Duration,
    style: OutputStyle,
) -> String {
    let total = results.scenarios.len();
    let mut passed = 0;
    let mut failed = 0;
    let mut skipped = 0;
    let mut ambiguous = 0;
    let mut output = String::new();

    for (index, result) in results.scenarios.values().enumerate() {
        let status = match result.outcome_with_trusted_fixture(true) {
            ScenarioOutcome::Compliant => {
                passed += 1;
                style.success(&format!("{:>12}", "PASS"))
            }
            ScenarioOutcome::NonCompliant | ScenarioOutcome::FixtureFailure => {
                failed += 1;
                style.failure(&format!("{:>12}", "FAIL"))
            }
            ScenarioOutcome::NotApplicable => {
                skipped += 1;
                style.warning(&format!("{:>12}", "SKIP"))
            }
            ScenarioOutcome::Ambiguous | ScenarioOutcome::Missing => {
                ambiguous += 1;
                style.unknown(&format!("{:>12}", "UNKNOWN"))
            }
        };
        let _ = writeln!(
            output,
            "{status} ({}/{total}) {}",
            index + 1,
            result.scenario
        );
    }

    let divider = style.info("────────────");
    let summary = if failed > 0 {
        style.failure_heading("Summary")
    } else if ambiguous > 0 {
        style.unknown_heading("Summary")
    } else {
        style.success_heading("Summary")
    };
    let _ = write!(
        output,
        "{divider}\n     {summary} [{:>8.3}s] {total} scenarios run for {target}: {passed} passed, {failed} failed, {skipped} skipped, {ambiguous} unknown",
        elapsed.as_secs_f64()
    );
    output
}

fn conformance_topologies(lanes: &[ConformanceTarget]) -> Vec<StackMode> {
    let mut topologies = Vec::new();
    if lanes.contains(&ConformanceTarget::BuiltInDataPlane) {
        topologies.push(StackMode::Controlplane);
    }
    if lanes.contains(&ConformanceTarget::ExternalDataPlane) {
        topologies.push(StackMode::Dataplane);
    }
    if topologies.is_empty() {
        topologies.push(StackMode::Controlplane);
    }
    topologies
}

const fn conformance_topology_label(topology: StackMode) -> &'static str {
    match topology {
        StackMode::Controlplane => "built-in data-plane route",
        StackMode::Dataplane => "external data-plane route",
    }
}

fn parse_conformance_fixture_endpoint(output: &[u8]) -> anyhow::Result<url::Url> {
    let output = std::str::from_utf8(output).context("Compose fixture port output is not UTF-8")?;
    let address = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| anyhow!("Compose did not publish the conformance fixture port"))?
        .parse::<std::net::SocketAddr>()
        .context("Compose returned an invalid conformance fixture address")?;
    if !address.ip().is_loopback() {
        return Err(anyhow!(
            "Compose published the conformance fixture on non-loopback address {}",
            address.ip()
        ));
    }
    url::Url::parse(&format!("http://{address}/mcp"))
        .context("failed to construct the direct conformance fixture URL")
}

struct OfficialConformanceRun<'a> {
    topology: StackMode,
    server_id: &'a str,
    token: &'a str,
    spec_version: &'a str,
    server_era: ConformanceServerEra,
    fixture: &'a ConformanceFixtureMetadata,
    cancellation: tokio::sync::watch::Receiver<bool>,
}

struct DirectConformanceRun<'a> {
    endpoint: &'a url::Url,
    spec_version: &'a str,
    server_era: ConformanceServerEra,
    fixture: &'a ConformanceFixtureMetadata,
    cancellation: tokio::sync::watch::Receiver<bool>,
}

struct ConformanceTargetRun<'a> {
    target: ConformanceTarget,
    endpoint: &'a url::Url,
    spec_version: &'a str,
    server_era: ConformanceServerEra,
    fixture: &'a ConformanceFixtureMetadata,
    cancellation: tokio::sync::watch::Receiver<bool>,
}

fn combine_cleanup_results(first: AppResult<()>, second: AppResult<()>) -> AppResult<()> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(first), Err(second)) => Err(AppFailure::from(anyhow!(
            "{first}; additionally conformance service cleanup failed: {second}"
        ))),
    }
}

async fn finish_phase_after_interrupt<F, I, T>(
    operation: F,
    interrupt: std::pin::Pin<&mut I>,
) -> (T, bool)
where
    F: Future<Output = T>,
    I: Future<Output = ()>,
{
    tokio::pin!(operation);
    tokio::select! {
        output = &mut operation => (output, false),
        () = interrupt => (operation.await, true),
    }
}

fn interrupted_conformance_failure() -> AppFailure {
    AppFailure::from(anyhow!("conformance workflow interrupted by Ctrl-C"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compliance::conformance::{
        CheckStatus, ConformanceCheck, ConformanceScenarioResult,
    };

    fn conformance_result(scenario: &str, status: CheckStatus) -> ConformanceScenarioResult {
        ConformanceScenarioResult {
            scenario: scenario.to_owned(),
            checks: vec![ConformanceCheck {
                id: format!("{scenario}-check"),
                name: None,
                description: None,
                status,
                timestamp: None,
                spec_references: Vec::new(),
                error_message: None,
                details: None,
                metadata: None,
                logs: None,
                extensions: Default::default(),
            }],
            source: PathBuf::from(format!("server-{scenario}/checks.json")),
        }
    }

    fn mixed_conformance_results() -> ConformanceResults {
        ConformanceResults {
            scenarios: [
                (
                    "passing".to_owned(),
                    conformance_result("passing", CheckStatus::Success),
                ),
                (
                    "failing".to_owned(),
                    conformance_result("failing", CheckStatus::Failure),
                ),
            ]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn lane_selection_uses_only_required_stack_topologies() {
        assert_eq!(
            conformance_topologies(&[ConformanceTarget::FixtureDirect]),
            [StackMode::Controlplane]
        );
        assert_eq!(
            conformance_topologies(&[
                ConformanceTarget::FixtureDirect,
                ConformanceTarget::ExternalDataPlane,
            ]),
            [StackMode::Dataplane]
        );
        assert_eq!(
            conformance_topologies(&[
                ConformanceTarget::BuiltInDataPlane,
                ConformanceTarget::ExternalDataPlane,
            ]),
            [StackMode::Controlplane, StackMode::Dataplane]
        );
    }

    #[test]
    fn direct_fixture_endpoint_accepts_only_loopback_bindings() {
        assert_eq!(
            parse_conformance_fixture_endpoint(b"127.0.0.1:49152\n")
                .expect("IPv4 loopback should be accepted")
                .as_str(),
            "http://127.0.0.1:49152/mcp"
        );
        assert_eq!(
            parse_conformance_fixture_endpoint(b"[::1]:49153\n")
                .expect("IPv6 loopback should be accepted")
                .as_str(),
            "http://[::1]:49153/mcp"
        );
        assert!(
            parse_conformance_fixture_endpoint(b"0.0.0.0:49154\n")
                .expect_err("wildcard bindings must be rejected")
                .to_string()
                .contains("non-loopback")
        );
    }

    #[test]
    fn cleanup_errors_preserve_the_primary_failure_and_cleanup_context() {
        let primary = AppFailure::from(anyhow!("runner failed"));
        let cleanup = Err(AppFailure::from(anyhow!("cleanup failed")));

        let error = finish_with_cleanup(Some(primary), cleanup)
            .expect_err("both failures must remain visible")
            .to_string();

        assert!(error.contains("runner failed"));
        assert!(error.contains("cleanup failed"));
        assert!(error.find("runner failed") < error.find("cleanup failed"));
    }

    #[test]
    fn independent_cleanup_failures_are_combined() {
        let error = combine_cleanup_results(
            Err(AppFailure::from(anyhow!("API cleanup failed"))),
            Err(AppFailure::from(anyhow!("service cleanup failed"))),
        )
        .expect_err("both cleanup failures must be returned")
        .to_string();

        assert!(error.contains("API cleanup failed"));
        assert!(error.contains("service cleanup failed"));
    }

    #[test]
    fn conformance_lane_header_names_the_lane_oracle_and_specification() {
        assert_eq!(
            render_conformance_lane_header(
                ConformanceTarget::FixtureDirect,
                40,
                "2026-07-28",
                ConformanceServerEra::Legacy,
                OutputStyle::plain(),
            ),
            "────────────\n MCP conformance lane: fixture direct\n    Starting 40 scenarios with @modelcontextprotocol/conformance@0.2.0-alpha.11 (client 2026-07-28, server legacy)"
        );
    }

    #[test]
    fn conformance_lane_results_use_one_nextest_style_line_per_scenario() {
        let results = mixed_conformance_results();

        let rendered = render_conformance_lane_results(
            ConformanceTarget::ExternalDataPlane,
            &results,
            Duration::from_millis(1_250),
            OutputStyle::plain(),
        );

        assert_eq!(
            rendered,
            "        FAIL (1/2) failing\n        PASS (2/2) passing\n────────────\n     Summary [   1.250s] 2 scenarios run for external data-plane route: 1 passed, 1 failed, 0 skipped, 0 unknown"
        );
        assert_eq!(
            rendered.lines().filter(|line| line.contains(" (")).count(),
            2
        );
        assert!(!rendered.contains("Checks:"));
        assert!(!rendered.contains("Running scenario"));
    }

    #[test]
    fn colored_conformance_output_styles_lane_statuses_and_failed_summary() {
        let header = render_conformance_lane_header(
            ConformanceTarget::ExternalDataPlane,
            2,
            "2026-07-28",
            ConformanceServerEra::Modern,
            OutputStyle::colored(),
        );
        let results = render_conformance_lane_results(
            ConformanceTarget::ExternalDataPlane,
            &mixed_conformance_results(),
            Duration::from_millis(1_250),
            OutputStyle::colored(),
        );

        assert!(header.contains("\x1b[36m────────────\x1b[0m"));
        assert!(
            header.contains("\x1b[1;36m MCP conformance lane: external data-plane route\x1b[0m")
        );
        assert!(results.contains("\x1b[31m        FAIL\x1b[0m (1/2) failing"));
        assert!(results.contains("\x1b[32m        PASS\x1b[0m (2/2) passing"));
        assert!(results.contains("     \x1b[1;31mSummary\x1b[0m [   1.250s]"));
    }
}
