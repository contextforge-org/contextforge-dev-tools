use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
use std::os::unix::fs::symlink;

use anyhow::anyhow;
use cf_integration::platform::PlatformError;
use cf_integration::platform::checkout::{
    CheckoutManager, CheckoutPlan, CheckoutRequest, CheckoutStatus,
};
use cf_integration::platform::process::{
    CapturedOutput, CommandSpec, ProcessRunner, SystemProcessRunner,
};

#[derive(Default)]
struct RecordingRunner {
    commands: RefCell<Vec<CommandSpec>>,
    results: RefCell<VecDeque<Result<(), &'static str>>>,
}

impl RecordingRunner {
    fn with_results(results: impl IntoIterator<Item = Result<(), &'static str>>) -> Self {
        Self {
            commands: RefCell::new(Vec::new()),
            results: RefCell::new(results.into_iter().collect()),
        }
    }

    fn commands(&self) -> Vec<CommandSpec> {
        self.commands.borrow().clone()
    }

    fn record(&self, spec: &CommandSpec) -> Result<(), PlatformError> {
        self.commands.borrow_mut().push(spec.clone());
        match self.results.borrow_mut().pop_front().unwrap_or(Ok(())) {
            Ok(()) => Ok(()),
            Err(message) => Err(PlatformError::Native(anyhow!(message))),
        }
    }
}

impl ProcessRunner for RecordingRunner {
    fn run(&self, spec: &CommandSpec) -> Result<(), PlatformError> {
        self.record(spec)
    }

    fn capture_stdout(&self, spec: &CommandSpec) -> Result<Vec<u8>, PlatformError> {
        self.record(spec)?;
        Ok(Vec::new())
    }

    fn capture_output(&self, _spec: &CommandSpec) -> Result<CapturedOutput, PlatformError> {
        unreachable!("checkout does not capture output")
    }

    fn run_to_log(&self, _spec: &CommandSpec, _log_path: &Path) -> Result<(), PlatformError> {
        unreachable!("checkout does not redirect output")
    }
}

fn assert_command(spec: &CommandSpec, arguments: &[&OsStr]) {
    assert_eq!(spec.program(), OsStr::new("git"));
    assert_eq!(
        spec.arguments(),
        arguments
            .iter()
            .map(|argument| OsString::from(*argument))
            .collect::<Vec<_>>()
    );
    assert_eq!(spec.working_directory(), None);
    assert!(spec.environment().is_empty());
}

#[test]
fn generated_branch_plan_resets_to_the_matching_remote_head() {
    let integration = Path::new("/work/repo/.integration");
    let directory = integration.join("mcp-context-forge");
    let request = CheckoutRequest::controlplane(
        &directory,
        "https://example.invalid/controlplane.git",
        "main",
    );

    let plan = CheckoutPlan::new(integration, &request);

    assert!(plan.is_generated());
    assert_command(
        &plan.checkout_command(true),
        &[
            OsStr::new("-C"),
            directory.as_os_str(),
            OsStr::new("checkout"),
            OsStr::new("-q"),
            OsStr::new("-B"),
            OsStr::new("main"),
            OsStr::new("origin/main"),
        ],
    );
    assert_eq!(plan.generated_cleanup_commands().len(), 2);
    assert_command(
        &plan.generated_cleanup_commands()[0],
        &[
            OsStr::new("-C"),
            directory.as_os_str(),
            OsStr::new("reset"),
            OsStr::new("--hard"),
            OsStr::new("--quiet"),
            OsStr::new("HEAD"),
        ],
    );
    assert_command(
        &plan.generated_cleanup_commands()[1],
        &[
            OsStr::new("-C"),
            directory.as_os_str(),
            OsStr::new("clean"),
            OsStr::new("-ffd"),
            OsStr::new("-q"),
        ],
    );
}

