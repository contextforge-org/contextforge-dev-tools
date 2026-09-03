//! Managed stack, target, and credential session scope shared by workflows.

use super::*;

const PUBLISHER_SNAPSHOT_LUA: &str = r#"
for _, key in ipairs(redis.call('KEYS', '*UserConfig*')) do
    local value = redis.call('GET', key)
    if value then
        local decoded, config = pcall(cmsgpack.unpack, value)
        if decoded
            and type(config) == 'table'
            and type(config.virtual_hosts) == 'table'
            and config.virtual_hosts[ARGV[1]] ~= nil then
            return 1
        end
    end
end
return 0
"#;

struct ManagedSessionScope<'a, R> {
    runtime: &'a RuntimeContext<R>,
    topology: StackMode,
    standalone: bool,
    token: Option<ManagedBearerToken>,
}

impl<'a, R: ProcessRunner> ManagedSessionScope<'a, R> {
    fn new(runtime: &'a RuntimeContext<R>, topology: StackMode, standalone: bool) -> Self {
        Self {
            runtime,
            topology,
            standalone,
            token: None,
        }
    }

    async fn finish(self, primary: AppResult<()>) -> AppResult<()> {
        let mut cleanup_failures = Vec::new();
        if self.standalone
            && self
                .token
                .as_ref()
                .is_some_and(|token| token.catalog_id.is_some())
            && let Err(error) = self.runtime.restore_control_plane_gateway().await
        {
            cleanup_failures.push(error);
        }
        if let Some(token) = self.token.as_ref()
            && let Err(error) = self.runtime.revoke_managed_token(token).await
        {
            cleanup_failures.push(error);
        }
        if let Err(error) = self
            .runtime
            .cleanup_quiet(topology_selection(self.topology), CleanupKind::Down)
        {
            cleanup_failures.push(error);
        }
        finish_with_cleanup_failures(primary.err(), cleanup_failures)
    }
}

