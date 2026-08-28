use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use cf_integration::platform::PlatformError;
#[cfg(unix)]
use cf_integration::platform::process::LoggingProcessRunner;
use cf_integration::platform::process::{
    CapturedOutput, CommandSpec, ProcessRunner, SystemProcessRunner,
};
#[cfg(unix)]
use tempfile::TempDir;

#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn assert_runner_interface(_runner: &dyn ProcessRunner) {}

struct FakeProcessRunner;

impl ProcessRunner for FakeProcessRunner {
    fn run(&self, _spec: &CommandSpec) -> Result<(), PlatformError> {
        Ok(())
    }

    fn capture_stdout(&self, _spec: &CommandSpec) -> Result<Vec<u8>, PlatformError> {
        Ok(b"synthetic stdout".to_vec())
    }

    fn capture_output(&self, _spec: &CommandSpec) -> Result<CapturedOutput, PlatformError> {
        Ok(CapturedOutput::new(
            b"synthetic stdout".to_vec(),
            b"synthetic stderr".to_vec(),
        ))
    }

    fn run_to_log(&self, _spec: &CommandSpec, _log_path: &Path) -> Result<(), PlatformError> {
        Ok(())
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn logging_runner_hides_ordinary_output_in_an_aggregate_log() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let script = executable_script(
        &directory,
        "aggregate-log.sh",
        "printf 'ordinary stdout\\n'; printf 'ordinary stderr\\n' >&2",
    );
    let log_path = directory.path().join("setup.log");
    let system = SystemProcessRunner;
    let runner = LoggingProcessRunner::new(&system, &log_path);

    runner
        .run_async(&CommandSpec::new(script))
        .await
        .expect("logged child should succeed");

    let log = fs::read(&log_path).expect("aggregate log should be readable");
    assert!(
        log.windows(b"ordinary stdout\n".len())
            .any(|part| part == b"ordinary stdout\n")
    );
    assert!(
        log.windows(b"ordinary stderr\n".len())
            .any(|part| part == b"ordinary stderr\n")
    );
}

#[cfg(unix)]
fn executable_script(directory: &TempDir, name: &str, body: &str) -> PathBuf {
    let path = directory.path().join(name);
    fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n"))
        .expect("temporary script should be written");
    let mut permissions = fs::metadata(&path)
        .expect("temporary script metadata should be readable")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&path, permissions).expect("temporary script should be executable");
    path
}

#[test]
fn command_spec_builder_exposes_program_arguments_cwd_and_sorted_environment() {
    let cwd = PathBuf::from("working-directory");
    let spec = CommandSpec::new(OsString::from("program"))
        .arg(OsString::from("first"))
        .args([OsString::from("second"), OsString::from("third")])
        .cwd(cwd.clone())
        .env(OsString::from("Z_KEY"), OsString::from("last"))
        .env(OsString::from("A_KEY"), OsString::from("first"));

    assert_eq!(spec.program(), OsStr::new("program"));
    assert_eq!(
        spec.arguments(),
        [
            OsString::from("first"),
            OsString::from("second"),
            OsString::from("third")
        ]
    );
    assert_eq!(spec.working_directory(), Some(cwd.as_path()));
    assert_eq!(
        spec.environment().keys().collect::<Vec<_>>(),
        [OsStr::new("A_KEY"), OsStr::new("Z_KEY")]
    );
    assert_eq!(
        spec.environment().get(OsStr::new("A_KEY")),
        Some(&OsString::from("first"))
    );
    assert!(spec.inherits_environment());
}

#[test]
fn command_spec_can_request_an_isolated_child_environment() {
    let spec = CommandSpec::new("program")
        .clear_environment()
        .env("PATH", "/safe/bin");

    assert!(!spec.inherits_environment());
    assert_eq!(
        spec.environment().get(OsStr::new("PATH")),
        Some(&OsString::from("/safe/bin"))
    );
}

#[test]
fn command_spec_preserves_empty_values_and_redacts_environment_debug_output() {
    const SECRET: &str = "process-secret-77c2bc";
    let spec = CommandSpec::new(OsString::new())
        .arg(OsString::new())
        .env(OsString::from("EMPTY"), OsString::new())
        .env(OsString::from("SECRET"), OsString::from(SECRET));

    assert_eq!(spec.program(), OsStr::new(""));
    assert_eq!(spec.arguments(), [OsString::new()]);
    assert_eq!(
        spec.environment().get(OsStr::new("EMPTY")),
        Some(&OsString::new())
    );
    let debug = format!("{spec:?}");
    assert!(debug.contains("SECRET"));
    assert!(!debug.contains(SECRET));
}