#[test]
fn normalized_generated_paths_do_not_escape_the_integration_directory() {
    let integration = Path::new("/work/repo/.integration/./runtime/..");
    let generated = CheckoutRequest::controlplane(
        "/work/repo/.integration/checkouts/../controlplane",
        "repo",
        "main",
    );
    let external =
        CheckoutRequest::controlplane("/work/repo/.integration/../controlplane", "repo", "main");

    assert!(CheckoutPlan::new(integration, &generated).is_generated());
    assert!(!CheckoutPlan::new(integration, &external).is_generated());
    assert!(
        !CheckoutPlan::new(
            integration,
            &CheckoutRequest::controlplane(integration, "repo", "main",)
        )
        .is_generated()
    );
}

#[test]
fn integration_root_cannot_be_used_as_a_checkout() {
    let temporary = tempfile::tempdir().expect("temporary directory should be created");
    let integration = temporary.path().join(".integration");
    let request = CheckoutRequest::controlplane(&integration, "upstream", "main");
    let runner = RecordingRunner::default();
    let manager = CheckoutManager::new(&runner);
    let mut warnings = Vec::new();

    let error = manager
        .ensure(&integration, &request, &mut warnings)
        .expect_err("the integration state root must never be replaced recursively");

    assert!(error.to_string().contains("integration root itself"));
    assert!(runner.commands().is_empty());
    assert!(warnings.is_empty());
}

#[test]
fn external_branch_plan_never_probes_or_resets_the_branch() {
    let integration = Path::new("/work/repo/.integration");
    let directory = Path::new("/work/checkouts/controlplane");
    let request = CheckoutRequest::controlplane(directory, "repo", "release");

    let plan = CheckoutPlan::new(integration, &request);

    assert!(!plan.is_generated());
    assert!(plan.generated_cleanup_commands().is_empty());
    assert_command(
        &plan.checkout_command(true),
        &[
            OsStr::new("-C"),
            directory.as_os_str(),
            OsStr::new("checkout"),
            OsStr::new("-q"),
            OsStr::new("release"),
        ],
    );
}

#[test]
fn generated_tag_or_sha_without_a_remote_branch_uses_plain_checkout() {
    let integration = Path::new("/work/repo/.integration");
    let directory = integration.join("controlplane");

    for reference in ["v1.2.3", "0123456789abcdef"] {
        let request = CheckoutRequest::controlplane(&directory, "repo", reference);
        let plan = CheckoutPlan::new(integration, &request);

        assert_command(
            &plan.checkout_command(false),
            &[
                OsStr::new("-C"),
                directory.as_os_str(),
                OsStr::new("checkout"),
                OsStr::new("-q"),
                OsStr::new(reference),
            ],
        );
    }
}

#[test]
fn missing_checkout_is_cloned_before_fetch_and_checkout() {
    let temporary = tempfile::tempdir().expect("temporary directory should be created");
    let integration = temporary.path().join(".integration");
    let directory = integration.join("controlplane");
    let request = CheckoutRequest::controlplane(&directory, "upstream", "main");
    let runner = RecordingRunner::default();
    let manager = CheckoutManager::new(&runner);
    let mut warnings = Vec::new();

    let status = manager
        .ensure(&integration, &request, &mut warnings)
        .expect("recorded checkout should succeed");

    assert_eq!(status, CheckoutStatus::Updated);
    assert!(warnings.is_empty());
    assert!(integration.is_dir());
    let commands = runner.commands();
    assert_eq!(commands.len(), 10);
    assert_command(
        &commands[0],
        &[
            OsStr::new("clone"),
            OsStr::new("-q"),
            OsStr::new("upstream"),
            directory.as_os_str(),
        ],
    );
    assert_command(
        &commands[1],
        &[
            OsStr::new("-C"),
            directory.as_os_str(),
            OsStr::new("config"),
            OsStr::new("--get-regexp"),
            OsStr::new("^remote\\.origin\\.url$"),
            OsStr::new("^upstream$"),
        ],
    );
    assert_command(
        &commands[2],
        &[
            OsStr::new("-C"),
            directory.as_os_str(),
            OsStr::new("remote"),
            OsStr::new("set-url"),
            OsStr::new("origin"),
            OsStr::new("upstream"),
        ],
    );
    assert_command(
        &commands[3],
        &[
            OsStr::new("-C"),
            directory.as_os_str(),
            OsStr::new("fetch"),
            OsStr::new("-q"),
            OsStr::new("--prune"),
            OsStr::new("--prune-tags"),
            OsStr::new("--tags"),
            OsStr::new("--force"),
            OsStr::new("origin"),
        ],
    );
    assert_command(
        &commands[7],
        &[
            OsStr::new("-C"),
            directory.as_os_str(),
            OsStr::new("checkout"),
            OsStr::new("-q"),
            OsStr::new("-B"),
            OsStr::new("main"),
            OsStr::new("origin/main"),
        ],
    );
}

