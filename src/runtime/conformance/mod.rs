//! Official conformance orchestration.

mod reports;

use super::*;
use reports::*;
use std::fmt::Write as _;
use std::time::Instant;

use crate::conformance::DEFAULT_MCP_SPEC_VERSION;
use crate::conformance::results::{DEFAULT_CONFORMANCE_SUITE, ScenarioOutcome};

const CLIENT_CONFORMANCE_SERVER_ID: &str = "dataplane-client-conformance";

impl<R: ProcessRunner> RuntimeContext<R> {
    fn require_docker_daemon(&self) -> AppResult<()> {
        self.runner
            .capture_stdout(&CommandSpec::new("docker").args([
                "info",
                "--format",
                "{{.ServerVersion}}",
            ]))
            .map(|_| ())
            .map_err(|error| {
                AppFailure::from(anyhow!(
                    "Docker daemon is unavailable; start the selected Docker context and retry: {error}"
                ))
            })
    }

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

    fn standalone_conformance_project(&self) -> ComposeProject {
        let project_name = format!(
            "{}-conformance-fixture",
            self.config.integration_project().value.to_string_lossy()
        );
        ComposeProject::conformance_fixture(self.config.asset_root(), project_name.into())
    }

    fn standalone_conformance_environment(
        &self,
        command: CommandSpec,
        server_era: ConformanceServerEra,
    ) -> CommandSpec {
        let command_environment = command.environment().clone();
        let mut command = command.cwd(self.config.root());
        for (key, value) in self.config.environment().iter() {
            if !command_environment.contains_key(key) {
                command = command.env(key.clone(), value.value.clone());
            }
        }
        command
            .env("CF_INTEGRATION_ROOT", self.config.asset_root().as_os_str())
            .env(CONFORMANCE_SERVER_ERA_ENV, server_era.label())
    }

    async fn start_standalone_conformance_fixture(
        &self,
        server_era: ConformanceServerEra,
    ) -> AppResult<()> {
        let project = self.standalone_conformance_project();
        let build = self.standalone_conformance_environment(
            project.command(["build", OFFICIAL_CONFORMANCE_SERVICE]),
            server_era,
        );
        self.runner.run_async(&build).await?;

        let up = self.standalone_conformance_environment(
            project.command(["up", "-d", "--wait", OFFICIAL_CONFORMANCE_SERVICE]),
            server_era,
        );
        self.runner.run_async(&up).await.map_err(AppFailure::from)
    }

    fn standalone_conformance_fixture_endpoint(
        &self,
        server_era: ConformanceServerEra,
    ) -> AppResult<url::Url> {
        let command = self.standalone_conformance_environment(
            self.standalone_conformance_project().command([
                "port",
                OFFICIAL_CONFORMANCE_SERVICE,
                "3000",
            ]),
            server_era,
        );
        let output = self.runner.capture_stdout(&command)?;
        parse_conformance_fixture_endpoint(&output).map_err(AppFailure::from)
    }

    async fn stop_standalone_conformance_fixture(
        &self,
        server_era: ConformanceServerEra,
    ) -> AppResult<()> {
        let command = self.standalone_conformance_environment(
            self.standalone_conformance_project().command([
                "down",
                "--volumes",
                "--remove-orphans",
            ]),
            server_era,
        );
        self.runner
            .run_async(&command)
            .await
            .map_err(AppFailure::from)
    }

    pub(super) async fn start_conformance_service(
        &self,
        topology: StackMode,
        server_era: ConformanceServerEra,
    ) -> AppResult<()> {
        self.build_conformance_service(topology, server_era).await?;
        self.start_conformance_containers(topology, server_era)
            .await
    }

    pub(super) async fn build_conformance_service(
        &self,
        topology: StackMode,
        server_era: ConformanceServerEra,
    ) -> AppResult<()> {
        let project = self.conformance_compose_project(topology);
        let build = project.command(["build", OFFICIAL_CONFORMANCE_SERVICE]);
        let build = self
            .compose_environment(build, topology, true)?
            .env(CONFORMANCE_SERVER_ERA_ENV, server_era.label());
        Ok(self.runner.run_async(&build).await?)
    }

