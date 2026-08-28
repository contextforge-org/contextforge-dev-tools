//! Source-checkout synchronization policy.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;

use crate::infrastructure::error::InfrastructureError;
use crate::infrastructure::process::{CommandSpec, ProcessRunner};

static GENERATED_REPLACEMENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq, Eq)]
enum CheckoutKind {
    Controlplane,
    Dataplane,
}

/// One source checkout managed by the integration harness.
#[derive(Clone, PartialEq, Eq)]
pub struct CheckoutRequest {
    kind: CheckoutKind,
    directory: PathBuf,
    repository: OsString,
    reference: OsString,
}

impl CheckoutRequest {
    /// Creates a control-plane checkout request.
    pub fn controlplane(
        directory: impl Into<PathBuf>,
        repository: impl Into<OsString>,
        reference: impl Into<OsString>,
    ) -> Self {
        Self {
            kind: CheckoutKind::Controlplane,
            directory: directory.into(),
            repository: repository.into(),
            reference: reference.into(),
        }
    }

    /// Creates a dataplane checkout request.
    pub fn dataplane(
        directory: impl Into<PathBuf>,
        repository: impl Into<OsString>,
        reference: impl Into<OsString>,
    ) -> Self {
        Self {
            kind: CheckoutKind::Dataplane,
            directory: directory.into(),
            repository: repository.into(),
            reference: reference.into(),
        }
    }

    fn is_disabled(&self) -> bool {
        self.kind == CheckoutKind::Dataplane && self.reference.is_empty()
    }

    fn fetch_warning(&self) -> String {
        let repository = self.repository.to_string_lossy();
        match self.kind {
            CheckoutKind::Controlplane => {
                format!("warning: fetch from {repository} failed; using existing checkout")
            }
            CheckoutKind::Dataplane => format!(
                "warning: fetch from {repository} failed; using existing dataplane checkout"
            ),
        }
    }
}

/// Whether a requested source checkout was synchronized or intentionally skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutStatus {
    /// The checkout was cloned or refreshed and checked out.
    Updated,
    /// Dataplane source mode was disabled because its ref was empty.
    Skipped,
}

/// Deterministic Git commands for one checkout request.
pub struct CheckoutPlan {
    generated: bool,
    clone: CommandSpec,
    fetch: CommandSpec,
    plain_checkout: CommandSpec,
    remote_checkout: Option<CommandSpec>,
    generated_cleanup: Vec<CommandSpec>,
}

impl CheckoutPlan {
    /// Builds a checkout plan without accessing the filesystem or starting a process.
    #[must_use]
    pub fn new(integration_directory: &Path, request: &CheckoutRequest) -> Self {
        let generated = is_path_within(&request.directory, integration_directory);
        let clone = clone_command(request, &request.directory);
        let mut fetch_arguments = vec![
            OsString::from("fetch"),
            OsString::from("-q"),
            OsString::from("--prune"),
        ];
        if generated {
            fetch_arguments.push(OsString::from("--prune-tags"));
        }
        fetch_arguments.extend([
            OsString::from("--tags"),
            OsString::from("--force"),
            OsString::from("origin"),
        ]);
        let fetch = git_in(&request.directory).args(fetch_arguments);
        let plain_checkout = git_in(&request.directory).args([
            OsString::from("checkout"),
            OsString::from("-q"),
            request.reference.clone(),
        ]);
        let (remote_checkout, generated_cleanup) = if generated {
            let remote_target = prefixed("origin/", &request.reference);
            let cleanup = vec![
                git_in(&request.directory).args([
                    OsString::from("reset"),
                    OsString::from("--hard"),
                    OsString::from("--quiet"),
                    OsString::from("HEAD"),
                ]),
                git_in(&request.directory).args([
                    OsString::from("clean"),
                    OsString::from("-ffd"),
                    OsString::from("-q"),
                ]),
            ];
            (
                Some(git_in(&request.directory).args([
                    OsString::from("checkout"),
                    OsString::from("-q"),
                    OsString::from("-B"),
                    request.reference.clone(),
                    remote_target,
                ])),
                cleanup,
            )
        } else {
            (None, Vec::new())
        };

        Self {
            generated,
            clone,
            fetch,
            plain_checkout,
            remote_checkout,
            generated_cleanup,
        }
    }