#[test]
fn failed_fetch_warns_and_uses_the_existing_generated_remote_ref() {
    let temporary = tempfile::tempdir().expect("temporary directory should be created");
    let integration = temporary.path().join(".integration");
    let directory = integration.join("controlplane");
    fs::create_dir_all(directory.join(".git")).expect("fake checkout should be created");
    let request = CheckoutRequest::controlplane(&directory, "upstream", "main");
    let runner = RecordingRunner::with_results([Ok(()), Ok(()), Err("offline"), Ok(())]);
    let manager = CheckoutManager::new(&runner);
    let mut warnings = Vec::new();

    let status = manager
        .ensure(&integration, &request, &mut warnings)
        .expect("an existing ref should still be checked out");

    assert_eq!(status, CheckoutStatus::Updated);
    assert_eq!(
        warnings,
        ["warning: fetch from upstream failed; using existing checkout"]
    );
    let commands = runner.commands();
    assert_eq!(commands.len(), 9);
    assert!(commands.iter().any(|command| {
        command
            .arguments()
            .iter()
            .any(|argument| argument == OsStr::new("origin/main"))
    }));
}

#[test]
fn failed_dataplane_fetch_uses_the_dataplane_specific_warning() {
    let temporary = tempfile::tempdir().expect("temporary directory should be created");
    let integration = temporary.path().join(".integration");
    let directory = integration.join("dataplane");
    fs::create_dir_all(directory.join(".git")).expect("fake checkout should be created");
    let request = CheckoutRequest::dataplane(&directory, "dataplane-upstream", "main");
    let runner = RecordingRunner::with_results([Ok(()), Ok(()), Err("offline"), Ok(())]);
    let manager = CheckoutManager::new(&runner);
    let mut warnings = Vec::new();

    manager
        .ensure(&integration, &request, &mut warnings)
        .expect("an existing dataplane ref should still be checked out");

    assert_eq!(
        warnings,
        ["warning: fetch from dataplane-upstream failed; using existing dataplane checkout"]
    );
}

#[test]
fn failed_fetch_still_surfaces_an_unknown_local_ref_failure_and_warning() {
    let temporary = tempfile::tempdir().expect("temporary directory should be created");
    let integration = temporary.path().join(".integration");
    let directory = temporary.path().join("external");
    fs::create_dir_all(directory.join(".git")).expect("fake checkout should be created");
    let request = CheckoutRequest::controlplane(&directory, "upstream", "missing-ref");
    let runner = RecordingRunner::with_results([Ok(()), Err("offline"), Err("unknown ref")]);
    let manager = CheckoutManager::new(&runner);
    let mut warnings = Vec::new();

    let failure = manager
        .ensure(&integration, &request, &mut warnings)
        .expect_err("the checkout error must remain fatal");

    assert_eq!(failure.to_string(), "unknown ref");
    assert_eq!(
        warnings,
        ["warning: fetch from upstream failed; using existing checkout"]
    );
    assert_eq!(runner.commands().len(), 3);
}

#[test]
fn dataplane_with_an_empty_ref_is_skipped_without_filesystem_or_process_changes() {
    let temporary = tempfile::tempdir().expect("temporary directory should be created");
    let integration = temporary.path().join("does-not-exist");
    let request =
        CheckoutRequest::dataplane(integration.join("dataplane"), "dataplane-upstream", "");
    let runner = RecordingRunner::default();
    let manager = CheckoutManager::new(&runner);
    let mut warnings = Vec::new();

    let status = manager
        .ensure(&integration, &request, &mut warnings)
        .expect("published-image mode should be a no-op");

    assert_eq!(status, CheckoutStatus::Skipped);
    assert!(warnings.is_empty());
    assert!(runner.commands().is_empty());
    assert!(!integration.exists());
}

