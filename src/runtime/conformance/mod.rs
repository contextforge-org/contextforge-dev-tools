//! Official conformance orchestration.

mod reports;

use super::*;
use reports::*;
use std::fmt::Write as _;
use std::time::Instant;

use crate::conformance::results::{DEFAULT_CONFORMANCE_SUITE, ScenarioOutcome};

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
                client_versions,
                server_eras,
                results_dir,
                baseline_dir,
                bless,
                output_dir,
            } => {
                let artifact_root = results_dir
                    .as_deref()
                    .unwrap_or_else(|| self.config.integration_dir());
                let baseline_root = baseline_dir
                    .unwrap_or_else(|| self.config.root().join("tests/conformance/baselines"));
                let report_root = output_dir.unwrap_or_else(|| self.config.root().join("reports"));
                let mut failures = Vec::new();
                let mut updates = Vec::<BaselineUpdate>::new();

                for (client_version, server_era) in
                    conformance_matrix(&client_versions, &server_eras)
                {
                    let paths = ConformancePaths::new(
                        artifact_root,
                        report_root.clone(),
                        &client_version,
                        server_era,
                    );
                    let setup_log = paths.setup_log();
                    let setup_result = prepare_setup_log(&setup_log);
                    if let Err(error) = setup_result {
                        failures.push(format!("{} setup: {error}", paths.identity()));
                        continue;
                    }
                    let quiet_runner = LoggingProcessRunner::new(&self.runner, &setup_log);
                    let executor = RuntimeContext::new(self.config.clone(), quiet_runner);
                    let matrix_started = Instant::now();
                    if let Err(error) = executor
                        .run_conformance(&lanes, &client_version, server_era, &paths)
                        .await
                    {
                        failures.push(format!("{}: {error}", paths.identity()));
                    }
                    println!(
                        "{} {}",
                        OutputStyle::stdout().info("  Setup output"),
                        setup_log.display()
                    );

                    let evaluated = executor
                        .load_selected_conformance_results(&paths, &lanes)
                        .and_then(|results| {
                            let evaluation = evaluate_baselines(
                                &results,
                                &lanes,
                                &baseline_root,
                                &client_version,
                                server_era,
                                bless,
                            )
                            .map_err(AppFailure::from)?;
                            println!(
                                "{}",
                                render_conformance_baseline_results(
                                    &results,
                                    &evaluation.comparisons,
                                    matrix_started.elapsed(),
                                    OutputStyle::stdout(),
                                    bless,
                                )
                            );
                            Ok(evaluation)
                        });
                    match evaluated {
                        Ok(evaluation) => {
                            for comparison in &evaluation.comparisons {
                                let report = paths.baseline_report(comparison.lane);
                                if let Err(error) = write_baseline_report(
                                    &report,
                                    &client_version,
                                    server_era,
                                    comparison,
                                ) {
                                    failures.push(format!(
                                        "{} {} baseline report: {error}",
                                        paths.identity(),
                                        comparison.lane.slug()
                                    ));
                                }
                                if !bless && !comparison.matches() {
                                    failures.push(format!(
                                        "{} {} baseline mismatch: unexpected={:?}; stale={:?}",
                                        paths.identity(),
                                        comparison.lane.slug(),
                                        comparison.unexpected,
                                        comparison.stale
                                    ));
                                }
                            }
                            updates.extend(evaluation.updates);
                        }
                        Err(error) => {
                            failures.push(format!("{} baseline gate: {error}", paths.identity()));
                        }
                    }
                }

                if failures.is_empty() && bless {
                    bless_baselines_transactionally(&baseline_root, &updates)
                        .map_err(AppFailure::from)?;
                    println!(
                        "{} {}",
                        OutputStyle::stdout().success("Conformance baselines updated:"),
                        baseline_root.display()
                    );
                }
                if failures.is_empty() {
                    Ok(())
                } else {
                    Err(AppFailure::from(anyhow!(
                        "conformance matrix completed with failures:\n- {}",
                        failures.join("\n- ")
                    )))
                }
            }
            ConformanceAction::Report {
                results_dir,
                output_dir,
            } => self.regenerate_conformance_report(results_dir.as_deref(), output_dir.as_deref()),
        }
    }

    async fn run_conformance(
        &self,
        lanes: &[SemanticLane],
        spec_version: &str,
        server_era: ConformanceServerEra,
        paths: &ConformancePaths,
    ) -> AppResult<()> {
        self.run_conformance_with_interrupt(lanes, spec_version, server_era, paths, async {
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await;
            }
        })
        .await
    }

    async fn run_conformance_with_interrupt<I>(
        &self,
        lanes: &[SemanticLane],
        spec_version: &str,
        server_era: ConformanceServerEra,
        paths: &ConformancePaths,
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

        paths.clear_conformance()?;

        let topologies = conformance_topologies(lanes);
        let mut direct_complete = false;
        let mut failures = Vec::new();
        let mut interrupted = false;
        tokio::pin!(interrupt);
        let (cancellation_sender, cancellation_receiver) = tokio::sync::watch::channel(false);

        let cleanup_progress = Activity::start("clear prior integration stacks");
        let cleanup_result = self.cleanup(TopologySelection::All, CleanupKind::Reset);
        cleanup_progress.finish(cleanup_result.is_ok());
        cleanup_result?;

        for topology in topologies {
            let target = conformance_target(topology);
            let run_routed = lanes.contains(&target);
            let stack_progress =
                Activity::start(format!("prepare {}", conformance_topology_label(topology)));
            let mut topology_failure = self.stack_up_for_conformance(topology, true).await.err();
            stack_progress.finish(topology_failure.is_none());
            let mut fixture_state = None;
            let mut fixture_metadata = None;
            let mut fixture_endpoint = None;
            let mut service_started = false;
            let mut managed_token = None;

            if topology_failure.is_none() {
                let fixture_progress = Activity::start(format!(
                    "start the official fixture for {}",
                    conformance_topology_label(topology)
                ));
                let (start_result, start_interrupted) = finish_phase_after_interrupt(
                    self.start_conformance_service(topology, server_era),
                    interrupt.as_mut(),
                )
                .await;
                fixture_progress.finish(start_result.is_ok() && !start_interrupted);
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

            // Routed baselines subtract findings reproduced by the direct fixture,
            // so every selected matrix needs one direct run even when that lane is
            // not itself selected for baseline gating.
            if topology_failure.is_none() && !direct_complete {
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
                        let direct = self.run_official_conformance_direct(&run, paths);
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
                        let provision_progress = Activity::start(format!(
                            "register the official fixture for {}",
                            conformance_topology_label(topology)
                        ));
                        let (provision_result, provision_interrupted) =
                            finish_phase_after_interrupt(
                                client.provision(OFFICIAL_CONFORMANCE_BACKEND_URL),
                                interrupt.as_mut(),
                            )
                            .await;
                        provision_progress
                            .finish(provision_result.is_ok() && !provision_interrupted);
                        interrupted |= provision_interrupted;
                        match provision_result {
                            Ok(fixture) => {
                                if interrupted {
                                    topology_failure = Some(interrupted_conformance_failure());
                                } else if topology == StackMode::Dataplane
                                    && let Err(error) = {
                                        let publisher_progress = Activity::start(
                                            "wait for the external data-plane configuration",
                                        );
                                        let result = self
                                            .wait_for_publisher_snapshot_quiet(&fixture.server_id)
                                            .await;
                                        publisher_progress.finish(result.is_ok());
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
                                    paths,
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

        if !interrupted
            && [
                SemanticLane::FixtureDirect,
                SemanticLane::BuiltInDataPlane,
                SemanticLane::ExternalDataPlane,
            ]
            .iter()
            .all(|lane| lanes.contains(lane))
        {
            match self.write_comparison_from_artifacts(
                paths,
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
        paths: &ConformancePaths,
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
                &SemanticLaneRun {
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
        paths: &ConformancePaths,
    ) -> AppResult<()> {
        self.run_official_conformance_target(
            &SemanticLaneRun {
                target: SemanticLane::FixtureDirect,
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
        run: &SemanticLaneRun<'_>,
        paths: &ConformancePaths,
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
                oracle: crate::conformance::results::OFFICIAL_CONFORMANCE_PACKAGE.to_owned(),
                target: run.target.label().to_owned(),
                client_version: run.spec_version.to_owned(),
                server_era: run.server_era,
                suite: DEFAULT_CONFORMANCE_SUITE.to_owned(),
                fixture: run.fixture.clone(),
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
        let runner_progress = Activity::start(format!(
            "run {} scenarios for {}",
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
        runner_progress.finish(process_result.is_ok());

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
            "{} {}",
            style.info("   Artifacts"),
            lane_paths.root.display()
        );
        println!(
            "{} {}",
            style.info(" Full output"),
            lane_paths.runner_log.display()
        );
        Ok(())
    }
}

fn prepare_setup_log(path: &Path) -> AppResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppFailure::from(anyhow!("conformance setup log has no parent")))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create conformance log directory {parent:?}"))
        .map_err(AppFailure::from)?;
    fs::write(path, [])
        .with_context(|| format!("failed to clear conformance log {path:?}"))
        .map_err(AppFailure::from)
}

fn conformance_matrix(
    client_versions: &[String],
    server_eras: &[ConformanceServerEra],
) -> Vec<(String, ConformanceServerEra)> {
    client_versions
        .iter()
        .flat_map(|client_version| {
            server_eras
                .iter()
                .map(|server_era| (client_version.clone(), *server_era))
        })
        .collect()
}

fn render_conformance_lane_header(
    target: SemanticLane,
    scenario_count: usize,
    spec_version: &str,
    server_era: ConformanceServerEra,
    style: OutputStyle,
) -> String {
    let divider = style.info("────────────");
    let lane = style.heading(&format!(" MCP conformance lane: {target}"));
    format!(
        "{divider}\n{lane}\n    Starting {scenario_count} scenarios with {} (client {spec_version}, server {server_era})",
        crate::conformance::results::OFFICIAL_CONFORMANCE_PACKAGE
    )
}

fn render_conformance_baseline_results(
    results: &BTreeMap<SemanticLane, ConformanceResults>,
    comparisons: &[BaselineComparison],
    elapsed: Duration,
    style: OutputStyle,
    blessing: bool,
) -> String {
    let mut passed = 0;
    let mut expected_failures = 0;
    let mut unexpected_passes = 0;
    let mut failed = 0;
    let mut skipped = 0;
    let mut ambiguous = 0;
    let mut output = String::new();

    for comparison in comparisons {
        let Some(lane_results) = results.get(&comparison.lane) else {
            continue;
        };
        let total = lane_results.scenarios.len();
        let divider = style.info("────────────");
        let heading = style.heading(&format!(" MCP conformance results: {}", comparison.lane));
        let _ = writeln!(output, "{divider}\n{heading}");
        for (index, result) in lane_results.scenarios.values().enumerate() {
            let status = conformance_test_status(result, comparison, blessing);
            match status {
                TestStatus::Pass => passed += 1,
                TestStatus::ExpectedFailure => expected_failures += 1,
                TestStatus::UnexpectedPass => unexpected_passes += 1,
                TestStatus::Fail => failed += 1,
                TestStatus::Skip => skipped += 1,
                TestStatus::Unknown | TestStatus::Retry => ambiguous += 1,
            }
            let name = format!("{}::{}", comparison.lane.slug(), result.scenario);
            let _ = writeln!(
                output,
                "{}",
                style.test_result(status, &name, None, Some((index + 1, total)))
            );
        }
    }

    let divider = style.info("────────────");
    let summary = if failed > 0 || unexpected_passes > 0 {
        style.failure_heading("Summary")
    } else if ambiguous > 0 {
        style.unknown_heading("Summary")
    } else {
        style.success_heading("Summary")
    };
    let _ = write!(
        output,
        "{divider}\n     {summary} [{:>8.3}s] {passed} passed, {expected_failures} xfailed, {unexpected_passes} xpassed, {failed} failed, {skipped} skipped, {ambiguous} unknown",
        elapsed.as_secs_f64()
    );
    output
}

fn conformance_test_status(
    result: &crate::conformance::results::ConformanceScenarioResult,
    comparison: &BaselineComparison,
    blessing: bool,
) -> TestStatus {
    let scenario = result.scenario.as_str();
    if !blessing
        && comparison
            .unexpected
            .iter()
            .any(|finding| finding.scenario == scenario)
    {
        return TestStatus::Fail;
    }
    if !blessing
        && comparison
            .stale
            .iter()
            .any(|finding| finding.scenario == scenario)
    {
        return TestStatus::UnexpectedPass;
    }
    if comparison
        .actual
        .iter()
        .any(|finding| finding.scenario == scenario)
    {
        return TestStatus::ExpectedFailure;
    }
    match result.gated_outcome() {
        ScenarioOutcome::Compliant => TestStatus::Pass,
        ScenarioOutcome::NonCompliant | ScenarioOutcome::FixtureFailure => {
            TestStatus::ExpectedFailure
        }
        ScenarioOutcome::NotApplicable => TestStatus::Skip,
        ScenarioOutcome::Ambiguous | ScenarioOutcome::Missing => TestStatus::Unknown,
    }
}

fn conformance_topologies(lanes: &[SemanticLane]) -> Vec<StackMode> {
    let mut topologies = Vec::new();
    if lanes.contains(&SemanticLane::BuiltInDataPlane) {
        topologies.push(StackMode::Controlplane);
    }
    if lanes.contains(&SemanticLane::ExternalDataPlane) {
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

struct SemanticLaneRun<'a> {
    target: SemanticLane,
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
    use crate::conformance::results::{CheckStatus, ConformanceCheck, ConformanceScenarioResult};

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

    fn scored_finding(scenario: &str) -> crate::conformance::baseline::ScoredFinding {
        crate::conformance::baseline::ScoredFinding {
            scenario: scenario.to_owned(),
            check: "check".to_owned(),
            status: crate::conformance::baseline::ScoredStatus::Failure,
        }
    }

    fn result_map(results: ConformanceResults) -> BTreeMap<SemanticLane, ConformanceResults> {
        [(SemanticLane::ExternalDataPlane, results)]
            .into_iter()
            .collect()
    }

    #[test]
    fn lane_selection_uses_only_required_stack_topologies() {
        assert_eq!(
            conformance_topologies(&[SemanticLane::FixtureDirect]),
            [StackMode::Controlplane]
        );
        assert_eq!(
            conformance_topologies(
                &[SemanticLane::FixtureDirect, SemanticLane::ExternalDataPlane,]
            ),
            [StackMode::Dataplane]
        );
        assert_eq!(
            conformance_topologies(&[
                SemanticLane::BuiltInDataPlane,
                SemanticLane::ExternalDataPlane,
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
                SemanticLane::FixtureDirect,
                40,
                "2026-07-28",
                ConformanceServerEra::Legacy,
                OutputStyle::plain(),
            ),
            "────────────\n MCP conformance lane: fixture direct\n    Starting 40 scenarios with @modelcontextprotocol/conformance@0.2.0-alpha.11 (client 2026-07-28, server legacy)"
        );
    }

    #[test]
    fn conformance_matrix_is_the_ordered_cartesian_product() {
        assert_eq!(
            conformance_matrix(
                &["2025-11-25".to_owned(), "2026-07-28".to_owned()],
                &[ConformanceServerEra::Legacy, ConformanceServerEra::Modern,],
            ),
            [
                ("2025-11-25".to_owned(), ConformanceServerEra::Legacy),
                ("2025-11-25".to_owned(), ConformanceServerEra::Modern),
                ("2026-07-28".to_owned(), ConformanceServerEra::Legacy),
                ("2026-07-28".to_owned(), ConformanceServerEra::Modern),
            ]
        );
    }

    #[test]
    fn conformance_baseline_results_render_pass_and_expected_failure_per_scenario() {
        let results = result_map(mixed_conformance_results());
        let failing = scored_finding("failing");
        let comparison = BaselineComparison {
            lane: SemanticLane::ExternalDataPlane,
            actual: vec![failing.clone()],
            expected: vec![failing],
            unexpected: Vec::new(),
            stale: Vec::new(),
        };

        let rendered = render_conformance_baseline_results(
            &results,
            &[comparison],
            Duration::from_millis(1_250),
            OutputStyle::plain(),
            false,
        );

        assert_eq!(
            rendered,
            "────────────\n MCP conformance results: external data-plane route\n       XFAIL (1/2) external-data-plane::failing\n        PASS (2/2) external-data-plane::passing\n────────────\n     Summary [   1.250s] 1 passed, 1 xfailed, 0 xpassed, 0 failed, 0 skipped, 0 unknown"
        );
    }

    #[test]
    fn colored_conformance_output_distinguishes_expected_and_unexpected_results() {
        let header = render_conformance_lane_header(
            SemanticLane::ExternalDataPlane,
            4,
            "2026-07-28",
            ConformanceServerEra::Modern,
            OutputStyle::colored(),
        );
        let results = result_map(ConformanceResults {
            scenarios: [
                (
                    "expected".to_owned(),
                    conformance_result("expected", CheckStatus::Failure),
                ),
                (
                    "passing".to_owned(),
                    conformance_result("passing", CheckStatus::Success),
                ),
                (
                    "stale".to_owned(),
                    conformance_result("stale", CheckStatus::Success),
                ),
                (
                    "unexpected".to_owned(),
                    conformance_result("unexpected", CheckStatus::Failure),
                ),
            ]
            .into_iter()
            .collect(),
        });
        let expected = scored_finding("expected");
        let unexpected = scored_finding("unexpected");
        let stale = scored_finding("stale");
        let comparison = BaselineComparison {
            lane: SemanticLane::ExternalDataPlane,
            actual: vec![expected.clone(), unexpected.clone()],
            expected: vec![expected, stale.clone()],
            unexpected: vec![unexpected],
            stale: vec![stale],
        };
        let rendered = render_conformance_baseline_results(
            &results,
            &[comparison],
            Duration::from_millis(1_250),
            OutputStyle::colored(),
            false,
        );

        assert!(header.contains("\x1b[36m────────────\x1b[0m"));
        assert!(
            header.contains("\x1b[1;36m MCP conformance lane: external data-plane route\x1b[0m")
        );
        assert!(rendered.contains("\x1b[33m       XFAIL\x1b[0m"));
        assert!(rendered.contains("\x1b[32m        PASS\x1b[0m"));
        assert!(rendered.contains("\x1b[31m       XPASS\x1b[0m"));
        assert!(rendered.contains("\x1b[31m        FAIL\x1b[0m"));
        assert!(rendered.contains("     \x1b[1;31mSummary\x1b[0m [   1.250s]"));
    }
}
