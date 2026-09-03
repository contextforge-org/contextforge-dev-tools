//! Stack lifecycle and Docker Compose orchestration.

mod sources;

use super::*;

const COMPOSE_PROTOCOL_VERSION_ENV: &str = "MCP_PROTOCOL_VERSION";

impl<R: ProcessRunner> RuntimeContext<R> {
    pub(super) async fn execute_stack(&self, action: StackAction) -> AppResult<()> {
        match action {
            StackAction::Up { topology, fresh } => {
                self.stack_up_for_conformance(topology, fresh).await?;

                let build_log = self
                    .config
                    .integration_dir()
                    .join("logs/conformance-build.log");
                prepare_conformance_build_log(&build_log)?;
                let quiet_runner = LoggingProcessRunner::new(&self.runner, &build_log);
                let quiet_runtime = RuntimeContext::new(self.config.clone(), quiet_runner);
                let build_progress = Activity::spinner("Building conformance image");
                let build_result = quiet_runtime
                    .build_conformance_service(topology, DEFAULT_CONFORMANCE_SERVER_ERA)
                    .await;
                build_progress.finish(build_result.is_ok());
                if let Err(error) = build_result {
                    eprintln!(
                        "{} {}",
                        OutputStyle::stderr().failure("Build output:"),
                        build_log.display()
                    );
                    return Err(error);
                }

                self.start_conformance_containers(topology, DEFAULT_CONFORMANCE_SERVER_ERA)
                    .await?;
                let conformance_endpoint = self.conformance_fixture_endpoint(topology)?;
                Activity::completed("Integration stack ready");
                self.print_stack_summary(topology, &conformance_endpoint)
            }
            StackAction::Down { lane, volumes } => self.cleanup(
                lane,
                if volumes {
                    CleanupKind::Reset
                } else {
                    CleanupKind::Down
                },
            ),
            StackAction::Status(mode) => {
                self.require_mode_sources(mode)?;
                let command = StackCommandPlan::status(self.conformance_compose_project(mode));
                Ok(self.runner.run(&self.compose_environment(
                    command.command().clone(),
                    mode,
                    true,
                )?)?)
            }
            StackAction::Logs {
                topology: mode,
                services,
            } => {
                self.require_mode_sources(mode)?;
                let command =
                    StackCommandPlan::logs(self.conformance_compose_project(mode), services);
                Ok(self.runner.run(&self.compose_environment(
                    command.command().clone(),
                    mode,
                    true,
                )?)?)
            }
            StackAction::Config(mode) => {
                self.require_mode_sources(mode)?;
                if mode == StackMode::Dataplane {
                    self.validate_compose_contract()?;
                }
                let command =
                    StackCommandPlan::config(self.conformance_compose_project(mode), mode);
                Ok(self.runner.run(&self.compose_environment(
                    command.command().clone(),
                    mode,
                    true,
                )?)?)
            }
        }
    }

    pub(super) async fn stack_up(&self, mode: StackMode, fresh: bool) -> AppResult<()> {
        self.stack_up_with_project(mode, fresh, self.compose_project(mode), false)
            .await
    }

    pub(super) async fn stack_up_for_conformance(
        &self,
        mode: StackMode,
        fresh: bool,
    ) -> AppResult<()> {
        self.stack_up_with_project(mode, fresh, self.conformance_runtime_project(mode), false)
            .await
    }

    pub(super) async fn stack_up_with_project(
        &self,
        mode: StackMode,
        fresh: bool,
        project: ComposeProject,
        report_progress: bool,
    ) -> AppResult<()> {
        self.ensure_mode_sources(mode)?;
        if mode == StackMode::Dataplane {
            self.validate_compose_contract()?;
        }

        let build = self.resolve_build(mode, report_progress)?;
        self.pull_images(mode, build, report_progress)?;
        if mode == StackMode::Dataplane
            && !fresh
            && !self.environment_flag("CF_FORCE_STACK_RESTART", false)
            && !build
            && self.integration_freshness()? == StackFreshness::Current
        {
            if report_progress {
                println!(
                    "{}",
                    OutputStyle::stdout()
                        .success("Integration stack already current; skipping Docker Compose up.")
                );
            }
            return self.wait_for_public_endpoint(mode, report_progress).await;
        }

        if fresh {
            self.cleanup(topology_selection(mode), CleanupKind::Reset)?;
        }
        self.ensure_other_stack_stopped(mode)?;
        if mode == StackMode::Controlplane {
            fs::create_dir_all(self.config.controlplane_dir().join("reports"))
                .context("failed to create control-plane report directory")
                .map_err(AppFailure::from)?;
        }

        let start_locust = self.environment_flag("CONTROLPLANE_START_LOCUST_UI", false);
        let locust_workers = self
            .environment_text("CONTROLPLANE_LOCUST_WORKERS")
            .filter(|value| !value.is_empty())
            .unwrap_or("1")
            .parse::<usize>()
            .map_err(|_| {
                AppFailure::from(anyhow!("CONTROLPLANE_LOCUST_WORKERS must be an integer"))
            })?;
        let command = StackCommandPlan::up(project, mode, build, start_locust, locust_workers);
        let command = self.compose_environment(command.command().clone(), mode, true)?;
        let (controlplane_pull_policy, dataplane_pull_policy) = compose_pull_policies(
            mode,
            build,
            !self.config.dataplane_ref().value.is_empty(),
            self.config.controlplane_image().pull_policy(),
            self.config.dataplane_image().pull_policy(),
        );
        self.runner.run(
            &command
                .env("CF_CONTROLPLANE_PULL_POLICY", controlplane_pull_policy)
                .env("CF_DATAPLANE_PULL_POLICY", dataplane_pull_policy),
        )?;
        self.wait_for_public_endpoint(mode, report_progress).await?;
        if report_progress {
            println!(
                "{}",
                OutputStyle::stdout().success(&format!("{} stack started.", mode.lane_label()))
            );
        }
        Ok(())
    }

