//! Docker lifecycle shared by conformance and Inspector.

use super::*;

impl<R: ProcessRunner> RuntimeContext<R> {
    pub(super) async fn run_tool(
        &self,
        compose: CommandSpec,
        arguments: &[OsString],
        artifacts: Option<&Path>,
        log: Option<&Path>,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> AppResult<()> {
        let compose = self.host_identity_environment(compose)?;
        let build = compose.clone().args(["build", "mcp_tools"]);
        self.run_tool_process(&build, log, cancellation.clone())
            .await?;
        if *cancellation.borrow() {
            return Err(AppFailure::from(anyhow!(
                "tool cancelled before container startup"
            )));
        }
        let name = format!("cf-mcp-tools-{}", uuid::Uuid::new_v4().simple());
        let mut command =
            compose.args(["run", "--no-deps", "--pull", "never", "-T", "--name", &name]);
        if let Some(directory) = artifacts {
            let mut volume = directory.as_os_str().to_owned();
            volume.push(":/artifacts");
            command = command.arg("--volume").arg(volume);
        }
        command = command.arg("mcp_tools").args(arguments.iter().cloned());
        let result = self.run_tool_process(&command, log, cancellation).await;
        // Killing the Docker client does not stop its container. Always remove
        // the named container on the same daemon, including after cancellation.
        let mut removal = CommandSpec::new("docker").args(["rm", "--force", &name]);
        if !command.inherits_environment() {
            removal = removal.clear_environment();
        }
        if let Some(directory) = command.working_directory() {
            removal = removal.cwd(directory);
        }
        for (key, value) in command.environment() {
            removal = removal.env(key, value);
        }
        let cleanup = self
            .runner
            .run_async(&removal)
            .await
            .map_err(AppFailure::from);
        finish_with_cleanup(result.err(), cleanup)
    }

    async fn run_tool_process(
        &self,
        command: &CommandSpec,
        log: Option<&Path>,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> AppResult<()> {
        match log {
            Some(path) => {
                self.runner
                    .run_async_cancellable_to_log(command, cancellation, path)
                    .await
            }
            None => {
                self.runner
                    .run_async_cancellable(command, cancellation)
                    .await
            }
        }
        .map_err(AppFailure::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::process::CapturedOutput;
    use std::cell::RefCell;
    use std::pin::Pin;

    struct FailingRunner {
        commands: RefCell<Vec<CommandSpec>>,
        fail_at: &'static str,
        interrupt: bool,
        cancellation: tokio::sync::watch::Sender<bool>,
    }

    impl ProcessRunner for FailingRunner {
        fn run(&self, spec: &CommandSpec) -> Result<(), InfrastructureError> {
            self.commands.borrow_mut().push(spec.clone());
            Ok(())
        }
        fn run_async<'a>(
            &'a self,
            spec: &'a CommandSpec,
        ) -> Pin<Box<dyn Future<Output = Result<(), InfrastructureError>> + 'a>> {
            Box::pin(async move {
                self.run(spec)?;
                if spec.arguments().contains(&OsString::from(self.fail_at)) {
                    if self.interrupt {
                        self.cancellation.send_replace(true);
                        std::future::pending::<()>().await;
                    }
                    #[cfg(unix)]
                    let status = <std::process::ExitStatus as std::os::unix::process::ExitStatusExt>::from_raw(1 << 8);
                    #[cfg(windows)]
                    let status = <std::process::ExitStatus as std::os::windows::process::ExitStatusExt>::from_raw(1);
                    return Err(InfrastructureError::child_exit("docker".into(), status));
                }
                Ok(())
            })
        }
        fn capture_stdout(&self, _: &CommandSpec) -> Result<Vec<u8>, InfrastructureError> {
            Ok(b"1000".to_vec())
        }
        fn capture_output(&self, _: &CommandSpec) -> Result<CapturedOutput, InfrastructureError> {
            Ok(CapturedOutput::new(Vec::new(), Vec::new()))
        }
        fn run_to_log(&self, spec: &CommandSpec, _: &Path) -> Result<(), InfrastructureError> {
            self.run(spec)
        }
    }

    #[tokio::test]
    async fn failure_and_cancellation_remove_the_container_after_reaping_the_docker_client() {
        for interrupt in [false, true] {
            let directory = tempfile::tempdir().expect("workspace");
            let config =
                crate::runtime::tests::app_config(directory.path(), "http://127.0.0.1:8080", &[]);
            let (sender, receiver) = tokio::sync::watch::channel(false);
            let runner = FailingRunner {
                commands: RefCell::default(),
                fail_at: "run",
                interrupt,
                cancellation: sender,
            };
            let runtime = RuntimeContext::new(config, runner);
            let result = runtime
                .run_tool(
                    CommandSpec::new("docker")
                        .arg("compose")
                        .clear_environment()
                        .cwd(directory.path())
                        .env("DOCKER_HOST", "tcp://test-daemon:2376")
                        .env("DOCKER_TLS_VERIFY", "1")
                        .env("DOCKER_CERT_PATH", "relative/certs"),
                    &["inspect".into()],
                    None,
                    None,
                    receiver,
                )
                .await;
            assert!(result.is_err());
            let commands = runtime.runner.commands.borrow();
            let run = &commands[1];
            let name = run
                .arguments()
                .windows(2)
                .find(|args| args[0] == "--name")
                .expect("container name")[1]
                .clone();
            assert_eq!(commands[2].program(), "docker");
            assert_eq!(
                commands[2].arguments(),
                [OsString::from("rm"), "--force".into(), name]
            );
            assert_eq!(commands.len(), 3);
            assert_eq!(commands[2].environment(), run.environment());
            assert_eq!(commands[2].working_directory(), run.working_directory());
            assert!(!commands[2].inherits_environment());
        }
    }

    #[tokio::test]
    async fn container_removal_failure_is_operational_and_runner_exit_is_preserved() {
        for fail_at in ["run", "rm"] {
            let directory = tempfile::tempdir().expect("workspace");
            let config =
                crate::runtime::tests::app_config(directory.path(), "http://127.0.0.1:8080", &[]);
            let (sender, receiver) = tokio::sync::watch::channel(false);
            let runtime = RuntimeContext::new(
                config,
                FailingRunner {
                    commands: RefCell::default(),
                    fail_at,
                    interrupt: false,
                    cancellation: sender,
                },
            );
            let failure = runtime
                .run_tool(
                    CommandSpec::new("docker").arg("compose"),
                    &["server".into()],
                    None,
                    None,
                    receiver,
                )
                .await
                .expect_err("configured Docker command fails");
            if fail_at == "rm" {
                assert!(matches!(failure, AppFailure::Native(_)), "{failure}");
                assert!(failure.to_string().contains("cleanup failed"));
            } else {
                assert!(matches!(
                    failure,
                    AppFailure::Infrastructure(InfrastructureError::ChildExit { .. })
                ));
            }
            assert_eq!(runtime.runner.commands.borrow().len(), 3);
        }
    }
}