struct GitFixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    origin: PathBuf,
    seed: PathBuf,
}

impl GitFixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().expect("temporary directory should be created");
        let root = temporary.path().to_path_buf();
        let origin = root.join("origin.git");
        let seed = root.join("seed");
        git(None, ["init", "--bare", path_text(&origin)]);
        git(None, ["clone", path_text(&origin), path_text(&seed)]);
        git(Some(&seed), ["config", "user.name", "Checkout Test"]);
        git(
            Some(&seed),
            ["config", "user.email", "checkout@example.invalid"],
        );
        git(Some(&seed), ["checkout", "-b", "main"]);
        fs::write(seed.join("state.txt"), "origin\n").expect("seed file should be written");
        git(Some(&seed), ["add", "state.txt"]);
        git(Some(&seed), ["commit", "-m", "origin commit"]);
        git(Some(&seed), ["push", "-u", "origin", "main"]);
        Self {
            _temporary: temporary,
            root,
            origin,
            seed,
        }
    }

    fn request(&self, directory: impl Into<PathBuf>, reference: &str) -> CheckoutRequest {
        CheckoutRequest::controlplane(directory, self.origin.as_os_str(), OsStr::new(reference))
    }
}

#[test]
fn real_generated_checkout_resets_a_local_branch_to_origin() {
    let fixture = GitFixture::new();
    let integration = fixture.root.join(".integration");
    let checkout = integration.join("controlplane");
    let request = fixture.request(&checkout, "main");
    let manager = CheckoutManager::new(&SystemProcessRunner);
    let mut warnings = Vec::new();
    manager
        .ensure(&integration, &request, &mut warnings)
        .expect("initial generated checkout should succeed");
    git(Some(&checkout), ["config", "user.name", "Checkout Test"]);
    git(
        Some(&checkout),
        ["config", "user.email", "checkout@example.invalid"],
    );
    fs::write(checkout.join("local.txt"), "local\n").expect("local file should be written");
    git(Some(&checkout), ["add", "local.txt"]);
    git(Some(&checkout), ["commit", "-m", "local commit"]);
    let local_head = git_stdout(&checkout, ["rev-parse", "HEAD"]);
    fs::write(checkout.join("state.txt"), "dirty generated state\n")
        .expect("tracked generated file should be dirtied");
    fs::write(checkout.join("untracked.txt"), "stale generated state\n")
        .expect("untracked generated file should be created");

    manager
        .ensure(&integration, &request, &mut warnings)
        .expect("generated checkout refresh should succeed");

    let head = git_stdout(&checkout, ["rev-parse", "HEAD"]);
    let origin_head = git_stdout(&checkout, ["rev-parse", "origin/main"]);
    assert_ne!(head, local_head);
    assert_eq!(head, origin_head);
    assert_eq!(
        fs::read_to_string(checkout.join("state.txt")).expect("tracked file should remain"),
        "origin\n"
    );
    assert!(!checkout.join("untracked.txt").exists());
    assert!(git_stdout(&checkout, ["status", "--porcelain"]).is_empty());
    assert!(warnings.is_empty());
}

#[test]
fn real_generated_checkout_discards_conflicting_wip_before_switching_refs() {
    let fixture = GitFixture::new();
    git(Some(&fixture.seed), ["checkout", "-b", "release"]);
    fs::write(fixture.seed.join("state.txt"), "release\n")
        .expect("release state should be written");
    git(Some(&fixture.seed), ["add", "state.txt"]);
    git(Some(&fixture.seed), ["commit", "-m", "release commit"]);
    git(Some(&fixture.seed), ["push", "-u", "origin", "release"]);

    let integration = fixture.root.join(".integration");
    let checkout = integration.join("controlplane");
    let manager = CheckoutManager::new(&SystemProcessRunner);
    let mut warnings = Vec::new();
    manager
        .ensure(
            &integration,
            &fixture.request(&checkout, "main"),
            &mut warnings,
        )
        .expect("initial generated checkout should succeed");
    fs::write(checkout.join("state.txt"), "conflicting dirty state\n")
        .expect("generated checkout should be dirtied");

    manager
        .ensure(
            &integration,
            &fixture.request(&checkout, "release"),
            &mut warnings,
        )
        .expect("generated checkout should clean before switching refs");

    assert_eq!(
        fs::read_to_string(checkout.join("state.txt")).expect("release state should exist"),
        "release\n"
    );
    assert!(git_stdout(&checkout, ["status", "--porcelain"]).is_empty());
}

