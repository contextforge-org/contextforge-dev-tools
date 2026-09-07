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
const STANDALONE_TENANT_ID: &str = "cf-integration";
const STANDALONE_USER_ID: &str = "cf-integration@example.invalid";

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
        if let Some(token) = self.token.as_ref()
            && let Err(error) = self.runtime.revoke_managed_token(token).await
        {
            cleanup_failures.push(error);
        }
        let cleanup = if self.standalone {
            self.runtime.cleanup_standalone_dataplane(CleanupKind::Down)
        } else {
            self.runtime
                .cleanup_quiet(topology_selection(self.topology), CleanupKind::Down)
        };
        if let Err(error) = cleanup {
            cleanup_failures.push(error);
        }
        finish_with_cleanup_failures(primary.err(), cleanup_failures)
    }
}

impl<R: ProcessRunner> RuntimeContext<R> {
    pub(super) fn standalone_dataplane_token(
        &self,
        observability: bool,
    ) -> AppResult<ManagedBearerToken> {
        let command = self.standalone_dataplane_project(observability).command([
            "run",
            "--rm",
            "--no-deps",
            "config_writer",
            "token",
            STANDALONE_TENANT_ID,
            STANDALONE_USER_ID,
        ]);
        let command = self.standalone_dataplane_environment(command, true)?;
        let output = self.runner.capture_stdout(&command)?;
        let token = std::str::from_utf8(&output)
            .context("standalone dataplane token helper returned non-UTF-8 output")
            .map_err(AppFailure::from)?
            .trim();
        if token.split('.').count() != 3 {
            return Err(AppFailure::from(anyhow!(
                "standalone dataplane token helper returned an invalid JWT"
            )));
        }
        Ok(ManagedBearerToken::unmanaged(token.to_owned()))
    }

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
        let primary = async {
            self.stack_up(topology, false).await?;
            self.prepare_test_target(topology, server_id).await?;
            operation().await
        }
        .await;
        scope.finish(primary).await
    }

    pub(super) async fn with_managed_authenticated_target<F, Fut>(
        &self,
        topology: StackMode,
        server_id: &str,
        standalone: bool,
        observability: bool,
        protocol_version: &ProtocolVersion,
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
        let primary = async {
            let token = if standalone {
                self.stack_up_standalone_dataplane(false, observability)
                    .await?;
                self.start_standalone_fixture(protocol_version, observability)
                    .await?;
                self.standalone_dataplane_token(observability)?
            } else {
                let project = self.performance_compose_project(topology, observability);
                self.stack_up_with_project(topology, false, project, false, observability)
                    .await?;
                self.prepare_test_target(topology, server_id).await?;
                self.managed_bearer_token(topology, server_id).await?
            };
            let value = token.value.clone();
            scope.token = Some(token);
            let tool_names = if standalone {
                self.publish_standalone_conformance_config(
                    server_id,
                    protocol_version.wire_version(),
                    &value,
                    observability,
                )?
            } else {
                Vec::new()
            };
            operation(value, tool_names).await
        }
        .await;
        scope.finish(primary).await
    }

    async fn start_standalone_fixture(
        &self,
        protocol_version: &ProtocolVersion,
        observability: bool,
    ) -> AppResult<()> {
        let server_era = match protocol_version {
            ProtocolVersion::Modern => ConformanceServerEra::Modern,
            ProtocolVersion::Legacy => ConformanceServerEra::Legacy,
        };
        self.start_conformance_service(StackMode::Dataplane, server_era, true, observability)
            .await
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