    /// Returns whether the normalized checkout path is inside the integration directory.
    #[must_use]
    pub fn is_generated(&self) -> bool {
        self.generated
    }

    /// Selects the reset or plain checkout command from the remote probe result.
    pub fn checkout_command(&self, remote_branch_exists: bool) -> CommandSpec {
        if remote_branch_exists {
            self.remote_checkout
                .clone()
                .unwrap_or_else(|| self.plain_checkout.clone())
        } else {
            self.plain_checkout.clone()
        }
    }

    /// Returns destructive cleanup commands used only for generated checkouts.
    pub fn generated_cleanup_commands(&self) -> &[CommandSpec] {
        &self.generated_cleanup
    }
}

/// Executes source-checkout plans through an injected process runner.
pub struct CheckoutManager<'runner, Runner: ProcessRunner + ?Sized> {
    runner: &'runner Runner,
}

impl<'runner, Runner: ProcessRunner + ?Sized> CheckoutManager<'runner, Runner> {
    /// Creates a checkout manager using `runner` for every Git invocation.
    #[must_use]
    pub fn new(runner: &'runner Runner) -> Self {
        Self { runner }
    }

    /// Ensures one configured source checkout exists at the requested ref.
    ///
    /// Fetch failures append the shell-compatible diagnostic to `warnings`, then
    /// checkout continues using locally available refs. This keeps the warning
    /// available to the caller even if the subsequent checkout fails.
    ///
    /// # Errors
    ///
    /// Returns a failure when the integration directory cannot be created, a
    /// missing repository cannot be cloned, or the requested ref cannot be
    /// checked out.
    pub fn ensure(
        &self,
        integration_directory: &Path,
        request: &CheckoutRequest,
        warnings: &mut Vec<String>,
    ) -> Result<CheckoutStatus, InfrastructureError> {
        if request.is_disabled() {
            return Ok(CheckoutStatus::Skipped);
        }

        fs::create_dir_all(integration_directory).with_context(|| {
            format!("failed to create integration checkout directory {integration_directory:?}")
        })?;

        if normalized_paths_equal(&request.directory, integration_directory) {
            return Err(InfrastructureError::from(anyhow::anyhow!(
                "checkout path {:?} must be a child of integration state {:?}, not the integration root itself",
                request.directory,
                integration_directory
            )));
        }

        let plan = CheckoutPlan::new(integration_directory, request);
        if plan.is_generated() {
            validate_generated_checkout_boundary(integration_directory, &request.directory)?;
        }
        if fs::symlink_metadata(request.directory.join(".git")).is_err() {
            self.runner.run(&plan.clone)?;
        }

        let generated_origin_matches = plan.is_generated()
            && self
                .runner
                .capture_stdout(&origin_match_probe(request))
                .is_ok();
        if plan.is_generated() && !generated_origin_matches {
            self.replace_generated_checkout(integration_directory, request)?;
        }
        if plan.is_generated() {
            self.runner.run(&origin_set_command(request))?;
        } else if self
            .runner
            .capture_stdout(&origin_match_probe(request))
            .is_err()
        {
            return Err(InfrastructureError::from(anyhow::anyhow!(
                "external checkout {:?} origin does not match configured repository {:?}; refusing to mutate the external worktree",
                request.directory,
                request.repository
            )));
        }

        let fetch_succeeded = self.runner.run(&plan.fetch).is_ok();
        if !fetch_succeeded {
            warnings.push(request.fetch_warning());
        }

        let remote_branch_probe = remote_branch_probe(request);
        let remote_branch_exists = (plan.is_generated() || fetch_succeeded)
            && self.runner.run(&remote_branch_probe).is_ok();
        if fetch_succeeded && !remote_branch_exists && !self.verified_nonbranch_ref(request)? {
            return Err(InfrastructureError::from(anyhow::anyhow!(
                "configured ref {:?} is not a fetched origin branch, tag, or commit for checkout {:?}",
                request.reference,
                request.directory
            )));
        }

        for command in plan.generated_cleanup_commands() {
            self.runner.run(command)?;
        }

        self.runner
            .run(&plan.checkout_command(remote_branch_exists))?;
        for command in plan.generated_cleanup_commands() {
            self.runner.run(command)?;
        }

        Ok(CheckoutStatus::Updated)
    }