#[test]
fn successful_fetch_rejects_a_deleted_remote_branch_instead_of_using_stale_local_state() {
    let fixture = GitFixture::new();
    git(Some(&fixture.seed), ["checkout", "-b", "obsolete"]);
    git(Some(&fixture.seed), ["push", "-u", "origin", "obsolete"]);
    let integration = fixture.root.join(".integration");
    let checkout = integration.join("controlplane");
    let request = fixture.request(&checkout, "obsolete");
    let manager = CheckoutManager::new(&SystemProcessRunner);
    let mut warnings = Vec::new();
    manager
        .ensure(&integration, &request, &mut warnings)
        .expect("initial remote branch checkout should succeed");
    fs::write(
        checkout.join("state.txt"),
        "generated WIP before missing branch\n",
    )
    .expect("generated checkout should be dirtied");
    git(Some(&fixture.seed), ["checkout", "main"]);
    git(
        Some(&fixture.seed),
        ["push", "origin", "--delete", "obsolete"],
    );

    let error = manager
        .ensure(&integration, &request, &mut warnings)
        .expect_err("a pruned remote branch must not fall back to a stale local branch");

    assert!(error.to_string().contains("not a fetched origin branch"));
    assert_eq!(
        fs::read_to_string(checkout.join("state.txt")).expect("WIP should remain on rejection"),
        "generated WIP before missing branch\n"
    );
}

#[test]
fn successful_generated_fetch_rejects_a_deleted_remote_tag() {
    let fixture = GitFixture::new();
    git(Some(&fixture.seed), ["tag", "obsolete-tag"]);
    git(Some(&fixture.seed), ["push", "origin", "obsolete-tag"]);
    let integration = fixture.root.join(".integration");
    let checkout = integration.join("controlplane");
    let manager = CheckoutManager::new(&SystemProcessRunner);
    let mut warnings = Vec::new();
    manager
        .ensure(
            &integration,
            &fixture.request(&checkout, "obsolete-tag"),
            &mut warnings,
        )
        .expect("initial remote tag checkout should succeed");
    git(Some(&fixture.seed), ["tag", "--delete", "obsolete-tag"]);
    git(
        Some(&fixture.seed),
        ["push", "origin", ":refs/tags/obsolete-tag"],
    );

    let error = manager
        .ensure(
            &integration,
            &fixture.request(&checkout, "obsolete-tag"),
            &mut warnings,
        )
        .expect_err("a pruned remote tag must not fall back to stale local state");

    assert!(error.to_string().contains("not a fetched origin branch"));
    assert!(!git_succeeds(
        &checkout,
        ["show-ref", "--verify", "--quiet", "refs/tags/obsolete-tag",],
    ));
    assert!(warnings.is_empty());
}