#[test]
fn command_spec_debug_omits_argument_values_and_reports_only_the_count() {
    const ARGUMENT_SECRET: &str = "argument-secret-f5c8c7";
    let spec = CommandSpec::new("program")
        .arg("ordinary-argument")
        .arg(ARGUMENT_SECRET);

    let debug = format!("{spec:?}");

    assert!(debug.contains("arg_count: 2"), "{debug}");
    assert!(!debug.contains("ordinary-argument"), "{debug}");
    assert!(!debug.contains(ARGUMENT_SECRET), "{debug}");
}

#[test]
fn external_fake_runner_can_construct_and_return_captured_output() {
    let runner: &dyn ProcessRunner = &FakeProcessRunner;

    let output = runner
        .capture_output(&CommandSpec::new("synthetic-program"))
        .expect("fake capture should succeed");

    assert_eq!(output.stdout(), b"synthetic stdout");
    assert_eq!(output.stderr(), b"synthetic stderr");
    assert_eq!(
        output.into_parts(),
        (b"synthetic stdout".to_vec(), b"synthetic stderr".to_vec())
    );
}

#[cfg(unix)]
#[test]
fn command_spec_preserves_non_utf8_arguments_and_environment() {
    let argument = OsString::from_vec(vec![b'a', 0xff, b'b']);
    let value = OsString::from_vec(vec![b'v', 0xfe]);
    let spec = CommandSpec::new("program")
        .arg(argument.clone())
        .env("RAW_VALUE", value.clone());

    assert_eq!(spec.arguments(), [argument]);
    assert_eq!(
        spec.environment().get(OsStr::new("RAW_VALUE")),
        Some(&value)
    );
}

#[cfg(unix)]
#[test]
fn runner_propagates_cwd_and_environment_overrides() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let script = executable_script(
        &directory,
        "cwd-env.sh",
        "printf '%s\\n%s\\n' \"$PWD\" \"$PROCESS_TEST_VALUE\"",
    );
    let spec = CommandSpec::new(script)
        .cwd(directory.path())
        .env("PROCESS_TEST_VALUE", "from-command-spec");

    let stdout = SystemProcessRunner
        .capture_stdout(&spec)
        .expect("script should run successfully");

    let canonical_directory = fs::canonicalize(directory.path())
        .expect("temporary directory should have a canonical path");
    let expected = format!("{}\nfrom-command-spec\n", canonical_directory.display());
    assert_eq!(stdout, expected.as_bytes());
}

#[cfg(unix)]
#[test]
fn runner_clears_parent_environment_when_requested() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let script = executable_script(
        &directory,
        "isolated-env.sh",
        "if env | grep '^CARGO_MANIFEST_DIR=' >/dev/null; then exit 41; fi\nprintf '%s\\n' \"$PROCESS_ALLOWED_VALUE\"",
    );
    let result = SystemProcessRunner.capture_stdout(
        &CommandSpec::new(script)
            .clear_environment()
            .env("PATH", "/usr/bin:/bin")
            .env("PROCESS_ALLOWED_VALUE", "allowed"),
    );

    assert_eq!(
        result.expect("isolated child should not receive the parent secret"),
        b"allowed\n"
    );
}