    pub(super) async fn wait_for_public_endpoint(
        &self,
        mode: StackMode,
        report_progress: bool,
    ) -> AppResult<()> {
        let endpoint = self.public_mcp_endpoint(mode)?;
        if report_progress {
            eprintln!(
                "{}",
                OutputStyle::stderr().info(&format!(
                    "Waiting up to {}s for the public {} MCP endpoint.",
                    STACK_READY_TIMEOUT.as_secs(),
                    mode.lane_label()
                ))
            );
        }
        wait_for_http_endpoint(&endpoint, mode, STACK_READY_TIMEOUT).await
    }

    fn public_mcp_endpoint(&self, mode: StackMode) -> AppResult<url::Url> {
        GatewayClient::builder(
            gateway_topology(mode),
            self.base_url()?,
            self.default_server_id(),
            "readiness-probe",
        )
        .build()
        .context("failed to construct the public MCP endpoint")
        .map_err(AppFailure::from)
        .map(|client| client.endpoint().clone())
    }

    fn print_stack_summary(
        &self,
        mode: StackMode,
        conformance_endpoint: &url::Url,
    ) -> AppResult<()> {
        let observability_ui = format!(
            "http://127.0.0.1:{}",
            self.environment_text("CF_OBSERVABILITY_UI_PORT")
                .filter(|port| !port.is_empty())
                .unwrap_or("3000")
        );
        let summary = format_stack_endpoint_summary(
            self.base_url()?,
            &self.public_mcp_endpoint(mode)?,
            conformance_endpoint,
            &observability_ui,
        );
        println!("{}", OutputStyle::stdout().info(&summary));
        Ok(())
    }

    pub(super) fn compose_project(&self, mode: StackMode) -> ComposeProject {
        self.routed_compose_project(mode)
            .with_observability(self.config.asset_root(), mode == StackMode::Dataplane)
    }

    pub(super) fn performance_compose_project(
        &self,
        mode: StackMode,
        observability: bool,
    ) -> ComposeProject {
        let project = self.routed_compose_project(mode);
        if observability {
            project.with_observability(self.config.asset_root(), mode == StackMode::Dataplane)
        } else {
            project
        }
    }

    fn routed_compose_project(&self, mode: StackMode) -> ComposeProject {
        let project = match mode {
            StackMode::Dataplane => ComposeProject::dataplane(
                self.config.asset_root(),
                self.config.controlplane_dir(),
                self.config.integration_project().value.clone(),
                !self.config.dataplane_ref().value.is_empty(),
            ),
            StackMode::Controlplane => ComposeProject::controlplane(
                self.config.asset_root(),
                self.config.controlplane_dir(),
                self.config.controlplane_project().value.clone(),
                self.environment_flag("CONTROLPLANE_ENABLE_SSO", false),
            ),
        };
        project.with_conformance_overlay(self.config.asset_root())
    }

    pub(super) fn conformance_compose_project(&self, mode: StackMode) -> ComposeProject {
        self.conformance_runtime_project(mode)
            .with_conformance_fixture(self.config.asset_root())
    }

    pub(super) fn conformance_runtime_project(&self, mode: StackMode) -> ComposeProject {
        self.compose_project(mode)
            .with_conformance_runtime(self.config.asset_root())
    }