#[test]
fn generated_checkout_updates_origin_when_the_configured_repository_changes() {
    let first = GitFixture::new();
    let second = GitFixture::new();
    fs::write(first.seed.join("first-only.txt"), "first repository only\n")
        .expect("first-only state should be written");
    git(Some(&first.seed), ["add", "first-only.txt"]);
    git(Some(&first.seed), ["commit", "-m", "first-only commit"]);
    git(Some(&first.seed), ["tag", "first-only-tag"]);
    git(
        Some(&first.seed),
        ["push", "origin", "main", "first-only-tag"],
    );
    let first_only_revision = git_stdout(&first.seed, ["rev-parse", "HEAD"]);
    fs::write(second.seed.join("state.txt"), "second origin\n")
        .expect("second origin state should be written");
    git(Some(&second.seed), ["add", "state.txt"]);
    git(Some(&second.seed), ["commit", "-m", "second origin commit"]);
    git(Some(&second.seed), ["push", "origin", "main"]);
    let integration = first.root.join(".integration");
    let checkout = integration.join("controlplane");
    let manager = CheckoutManager::new(&SystemProcessRunner);
    let mut warnings = Vec::new();
    manager
        .ensure(
            &integration,
            &first.request(&checkout, "main"),
            &mut warnings,
        )
        .expect("initial generated checkout should succeed");

    manager
        .ensure(
            &integration,
            &CheckoutRequest::controlplane(&checkout, second.origin.as_os_str(), "main"),
            &mut warnings,
        )
        .expect("generated checkout should follow the configured repository");

    assert_eq!(
        git_stdout(&checkout, ["remote", "get-url", "origin"]),
        path_text(&second.origin)
    );
    assert_eq!(
        fs::read_to_string(checkout.join("state.txt")).expect("second state should be checked out"),
        "second origin\n"
    );
    assert!(!git_succeeds(
        &checkout,
        [
            "show-ref",
            "--verify",
            "--quiet",
            "refs/tags/first-only-tag",
        ],
    ));
    let first_only_commit = format!("{first_only_revision}^{{commit}}");
    assert!(
        !git_succeeds(&checkout, ["cat-file", "-e", &first_only_commit]),
        "a fresh-origin checkout must not retain objects from the previous repository"
    );
}

#[test]
fn generated_checkout_rejects_two_offline_origin_change_attempts_without_using_old_refs() {
    let first = GitFixture::new();
    let second = GitFixture::new();
    let integration = first.root.join(".integration");
    let checkout = integration.join("controlplane");
    let manager = CheckoutManager::new(&SystemProcessRunner);
    let mut warnings = Vec::new();
    manager
        .ensure(
            &integration,
            &first.request(&checkout, "main"),
            &mut warnings,
        )
        .expect("initial generated checkout should succeed");
    let first_head = git_stdout(&checkout, ["rev-parse", "HEAD"]);
    fs::write(checkout.join("state.txt"), "WIP from first repository\n")
        .expect("generated checkout should be dirtied");
    let changed_request =
        CheckoutRequest::controlplane(&checkout, second.origin.as_os_str(), OsStr::new("main"));
    fs::rename(&second.origin, second.root.join("offline-origin.git"))
        .expect("second origin should be made unavailable");

    for attempt in 1..=2 {
        let result = manager.ensure(&integration, &changed_request, &mut warnings);
        assert!(
            result.is_err(),
            "offline changed origin attempt {attempt} must not reuse refs from the previous repository"
        );
        assert_eq!(
            git_stdout(&checkout, ["remote", "get-url", "origin"]),
            path_text(&first.origin),
            "a failed replacement must leave the previous origin intact"
        );
        assert_eq!(git_stdout(&checkout, ["rev-parse", "HEAD"]), first_head);
        assert_eq!(
            fs::read_to_string(checkout.join("state.txt"))
                .expect("rejected checkout WIP should remain"),
            "WIP from first repository\n"
        );
    }
    assert!(warnings.is_empty());
}

#[test]
fn generated_checkout_accepts_a_verified_commit_hash() {
    let fixture = GitFixture::new();
    let revision = git_stdout(&fixture.seed, ["rev-parse", "HEAD"]);
    let integration = fixture.root.join(".integration");
    let checkout = integration.join("controlplane");
    let manager = CheckoutManager::new(&SystemProcessRunner);
    let mut warnings = Vec::new();

    manager
        .ensure(
            &integration,
            &fixture.request(&checkout, &revision),
            &mut warnings,
        )
        .expect("a fetched commit hash should be a valid checkout target");

    assert_eq!(git_stdout(&checkout, ["rev-parse", "HEAD"]), revision);
}

