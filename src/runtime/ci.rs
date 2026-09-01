//! Repository CI orchestration kept out of GitHub Actions shell blocks.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{AppFailure, AppResult, RuntimeContext};
use crate::app::CiAction;
use crate::infrastructure::process::{CommandSpec, ProcessRunner};

const ARTIFACT_WAIT_ATTEMPTS: usize = 60;
const ARTIFACT_WAIT_INTERVAL: Duration = Duration::from_secs(10);
const CRATES_IO_USER_AGENT: &str = "contextforge-dev-tools-release";

#[derive(Debug, Deserialize)]
struct ArtifactList {
    artifacts: Vec<WorkflowArtifact>,
}

#[derive(Debug, Deserialize)]
struct WorkflowArtifact {
    expired: bool,
    workflow_run: Option<WorkflowRun>,
}

#[derive(Debug, Deserialize)]
struct WorkflowRun {
    id: u64,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    id: u64,
    tag_name: String,
    draft: bool,
}

#[derive(Debug, Deserialize)]
struct GitReference {
    #[serde(rename = "ref")]
    name: String,
}

#[derive(Debug, Deserialize)]
struct ReleasePlzRelease {
    package_name: String,
    #[serde(default)]
    tag: String,
}

impl<R: ProcessRunner> RuntimeContext<R> {
    pub(super) async fn execute_ci(&self, action: CiAction) -> AppResult<()> {
        match action {
            action @ CiAction::PrepareImage { .. } => self.prepare_ci_image(&action).await,
            CiAction::PrepareRelease => self.prepare_release_state(),
            CiAction::SelectRelease => self.select_release_tag(),
        }
    }

    async fn prepare_ci_image(&self, action: &CiAction) -> AppResult<()> {
        let CiAction::PrepareImage {
            artifact: artifact_prefix,
            binary,
            image,
            repository,
            revision,
            dockerfile,
            target,
            download_dir,
        } = action
        else {
            return Err(AppFailure::from(anyhow!(
                "internal CI action was not an image preparation request"
            )));
        };
        let revision = match revision {
            Some(revision) => revision.clone(),
            None => self.capture_text(
                &CommandSpec::new("git")
                    .args(["rev-parse", "HEAD"])
                    .cwd(self.config.root()),
            )?,
        };
        validate_revision(&revision)?;
        let artifact_name = format!("{artifact_prefix}-{revision}");
        let run_id = self.wait_for_artifact(repository, &artifact_name).await?;

        if download_dir.exists() {
            return Err(AppFailure::from(anyhow!(
                "CI artifact directory {} already exists; remove it before retrying",
                download_dir.display()
            )));
        }
        self.runner
            .run_async(
                &CommandSpec::new("gh")
                    .args([
                        OsString::from("run"),
                        OsString::from("download"),
                        OsString::from(run_id.to_string()),
                        OsString::from("--repo"),
                        OsString::from(repository),
                        OsString::from("--name"),
                        OsString::from(&artifact_name),
                        OsString::from("--dir"),
                        download_dir.as_os_str().to_owned(),
                    ])
                    .cwd(self.config.root()),
            )
            .await?;

        let downloaded_binary = download_dir.join(binary);
        let metadata = fs::metadata(&downloaded_binary).with_context(|| {
            format!(
                "artifact {artifact_name} did not contain {}",
                binary.display()
            )
        })?;
        if !metadata.is_file() {
            return Err(AppFailure::from(anyhow!(
                "artifact binary {} is not a regular file",
                downloaded_binary.display()
            )));
        }
        make_executable(&downloaded_binary, metadata.permissions())?;

        let mut prebuilt_context = OsString::from("prebuilt=");
        prebuilt_context.push(download_dir);
        self.runner
            .run_async(
                &CommandSpec::new("docker")
                    .args([
                        OsString::from("buildx"),
                        OsString::from("build"),
                        OsString::from("--load"),
                        OsString::from("--target"),
                        OsString::from(target),
                        OsString::from("--build-context"),
                        prebuilt_context,
                        OsString::from("--tag"),
                        OsString::from(image),
                        OsString::from("--file"),
                        dockerfile.as_os_str().to_owned(),
                        OsString::from("."),
                    ])
                    .cwd(self.config.root()),
            )
            .await?;
        Ok(())
    }