    pub(super) fn compose_environment(
        &self,
        command: CommandSpec,
        mode: StackMode,
        checkout_labels: bool,
    ) -> AppResult<CommandSpec> {
        let controlplane_image = self.resolved_controlplane_image()?;
        let command_environment = command.environment().clone();
        let mut command = if command.working_directory().is_some() {
            command
        } else {
            command.cwd(self.config.root())
        };
        for (key, value) in self.config.environment().iter() {
            if !command_environment.contains_key(key) {
                command = command.env(key.clone(), value.value.clone());
            }
        }
        if let Some(protocol_version) = compose_protocol_version(
            &command_environment,
            self.environment_text(COMPOSE_PROTOCOL_VERSION_ENV),
        )? {
            command = command.env(COMPOSE_PROTOCOL_VERSION_ENV, protocol_version);
        }
        let (controlplane_pull_policy, dataplane_pull_policy) = compose_pull_policies(
            mode,
            false,
            !self.config.dataplane_ref().value.is_empty(),
            self.config.controlplane_image().pull_policy(),
            self.config.dataplane_image().pull_policy(),
        );
        command = command
            .env("CF_INTEGRATION_ROOT", self.config.asset_root().as_os_str())
            .env(
                "CF_INTEGRATION_DIR",
                self.config.integration_dir().as_os_str(),
            )
            .env(
                "CF_CONTROLPLANE_DIR",
                self.config.controlplane_dir().as_os_str(),
            )
            .env("CF_DATAPLANE_DIR", self.config.dataplane_dir().as_os_str())
            .env("CF_CONTROLPLANE_IMAGE", controlplane_image.clone())
            .env("CF_CONTROLPLANE_PULL_POLICY", controlplane_pull_policy)
            .env("IMAGE_LOCAL", controlplane_image)
            .env(
                "FAST_TIME_IMAGE",
                self.config.fast_time_expected_image().value.clone(),
            )
            .env(
                "CF_DATAPLANE_IMAGE",
                self.config.dataplane_image().resolved().to_owned(),
            )
            .env("CF_DATAPLANE_PULL_POLICY", dataplane_pull_policy)
            .env("CF_DATAPLANE_PLATFORM", self.dataplane_platform()?)
            .env("JWT_SECRET_KEY", self.config.jwt_secret_key().value.clone())
            .env(
                "AUTH_ENCRYPTION_SECRET",
                self.config.auth_encryption_secret().value.clone(),
            )
            .env("MCP_CLI_BASE_URL", self.config.base_url().value.clone())
            .env(
                "PLATFORM_ADMIN_EMAIL",
                self.config.platform_admin_email().value.clone(),
            )
            .env(
                "PLATFORM_ADMIN_PASSWORD",
                self.config.platform_admin_password().value.clone(),
            )
            .env(
                "KEY_FILE_PASSWORD",
                self.config.key_file_password().value.clone(),
            );
        command = with_default_conformance_server_era(command);

        for (key, default) in [
            ("PASSWORD_CHANGE_ENFORCEMENT_ENABLED", "false"),
            ("ADMIN_REQUIRE_PASSWORD_CHANGE_ON_BOOTSTRAP", "false"),
            ("REQUIRE_PASSWORD_CHANGE_FOR_DEFAULT_PASSWORD", "false"),
            ("GATEWAY_REPLICAS", "1"),
            ("GATEWAY_CPU_RESERVATION", "1"),
            ("GATEWAY_MEM_LIMIT", "2G"),
            ("GATEWAY_MEM_RESERVATION", "512M"),
        ] {
            if self.config.environment().get(OsStr::new(key)).is_none() {
                command = command.env(key, default);
            }
        }
        let needs_docker_cpus = ["GATEWAY_CPU_LIMIT", "GUNICORN_WORKERS"]
            .into_iter()
            .any(|key| self.config.environment().get(OsStr::new(key)).is_none());
        let docker_cpus = if needs_docker_cpus {
            let value = self.capture_text(&CommandSpec::new("docker").args([
                "info",
                "--format",
                "{{.NCPU}}",
            ]))?;
            if !value.parse::<usize>().is_ok_and(|value| value > 0) {
                return Err(AppFailure::from(anyhow!(
                    "Docker returned an invalid CPU count"
                )));
            }
            Some(value)
        } else {
            None
        };
        for key in ["GATEWAY_CPU_LIMIT", "GUNICORN_WORKERS"] {
            if self.config.environment().get(OsStr::new(key)).is_none() {
                command = command.env(key, docker_cpus.as_deref().unwrap_or("4"));
            }
        }
        for (key, argument) in [("HOST_UID", "-u"), ("HOST_GID", "-g")] {
            if self.config.environment().get(OsStr::new(key)).is_none() {
                let value = self.host_identity(argument)?;
                command = command.env(key, value);
            }
        }
        if self
            .config
            .environment()
            .get(OsStr::new("LOCUST_EXPECT_WORKERS"))
            .is_none()
        {
            command = command.env(
                "LOCUST_EXPECT_WORKERS",
                self.environment_text("CONTROLPLANE_LOCUST_WORKERS")
                    .filter(|value| !value.is_empty())
                    .unwrap_or("1"),
            );
        }
        if mode == StackMode::Controlplane {
            command = command.env(
                "COMPOSE_PROJECT_NAME",
                self.config.controlplane_project().value.clone(),
            );
        }
        if checkout_labels {
            command = self.add_checkout_labels(command, mode)?;
        }
        Ok(command)
    }

    fn add_checkout_labels(
        &self,
        mut command: CommandSpec,
        mode: StackMode,
    ) -> AppResult<CommandSpec> {
        let controlplane_revision =
            self.git_required(self.config.controlplane_dir(), ["rev-parse", "HEAD"])?;
        let controlplane_branch =
            self.git_required(self.config.controlplane_dir(), ["branch", "--show-current"])?;
        let controlplane_ref = if controlplane_branch.is_empty() {
            self.config
                .controlplane_ref()
                .value
                .to_string_lossy()
                .into_owned()
        } else {
            controlplane_branch
        };
        command = command
            .env("CF_CONTROLPLANE_CHECKOUT_REVISION", controlplane_revision)
            .env("CF_CONTROLPLANE_CHECKOUT_REF", controlplane_ref);
        if mode == StackMode::Dataplane && !self.config.dataplane_ref().value.is_empty() {
            let revision = self.git_required(self.config.dataplane_dir(), ["rev-parse", "HEAD"])?;
            let branch =
                self.git_required(self.config.dataplane_dir(), ["branch", "--show-current"])?;
            let reference = if branch.is_empty() {
                self.config
                    .dataplane_ref()
                    .value
                    .to_string_lossy()
                    .into_owned()
            } else {
                branch
            };
            command = command
                .env("CF_DATAPLANE_CHECKOUT_REVISION", revision)
                .env("CF_DATAPLANE_CHECKOUT_REF", reference);
        }
        Ok(command)
    }

    fn validate_compose_contract(&self) -> AppResult<()> {
        let command = self
            .compose_project(StackMode::Dataplane)
            .command(["config", "--format", "json"]);
        let command = self.compose_environment(command, StackMode::Dataplane, true)?;
        let rendered = self.runner.capture_stdout(&command)?;
        let rendered: serde_json::Value = serde_json::from_slice(&rendered)
            .context("failed to parse rendered integration Compose JSON")
            .map_err(AppFailure::from)?;
        let expected = required_text(
            &self.config.fast_time_expected_image().value,
            "CF_FAST_TIME_EXPECTED_IMAGE",
        )?;
        let violations = validate_integration_contract(&rendered, expected);
        if violations.is_empty() {
            return Ok(());
        }
        let details = violations
            .into_iter()
            .map(|violation| format!("  - {violation}"))
            .collect::<Vec<_>>()
            .join("\n");
        Err(AppFailure::from(anyhow!(
            "Compose contract failed:\n{details}"
        )))
    }