#[cfg(unix)]
#[test]
fn generated_symlink_to_external_checkout_is_rejected_without_touching_wip() {
    let fixture = GitFixture::new();
    let integration = fixture.root.join(".integration");
    fs::create_dir_all(&integration).expect("integration directory should be created");
    let external = fixture.root.join("external-controlplane");
    let manager = CheckoutManager::new(&SystemProcessRunner);
    let mut warnings = Vec::new();
    manager
        .ensure(
            &integration,
            &fixture.request(&external, "main"),
            &mut warnings,
        )
        .expect("external checkout should be created");
    fs::write(external.join("state.txt"), "external dirty state\n")
        .expect("external checkout should be dirtied");
    fs::write(external.join("untracked.txt"), "external untracked state\n")
        .expect("external untracked file should be created");
    let linked = integration.join("controlplane");
    symlink(&external, &linked).expect("generated-looking symlink should be created");

    let error = manager
        .ensure(
            &integration,
            &fixture.request(&linked, "main"),
            &mut warnings,
        )
        .expect_err("a generated path resolving outside state must be rejected");

    assert!(error.to_string().contains("resolves outside"));
    assert_eq!(
        fs::read_to_string(external.join("state.txt")).expect("external WIP should remain"),
        "external dirty state\n"
    );
    assert!(external.join("untracked.txt").is_file());
}

#[test]
fn real_external_checkout_preserves_its_local_branch_position_after_fetch() {
    let fixture = GitFixture::new();
    let integration = fixture.root.join(".integration");
    let checkout = fixture.root.join("external-controlplane");
    let request = fixture.request(&checkout, "main");
    let manager = CheckoutManager::new(&SystemProcessRunner);
    let mut warnings = Vec::new();
    manager
        .ensure(&integration, &request, &mut warnings)
        .expect("initial external checkout should succeed");
    git(Some(&checkout), ["config", "user.name", "Checkout Test"]);
    git(
        Some(&checkout),
        ["config", "user.email", "checkout@example.invalid"],
    );
    fs::write(checkout.join("local.txt"), "local\n").expect("local file should be written");
    git(Some(&checkout), ["add", "local.txt"]);
    git(Some(&checkout), ["commit", "-m", "local commit"]);
    git(Some(&checkout), ["tag", "external-local-only"]);
    let local_head = git_stdout(&checkout, ["rev-parse", "HEAD"]);
    fs::write(checkout.join("state.txt"), "external work in progress\n")
        .expect("external tracked WIP should be created");
    fs::write(checkout.join("untracked.txt"), "external untracked WIP\n")
        .expect("external untracked WIP should be created");

    manager
        .ensure(&integration, &request, &mut warnings)
        .expect("external checkout refresh should succeed");

    assert_eq!(git_stdout(&checkout, ["rev-parse", "HEAD"]), local_head);
    assert_eq!(
        fs::read_to_string(checkout.join("state.txt")).expect("external WIP should remain"),
        "external work in progress\n"
    );
    assert!(checkout.join("untracked.txt").is_file());
    assert!(git_succeeds(
        &checkout,
        [
            "show-ref",
            "--verify",
            "--quiet",
            "refs/tags/external-local-only",
        ],
    ));
    assert!(warnings.is_empty());
}

#[test]
fn external_checkout_rejects_a_repository_mismatch_without_touching_wip() {
    let first = GitFixture::new();
    let second = GitFixture::new();
    let integration = first.root.join(".integration");
    let checkout = first.root.join("external-controlplane");
    let manager = CheckoutManager::new(&SystemProcessRunner);
    let mut warnings = Vec::new();
    manager
        .ensure(
            &integration,
            &first.request(&checkout, "main"),
            &mut warnings,
        )
        .expect("initial external checkout should succeed");
    fs::write(checkout.join("state.txt"), "external tracked WIP\n")
        .expect("external checkout should be dirtied");
    fs::write(checkout.join("untracked.txt"), "external untracked WIP\n")
        .expect("external untracked file should be written");

    let error = manager
        .ensure(
            &integration,
            &CheckoutRequest::controlplane(&checkout, second.origin.as_os_str(), "main"),
            &mut warnings,
        )
        .expect_err("external checkout origin mismatch must fail without mutation");

    assert!(error.to_string().contains("origin does not match"));
    assert!(error.to_string().contains("refusing to mutate"));
    assert_eq!(
        fs::read_to_string(checkout.join("state.txt")).expect("tracked WIP should remain"),
        "external tracked WIP\n"
    );
    assert!(checkout.join("untracked.txt").is_file());
    assert_eq!(
        git_stdout(&checkout, ["remote", "get-url", "origin"]),
        path_text(&first.origin)
    );
}

