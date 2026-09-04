//! Upstream live-gateway test orchestration.

use super::*;

const LIVE_ALL_TARGETS: [&str; 3] = [
    "test-mcp-protocol-e2e",
    "test-mcp-rbac",
    "test-protocol-compliance-gateway",
];

impl<R: ProcessRunner> RuntimeContext<R> {
    pub(super) async fn run_live(
        &self,
        lane: SemanticLane,
        standalone: bool,
        group: LiveGroup,
        protocol_version: &ProtocolVersion,
    ) -> AppResult<()> {
        match lane {
            SemanticLane::FixtureDirect => {
                if group != LiveGroup::Protocol {
                    return Err(AppFailure::from(anyhow!(
                        "fixture-direct live lane requires the protocol group"
                    )));
                }
                self.ensure_controlplane()?;
                self.run_controlplane_make(
                    StackMode::Controlplane,
                    "test-protocol-compliance-reference",
                    protocol_version,
                )
            }
            SemanticLane::BuiltInDataPlane => {
                self.run_routed_live(StackMode::Controlplane, false, group, protocol_version)
                    .await
            }
            SemanticLane::ExternalDataPlane => {
                self.run_routed_live(StackMode::Dataplane, standalone, group, protocol_version)
                    .await
            }
        }
    }

    async fn run_routed_live(
        &self,
        topology: StackMode,
        standalone: bool,
        group: LiveGroup,
        protocol_version: &ProtocolVersion,
    ) -> AppResult<()> {
        let server_id = self.default_server_id().to_owned();
        if standalone {
            return self
                .with_managed_authenticated_target(
                    topology,
                    &server_id,
                    true,
                    protocol_version,
                    |token, tool_names| async move {
                        self.run_standalone_live(
                            topology,
                            group,
                            protocol_version,
                            token,
                            tool_names,
                        )
                        .await
                    },
                )
                .await;
        }
        self.with_managed_test_target(topology, &server_id, || async {
            self.run_live_group(topology, group, protocol_version)
        })
        .await
    }

    async fn run_standalone_live(
        &self,
        topology: StackMode,
        group: LiveGroup,
        protocol_version: &ProtocolVersion,
        token: String,
        tool_names: Vec<String>,
    ) -> AppResult<()> {
        let server_id = self.default_server_id().to_owned();
        let config = ProbeConfig {
            mode: gateway_topology(topology),
            base_url: self.base_url()?.to_owned(),
            server_id: server_id.clone(),
            bearer_token: token.clone(),
            config_timeout: Duration::ZERO,
            retry_interval: Duration::ZERO,
            request_timeout: Duration::from_secs(
                self.environment_u64("CF_PROBE_REQUEST_TIMEOUT", 30)?,
            ),
            protocol_version: protocol_version.wire_version().to_owned(),
            tool_names,
            output_style: OutputStyle::stdout(),
        };
        let transport =
            GatewayClient::builder(config.mode, &config.base_url, &config.server_id, &token)
                .protocol_version(config.protocol_version.clone())
                .build()
                .context("failed to construct the standalone live gateway endpoint")
                .map_err(AppFailure::from)?;
        let started = std::time::Instant::now();
        let stdout = std::io::stdout();
        let mut output = stdout.lock();
        let result = run_probe(&transport, &config, &mut output)
            .await
            .map_err(AppFailure::from);
        println!(
            "{}",
            OutputStyle::stdout().test_result(
                if result.is_ok() {
                    TestStatus::Pass
                } else {
                    TestStatus::Fail
                },
                &format!("live::standalone-{}", live_group_label(group)),
                Some(started.elapsed()),
                None,
            )
        );
        result
    }

    fn run_live_group(
        &self,
        topology: StackMode,
        group: LiveGroup,
        protocol_version: &ProtocolVersion,
    ) -> AppResult<()> {
        match group {
            LiveGroup::Mcp => {
                self.run_controlplane_make(topology, "test-mcp-protocol-e2e", protocol_version)
            }
            LiveGroup::Rbac => {
                self.run_controlplane_make(topology, "test-mcp-rbac", protocol_version)
            }
            LiveGroup::Protocol => self.run_controlplane_make(
                topology,
                "test-protocol-compliance-gateway",
                protocol_version,
            ),
            LiveGroup::All => self.run_live_all(topology, protocol_version),
        }
    }