    fn verified_nonbranch_ref(
        &self,
        request: &CheckoutRequest,
    ) -> Result<bool, InfrastructureError> {
        if self.runner.run(&tag_probe(request)).is_ok() {
            return Ok(true);
        }
        if !is_commit_hash(&request.reference) {
            return Ok(false);
        }
        Ok(self.runner.run(&commit_probe(request)).is_ok())
    }

    fn replace_generated_checkout(
        &self,
        integration_directory: &Path,
        request: &CheckoutRequest,
    ) -> Result<(), InfrastructureError> {
        let staging = unique_generated_sibling(&request.directory, "replacement")?;
        let backup = unique_generated_sibling(&request.directory, "previous")?;
        validate_generated_checkout_boundary(integration_directory, &staging)?;
        validate_generated_checkout_boundary(integration_directory, &backup)?;

        if let Err(error) = self.runner.run(&clone_command(request, &staging)) {
            remove_path_if_present(&staging).with_context(|| {
                format!("failed to clean incomplete generated clone {staging:?}")
            })?;
            return Err(error);
        }

        if let Err(error) = fs::rename(&request.directory, &backup) {
            remove_path_if_present(&staging)
                .with_context(|| format!("failed to clean fresh generated clone {staging:?}"))?;
            return Err(InfrastructureError::from(
                anyhow::Error::from(error).context(format!(
                    "failed to preserve generated checkout {:?} before replacing its repository",
                    request.directory
                )),
            ));
        }
        if let Err(replace_error) = fs::rename(&staging, &request.directory) {
            if let Err(restore_error) = fs::rename(&backup, &request.directory) {
                return Err(InfrastructureError::from(anyhow::anyhow!(
                    "failed to install fresh generated checkout {:?}: {replace_error}; failed to restore the previous checkout from {backup:?}: {restore_error}",
                    request.directory
                )));
            }
            remove_path_if_present(&staging)
                .with_context(|| format!("failed to clean fresh generated clone {staging:?}"))?;
            return Err(InfrastructureError::from(
                anyhow::Error::from(replace_error).context(format!(
                    "failed to install fresh generated checkout {:?}",
                    request.directory
                )),
            ));
        }
        remove_path_if_present(&backup)
            .with_context(|| format!("failed to remove replaced generated checkout {backup:?}"))?;
        Ok(())
    }
}

fn git_in(directory: &Path) -> CommandSpec {
    CommandSpec::new("git").args([OsString::from("-C"), directory.as_os_str().to_owned()])
}

fn clone_command(request: &CheckoutRequest, directory: &Path) -> CommandSpec {
    CommandSpec::new("git").args([
        OsString::from("clone"),
        OsString::from("-q"),
        request.repository.clone(),
        directory.as_os_str().to_owned(),
    ])
}

fn prefixed(prefix: &str, value: &OsStr) -> OsString {
    let mut result = OsString::from(prefix);
    result.push(value);
    result
}

fn origin_set_command(request: &CheckoutRequest) -> CommandSpec {
    git_in(&request.directory).args([
        OsString::from("remote"),
        OsString::from("set-url"),
        OsString::from("origin"),
        request.repository.clone(),
    ])
}

fn origin_match_probe(request: &CheckoutRequest) -> CommandSpec {
    git_in(&request.directory).args([
        OsString::from("config"),
        OsString::from("--get-regexp"),
        OsString::from("^remote\\.origin\\.url$"),
        exact_git_regex(&request.repository),
    ])
}

fn remote_branch_probe(request: &CheckoutRequest) -> CommandSpec {
    git_in(&request.directory).args([
        OsString::from("show-ref"),
        OsString::from("--verify"),
        OsString::from("--quiet"),
        prefixed("refs/remotes/origin/", &request.reference),
    ])
}