    fn resolve_build(&self, mode: StackMode, report_progress: bool) -> AppResult<bool> {
        let setting = required_text(&self.config.compose_build().value, "CF_COMPOSE_BUILD")?;
        let mode_setting =
            BuildMode::from_str(setting).map_err(|error| AppFailure::from(anyhow!(error)))?;
        let controlplane_checkout_revision =
            Some(self.git_required(self.config.controlplane_dir(), ["rev-parse", "HEAD"])?);
        let controlplane_image = self.resolved_controlplane_image()?;
        let (controlplane_image_present, controlplane_image_revision) =
            self.image_state(&controlplane_image)?;
        let dataplane_source = (!self.config.dataplane_ref().value.is_empty()).then(|| {
            self.config
                .dataplane_ref()
                .value
                .to_string_lossy()
                .into_owned()
        });
        let dataplane_checkout_revision = if dataplane_source.is_some() {
            Some(self.git_required(self.config.dataplane_dir(), ["rev-parse", "HEAD"])?)
        } else {
            None
        };
        let (dataplane_image_present, dataplane_image_revision) =
            self.image_state(self.config.dataplane_image().resolved())?;
        let decision = resolve_build(
            mode_setting,
            &BuildInputs {
                controlplane_image_prebuilt: self.config.controlplane_image().is_prebuilt(),
                controlplane_image_present,
                controlplane_checkout_revision,
                controlplane_image_revision,
                include_dataplane: mode == StackMode::Dataplane,
                dataplane_source_ref: dataplane_source,
                dataplane_image_present,
                dataplane_checkout_revision,
                dataplane_image_revision,
            },
        );
        if report_progress {
            for reason in decision.reasons {
                println!(
                    "{}",
                    OutputStyle::stdout().info(&format!("CF_COMPOSE_BUILD: {reason}"))
                );
            }
        }
        Ok(decision.build)
    }

    fn image_state(&self, image: &OsStr) -> AppResult<(bool, Option<String>)> {
        let image_id = if image.to_string_lossy().contains("@sha256:") {
            let images = self.capture_text(&CommandSpec::new("docker").args([
                "image",
                "ls",
                "--digests",
                "--no-trunc",
                "--format",
                "{{.Repository}}@{{.Digest}}\t{{.ID}}",
            ]))?;
            images.lines().find_map(|line| {
                let (reference, id) = line.split_once('\t')?;
                (reference == image.to_string_lossy()).then(|| id.to_owned())
            })
        } else {
            self.capture_text(&CommandSpec::new("docker").args([
                OsString::from("image"),
                OsString::from("ls"),
                OsString::from("--quiet"),
                OsString::from("--no-trunc"),
                image.to_owned(),
            ]))?
            .lines()
            .next()
            .map(str::to_owned)
        };
        let Some(image_id) = image_id else {
            return Ok((false, None));
        };
        let revision = self.capture_text(&CommandSpec::new("docker").args([
            OsString::from("image"),
            OsString::from("inspect"),
            OsString::from(image_id),
            OsString::from("--format"),
            OsString::from("{{ index .Config.Labels \"org.opencontainers.image.revision\" }}"),
        ]))?;
        Ok((true, (!revision.is_empty()).then_some(revision)))
    }

    fn pull_images(&self, mode: StackMode, build: bool, report_progress: bool) -> AppResult<()> {
        if !build && self.config.controlplane_image().is_prebuilt() {
            let controlplane_image = self.resolved_controlplane_image()?;
            self.pull_if_changed(
                "cf-controlplane",
                &controlplane_image,
                None,
                self.config.controlplane_image().pull_policy(),
                report_progress,
            )?;
        }
        if mode == StackMode::Dataplane && self.config.dataplane_ref().value.is_empty() {
            let platform = self.dataplane_platform()?;
            self.pull_if_changed(
                "cf-dataplane",
                self.config.dataplane_image().resolved(),
                Some(platform.as_os_str()),
                self.config.dataplane_image().pull_policy(),
                report_progress,
            )?;
        }
        Ok(())
    }