#[cfg(unix)]
#[test]
fn inherited_mode_returns_success() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let script = executable_script(&directory, "success.sh", ":");
    let runner = SystemProcessRunner;
    assert_runner_interface(&runner);

    runner
        .run(&CommandSpec::new(script))
        .expect("successful inherited process should return success");
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn async_runner_keeps_a_single_thread_executor_responsive() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let script = executable_script(&directory, "slow-success.sh", "sleep 0.2");
    let executor_progressed = Arc::new(AtomicBool::new(false));
    let progress_flag = Arc::clone(&executor_progressed);
    let heartbeat = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(25)).await;
        progress_flag.store(true, Ordering::SeqCst);
    });

    SystemProcessRunner
        .run_async(&CommandSpec::new(script))
        .await
        .expect("asynchronous child should succeed");

    assert!(
        executor_progressed.load(Ordering::SeqCst),
        "waiting for a child must not starve loopback proxy tasks"
    );
    heartbeat.await.expect("heartbeat task should join");
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn cancellable_async_runner_kills_and_reaps_active_child() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let pid_path = directory.path().join("child.pid");
    let script = executable_script(
        &directory,
        "long-lived.sh",
        "printf '%s' \"$$\" > \"$PROCESS_PID_FILE\"\nexec sleep 60",
    );
    let spec = CommandSpec::new(script).env("PROCESS_PID_FILE", pid_path.as_os_str());
    let (cancellation_sender, cancellation_receiver) = tokio::sync::watch::channel(false);
    let cancellation_pid_path = pid_path.clone();
    let cancel = tokio::spawn(async move {
        for _ in 0..200 {
            if cancellation_pid_path.is_file() {
                cancellation_sender.send_replace(true);
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("child did not publish its PID before cancellation deadline");
    });

    let error = tokio::time::timeout(
        Duration::from_secs(5),
        SystemProcessRunner.run_async_cancellable(&spec, cancellation_receiver),
    )
    .await
    .expect("cancellable child should return promptly")
    .expect_err("cancellation should be reported");
    cancel.await.expect("cancellation task should join");

    assert!(error.to_string().contains("cancelled and reaped"));
    let pid = fs::read_to_string(&pid_path)
        .expect("child PID should be recorded")
        .parse::<u32>()
        .expect("child PID should be numeric");
    let still_running = std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("kill probe should execute")
        .success();
    assert!(!still_running, "cancelled child {pid} must be gone");
}

#[cfg(unix)]
#[test]
fn capture_stdout_returns_exact_bytes_while_stderr_is_inherited() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let script = executable_script(
        &directory,
        "stdout.sh",
        "printf 'out\\000bytes'; printf 'inherited stderr\\n' >&2",
    );

    let stdout = SystemProcessRunner
        .capture_stdout(&CommandSpec::new(script))
        .expect("stdout should be captured");

    assert_eq!(stdout, b"out\0bytes");
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn cancellable_log_runner_records_both_streams_and_replaces_stale_output() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let script = executable_script(
        &directory,
        "logged-async.sh",
        "printf 'stdout-line\\n'; printf 'stderr-line\\n' >&2",
    );
    let log_path = directory.path().join("process.log");
    fs::write(&log_path, b"stale output\n").expect("stale log should be written");
    let (_cancellation_sender, cancellation_receiver) = tokio::sync::watch::channel(false);

    SystemProcessRunner
        .run_async_cancellable_to_log(&CommandSpec::new(script), cancellation_receiver, &log_path)
        .await
        .expect("logged child should succeed");

    assert_eq!(
        fs::read(&log_path).expect("process log should be readable"),
        b"stdout-line\nstderr-line\n"
    );
}

#[cfg(unix)]
#[test]
fn capture_output_returns_exact_stdout_and_stderr_bytes() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let script = executable_script(
        &directory,
        "output.sh",
        "printf 'stdout\\n'; printf 'stderr\\000bytes' >&2",
    );

    let output = SystemProcessRunner
        .capture_output(&CommandSpec::new("/bin/sh").arg(script))
        .expect("both streams should be captured");

    assert_eq!(output.stdout(), b"stdout\n");
    assert_eq!(output.stderr(), b"stderr\0bytes");
}

#[cfg(unix)]
#[test]
fn log_mode_appends_both_streams_to_one_file() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let script = executable_script(
        &directory,
        "logged.sh",
        "printf 'stdout-line\\n'; printf 'stderr-line\\n' >&2",
    );
    let log_path = directory.path().join("process.log");
    fs::write(&log_path, b"existing-line\n").expect("initial log should be written");

    SystemProcessRunner
        .run_to_log(&CommandSpec::new(script), &log_path)
        .expect("logged process should run successfully");

    let log = fs::read(&log_path).expect("process log should be readable");
    assert!(log.starts_with(b"existing-line\n"));
    assert!(
        log.windows(b"stdout-line\n".len())
            .any(|part| part == b"stdout-line\n")
    );
    assert!(
        log.windows(b"stderr-line\n".len())
            .any(|part| part == b"stderr-line\n")
    );
}

#[test]
fn missing_program_has_safe_context_and_native_exit_code() {
    const SECRET: &str = "must-not-appear-924bce";
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let missing = directory.path().join("missing-program");
    let spec = CommandSpec::new(&missing)
        .cwd(directory.path())
        .env("SECRET", SECRET);

    let failure = SystemProcessRunner
        .run(&spec)
        .expect_err("missing program should fail to spawn");

    assert!(matches!(failure, PlatformError::Native(_)));
    assert_eq!(failure.exit_code(), 1);
    let message = failure.to_string();
    assert!(message.contains("spawn"), "{message}");
    assert!(
        message.contains(missing.to_string_lossy().as_ref()),
        "{message}"
    );
    assert!(
        message.contains(directory.path().to_string_lossy().as_ref()),
        "{message}"
    );
    assert!(!message.contains(SECRET), "{message}");
}