    async fn wait_for_artifact(&self, repository: &str, artifact: &str) -> AppResult<u64> {
        let endpoint = format!("repos/{repository}/actions/artifacts");
        for attempt in 1..=ARTIFACT_WAIT_ATTEMPTS {
            let response: ArtifactList = self.capture_ci_json(
                &CommandSpec::new("gh").args([
                    "api",
                    "--method",
                    "GET",
                    &endpoint,
                    "-f",
                    &format!("name={artifact}"),
                ]),
                "GitHub Actions artifact response",
            )?;
            if let Some(run_id) = artifact_run_id(&response) {
                return Ok(run_id);
            }
            eprintln!("Waiting for CI artifact {artifact} ({attempt}/{ARTIFACT_WAIT_ATTEMPTS})");
            tokio::time::sleep(ARTIFACT_WAIT_INTERVAL).await;
        }
        Err(AppFailure::from(anyhow!(
            "CI artifact {artifact} was not produced within {} seconds",
            ARTIFACT_WAIT_INTERVAL.as_secs() * ARTIFACT_WAIT_ATTEMPTS as u64
        )))
    }

    fn prepare_release_state(&self) -> AppResult<()> {
        let repository = self.required_ci_environment("GITHUB_REPOSITORY")?;
        let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
        if self.crate_is_published(env!("CARGO_PKG_VERSION"))? {
            return self.write_github_output("tag", &tag);
        }

        let pages: Vec<Vec<GitHubRelease>> = self.capture_ci_json(
            &CommandSpec::new("gh").args([
                "api",
                "--paginate",
                "--slurp",
                &format!("repos/{repository}/releases"),
            ]),
            "GitHub release list",
        )?;
        let matching = pages
            .into_iter()
            .flatten()
            .filter(|release| release.tag_name == tag)
            .collect::<Vec<_>>();
        if matching.iter().any(|release| !release.draft) {
            return Err(AppFailure::from(anyhow!(
                "refusing to replace published GitHub release {tag}"
            )));
        }
        for release in matching {
            self.runner.run(&CommandSpec::new("gh").args([
                "api",
                "--method",
                "DELETE",
                &format!("repos/{repository}/releases/{}", release.id),
            ]))?;
        }

        let references: Vec<GitReference> = self.capture_ci_json(
            &CommandSpec::new("gh").args([
                "api",
                &format!("repos/{repository}/git/matching-refs/tags/{tag}"),
            ]),
            "GitHub tag references",
        )?;
        if references
            .iter()
            .any(|reference| reference.name == format!("refs/tags/{tag}"))
        {
            self.runner.run(&CommandSpec::new("gh").args([
                "api",
                "--method",
                "DELETE",
                &format!("repos/{repository}/git/refs/tags/{tag}"),
            ]))?;
        }

        let local_tag = self.capture_text(
            &CommandSpec::new("git")
                .args(["tag", "--list", &tag])
                .cwd(self.config.root()),
        )?;
        if local_tag.lines().any(|candidate| candidate == tag) {
            self.runner.run(
                &CommandSpec::new("git")
                    .args(["tag", "--delete", &tag])
                    .cwd(self.config.root()),
            )?;
        }
        self.write_github_output("tag", &tag)
    }

    fn select_release_tag(&self) -> AppResult<()> {
        let releases = self
            .environment_text("RELEASES")
            .filter(|releases| !releases.is_empty())
            .unwrap_or("[]");
        let releases: Vec<ReleasePlzRelease> = serde_json::from_str(releases)
            .context("failed to parse release-plz output")
            .map_err(AppFailure::from)?;
        if let Some(release) = releases
            .into_iter()
            .find(|release| release.package_name == "cf-integration" && !release.tag.is_empty())
        {
            return self.write_github_output("tag", &release.tag);
        }

        let candidate = self.required_ci_environment("CANDIDATE_TAG")?;
        let remote = self.capture_text(
            &CommandSpec::new("git")
                .args([
                    "ls-remote",
                    "--tags",
                    "origin",
                    &format!("refs/tags/{candidate}"),
                ])
                .cwd(self.config.root()),
        )?;
        if remote.is_empty() {
            return self.write_github_output("tag", "");
        }
        self.runner.run(
            &CommandSpec::new("git")
                .args([
                    "fetch",
                    "--force",
                    "origin",
                    &format!("refs/tags/{candidate}:refs/tags/{candidate}"),
                ])
                .cwd(self.config.root()),
        )?;
        let tagged_revision = self.capture_text(
            &CommandSpec::new("git")
                .args(["rev-list", "-n", "1", candidate])
                .cwd(self.config.root()),
        )?;
        if tagged_revision != self.required_ci_environment("GITHUB_SHA")? {
            return self.write_github_output("tag", "");
        }
        let version = candidate
            .strip_prefix('v')
            .ok_or_else(|| AppFailure::from(anyhow!("candidate release tag must start with v")))?;
        let selected = if self.crate_is_published(version)? {
            candidate
        } else {
            ""
        };
        self.write_github_output("tag", selected)
    }