    fn pull_if_changed(
        &self,
        label: &str,
        image: &OsStr,
        platform: Option<&OsStr>,
        pull_policy: ImagePullPolicy,
        report_progress: bool,
    ) -> AppResult<()> {
        let local_ids = self.capture_text(&CommandSpec::new("docker").args([
            OsString::from("image"),
            OsString::from("ls"),
            OsString::from("--quiet"),
            OsString::from("--no-trunc"),
            image.to_owned(),
        ]))?;
        let local_exists = !local_ids.is_empty();
        if pull_policy == ImagePullPolicy::Never {
            return require_preloaded_image(label, image, local_exists);
        }

        let inspect = CommandSpec::new("docker").args([
            OsString::from("buildx"),
            OsString::from("imagetools"),
            OsString::from("inspect"),
            image.to_owned(),
            OsString::from("--format"),
            OsString::from("{{.Manifest.Digest}}"),
        ]);
        let remote_digest = match self.capture_text(&inspect) {
            Ok(digest) => (!digest.is_empty()).then_some(digest),
            Err(error) if local_exists => {
                if report_progress {
                    eprintln!(
                        "{}",
                        OutputStyle::stderr().warning(&format!(
                            "{label} remote digest check failed; using the local image: {error}"
                        ))
                    );
                }
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if local_exists && let Some(digest) = remote_digest.as_deref() {
            let repo_digests = self.capture_text(&CommandSpec::new("docker").args([
                OsString::from("image"),
                OsString::from("inspect"),
                image.to_owned(),
                OsString::from("--format"),
                OsString::from("{{range .RepoDigests}}{{println .}}{{end}}"),
            ]))?;
            if repo_digests
                .lines()
                .any(|value| value.ends_with(&format!("@{digest}")))
            {
                if report_progress {
                    println!(
                        "{}",
                        OutputStyle::stdout()
                            .success(&format!("{label} image digest unchanged: {digest}"))
                    );
                }
                return Ok(());
            }
        } else if local_exists && remote_digest.is_none() {
            if report_progress {
                println!(
                    "{}",
                    OutputStyle::stdout().warning(&format!(
                        "{label} remote digest unavailable; using local image."
                    ))
                );
            }
            return Ok(());
        }

        let mut arguments = vec![OsString::from("pull")];
        if let Some(platform) = platform {
            arguments.push(OsString::from("--platform"));
            arguments.push(platform.to_owned());
        }
        arguments.push(image.to_owned());
        Ok(self
            .runner
            .run(&CommandSpec::new("docker").args(arguments))?)
    }

    fn integration_freshness(&self) -> AppResult<StackFreshness> {
        let controlplane_image = self.resolved_controlplane_image()?;
        let project = required_text(
            &self.config.integration_project().value,
            "CF_INTEGRATION_PROJECT",
        )?;
        let dataplane_source_enabled = !self.config.dataplane_ref().value.is_empty();
        let mut services = std::collections::BTreeMap::new();
        for service in [
            "gateway",
            "dataplane",
            "clickstack",
            "nginx",
            "postgres",
            "pgbouncer",
            "redis",
            "fast_time_server",
            "migration",
            "register_fast_time",
        ] {
            services.insert(service.to_owned(), self.service_snapshot(project, service)?);
        }
        let snapshot = FreshnessSnapshot {
            services,
            controlplane_checkout_revision: Some(
                self.git_required(self.config.controlplane_dir(), ["rev-parse", "HEAD"])?,
            ),
            dataplane_checkout_revision: if dataplane_source_enabled {
                Some(self.git_required(self.config.dataplane_dir(), ["rev-parse", "HEAD"])?)
            } else {
                None
            },
            controlplane_image_prebuilt: self.config.controlplane_image().is_prebuilt(),
            dataplane_source_enabled,
            expected_controlplane_image: required_text(
                &controlplane_image,
                "CF_CONTROLPLANE_IMAGE",
            )?
            .to_owned(),
            expected_dataplane_image: required_text(
                self.config.dataplane_image().resolved(),
                "CF_DATAPLANE_IMAGE",
            )?
            .to_owned(),
            expected_fast_time_image: required_text(
                &self.config.fast_time_expected_image().value,
                "CF_FAST_TIME_EXPECTED_IMAGE",
            )?
            .to_owned(),
        };
        Ok(snapshot.evaluate())
    }

    fn resolved_controlplane_image(&self) -> AppResult<OsString> {
        let setting = self.config.controlplane_image();
        if !setting.tracks_main_revision() {
            return Ok(setting.resolved().to_owned());
        }
        if let Some(image) = self.controlplane_image.get() {
            return Ok(image.clone());
        }

        let revisions = self.git_required(
            self.config.controlplane_dir(),
            [
                "rev-list",
                "--first-parent",
                "--max-count=50",
                "refs/remotes/origin/main",
            ],
        )?;
        let image = latest_published_controlplane_image(&revisions, |image| {
            self.runner
                .capture_output(&CommandSpec::new("docker").args([
                    OsString::from("manifest"),
                    OsString::from("inspect"),
                    image.to_owned(),
                ]))
                .is_ok()
        })
        .map_err(AppFailure::from)?;
        let _ = self.controlplane_image.set(image.clone());
        Ok(image)
    }

    fn service_snapshot(&self, project: &str, service: &str) -> AppResult<ServiceSnapshot> {
        let running_id = self.container_id(project, service, false)?;
        let all_id = self.container_id(project, service, true)?;
        let configured_image = running_id
            .as_deref()
            .map(|id| self.docker_inspect(id, "{{.Config.Image}}"))
            .transpose()?;
        let running_image = running_id
            .as_deref()
            .map(|id| self.docker_inspect(id, "{{.Image}}"))
            .transpose()?;
        let expected_image_id = configured_image
            .as_deref()
            .map(|image| {
                self.capture_text(
                    &CommandSpec::new("docker")
                        .args(["image", "inspect", image, "--format", "{{.Id}}"]),
                )
            })
            .transpose()?;
        let completed_successfully = if let Some(id) = all_id.as_deref() {
            self.docker_inspect(id, "{{.State.Status}}")? == "exited"
                && self.docker_inspect(id, "{{.State.ExitCode}}")? == "0"
        } else {
            false
        };
        let image_revision = running_id
            .as_deref()
            .map(|id| {
                self.docker_inspect(
                    id,
                    "{{ index .Config.Labels \"org.opencontainers.image.revision\" }}",
                )
            })
            .transpose()?
            .filter(|value| !value.is_empty());
        Ok(ServiceSnapshot {
            running: running_id.is_some(),
            completed_successfully,
            configured_image,
            running_image_matches_configured: running_image.is_some()
                && running_image == expected_image_id,
            image_revision,
        })
    }

    pub(super) fn container_id(
        &self,
        project: &str,
        service: &str,
        all: bool,
    ) -> AppResult<Option<String>> {
        let mut arguments = vec![OsString::from("ps")];
        arguments.push(OsString::from(if all { "-aq" } else { "-q" }));
        arguments.extend([
            OsString::from("--filter"),
            OsString::from(format!("label=com.docker.compose.project={project}")),
            OsString::from("--filter"),
            OsString::from(format!("label=com.docker.compose.service={service}")),
        ]);
        let output = self.capture_text(&CommandSpec::new("docker").args(arguments))?;
        Ok(output
            .lines()
            .next()
            .map(str::to_owned)
            .filter(|value| !value.is_empty()))
    }

    fn docker_inspect(&self, id: &str, format: &str) -> AppResult<String> {
        self.capture_text(&CommandSpec::new("docker").args(["inspect", id, "--format", format]))
    }

    pub(super) fn ensure_other_stack_stopped(&self, mode: StackMode) -> AppResult<()> {
        let (other, label) = match mode {
            StackMode::Dataplane => (
                required_text(
                    &self.config.controlplane_project().value,
                    "CF_CONTROLPLANE_PROJECT",
                )?,
                StackMode::Controlplane.lane_label(),
            ),
            StackMode::Controlplane => (
                required_text(
                    &self.config.integration_project().value,
                    "CF_INTEGRATION_PROJECT",
                )?,
                StackMode::Dataplane.lane_label(),
            ),
        };
        if self.project_has_running_containers(other)? {
            return Err(AppFailure::from(anyhow!(
                "the {label} stack is running on the same host ports; run `cf-integration stack down --lane all` first"
            )));
        }
        Ok(())
    }

    fn project_has_running_containers(&self, project: &str) -> AppResult<bool> {
        Ok(!self
            .capture_text(&CommandSpec::new("docker").args([
                "ps",
                "-q",
                "--filter",
                &format!("label=com.docker.compose.project={project}"),
            ]))?
            .is_empty())
    }

    pub(super) fn cleanup(&self, selection: LaneSelection, kind: CleanupKind) -> AppResult<()> {
        self.cleanup_with_output(selection, kind, true)
    }

    pub(super) fn cleanup_quiet(
        &self,
        selection: LaneSelection,
        kind: CleanupKind,
    ) -> AppResult<()> {
        self.cleanup_with_output(selection, kind, false)
    }

    fn cleanup_with_output(
        &self,
        selection: LaneSelection,
        kind: CleanupKind,
        inherit_output: bool,
    ) -> AppResult<()> {
        let mut cleanup_failures = Vec::new();
        for mode in selected_topologies(selection) {
            if self
                .config
                .controlplane_dir()
                .join("docker-compose.yml")
                .is_file()
            {
                let project = self
                    .compose_project(mode)
                    .with_profiles(["testing", "inspector", "sso"])
                    .with_conformance_fixture(self.config.asset_root());
                let command = StackCommandPlan::cleanup(project, kind);
                match self.compose_environment(command.command().clone(), mode, false) {
                    Ok(command) => {
                        let result = self.run_cleanup_command(&command, inherit_output);
                        if let Err(error) = result {
                            cleanup_failures.push(error.into());
                        }
                    }
                    Err(error) => cleanup_failures.push(error),
                }
            }
            let project = match mode {
                StackMode::Controlplane => &self.config.controlplane_project().value,
                StackMode::Dataplane => &self.config.integration_project().value,
            };
            match required_text(project, "Compose project name")
                .and_then(|project| self.remove_project_by_label(project, kind, inherit_output))
            {
                Ok(()) => {}
                Err(error) => cleanup_failures.push(error),
            }
        }
        finish_with_cleanup_failures(None, cleanup_failures)
    }

    fn remove_project_by_label(
        &self,
        project: &str,
        kind: CleanupKind,
        inherit_output: bool,
    ) -> AppResult<()> {
        let filter = format!("label=com.docker.compose.project={project}");
        for id in self.docker_list(["ps", "-aq", "--filter", filter.as_str()])? {
            self.run_cleanup_command(
                &CommandSpec::new("docker").args(["rm", "-f", id.as_str()]),
                inherit_output,
            )?;
        }
        for id in self.docker_list(["network", "ls", "-q", "--filter", filter.as_str()])? {
            let _ = self.run_cleanup_command(
                &CommandSpec::new("docker").args(["network", "rm", id.as_str()]),
                inherit_output,
            );
        }
        if kind == CleanupKind::Reset {
            for id in self.docker_list(["volume", "ls", "-q", "--filter", filter.as_str()])? {
                self.run_cleanup_command(
                    &CommandSpec::new("docker").args(["volume", "rm", id.as_str()]),
                    inherit_output,
                )?;
            }
        }
        Ok(())
    }

    fn run_cleanup_command(
        &self,
        command: &CommandSpec,
        inherit_output: bool,
    ) -> Result<(), InfrastructureError> {
        if inherit_output {
            self.runner.run(command)
        } else {
            self.runner.capture_output(command).map(drop)
        }
    }

    fn docker_list<const N: usize>(&self, arguments: [&str; N]) -> AppResult<Vec<String>> {
        let output = self
            .runner
            .capture_stdout(&CommandSpec::new("docker").args(arguments))?;
        let output = String::from_utf8(output)
            .context("Docker returned non-UTF-8 resource identifiers")
            .map_err(AppFailure::from)?;
        Ok(output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }

    fn dataplane_platform(&self) -> AppResult<OsString> {
        if self.config.dataplane_platform().value != "auto" {
            return Ok(self.config.dataplane_platform().value.clone());
        }
        if self.config.dataplane_ref().value.is_empty() {
            return Ok(OsString::from("linux/amd64"));
        }
        let platform = self.capture_text(&CommandSpec::new("docker").args([
            "version",
            "--format",
            "{{.Server.Os}}/{{.Server.Arch}}",
        ]))?;
        if platform.is_empty() {
            return Err(AppFailure::from(anyhow!(
                "Docker returned an empty server platform"
            )));
        }
        Ok(OsString::from(platform))
    }

    pub(super) fn git_required<const N: usize>(
        &self,
        directory: &Path,
        arguments: [&str; N],
    ) -> AppResult<String> {
        let mut command = CommandSpec::new("git").arg("-C").arg(directory.as_os_str());
        command = command.args(arguments);
        let output = self.runner.capture_stdout(&command)?;
        String::from_utf8(output)
            .context("Git returned non-UTF-8 revision data")
            .map(|value| value.trim().to_owned())
            .map_err(AppFailure::from)
    }

    pub(super) fn capture_text(&self, command: &CommandSpec) -> AppResult<String> {
        let output = self.runner.capture_stdout(command)?;
        String::from_utf8(output)
            .context("child process returned non-UTF-8 standard output")
            .map(|value| value.trim().to_owned())
            .map_err(AppFailure::from)
    }

    #[cfg(unix)]
    fn host_identity(&self, argument: &str) -> AppResult<String> {
        let value = self.capture_text(&CommandSpec::new("id").arg(argument))?;
        if value.parse::<u32>().is_err() {
            return Err(AppFailure::from(anyhow!(
                "id {argument} returned an invalid host identity"
            )));
        }
        Ok(value)
    }

    #[cfg(not(unix))]
    fn host_identity(&self, _argument: &str) -> AppResult<String> {
        Ok("1000".to_owned())
    }

    pub(super) fn environment_text(&self, key: &str) -> Option<&str> {
        self.config
            .environment()
            .get(OsStr::new(key))
            .and_then(|value| value.value.to_str())
    }

    pub(super) fn environment_flag(&self, key: &str, default: bool) -> bool {
        self.environment_text(key)
            .map_or(default, |value| matches!(value, "true" | "1"))
    }
}

fn require_preloaded_image(label: &str, image: &OsStr, local_exists: bool) -> AppResult<()> {
    if local_exists {
        return Ok(());
    }
    Err(AppFailure::from(anyhow!(
        "{label} image {} is not loaded locally; preload it or set its pull policy to always",
        image.to_string_lossy()
    )))
}

fn compose_protocol_version(
    command_environment: &BTreeMap<OsString, OsString>,
    configured: Option<&str>,
) -> AppResult<Option<&'static str>> {
    if command_environment.contains_key(OsStr::new(COMPOSE_PROTOCOL_VERSION_ENV)) {
        return Ok(None);
    }
    let mode = configured
        .filter(|value| !value.is_empty())
        .map(str::parse::<ProtocolVersion>)
        .transpose()
        .map_err(|error| {
            AppFailure::from(anyhow!("invalid {COMPOSE_PROTOCOL_VERSION_ENV}: {error}"))
        })?
        .unwrap_or_default();
    Ok(Some(mode.wire_version()))
}

fn compose_pull_policies(
    mode: StackMode,
    build: bool,
    dataplane_source: bool,
    controlplane: ImagePullPolicy,
    dataplane: ImagePullPolicy,
) -> (&'static str, &'static str) {
    let controlplane = if build && (mode == StackMode::Controlplane || !dataplane_source) {
        "build"
    } else {
        controlplane.as_str()
    };
    let dataplane = if dataplane_source {
        if build && mode == StackMode::Dataplane {
            "build"
        } else {
            "never"
        }
    } else {
        dataplane.as_str()
    };
    (controlplane, dataplane)
}

fn latest_published_controlplane_image(
    revisions: &str,
    mut is_published: impl FnMut(&OsStr) -> bool,
) -> anyhow::Result<OsString> {
    const IMAGE_PREFIX: &str = "ghcr.io/ibm/mcp-context-forge:";

    let mut inspected = 0;
    for revision in revisions
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(anyhow!(
                "Git returned an invalid control-plane main revision {revision:?}"
            ));
        }
        inspected += 1;
        let image = OsString::from(format!("{IMAGE_PREFIX}{revision}"));
        if is_published(&image) {
            return Ok(image);
        }
    }