impl<R: ProcessRunner> RuntimeContext<R> {
    pub(super) async fn with_managed_test_target<F, Fut>(
        &self,
        topology: StackMode,
        server_id: &str,
        operation: F,
    ) -> AppResult<()>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = AppResult<()>>,
    {
        let scope = ManagedSessionScope::new(self, topology, false);
        let primary = match self.stack_up(topology, false).await {
            Ok(()) => match self.prepare_test_target(topology, server_id).await {
                Ok(()) => operation().await,
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        scope.finish(primary).await
    }

    pub(super) async fn with_managed_authenticated_target<F, Fut>(
        &self,
        topology: StackMode,
        server_id: &str,
        standalone: bool,
        operation: F,
    ) -> AppResult<()>
    where
        F: FnOnce(String, Vec<String>) -> Fut,
        Fut: Future<Output = AppResult<()>>,
    {
        self.with_managed_authenticated_target_project(
            topology,
            server_id,
            standalone,
            self.compose_project(topology),
            operation,
        )
        .await
    }

    pub(super) async fn with_managed_performance_target<F, Fut>(
        &self,
        topology: StackMode,
        server_id: &str,
        standalone: bool,
        observability: bool,
        operation: F,
    ) -> AppResult<()>
    where
        F: FnOnce(String, Vec<String>) -> Fut,
        Fut: Future<Output = AppResult<()>>,
    {
        self.with_managed_authenticated_target_project(
            topology,
            server_id,
            standalone,
            self.performance_compose_project(topology, observability),
            operation,
        )
        .await
    }

    async fn with_managed_authenticated_target_project<F, Fut>(
        &self,
        topology: StackMode,
        server_id: &str,
        standalone: bool,
        project: ComposeProject,
        operation: F,
    ) -> AppResult<()>
    where
        F: FnOnce(String, Vec<String>) -> Fut,
        Fut: Future<Output = AppResult<()>>,
    {
        if standalone && topology != StackMode::Dataplane {
            return Err(AppFailure::from(anyhow!(
                "standalone mode requires the external lane"
            )));
        }
        let mut scope = ManagedSessionScope::new(self, topology, standalone);
        let primary = match self
            .stack_up_with_project(topology, false, project, false)
            .await
        {
            Ok(()) => match self
                .prepare_authenticated_target(topology, server_id, standalone)
                .await
            {
                Ok(()) => match self.managed_bearer_token(topology, server_id).await {
                    Ok(token) => {
                        let value = token.value.clone();
                        scope.token = Some(token);
                        let tool_names = if standalone {
                            self.isolate_external_dataplane(server_id, &value).await
                        } else {
                            Ok(Vec::new())
                        };
                        match tool_names {
                            Ok(tool_names) => operation(value, tool_names).await,
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        scope.finish(primary).await
    }

    async fn prepare_authenticated_target(
        &self,
        topology: StackMode,
        server_id: &str,
        standalone: bool,
    ) -> AppResult<()> {
        if standalone {
            self.ensure_other_stack_stopped(topology)?;
            return Ok(());
        }
        self.prepare_test_target(topology, server_id).await
    }

    pub(super) async fn prepare_test_target(
        &self,
        topology: StackMode,
        server_id: &str,
    ) -> AppResult<()> {
        self.ensure_other_stack_stopped(topology)?;
        if topology == StackMode::Dataplane {
            self.wait_for_publisher_snapshot(server_id).await?;
        }
        Ok(())
    }

    pub(super) async fn wait_for_publisher_snapshot(&self, server_id: &str) -> AppResult<()> {
        let timeout_seconds = self.environment_u64("CF_PUBLISHER_WAIT_SECONDS", 90)?;
        let redis = self.dataplane_redis_container()?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds);
        loop {
            let command = CommandSpec::new("docker").args([
                "exec",
                redis.as_str(),
                "redis-cli",
                "EVAL",
                PUBLISHER_SNAPSHOT_LUA,
                "0",
                server_id,
            ]);
            if self.capture_text(&command)?.as_str() == "1" {
                return Ok(());
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(AppFailure::from(anyhow!(
                    "publisher snapshot did not contain server {server_id} within {timeout_seconds}s; inspect the dataplane publisher and Redis logs"
                )));
            }
            tokio::time::sleep(
                deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_secs(2)),
            )
            .await;
        }
    }

    async fn isolate_external_dataplane(
        &self,
        server_id: &str,
        token: &str,
    ) -> AppResult<Vec<String>> {
        let project = self.compose_project(StackMode::Dataplane);
        let command = project.command([
            "--profile",
            "standalone-load",
            "up",
            "--detach",
            "--wait",
            "standalone_load_backend",
        ]);
        let command = self.compose_environment(command, StackMode::Dataplane, true)?;
        self.runner.run(&command)?;
        let command = StackCommandPlan::stop_service(project.clone(), "gateway");
        let command =
            self.compose_environment(command.command().clone(), StackMode::Dataplane, true)?;
        self.runner.run(&command)?;
        let command = project.command([
            "run",
            "--rm",
            "--no-deps",
            "-e",
            "MCPGATEWAY_BEARER_TOKEN",
            "--entrypoint",
            "python3",
            "gateway",
            "/opt/contextforge-integration/prepare_standalone_config.py",
            server_id,
            ProtocolVersion::Modern.wire_version(),
        ]);
        let command = self
            .compose_environment(command, StackMode::Dataplane, true)?
            .env("MCPGATEWAY_BEARER_TOKEN", token);
        let tool_names = self.capture_text(&command)?;
        let tool_names = serde_json::from_str::<Vec<String>>(&tool_names)
            .context("standalone config helper returned invalid tool names")
            .map_err(AppFailure::from)?;
        if tool_names.is_empty() {
            return Err(AppFailure::from(anyhow!(
                "standalone Redis config for server {server_id} contains no tools"
            )));
        }
        let command = StackCommandPlan::restart_service(project, "dataplane");
        let command =
            self.compose_environment(command.command().clone(), StackMode::Dataplane, true)?;
        self.runner.run(&command)?;
        self.wait_for_public_endpoint(StackMode::Dataplane, false)
            .await?;
        Ok(tool_names)
    }

    async fn restore_control_plane_gateway(&self) -> AppResult<()> {
        let project = self.compose_project(StackMode::Dataplane);
        let command = StackCommandPlan::start_service(project, "gateway");
        let command =
            self.compose_environment(command.command().clone(), StackMode::Dataplane, true)?;
        self.runner.run(&command)?;
        self.wait_for_public_endpoint(StackMode::Controlplane, false)
            .await
    }

    fn dataplane_redis_container(&self) -> AppResult<String> {
        let project = required_text(
            &self.config.integration_project().value,
            "CF_INTEGRATION_PROJECT",
        )?;
        self.container_id(project, "redis", false)?.ok_or_else(|| {
            AppFailure::from(anyhow!("the external lane Redis container is not running"))
        })
    }

    pub(super) fn environment_u64(&self, key: &str, default: u64) -> AppResult<u64> {
        self.environment_text(key).map_or(Ok(default), |value| {
            value
                .parse::<u64>()
                .map_err(|_| AppFailure::from(anyhow!("{key} must be a non-negative integer")))
        })
    }
}