    fn run_controlplane_make(
        &self,
        topology: StackMode,
        target: &str,
        protocol_version: &ProtocolVersion,
    ) -> AppResult<()> {
        let started = std::time::Instant::now();
        let result = (|| {
            let command = CommandSpec::new("make")
                .arg("-C")
                .arg(self.config.controlplane_dir().as_os_str())
                .arg(target);
            let command = self.live_protocol_environment(command, protocol_version)?;
            let command = self.compose_environment(command, topology, false)?;
            self.runner.run(&command).map_err(AppFailure::from)
        })();
        let status = if result.is_ok() {
            TestStatus::Pass
        } else {
            TestStatus::Fail
        };
        println!(
            "{}",
            OutputStyle::stdout().test_result(
                status,
                &format!("live::{target}"),
                Some(started.elapsed()),
                None,
            )
        );
        result
    }

    fn run_live_all(
        &self,
        topology: StackMode,
        protocol_version: &ProtocolVersion,
    ) -> AppResult<()> {
        combine_live_results(LIVE_ALL_TARGETS.map(|target| {
            (
                target,
                self.run_controlplane_make(topology, target, protocol_version),
            )
        }))
    }

    fn live_protocol_environment(
        &self,
        command: CommandSpec,
        protocol_version: &ProtocolVersion,
    ) -> AppResult<CommandSpec> {
        let inherited_python_path = self
            .config
            .environment()
            .get(OsStr::new("PYTHONPATH"))
            .map(|value| value.value.as_os_str());
        add_live_protocol_environment(
            command,
            &self
                .config
                .asset_root()
                .join("scripts")
                .join("live_protocol"),
            inherited_python_path,
            protocol_version.wire_version(),
        )
    }
}

const fn live_group_label(group: LiveGroup) -> &'static str {
    match group {
        LiveGroup::Mcp => "mcp",
        LiveGroup::Rbac => "rbac",
        LiveGroup::Protocol => "protocol",
        LiveGroup::All => "all",
    }
}

fn add_live_protocol_environment(
    command: CommandSpec,
    hook_directory: &Path,
    inherited_python_path: Option<&OsStr>,
    protocol_version: &str,
) -> AppResult<CommandSpec> {
    let python_paths = std::iter::once(hook_directory.to_path_buf()).chain(
        inherited_python_path
            .into_iter()
            .flat_map(std::env::split_paths),
    );
    let python_path = std::env::join_paths(python_paths)
        .context("failed to construct live-test PYTHONPATH")
        .map_err(AppFailure::from)?;
    Ok(command
        .env("PYTHONPATH", python_path)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("MCP_PROTOCOL_VERSION", protocol_version)
        .env("CF_LIVE_MCP_PROTOCOL_VERSION", protocol_version))
}

fn combine_live_results(
    results: impl IntoIterator<Item = (&'static str, AppResult<()>)>,
) -> AppResult<()> {
    let failures = results
        .into_iter()
        .filter_map(|(group, result)| result.err().map(|error| format!("{group}: {error}")))
        .collect::<Vec<_>>();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(AppFailure::from(anyhow!(
            "live-test groups failed: {}",
            failures.join("; ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_all_is_the_exact_union_of_documented_groups() {
        assert_eq!(
            LIVE_ALL_TARGETS,
            [
                "test-mcp-protocol-e2e",
                "test-mcp-rbac",
                "test-protocol-compliance-gateway"
            ]
        );
    }

    #[test]
    fn every_live_all_failure_is_preserved() {
        let error = combine_live_results([
            ("mcp", Err(AppFailure::from(anyhow!("first failure")))),
            ("rbac", Ok(())),
            ("protocol", Err(AppFailure::from(anyhow!("second failure")))),
        ])
        .expect_err("multiple failures should fail the live workflow")
        .to_string();

        assert!(error.contains("mcp: first failure"));
        assert!(error.contains("protocol: second failure"));
    }

    #[test]
    fn live_protocol_environment_prepends_hook_and_sets_selected_version() {
        let command = add_live_protocol_environment(
            CommandSpec::new("pytest"),
            Path::new("/harness/live-protocol"),
            Some(OsStr::new("/existing/python")),
            "2025-06-18",
        )
        .expect("live protocol environment should be valid");

        let environment = command.environment();
        assert_eq!(
            environment.get(OsStr::new("MCP_PROTOCOL_VERSION")),
            Some(&OsString::from("2025-06-18"))
        );
        assert_eq!(
            environment.get(OsStr::new("CF_LIVE_MCP_PROTOCOL_VERSION")),
            Some(&OsString::from("2025-06-18"))
        );
        assert_eq!(
            environment.get(OsStr::new("PYTHONDONTWRITEBYTECODE")),
            Some(&OsString::from("1"))
        );
        let paths = std::env::split_paths(
            environment
                .get(OsStr::new("PYTHONPATH"))
                .expect("PYTHONPATH should be set"),
        )
        .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                PathBuf::from("/harness/live-protocol"),
                PathBuf::from("/existing/python")
            ]
        );
    }
}
