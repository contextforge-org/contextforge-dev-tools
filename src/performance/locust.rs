//! Locust settings precedence and Docker Compose invocation.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::infrastructure::StackMode;
use crate::infrastructure::compose::ComposeProject;
use crate::infrastructure::config::AppConfig;
use crate::infrastructure::process::CommandSpec;

use crate::performance::LoadSettings;

const LOCUST_ADAPTER_NAME: &str = "locustfile_mcp.py";
const LOCUST_ADAPTER_CONTAINER_PATH: &str = "/mnt/locust-cf/locustfile_mcp.py";
const REQUEST_TIMEOUT_DEFAULT_SECONDS: &str = "60";
const REQUEST_TIMEOUT_ENV: &str = "LOCUST_REQUEST_TIMEOUT_SECONDS";
const REQUEST_TIMEOUT_ERROR: &str =
    "LOCUST_REQUEST_TIMEOUT_SECONDS must be a finite number greater than zero";
/// Prepared Docker Compose Locust invocation and its host report directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocustCommand {
    command: CommandSpec,
    report_dir: PathBuf,
}

impl LocustCommand {
    /// Builds a Locust invocation with an explicit MCP protocol version.
    ///
    /// # Errors
    ///
    /// Rejects an empty token or protocol version, a missing dataplane server
    /// ID, an invalid timeout, or an inaccessible report directory.
    pub(crate) fn new_with_protocol_version(
        config: &AppConfig,
        mode: StackMode,
        settings: &LoadSettings,
        bearer_token: &str,
        server_id: Option<&str>,
        protocol_version: &str,
    ) -> Result<Self> {
        if bearer_token.trim().is_empty() {
            bail!("Locust bearer token must not be empty");
        }
        if protocol_version.trim().is_empty() {
            bail!("Locust MCP protocol version must not be empty");
        }
        if mode == StackMode::Dataplane && server_id.is_none_or(|value| value.trim().is_empty()) {
            bail!("dataplane Locust server ID must not be empty");
        }
        let request_timeout = request_timeout_seconds(config)?;

        let mode_name = match mode {
            StackMode::Controlplane => "controlplane",
            StackMode::Dataplane => "dataplane",
        };
        let report_dir = config
            .integration_dir()
            .join("reports")
            .join("load")
            .join(mode_name)
            .join("locust");
        fs::create_dir_all(&report_dir).with_context(|| {
            format!("failed to create Locust report directory {:?}", report_dir)
        })?;

        let volume = volume_argument(&report_dir);
        let project = match mode {
            StackMode::Dataplane => ComposeProject::dataplane(
                config.asset_root(),
                config.controlplane_dir(),
                config.integration_project().value.clone(),
                !config.dataplane_ref().value.is_empty(),
            ),
            StackMode::Controlplane => ComposeProject::controlplane(
                config.asset_root(),
                config.controlplane_dir(),
                config.controlplane_project().value.clone(),
                controlplane_sso_enabled(config),
            ),
        };

        let command = match mode {
            StackMode::Dataplane => project.command([
                OsString::from("--profile"),
                OsString::from("testing"),
                OsString::from("run"),
                OsString::from("--rm"),
                OsString::from("--no-deps"),
                OsString::from("--volume"),
                volume,
                OsString::from("locust"),
            ]),
            StackMode::Controlplane => {
                let adapter_volume = adapter_volume_argument(config.asset_root());
                let arguments = vec![
                    OsString::from("run"),
                    OsString::from("--rm"),
                    OsString::from("--no-deps"),
                    OsString::from("--volume"),
                    volume,
                    OsString::from("--volume"),
                    adapter_volume,
                    OsString::from("-e"),
                    OsString::from("MCPGATEWAY_BEARER_TOKEN"),
                    OsString::from("-e"),
                    OsString::from("MCP_STACK_MODE"),
                    OsString::from("-e"),
                    OsString::from(REQUEST_TIMEOUT_ENV),
                    OsString::from("-e"),
                    OsString::from("MCP_PROTOCOL_VERSION"),
                    OsString::from("-e"),
                    OsString::from("MCP_TOOL_NAMES"),
                    OsString::from("--entrypoint"),
                    OsString::from("locust"),
                    OsString::from("locust"),
                    OsString::from("-f"),
                    OsString::from(LOCUST_ADAPTER_CONTAINER_PATH),
                    OsString::from("--host=http://nginx:80"),
                    OsString::from(format!("--users={}", settings.users().get())),
                    OsString::from(format!("--spawn-rate={}", settings.spawn_rate())),
                    OsString::from(format!("--run-time={}", settings.run_time())),
                    OsString::from("--headless"),
                    OsString::from("--html=/mnt/reports/locust_report.html"),
                    OsString::from("--csv=/mnt/reports/locust"),
                    OsString::from("--only-summary"),
                ];
                project.command(arguments)
            }
        };

        let mut command = command
            .env("CF_INTEGRATION_ROOT", config.asset_root().as_os_str())
            .env("CF_INTEGRATION_DIR", config.integration_dir().as_os_str())
            .env("MCP_STACK_MODE", mode_name)
            .env("MCPGATEWAY_BEARER_TOKEN", bearer_token)
            .env("LOCUST_LOCUSTFILE", LOCUST_ADAPTER_NAME)
            .env("LOCUST_MODE", "headless")
            .env("LOCUST_USERS", settings.users().get().to_string())
            .env("LOCUST_SPAWN_RATE", settings.spawn_rate().to_string())
            .env("LOCUST_RUN_TIME", settings.run_time())
            .env(REQUEST_TIMEOUT_ENV, request_timeout.to_string())
            .env("MCP_PROTOCOL_VERSION", protocol_version);
        match mode {
            StackMode::Dataplane => {
                command = command.env("MCP_SERVER_ID", server_id.unwrap_or_default());
            }
            StackMode::Controlplane => {
                let tool_names = configured_text(config, "MCP_TOOL_NAMES").unwrap_or("");
                command = command.env("MCP_TOOL_NAMES", tool_names);
            }
        }

        Ok(Self {
            command,
            report_dir,
        })
    }