    pub(super) async fn start_conformance_containers(
        &self,
        topology: StackMode,
        server_era: ConformanceServerEra,
    ) -> AppResult<()> {
        let project = self.conformance_compose_project(topology);
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
                self.require_docker_daemon()?;
                let artifact_root = results_dir
                    .as_deref()
                    .unwrap_or_else(|| self.config.integration_dir());
                let baseline_root = baseline_dir.unwrap_or_else(|| {
                    let root = if bless {
                        self.config.root()
                    } else {
                        self.config.asset_root()
                    };
                    root.join("tests/conformance/baselines")
                });
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
                    let run_result = executor
                        .run_conformance(&lanes, &client_version, server_era, &paths)
                        .await;
                    let results = if run_result.is_ok() {
                        executor.load_selected_conformance_results(&paths, &lanes)
                    } else {
                        executor.load_completed_conformance_results(&paths, &lanes)
                    };
                    if let Err(error) = run_result {
                        failures.push(format!(
                            "{}: {error}\n  Setup log: {}",
                            paths.identity(),
                            setup_log.display()
                        ));
                    }

                    let evaluated = results.and_then(|results| {
                        if results.is_empty() {
                            return Ok(None);
                        }
                        let completed_lanes = results.keys().copied().collect::<Vec<_>>();
                        match evaluate_baselines(
                            &results,
                            &completed_lanes,
                            &baseline_root,
                            &client_version,
                            server_era,
                            bless,
                        ) {
                            Ok(evaluation) => {
                                println!(
                                    "{}",
                                    render_conformance_results(
                                        &results,
                                        (&client_version, server_era),
                                        ConformanceDirection::Server,
                                        Some(&evaluation.comparisons),
                                        matrix_started.elapsed(),
                                        OutputStyle::stdout(),
                                        bless,
                                    )
                                );
                                Ok(Some(evaluation))
                            }
                            Err(error) => {
                                println!(
                                    "{}",
                                    render_conformance_results(
                                        &results,
                                        (&client_version, server_era),
                                        ConformanceDirection::Server,
                                        None,
                                        matrix_started.elapsed(),
                                        OutputStyle::stdout(),
                                        bless,
                                    )
                                );
                                Err(AppFailure::from(error))
                            }
                        }
                    });
                    match evaluated {
                        Ok(Some(evaluation)) => {
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
                        Ok(None) => {}
                        Err(error) => {
                            failures.push(format!("{} baseline gate: {error}", paths.identity()));
                        }
                    }

                    match executor.load_completed_client_conformance_results(&paths) {
                        Err(error) => {
                            failures.push(format!("{} client artifacts: {error}", paths.identity()))
                        }
                        Ok(client_results) if !client_results.is_empty() => {
                            match evaluate_client_baselines(
                                &client_results,
                                &[SemanticLane::ExternalDataPlane],
                                &baseline_root,
                                &client_version,
                                server_era,
                                bless,
                            ) {
                                Ok(evaluation) => {
                                    println!(
                                        "{}",
                                        render_conformance_results(
                                            &client_results,
                                            (&client_version, server_era),
                                            ConformanceDirection::Client,
                                            Some(&evaluation.comparisons),
                                            matrix_started.elapsed(),
                                            OutputStyle::stdout(),
                                            bless,
                                        )
                                    );
                                    for comparison in &evaluation.comparisons {
                                        let report = paths.client_baseline_report(comparison.lane);
                                        if let Err(error) = write_client_baseline_report(
                                            &report,
                                            &client_version,
                                            server_era,
                                            comparison,
                                        ) {
                                            failures.push(format!(
                                                "{} client {} baseline report: {error}",
                                                paths.identity(),
                                                comparison.lane.slug()
                                            ));
                                        }
                                        if !bless && !comparison.matches() {
                                            failures.push(format!(
                                                "{} client {} baseline mismatch: unexpected={:?}; stale={:?}",
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
                                    println!(
                                        "{}",
                                        render_conformance_results(
                                            &client_results,
                                            (&client_version, server_era),
                                            ConformanceDirection::Client,
                                            None,
                                            matrix_started.elapsed(),
                                            OutputStyle::stdout(),
                                            bless,
                                        )
                                    );
                                    failures.push(format!(
                                        "{} client baseline gate: {error}",
                                        paths.identity()
                                    ));
                                }
                            }
                        }
                        Ok(_) => {}
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
                        "conformance failed:\n- {}",
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
        paths.clear_conformance()?;

        let topologies = conformance_topologies(lanes);
        if !topologies.is_empty() {
            self.require_loopback_fixture_base_url()?;
        }
        let mut failures = Vec::new();
        let mut interrupted = false;
        tokio::pin!(interrupt);
        let (cancellation_sender, cancellation_receiver) = tokio::sync::watch::channel(false);

        let cleanup_progress = Activity::spinner("Clear prior conformance fixture");
        let cleanup_result = self.stop_standalone_conformance_fixture(server_era).await;
        cleanup_progress.finish(cleanup_result.is_ok());
        cleanup_result?;

        let fixture_progress = Activity::spinner("Start the official fixture");
        let (start_result, start_interrupted) = finish_phase_after_interrupt(
            self.start_standalone_conformance_fixture(server_era),
            interrupt.as_mut(),
        )
        .await;
        fixture_progress.finish(start_result.is_ok() && !start_interrupted);
        interrupted |= start_interrupted;
        let mut direct_failure = match start_result {
            Ok(()) if interrupted => Some(interrupted_conformance_failure()),
            Ok(()) => match self.standalone_conformance_fixture_endpoint(server_era) {
                Ok(endpoint) => {
                    let metadata = ConformanceFixtureMetadata {
                        repository: OFFICIAL_CONFORMANCE_REPOSITORY.to_owned(),
                        revision: OFFICIAL_CONFORMANCE_REVISION.to_owned(),
                        server_id: OFFICIAL_CONFORMANCE_SERVER_ID.to_owned(),
                    };
                    let direct_run = DirectConformanceRun {
                        endpoint: &endpoint,
                        spec_version,
                        server_era,
                        fixture: &metadata,
                        cancellation: cancellation_receiver.clone(),
                    };
                    let direct = self.run_official_conformance_direct(&direct_run, paths);
                    tokio::pin!(direct);
                    tokio::select! {
                        result = &mut direct => result.err(),
                        () = interrupt.as_mut() => {
                            interrupted = true;
                            cancellation_sender.send_replace(true);
                            let _ = direct.await;
                            Some(interrupted_conformance_failure())
                        }
                    }
                }
                Err(error) => Some(error),
            },
            Err(error) => Some(if interrupted {
                interrupted_conformance_failure()
            } else {
                error
            }),
        };
        direct_failure = finish_with_cleanup(
            direct_failure,
            self.stop_standalone_conformance_fixture(server_era).await,
        )
        .err();
        if let Some(error) = direct_failure {
            failures.push(format!("fixture direct: {error}"));
        }

        if !topologies.is_empty() && !interrupted {
            let cleanup_progress = Activity::spinner("Clear prior integration stacks");
            let cleanup_result = self.cleanup(TopologySelection::All, CleanupKind::Reset);
            cleanup_progress.finish(cleanup_result.is_ok());
            cleanup_result?;
        }

        for topology in topologies {
            if interrupted {
                break;
            }
            let target = conformance_target(topology);
            let run_routed = lanes.contains(&target);
            let stack_progress =
                Activity::spinner(format!("Prepare {}", topology.topology_label()));
            let mut topology_failure = self.stack_up_for_conformance(topology, true).await.err();
            stack_progress.finish(topology_failure.is_none());
            let mut fixture_state = None;
            let mut fixture_metadata = None;
            let mut service_started = false;
            let mut managed_token = None;

            if topology_failure.is_none() {
                let fixture_progress = Activity::spinner(format!(
                    "Start the official fixture for {}",
                    topology.topology_label()
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
                            fixture_metadata = Some(ConformanceFixtureMetadata {
                                repository: OFFICIAL_CONFORMANCE_REPOSITORY.to_owned(),
                                revision: OFFICIAL_CONFORMANCE_REVISION.to_owned(),
                                server_id: OFFICIAL_CONFORMANCE_SERVER_ID.to_owned(),
                            });
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

            if topology_failure.is_none() && run_routed {
                match self.admin_session_token().await.and_then(|token| {
                    ConformanceFixtureClient::builder(self.base_url()?, token)
                        .build()
                        .map_err(AppFailure::from)
                }) {
                    Ok(client) => {
                        let provision_progress = Activity::spinner(format!(
                            "Register the official fixture for {}",
                            topology.topology_label()
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
                                        let publisher_progress = Activity::spinner(
                                            "Wait for the external dataplane configuration",
                                        );
                                        let result = self
                                            .wait_for_publisher_snapshot(&fixture.server_id)
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
                let failure = format!("{} topology: {error}", topology.topology_label());
                failures.push(failure);
            }
            if interrupted {
                cancellation_sender.send_replace(true);
                break;
            }
        }

        if !interrupted
            && spec_version == DEFAULT_MCP_SPEC_VERSION
            && lanes.contains(&SemanticLane::ExternalDataPlane)
        {
            let client = self.run_external_client_conformance(
                spec_version,
                server_era,
                paths,
                cancellation_receiver.clone(),
            );
            tokio::pin!(client);
            tokio::select! {
                result = &mut client => {
                    if let Err(error) = result {
                        failures.push(format!("external dataplane client: {error}"));
                    }
                }
                () = interrupt.as_mut() => {
                    interrupted = true;
                    cancellation_sender.send_replace(true);
                    let _ = client.await;
                    failures.push("external dataplane client: conformance workflow interrupted by Ctrl-C".to_owned());
                }
            }
        }

        if failures.is_empty()
            && !interrupted
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
                    failures.push(failure);
                }
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(AppFailure::from(anyhow!(failures.join("; "))))
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

    async fn run_external_client_conformance(
        &self,
        spec_version: &str,
        server_era: ConformanceServerEra,
        paths: &ConformancePaths,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> AppResult<()> {
        expected_client_scenarios(spec_version).map_err(AppFailure::from)?;
        let stack_progress = Activity::spinner("Prepare external dataplane client conformance");
        let stack_result = self
            .stack_up_for_conformance(StackMode::Dataplane, true)
            .await;
        stack_progress.finish(stack_result.is_ok());
        let mut failure = stack_result.err();
        let mut token = None;
        let mut publisher_stopped = false;

        if failure.is_none() {
            match self.issue_conformance_token().await {
                Ok(issued) => token = Some(issued),
                Err(error) => failure = Some(error),
            }
        }
        if failure.is_none() {
            let progress = Activity::spinner("Pause the control-plane publisher");
            let result = self.set_control_plane_publisher(false).await;
            progress.finish(result.is_ok());
            if result.is_ok() {
                publisher_stopped = true;
            }
            failure = result.err();
        }
        if failure.is_none() {
            match token.as_ref() {
                Some(issued) => {
                    failure = self
                        .run_official_client_conformance(
                            spec_version,
                            server_era,
                            &issued.value,
                            paths,
                            cancellation,
                        )
                        .await
                        .err();
                }
                None => {
                    failure = Some(AppFailure::from(anyhow!(
                        "client conformance token was not available after issuance"
                    )));
                }
            }
        }

        if publisher_stopped {
            let progress = Activity::spinner("Restore the control-plane publisher");
            let result = self.set_control_plane_publisher(true).await;
            progress.finish(result.is_ok());
            failure = finish_with_cleanup(failure, result).err();
        }
        if let Some(token) = token.as_ref() {
            failure = finish_with_cleanup(failure, self.revoke_managed_token(token).await).err();
        }
        finish_with_cleanup(
            failure,
            self.cleanup(topology_selection(StackMode::Dataplane), CleanupKind::Down),
        )
    }

    async fn set_control_plane_publisher(&self, running: bool) -> AppResult<()> {
        let project = self.conformance_runtime_project(StackMode::Dataplane);
        let command = if running {
            project.command(["up", "-d", "--wait", "--no-deps", "gateway"])
        } else {
            project.command(["stop", "gateway"])
        };
        let command = self.compose_environment(command, StackMode::Dataplane, true)?;
        self.runner
            .run_async(&command)
            .await
            .map_err(AppFailure::from)
    }

    async fn run_official_client_conformance(
        &self,
        spec_version: &str,
        server_era: ConformanceServerEra,
        token: &str,
        paths: &ConformancePaths,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> AppResult<()> {
        let expected_scenarios =
            expected_client_scenarios(spec_version).map_err(AppFailure::from)?;
        let target = SemanticLane::ExternalDataPlane;
        let lane_paths = paths.client_conformance_lane(target);
        remove_file_if_exists(&lane_paths.completion)?;
        recreate_directory(&lane_paths.official_results)?;
        fs::create_dir_all(&lane_paths.root)
            .with_context(|| {
                format!(
                    "failed to create client-conformance artifact directory {:?}",
                    lane_paths.root
                )
            })
            .map_err(AppFailure::from)?;
        fs::write(&lane_paths.expected_failures, "client: []\n")
            .with_context(|| {
                format!(
                    "failed to write empty client expected-failure file {:?}",
                    lane_paths.expected_failures
                )
            })
            .map_err(AppFailure::from)?;
        write_run_metadata(
            &lane_paths.metadata,
            &ConformanceRunMetadata {
                oracle: crate::conformance::results::OFFICIAL_CONFORMANCE_PACKAGE.to_owned(),
                target: target.label().to_owned(),
                direction: ConformanceDirection::Client,
                client_version: spec_version.to_owned(),
                server_era,
                suite: "scoped".to_owned(),
                fixture: ConformanceFixtureMetadata {
                    repository: OFFICIAL_CONFORMANCE_REPOSITORY.to_owned(),
                    revision: OFFICIAL_CONFORMANCE_REVISION.to_owned(),
                    server_id: OFFICIAL_CONFORMANCE_SERVER_ID.to_owned(),
                },
            },
        )?;

        let compose = self.compose_environment(
            self.conformance_runtime_project(StackMode::Dataplane)
                .command(std::iter::empty::<&str>()),
            StackMode::Dataplane,
            true,
        )?;
        let compose_args = compose
            .arguments()
            .iter()
            .map(|argument| {
                argument
                    .to_str()
                    .context("client conformance Compose argument is not UTF-8")
            })
            .collect::<anyhow::Result<Vec<_>>>()
            .and_then(|arguments| {
                serde_json::to_string(&arguments)
                    .context("failed to serialize client conformance Compose arguments")
            })
            .map_err(AppFailure::from)?;
        let (client_command, client_path) = client_driver_command().map_err(AppFailure::from)?;
        let progress = Activity::spinner(format!(
            "Run external dataplane client ({} scenarios)",
            expected_scenarios.len()
        ));
        let mut operational_failures = Vec::new();
        for scenario in DEFAULT_CLIENT_CONFORMANCE_SCENARIOS {
            let mut command = allowlisted_npx_environment(
                official_client_command(
                    &client_command,
                    scenario,
                    spec_version,
                    &lane_paths.expected_failures,
                    &lane_paths.official_results,
                )
                .cwd(self.config.root()),
            );
            for (key, value) in compose.environment() {
                command = command.env(key.clone(), value.clone());
            }
            command = command
                .env(CLIENT_COMPOSE_ARGS_ENV, &compose_args)
                .env(CLIENT_BASE_URL_ENV, self.base_url()?)
                .env(CLIENT_SERVER_ID_ENV, CLIENT_CONFORMANCE_SERVER_ID)
                .env(CLIENT_TOKEN_ENV, token)
                .env("PATH", client_path.clone());
            let result = self
                .runner
                .run_async_cancellable_to_log(
                    &command,
                    cancellation.clone(),
                    &lane_paths.root.join(format!("runner-{scenario}.log")),
                )
                .await
                .map_err(AppFailure::from);
            if !conformance_process_completed(&result)
                && let Err(error) = result
            {
                operational_failures.push(format!("{scenario}: {error}"));
            }
        }

        match client_driver_failures(&lane_paths.official_results) {
            Ok(failures) => operational_failures.extend(
                failures
                    .into_iter()
                    .map(|failure| failure.replace(token, "[redacted]")),
            ),
            Err(error) => operational_failures.push(error.to_string()),
        }

        let results = load_client_results(&lane_paths.official_results).map_err(AppFailure::from);
        let validation = results.and_then(|results| {
            validate_scored_results(&results).map_err(AppFailure::from)?;
            mark_client_conformance_complete(
                &results,
                target,
                spec_version,
                &lane_paths.completion,
            )?;
            Ok(())
        });
        if let Err(error) = validation {
            operational_failures.push(error.to_string());
        }
        let result = if operational_failures.is_empty() {
            Ok(())
        } else {
            Err(AppFailure::from(anyhow!(
                "client conformance did not complete: {}",
                operational_failures.join("; ")
            )))
        };
        progress.finish(result.is_ok());
        result
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
                direction: ConformanceDirection::Server,
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
        let runner_progress = Activity::spinner(format!(
            "Run {} ({} scenarios)",
            run.target,
            expected_scenarios.len()
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
        runner_progress.finish(conformance_process_completed(&process_result));

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

fn client_driver_failures(root: &Path) -> anyhow::Result<Vec<String>> {
    let entries = fs::read_dir(root)
        .with_context(|| format!("failed to inspect client-conformance results {root:?}"))?;
    let mut run_directories = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("failed to read client-conformance entry in {root:?}"))?
            .path();
        if path.is_dir() {
            run_directories.push(path);
        }
    }
    run_directories.sort();

    let mut failures = Vec::new();
    for run_directory in run_directories {
        let stderr = run_directory.join("stderr.txt");
        let contents = match fs::read_to_string(&stderr) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read client-conformance stderr {stderr:?}")
                });
            }
        };
        if let Some(failure) = contents
            .lines()
            .find(|line| line.contains(CLIENT_DRIVER_FAILURE_PREFIX))
        {
            let run = run_directory
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or("unknown client scenario");
            failures.push(format!("{run}: {}", failure.trim()));
        }
    }
    Ok(failures)
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

fn render_conformance_results(
    results: &BTreeMap<SemanticLane, ConformanceResults>,
    matrix: (&str, ConformanceServerEra),
    direction: ConformanceDirection,
    comparisons: Option<&[BaselineComparison]>,
    elapsed: Duration,
    style: OutputStyle,
    blessing: bool,
) -> String {
    let (client_version, server_era) = matrix;
    let mut passed = 0;
    let mut expected_failures = 0;
    let mut unexpected_passes = 0;
    let mut failed = 0;
    let mut skipped = 0;
    let mut ambiguous = 0;
    let mut output = String::new();

    for (lane, lane_results) in results {
        let comparison = comparisons.and_then(|comparisons| {
            comparisons
                .iter()
                .find(|comparison| comparison.lane == *lane)
        });
        let total = lane_results.scenarios.len();
        let divider = style.info("────────────");
        let heading = style.heading(&format!(" MCP {direction} conformance results: {lane}"));
        let _ = writeln!(
            output,
            "{divider}\n{heading}\n Client protocol: {client_version}\n Server protocols: {} [{}]",
            server_era.label(),
            server_era.protocol_versions_label()
        );
        for (index, result) in lane_results.scenarios.values().enumerate() {
            let status = conformance_test_status(result, comparison, blessing);
            match status {
                TestStatus::Pass => passed += 1,
                TestStatus::ExpectedFailure => expected_failures += 1,
                TestStatus::UnexpectedPass => unexpected_passes += 1,
                TestStatus::Fail => failed += 1,
                TestStatus::Skip => skipped += 1,
                TestStatus::Unknown => ambiguous += 1,
            }
            let name = format!(
                "{}::{}::{}",
                direction.label(),
                lane.slug(),
                result.scenario
            );
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
    comparison: Option<&BaselineComparison>,
    blessing: bool,
) -> TestStatus {
    let scenario = result.scenario.as_str();
    if let Some(comparison) = comparison {
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
    }
    match result.gated_outcome() {
        ScenarioOutcome::Compliant => TestStatus::Pass,
        ScenarioOutcome::NonCompliant | ScenarioOutcome::FixtureFailure => {
            if comparison.is_some() {
                TestStatus::ExpectedFailure
            } else {
                TestStatus::Fail
            }
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
    topologies
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

fn client_driver_command() -> anyhow::Result<(String, OsString)> {
    let executable =
        std::env::current_exe().context("failed to locate the cf-integration binary")?;
    let file_name = executable
        .file_name()
        .and_then(OsStr::to_str)
        .context("cf-integration binary name is not UTF-8")?;
    if !file_name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(anyhow!(
            "cf-integration binary name contains characters unsupported by the official client runner"
        ));
    }
    let directory = executable
        .parent()
        .context("cf-integration binary path has no parent directory")?;
    let inherited = std::env::var_os("PATH").context("PATH is required for client conformance")?;
    let search_path = std::env::join_paths(
        std::iter::once(directory.to_owned()).chain(std::env::split_paths(&inherited)),
    )
    .context("failed to prepend cf-integration to the client-conformance PATH")?;
    Ok((
        format!("{file_name} {INTERNAL_CLIENT_COMMAND}"),
        search_path,
    ))
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
            name: String::new(),
            status: crate::conformance::baseline::ScoredStatus::Failure,
        }
    }

    fn result_map(results: ConformanceResults) -> BTreeMap<SemanticLane, ConformanceResults> {
        [(SemanticLane::ExternalDataPlane, results)]
            .into_iter()
            .collect()
    }

    #[test]
    fn client_driver_failures_are_not_hidden_by_completed_official_runs() {
        let directory = tempfile::tempdir().expect("temporary result root");
        let passing = directory.path().join("tools-call-run");
        let failing = directory.path().join("request-metadata-run");
        fs::create_dir_all(&passing).expect("passing result directory");
        fs::create_dir_all(&failing).expect("failing result directory");
        fs::write(
            passing.join("stderr.txt"),
            "Container client-driver Created\n",
        )
        .expect("passing stderr");
        fs::write(
            failing.join("stderr.txt"),
            format!("{CLIENT_DRIVER_FAILURE_PREFIX}: upstream returned HTTP 500\n"),
        )
        .expect("failing stderr");

        let failures = client_driver_failures(directory.path()).expect("inspect driver results");

        assert_eq!(
            failures,
            [format!(
                "request-metadata-run: {CLIENT_DRIVER_FAILURE_PREFIX}: upstream returned HTTP 500"
            )]
        );
    }

    #[test]
    fn lane_selection_uses_only_required_stack_topologies() {
        assert_eq!(conformance_topologies(&[SemanticLane::FixtureDirect]), []);
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

        let rendered = render_conformance_results(
            &results,
            ("2026-07-28", ConformanceServerEra::Legacy),
            ConformanceDirection::Server,
            Some(&[comparison]),
            Duration::from_millis(1_250),
            OutputStyle::plain(),
            false,
        );

        assert_eq!(
            rendered,
            "────────────\n MCP server conformance results: external dataplane\n Client protocol: 2026-07-28\n Server protocols: legacy [2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25]\n       XFAIL (1/2) server::external-data-plane::failing\n        PASS (2/2) server::external-data-plane::passing\n────────────\n     Summary [   1.250s] 1 passed, 1 xfailed, 0 xpassed, 0 failed, 0 skipped, 0 unknown"
        );
    }

    #[test]
    fn conformance_results_render_without_a_baseline_when_the_gate_cannot_load() {
        let rendered = render_conformance_results(
            &result_map(mixed_conformance_results()),
            ("2026-07-28", ConformanceServerEra::Modern),
            ConformanceDirection::Server,
            None,
            Duration::from_millis(1_250),
            OutputStyle::plain(),
            false,
        );

        assert_eq!(
            rendered,
            "────────────\n MCP server conformance results: external dataplane\n Client protocol: 2026-07-28\n Server protocols: modern [2026-07-28]\n        FAIL (1/2) server::external-data-plane::failing\n        PASS (2/2) server::external-data-plane::passing\n────────────\n     Summary [   1.250s] 1 passed, 0 xfailed, 0 xpassed, 1 failed, 0 skipped, 0 unknown"
        );
    }

    #[test]
    fn client_conformance_results_render_the_downstream_direction() {
        let rendered = render_conformance_results(
            &result_map(mixed_conformance_results()),
            ("2026-07-28", ConformanceServerEra::Modern),
            ConformanceDirection::Client,
            None,
            Duration::from_millis(1_250),
            OutputStyle::plain(),
            false,
        );

        assert!(rendered.contains("MCP client conformance results: external dataplane"));
        assert!(rendered.contains("Client protocol: 2026-07-28"));
        assert!(rendered.contains("Server protocols: modern [2026-07-28]"));
        assert!(rendered.contains("client::external-data-plane::failing"));
        assert!(rendered.contains("client::external-data-plane::passing"));
    }

    #[test]
    fn colored_conformance_output_distinguishes_expected_and_unexpected_results() {
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
        let rendered = render_conformance_results(
            &results,
            ("2026-07-28", ConformanceServerEra::Modern),
            ConformanceDirection::Server,
            Some(&[comparison]),
            Duration::from_millis(1_250),
            OutputStyle::colored(),
            false,
        );

        assert!(rendered.contains("\x1b[33m       XFAIL\x1b[0m"));
        assert!(rendered.contains("\x1b[32m        PASS\x1b[0m"));
        assert!(rendered.contains("\x1b[31m       XPASS\x1b[0m"));
        assert!(rendered.contains("\x1b[31m        FAIL\x1b[0m"));
        assert!(rendered.contains("     \x1b[1;31mSummary\x1b[0m [   1.250s]"));
    }
}