    fn crate_is_published(&self, version: &str) -> AppResult<bool> {
        let status = self.capture_text(&CommandSpec::new("curl").args([
            "--silent",
            "--show-error",
            "--output",
            "/dev/null",
            "--write-out",
            "%{http_code}",
            "--user-agent",
            CRATES_IO_USER_AGENT,
            &format!("https://crates.io/api/v1/crates/cf-integration/{version}"),
        ]))?;
        match status.as_str() {
            "200" => Ok(true),
            "404" => Ok(false),
            _ => Err(AppFailure::from(anyhow!(
                "crates.io returned HTTP {status} while checking cf-integration {version}"
            ))),
        }
    }

    fn capture_ci_json<T: DeserializeOwned>(
        &self,
        command: &CommandSpec,
        description: &str,
    ) -> AppResult<T> {
        let output = self.capture_text(command)?;
        serde_json::from_str(&output)
            .with_context(|| format!("failed to parse {description}"))
            .map_err(AppFailure::from)
    }

    fn required_ci_environment(&self, key: &str) -> AppResult<&str> {
        self.environment_text(key)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| AppFailure::from(anyhow!("{key} is required for this CI operation")))
    }

    fn write_github_output(&self, key: &str, value: &str) -> AppResult<()> {
        let output = self.required_ci_environment("GITHUB_OUTPUT")?;
        write_github_output(Path::new(output), key, value).map_err(AppFailure::from)
    }
}

fn artifact_run_id(response: &ArtifactList) -> Option<u64> {
    response
        .artifacts
        .iter()
        .find(|artifact| !artifact.expired)
        .and_then(|artifact| artifact.workflow_run.as_ref())
        .map(|run| run.id)
}

fn validate_revision(revision: &str) -> AppResult<()> {
    if matches!(revision.len(), 40 | 64) && revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(AppFailure::from(anyhow!(
            "CI artifact revision must be a 40- or 64-character Git object ID"
        )))
    }
}

fn write_github_output(path: &Path, key: &str, value: &str) -> anyhow::Result<()> {
    if key.contains(['\r', '\n']) || value.contains(['\r', '\n']) {
        bail!("GitHub Actions output values cannot contain newlines");
    }
    let mut output = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open GitHub Actions output {}", path.display()))?;
    writeln!(output, "{key}={value}")
        .with_context(|| format!("failed to write GitHub Actions output {}", path.display()))
}

#[cfg(unix)]
fn make_executable(path: &Path, mut permissions: fs::Permissions) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to make {} executable", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path, _permissions: fs::Permissions) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn artifact_selection_skips_expired_entries() {
        let response = ArtifactList {
            artifacts: vec![
                WorkflowArtifact {
                    expired: true,
                    workflow_run: Some(WorkflowRun { id: 1 }),
                },
                WorkflowArtifact {
                    expired: false,
                    workflow_run: Some(WorkflowRun { id: 2 }),
                },
            ],
        };

        let run_id = artifact_run_id(&response);

        assert_eq!(run_id, Some(2));
    }

    #[test]
    fn github_output_rejects_newline_injection() {
        let directory = tempdir().expect("create temporary output directory");
        let output = directory.path().join("github-output");

        let error = write_github_output(&output, "tag", "v1.0.0\nunsafe=true")
            .expect_err("newlines must not be written to GitHub Actions outputs");

        assert_eq!(
            error.to_string(),
            "GitHub Actions output values cannot contain newlines"
        );
        assert!(!output.exists());
    }

    #[test]
    fn revision_validation_accepts_git_sha1_and_sha256_ids_only() {
        assert!(validate_revision(&"a".repeat(40)).is_ok());
        assert!(validate_revision(&"b".repeat(64)).is_ok());
        assert!(validate_revision("main").is_err());
        assert!(validate_revision(&"z".repeat(40)).is_err());
    }
}