fn tag_probe(request: &CheckoutRequest) -> CommandSpec {
    let reference = if request
        .reference
        .to_str()
        .is_some_and(|reference| reference.starts_with("refs/tags/"))
    {
        request.reference.clone()
    } else {
        prefixed("refs/tags/", &request.reference)
    };
    git_in(&request.directory).args([
        OsString::from("show-ref"),
        OsString::from("--verify"),
        OsString::from("--quiet"),
        reference,
    ])
}

fn commit_probe(request: &CheckoutRequest) -> CommandSpec {
    let mut reference = request.reference.clone();
    reference.push("^{commit}");
    git_in(&request.directory).args([OsString::from("cat-file"), OsString::from("-e"), reference])
}

fn is_commit_hash(reference: &OsStr) -> bool {
    reference.to_str().is_some_and(|reference| {
        (7..=64).contains(&reference.len())
            && reference.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn exact_git_regex(value: &OsStr) -> OsString {
    let value = value.to_string_lossy();
    let mut pattern = String::with_capacity(value.len() + 2);
    pattern.push('^');
    for character in value.chars() {
        if matches!(
            character,
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
        ) {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern.push('$');
    pattern.into()
}

fn is_path_within(path: &Path, directory: &Path) -> bool {
    let path = normalize_path(path);
    let directory = normalize_path(directory);
    path != directory && path.starts_with(directory)
}

fn normalized_paths_equal(first: &Path, second: &Path) -> bool {
    normalize_path(first) == normalize_path(second)
}

fn unique_generated_sibling(directory: &Path, role: &str) -> Result<PathBuf, InfrastructureError> {
    let parent = directory.parent().ok_or_else(|| {
        anyhow::anyhow!("generated checkout {directory:?} has no parent directory")
    })?;
    let file_name = directory.file_name().ok_or_else(|| {
        anyhow::anyhow!("generated checkout {directory:?} has no final path component")
    })?;

    loop {
        let sequence = GENERATED_REPLACEMENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut candidate_name = OsString::from(".");
        candidate_name.push(file_name);
        candidate_name.push(format!(
            ".cf-integration-{role}-{}-{sequence}",
            std::process::id()
        ));
        let candidate = parent.join(candidate_name);
        match fs::symlink_metadata(&candidate) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Err(error) => {
                return Err(InfrastructureError::from(
                    anyhow::Error::from(error).context(format!(
                        "failed to inspect generated replacement path {candidate:?}"
                    )),
                ));
            }
        }
    }
}

fn remove_path_if_present(path: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => match normalized.components().next_back() {
                Some(Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(Component::ParentDir) | None => normalized.push(component.as_os_str()),
                Some(Component::Prefix(_) | Component::RootDir | Component::CurDir) => {}
            },
        }
    }

    normalized
}

fn validate_generated_checkout_boundary(
    integration_directory: &Path,
    checkout_directory: &Path,
) -> Result<(), InfrastructureError> {
    let canonical_integration = fs::canonicalize(integration_directory).with_context(|| {
        format!("failed to resolve generated integration directory {integration_directory:?}")
    })?;
    let mut existing_ancestor = checkout_directory;
    while fs::symlink_metadata(existing_ancestor).is_err() {
        existing_ancestor = existing_ancestor.parent().ok_or_else(|| {
            anyhow::anyhow!(
                "generated checkout {checkout_directory:?} has no existing filesystem ancestor"
            )
        })?;
    }
    let canonical_ancestor = fs::canonicalize(existing_ancestor).with_context(|| {
        format!("failed to resolve generated checkout path {checkout_directory:?}")
    })?;
    if !canonical_ancestor.starts_with(&canonical_integration) {
        return Err(InfrastructureError::from(anyhow::anyhow!(
            "generated checkout path {checkout_directory:?} resolves outside integration state {integration_directory:?}; configure the external checkout path explicitly to preserve its worktree"
        )));
    }
    Ok(())
}