    Err(anyhow!(
        "no published control-plane image was found in the newest {inspected} commits on main"
    ))
}

fn format_stack_endpoint_summary(
    public_origin: &str,
    public_mcp_endpoint: &url::Url,
    conformance_endpoint: &url::Url,
    observability_ui: &str,
) -> String {
    format!(
        "Gateway/API: {public_origin}\nPublic MCP: {public_mcp_endpoint}\nConformance MCP (direct): {conformance_endpoint}\nObservability: {observability_ui}"
    )
}

fn prepare_conformance_build_log(path: &Path) -> AppResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppFailure::from(anyhow!("conformance build log has no parent")))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create conformance build log directory {parent:?}"))
        .map_err(AppFailure::from)?;
    fs::write(path, [])
        .with_context(|| format!("failed to clear conformance build log {path:?}"))
        .map_err(AppFailure::from)
}

fn with_default_conformance_server_era(command: CommandSpec) -> CommandSpec {
    if command
        .environment()
        .get(OsStr::new(CONFORMANCE_SERVER_ERA_ENV))
        .is_some_and(|value| !value.is_empty())
    {
        command
    } else {
        command.env(
            CONFORMANCE_SERVER_ERA_ENV,
            DEFAULT_CONFORMANCE_SERVER_ERA.label(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_tracking_selects_the_newest_published_controlplane_commit_image() {
        let newest = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let published = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let mut inspected = Vec::new();

        let image =
            latest_published_controlplane_image(&format!("{newest}\n{published}\n"), |candidate| {
                inspected.push(candidate.to_owned());
                candidate == OsStr::new(&format!("ghcr.io/ibm/mcp-context-forge:{published}"))
            })
            .expect("a published main image should be selected");

        assert_eq!(
            image,
            OsString::from(format!("ghcr.io/ibm/mcp-context-forge:{published}"))
        );
        assert_eq!(inspected.len(), 2);
    }

    #[test]
    fn main_tracking_fails_when_no_main_commit_image_is_published() {
        let error = latest_published_controlplane_image(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
            |_| false,
        )
        .expect_err("an unpublished main history must fail explicitly");

        assert!(error.to_string().contains("newest 1 commits on main"));
    }

    #[test]
    fn never_pull_policy_accepts_a_preloaded_image() {
        require_preloaded_image("cf-dataplane", OsStr::new("local/dataplane:test"), true)
            .expect("a preloaded image should satisfy the never pull policy");
    }

    #[test]
    fn never_pull_policy_rejects_a_missing_local_image() {
        let error =
            require_preloaded_image("cf-dataplane", OsStr::new("local/dataplane:test"), false)
                .expect_err("a missing preloaded image must fail");

        assert_eq!(
            error.to_string(),
            "cf-dataplane image local/dataplane:test is not loaded locally; preload it or set its pull policy to always"
        );
    }

    #[test]
    fn source_dataplane_build_does_not_change_the_controlplane_policy() {
        assert_eq!(
            compose_pull_policies(
                StackMode::Dataplane,
                true,
                true,
                ImagePullPolicy::Always,
                ImagePullPolicy::Always,
            ),
            ("always", "build")
        );
    }

    #[test]
    fn current_source_dataplane_uses_its_cached_image_without_rebuilding() {
        assert_eq!(
            compose_pull_policies(
                StackMode::Dataplane,
                false,
                true,
                ImagePullPolicy::Always,
                ImagePullPolicy::Always,
            ),
            ("always", "never")
        );
    }

    #[test]
    fn controlplane_build_uses_compose_build_policy() {
        assert_eq!(
            compose_pull_policies(
                StackMode::Controlplane,
                true,
                false,
                ImagePullPolicy::Always,
                ImagePullPolicy::Always,
            ),
            ("build", "always")
        );
    }

    #[test]
    fn compose_commands_default_to_the_latest_modern_conformance_era() {
        let command = with_default_conformance_server_era(CommandSpec::new("docker"));

        assert_eq!(
            command
                .environment()
                .get(OsStr::new(CONFORMANCE_SERVER_ERA_ENV)),
            Some(&OsString::from("modern"))
        );
    }

    #[test]
    fn compose_translates_semantic_protocol_modes_to_wire_revisions() {
        let command_environment = BTreeMap::new();

        assert_eq!(
            compose_protocol_version(&command_environment, None)
                .expect("default protocol mode should resolve"),
            Some("2026-07-28")
        );
        assert_eq!(
            compose_protocol_version(&command_environment, Some("legacy"))
                .expect("legacy protocol mode should resolve"),
            Some("2025-11-25")
        );
    }

    #[test]
    fn compose_preserves_an_explicit_internal_wire_revision() {
        let command_environment = BTreeMap::from([(
            OsString::from(COMPOSE_PROTOCOL_VERSION_ENV),
            OsString::from("2025-11-25"),
        )]);

        assert_eq!(
            compose_protocol_version(&command_environment, Some("modern"))
                .expect("explicit command environment should be preserved"),
            None
        );
    }

    #[test]
    fn explicit_conformance_era_is_not_replaced_by_the_stack_default() {
        let command = with_default_conformance_server_era(
            CommandSpec::new("docker").env(CONFORMANCE_SERVER_ERA_ENV, "legacy"),
        );

        assert_eq!(
            command
                .environment()
                .get(OsStr::new(CONFORMANCE_SERVER_ERA_ENV)),
            Some(&OsString::from("legacy"))
        );
    }

    #[test]
    fn stack_endpoint_summary_preserves_the_three_connection_addresses() {
        let public_mcp =
            url::Url::parse("http://127.0.0.1:8080/servers/server-id/mcp").expect("public URL");
        let conformance = url::Url::parse("http://127.0.0.1:49152/mcp").expect("conformance URL");

        let summary = format_stack_endpoint_summary(
            "http://127.0.0.1:8080",
            &public_mcp,
            &conformance,
            "http://127.0.0.1:3000",
        );

        assert_eq!(
            summary,
            "Gateway/API: http://127.0.0.1:8080\nPublic MCP: http://127.0.0.1:8080/servers/server-id/mcp\nConformance MCP (direct): http://127.0.0.1:49152/mcp\nObservability: http://127.0.0.1:3000"
        );
    }

    #[test]
    fn conformance_build_log_is_cleared_before_quiet_execution() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("logs/conformance-build.log");
        fs::create_dir_all(path.parent().expect("log should have a parent"))
            .expect("log directory should be created");
        fs::write(&path, "stale output").expect("stale log should be written");

        prepare_conformance_build_log(&path).expect("conformance build log should be prepared");

        assert_eq!(
            fs::read(path).expect("stack setup log should be readable"),
            b""
        );
    }
}