#[test]
fn real_external_git_worktree_is_reused_and_preserves_wip() {
    let fixture = GitFixture::new();
    let integration = fixture.root.join(".integration");
    let worktree = fixture.root.join("external-worktree");
    git(
        Some(&fixture.seed),
        [
            "worktree",
            "add",
            "-b",
            "local-worktree",
            path_text(&worktree),
            "main",
        ],
    );
    git(Some(&worktree), ["push", "-u", "origin", "local-worktree"]);
    assert!(worktree.join(".git").is_file());
    fs::write(worktree.join("state.txt"), "worktree tracked WIP\n")
        .expect("tracked worktree file should be dirtied");
    fs::write(worktree.join("untracked.txt"), "worktree untracked WIP\n")
        .expect("untracked worktree file should be created");
    let request = fixture.request(&worktree, "local-worktree");
    let manager = CheckoutManager::new(&SystemProcessRunner);
    let mut warnings = Vec::new();

    manager
        .ensure(&integration, &request, &mut warnings)
        .expect("an existing Git worktree should be fetched without cloning over it");

    assert_eq!(
        fs::read_to_string(worktree.join("state.txt")).expect("tracked WIP should remain"),
        "worktree tracked WIP\n"
    );
    assert!(worktree.join("untracked.txt").is_file());
    assert!(warnings.is_empty());
}

#[test]
fn real_tag_checkout_uses_plain_checkout_and_detaches_head() {
    let fixture = GitFixture::new();
    git(Some(&fixture.seed), ["tag", "v1.0.0"]);
    git(Some(&fixture.seed), ["push", "origin", "v1.0.0"]);
    let integration = fixture.root.join(".integration");
    let checkout = integration.join("tagged-controlplane");
    let request = fixture.request(&checkout, "v1.0.0");
    let manager = CheckoutManager::new(&SystemProcessRunner);
    let mut warnings = Vec::new();

    manager
        .ensure(&integration, &request, &mut warnings)
        .expect("tag checkout should succeed");

    let status = Command::new("git")
        .args([
            "-C",
            path_text(&checkout),
            "symbolic-ref",
            "--quiet",
            "HEAD",
        ])
        .status()
        .expect("git symbolic-ref should execute");
    assert!(!status.success(), "tag checkout should detach HEAD");
    assert_eq!(
        git_stdout(&checkout, ["rev-parse", "HEAD"]),
        git_stdout(&fixture.seed, ["rev-parse", "v1.0.0"])
    );
    assert!(warnings.is_empty());
}

fn path_text(path: &Path) -> &str {
    path.to_str()
        .expect("temporary test paths should contain valid UTF-8")
}

fn git<'a>(cwd: Option<&Path>, arguments: impl IntoIterator<Item = &'a str>) {
    let mut command = Command::new("git");
    command
        .args(arguments)
        .env("PRE_COMMIT_ALLOW_NO_CONFIG", "1");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command.output().expect("git should execute");
    assert!(
        output.status.success(),
        "git failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout<'a>(cwd: &Path, arguments: impl IntoIterator<Item = &'a str>) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(cwd)
        .output()
        .expect("git should execute");
    assert!(
        output.status.success(),
        "git failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git object IDs should be UTF-8")
        .trim()
        .to_owned()
}

fn git_succeeds<'a>(cwd: &Path, arguments: impl IntoIterator<Item = &'a str>) -> bool {
    Command::new("git")
        .args(arguments)
        .current_dir(cwd)
        .output()
        .expect("git should execute")
        .status
        .success()
}