    /// Returns the child-process specification.
    pub(crate) fn command(&self) -> &CommandSpec {
        &self.command
    }

    /// Returns the host directory receiving HTML and CSV reports.
    #[must_use]
    pub(crate) fn report_dir(&self) -> &Path {
        &self.report_dir
    }
}

/// Inspects every regular Locust artifact and removes all files containing the bearer token.
///
/// # Errors
///
/// Returns an error after attempting all removals when an artifact cannot be inspected,
/// a tainted artifact cannot be removed, or at least one credential-bearing artifact was
/// removed.
pub(crate) fn audit_reports(report_dir: &Path, bearer_token: &str) -> Result<()> {
    let mut files = Vec::new();
    let mut first_inspection_error = None;
    collect_report_files(report_dir, &mut files, &mut first_inspection_error);
    let mut tainted = Vec::new();
    for path in files {
        match fs::read(&path) {
            Ok(contents) if contains_bytes(&contents, bearer_token.as_bytes()) => {
                tainted.push(path);
            }
            Ok(_) => {}
            Err(error) if first_inspection_error.is_none() => {
                first_inspection_error = Some((path, error));
            }
            Err(_) => {}
        }
    }

    let tainted_count = tainted.len();
    let mut first_cleanup_error = None;
    for path in tainted {
        if let Err(error) = fs::remove_file(&path)
            && first_cleanup_error.is_none()
        {
            first_cleanup_error = Some((path, error));
        }
    }
    if let Some((path, error)) = first_cleanup_error {
        return Err(error)
            .with_context(|| format!("failed to remove tainted Locust report {path:?}"));
    }
    if tainted_count > 0 {
        bail!(
            "removed {tainted_count} Locust report artifact(s) because they contained a bearer credential"
        );
    }
    if let Some((path, error)) = first_inspection_error {
        return Err(error).with_context(|| format!("failed to inspect Locust report {path:?}"));
    }
    Ok(())
}

fn collect_report_files(
    directory: &Path,
    files: &mut Vec<PathBuf>,
    first_error: &mut Option<(PathBuf, io::Error)>,
) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            if first_error.is_none() {
                *first_error = Some((directory.to_path_buf(), error));
            }
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                if first_error.is_none() {
                    *first_error = Some((directory.to_path_buf(), error));
                }
                continue;
            }
        };
        let path = entry.path();
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => {
                collect_report_files(&path, files, first_error);
            }
            Ok(file_type) if file_type.is_file() => files.push(path),
            Ok(_) => {}
            Err(error) if first_error.is_none() => {
                *first_error = Some((path, error));
            }
            Err(_) => {}
        }
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn configured_text<'a>(config: &'a AppConfig, key: &str) -> Option<&'a str> {
    config
        .environment()
        .get(OsStr::new(key))
        .and_then(|value| value.value.to_str())
}

fn request_timeout_seconds(config: &AppConfig) -> Result<f64> {
    let value = config
        .environment()
        .get(OsStr::new(REQUEST_TIMEOUT_ENV))
        .map_or(OsStr::new(REQUEST_TIMEOUT_DEFAULT_SECONDS), |value| {
            value.value.as_os_str()
        });
    let timeout = value
        .to_str()
        .context(REQUEST_TIMEOUT_ERROR)?
        .parse::<f64>()
        .context(REQUEST_TIMEOUT_ERROR)?;
    if !timeout.is_finite() || timeout <= 0.0 {
        bail!(REQUEST_TIMEOUT_ERROR);
    }
    Ok(timeout)
}

fn volume_argument(report_dir: &Path) -> OsString {
    let mut argument = report_dir.as_os_str().to_owned();
    argument.push(":/mnt/reports");
    argument
}

fn adapter_volume_argument(root: &Path) -> OsString {
    let mut argument = root
        .join("scripts")
        .join(LOCUST_ADAPTER_NAME)
        .into_os_string();
    argument.push(":");
    argument.push(LOCUST_ADAPTER_CONTAINER_PATH);
    argument.push(":ro");
    argument
}

fn controlplane_sso_enabled(config: &AppConfig) -> bool {
    config
        .environment()
        .get(OsStr::new("CONTROLPLANE_ENABLE_SSO"))
        .and_then(|value| value.value.to_str())
        .is_some_and(|value| matches!(value, "true" | "1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_audit_removes_every_tainted_artifact_before_failing() {
        let directory = tempfile::tempdir().expect("temporary report directory");
        let nested = directory.path().join("nested");
        fs::create_dir(&nested).expect("nested report directory should be created");
        let first = directory.path().join("locust.html");
        let second = nested.join("locust_failures.csv");
        let safe = directory.path().join("locust_stats.csv");
        let token = "credential-present-in-every-tainted-report";
        fs::write(&first, format!("html {token}")).expect("first report should be written");
        fs::write(&second, format!("csv {token}")).expect("second report should be written");
        fs::write(&safe, "safe aggregate").expect("safe report should be written");

        let error = audit_reports(directory.path(), token)
            .expect_err("credential-bearing reports must fail closed");

        assert!(error.to_string().contains("removed 2 Locust report"));
        assert!(!first.exists());
        assert!(!second.exists());
        assert!(safe.is_file());
    }
}