#[test]
fn missing_program_without_configured_cwd_reports_inherited_cwd() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let missing = directory.path().join("missing-program");

    let failure = SystemProcessRunner
        .run(&CommandSpec::new(missing))
        .expect_err("missing program should fail to spawn");

    let message = failure.to_string();
    assert!(message.contains("inherited cwd"), "{message}");
    assert!(!message.contains("cwd None"), "{message}");
}

#[cfg(unix)]
#[test]
fn child_exit_code_is_preserved_without_process_output() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let script = executable_script(&directory, "exit-seven.sh", "exit 7");

    let failure = SystemProcessRunner
        .run(&CommandSpec::new(script))
        .expect_err("exit seven should be represented as a child failure");

    assert!(matches!(failure, PlatformError::ChildExit { .. }));
    assert_eq!(failure.exit_code(), 7);
}

#[cfg(unix)]
#[test]
fn signaled_child_maps_to_shell_exit_code() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let script = executable_script(&directory, "sigterm.sh", "kill -TERM $$");

    let failure = SystemProcessRunner
        .run(&CommandSpec::new(script))
        .expect_err("SIGTERM should be represented as a child failure");

    assert!(matches!(failure, PlatformError::ChildExit { .. }));
    assert_eq!(failure.exit_code(), 143);
}

#[cfg(windows)]
#[test]
fn windows_inherited_mode_preserves_success_and_nonzero_exit_codes() {
    assert_runner_interface(&SystemProcessRunner);
    SystemProcessRunner
        .run(&CommandSpec::new("cmd.exe").args(["/D", "/S", "/C", "exit /b 0"]))
        .expect("zero exit should succeed");

    let failure = SystemProcessRunner
        .run(&CommandSpec::new("cmd.exe").args(["/D", "/S", "/C", "exit /b 7"]))
        .expect_err("nonzero exit should be represented as a child failure");

    assert!(matches!(failure, PlatformError::ChildExit { .. }));
    assert_eq!(failure.exit_code(), 7);
}

#[cfg(windows)]
#[test]
fn windows_runner_propagates_cwd_and_environment_overrides() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let spec = CommandSpec::new("cmd.exe")
        .args(["/D", "/S", "/C", "echo %CD%& echo %PROCESS_TEST_VALUE%"])
        .cwd(directory.path())
        .env("PROCESS_TEST_VALUE", "from-command-spec");

    let stdout = SystemProcessRunner
        .capture_stdout(&spec)
        .expect("cmd.exe should expose cwd and environment");
    let stdout = String::from_utf8(stdout).expect("cmd.exe output should be UTF-8");
    let mut lines = stdout.lines();

    assert_eq!(
        lines
            .next()
            .expect("cwd line should be present")
            .to_ascii_lowercase(),
        directory.path().display().to_string().to_ascii_lowercase()
    );
    assert_eq!(lines.next().map(str::trim), Some("from-command-spec"));
}

#[cfg(windows)]
#[test]
fn windows_capture_output_returns_both_streams() {
    let output = SystemProcessRunner
        .capture_output(&CommandSpec::new("cmd.exe").args([
            "/D",
            "/S",
            "/C",
            "echo stdout-line& echo stderr-line 1>&2",
        ]))
        .expect("cmd.exe streams should be captured");

    assert!(
        output
            .stdout()
            .windows(b"stdout-line".len())
            .any(|part| part == b"stdout-line")
    );
    assert!(
        output
            .stderr()
            .windows(b"stderr-line".len())
            .any(|part| part == b"stderr-line")
    );
}

#[cfg(windows)]
#[test]
fn windows_log_mode_appends_both_streams() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let log_path = directory.path().join("process.log");
    fs::write(&log_path, b"existing-line\r\n").expect("initial log should be written");

    SystemProcessRunner
        .run_to_log(
            &CommandSpec::new("cmd.exe").args([
                "/D",
                "/S",
                "/C",
                "echo stdout-line& echo stderr-line 1>&2",
            ]),
            &log_path,
        )
        .expect("cmd.exe output should be appended");

    let log = fs::read(&log_path).expect("process log should be readable");
    assert!(log.starts_with(b"existing-line\r\n"));
    assert!(
        log.windows(b"stdout-line".len())
            .any(|part| part == b"stdout-line")
    );
    assert!(
        log.windows(b"stderr-line".len())
            .any(|part| part == b"stderr-line")
    );
}
