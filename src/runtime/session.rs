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
    token: Option<ManagedBearerToken>,
}

impl<'a, R: ProcessRunner> ManagedSessionScope<'a, R> {
    fn new(runtime: &'a RuntimeContext<R>, topology: StackMode) -> Self {
        Self {
            runtime,
            topology,
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
        let scope = ManagedSessionScope::new(self, topology);
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
        operation: F,
    ) -> AppResult<()>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = AppResult<()>>,
    {
        let mut scope = ManagedSessionScope::new(self, topology);
        let primary = match self.stack_up(topology, false).await {
            Ok(()) => match self.prepare_test_target(topology, server_id).await {
                Ok(()) => match self.managed_bearer_token(topology, server_id).await {
                    Ok(token) => {
                        let value = token.value.clone();
                        scope.token = Some(token);
                        operation(value).await
                    }
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        scope.finish(primary).await
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
        let project = required_text(
            &self.config.integration_project().value,
            "CF_INTEGRATION_PROJECT",
        )?;
        let redis = self.container_id(project, "redis", false)?.ok_or_else(|| {
            AppFailure::from(anyhow!(
                "cannot wait for publisher snapshot: the dataplane Redis container is not running"
            ))
        })?;
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

    pub(super) fn environment_u64(&self, key: &str, default: u64) -> AppResult<u64> {
        self.environment_text(key).map_or(Ok(default), |value| {
            value
                .parse::<u64>()
                .map_err(|_| AppFailure::from(anyhow!("{key} must be a non-negative integer")))
        })
    }
}
