//! Strict conformance baseline evaluation and transactional updates.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::results::{CheckStatus, ConformanceResults, ConformanceServerEra, SemanticLane};

const MAX_BASELINE_BYTES: u64 = 1024 * 1024;

/// The semantic lanes in their stable reporting order.
pub(crate) const ALL_CONFORMANCE_LANES: [SemanticLane; 3] = [
    SemanticLane::FixtureDirect,
    SemanticLane::BuiltInDataPlane,
    SemanticLane::ExternalDataPlane,
];

/// A baseline-eligible official check status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum ScoredStatus {
    /// A required check failed.
    Failure,
    /// The oracle emitted a scored warning.
    Warning,
}

/// Stable identity of one scored official finding.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScoredFinding {
    /// Official scenario name.
    pub(crate) scenario: String,
    /// Stable official check identifier.
    pub(crate) check: String,
    /// Official check name, which disambiguates reused specification identifiers.
    pub(crate) name: String,
    /// Scored status.
    pub(crate) status: ScoredStatus,
}

/// Strict YAML payload stored in one lane baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConformanceBaseline {
    /// Sorted scored findings expected for this lane.
    pub(crate) findings: Vec<ScoredFinding>,
}

/// One lane's evaluated baseline state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BaselineComparison {
    /// Evaluated lane.
    pub(crate) lane: SemanticLane,
    /// Actual findings after direct-fixture subtraction where applicable.
    pub(crate) actual: Vec<ScoredFinding>,
    /// Findings loaded from the checked-in baseline.
    pub(crate) expected: Vec<ScoredFinding>,
    /// Actual findings absent from the baseline.
    pub(crate) unexpected: Vec<ScoredFinding>,
    /// Baseline findings absent from the actual result.
    pub(crate) stale: Vec<ScoredFinding>,
}

impl BaselineComparison {
    /// Whether the actual scored findings exactly match the baseline.
    #[must_use]
    pub(crate) fn matches(&self) -> bool {
        self.unexpected.is_empty() && self.stale.is_empty()
    }
}

/// A staged baseline replacement produced by a successful evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BaselineUpdate {
    relative_path: PathBuf,
    document: ConformanceBaseline,
}

/// Results for one client-version/server-era combination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BaselineEvaluation {
    /// Per-lane comparisons in stable lane order.
    pub(crate) comparisons: Vec<BaselineComparison>,
    /// Documents to commit when blessing was requested.
    pub(crate) updates: Vec<BaselineUpdate>,
}

/// Evaluates every selected lane against its strict baseline.
///
/// Routed findings reproduced by the direct fixture are removed before the
/// routed lane is compared. Blessing still parses existing files when present,
/// so malformed baselines cannot be silently replaced.
pub(crate) fn evaluate_baselines(
    results: &BTreeMap<SemanticLane, ConformanceResults>,
    selected_lanes: &[SemanticLane],
    baseline_root: &Path,
    client_version: &str,
    server_era: ConformanceServerEra,
    bless: bool,
) -> Result<BaselineEvaluation> {
    let selected = selected_lanes.iter().copied().collect::<BTreeSet<_>>();
    if selected.is_empty() {
        bail!("at least one conformance lane must be selected");
    }
    let routed_selected = selected.iter().any(|lane| {
        matches!(
            lane,
            SemanticLane::BuiltInDataPlane | SemanticLane::ExternalDataPlane
        )
    });
    if routed_selected && !results.contains_key(&SemanticLane::FixtureDirect) {
        bail!("missing fixture-direct lane required to gate routed findings");
    }
    for lane in &selected {
        if !results.contains_key(lane) {
            bail!("missing selected conformance lane {}", lane.slug());
        }
    }

    let direct = results
        .get(&SemanticLane::FixtureDirect)
        .map(scored_findings)
        .transpose()?
        .unwrap_or_default();
    let mut comparisons = Vec::new();
    let mut updates = Vec::new();
    for lane in ALL_CONFORMANCE_LANES {
        if !selected.contains(&lane) {
            continue;
        }
        let mut actual = scored_findings(
            results
                .get(&lane)
                .ok_or_else(|| anyhow!("missing selected conformance lane {}", lane.slug()))?,
        )?;
        if lane != SemanticLane::FixtureDirect {
            actual = actual.difference(&direct).cloned().collect();
        }
        let path = baseline_path(baseline_root, client_version, server_era, lane);
        let loaded = read_baseline_optional(&path)?;
        if loaded.is_none() && !bless {
            bail!("missing conformance baseline {}", path.display());
        }
        let expected = loaded
            .map(|document| document.findings.into_iter().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        let unexpected = actual.difference(&expected).cloned().collect::<Vec<_>>();
        let stale = expected.difference(&actual).cloned().collect::<Vec<_>>();
        comparisons.push(BaselineComparison {
            lane,
            actual: actual.iter().cloned().collect(),
            expected: expected.iter().cloned().collect(),
            unexpected,
            stale,
        });
        if bless {
            updates.push(BaselineUpdate {
                relative_path: baseline_relative_path(client_version, server_era, lane),
                document: ConformanceBaseline {
                    findings: actual.into_iter().collect(),
                },
            });
        }
    }
    Ok(BaselineEvaluation {
        comparisons,
        updates,
    })
}

/// Writes one deterministic machine-readable lane report.
pub(crate) fn write_baseline_report(
    path: &Path,
    client_version: &str,
    server_era: ConformanceServerEra,
    comparison: &BaselineComparison,
) -> Result<()> {
    #[derive(Serialize)]
    #[serde(deny_unknown_fields)]
    struct Report<'a> {
        client_version: &'a str,
        server_era: ConformanceServerEra,
        lane: &'a str,
        actual: &'a [ScoredFinding],
        expected: &'a [ScoredFinding],
        unexpected: &'a [ScoredFinding],
        stale: &'a [ScoredFinding],
    }

    let parent = path
        .parent()
        .context("baseline report path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create baseline report directory {parent:?}"))?;
    let source = yaml_serde::to_string(&Report {
        client_version,
        server_era,
        lane: comparison.lane.slug(),
        actual: &comparison.actual,
        expected: &comparison.expected,
        unexpected: &comparison.unexpected,
        stale: &comparison.stale,
    })
    .context("failed to serialize baseline report")?;
    fs::write(path, source).with_context(|| format!("failed to write baseline report {path:?}"))
}

/// Commits all selected baseline updates as one directory transaction.
pub(crate) fn bless_baselines_transactionally(
    root: &Path,
    updates: &[BaselineUpdate],
) -> Result<()> {
    if updates.is_empty() {
        bail!("no conformance baselines were selected for blessing");
    }
    let parent = root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = root
        .file_name()
        .and_then(|value| value.to_str())
        .context("baseline root must have a UTF-8 directory name")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create baseline parent directory {parent:?}"))?;

    let transaction = Uuid::new_v4();
    let staging = parent.join(format!(".{name}.stage-{transaction}"));
    let backup = parent.join(format!(".{name}.backup-{transaction}"));
    let mut staging_guard = DirectoryGuard::new(staging.clone());
    let mut backup_guard = DirectoryGuard::new(backup.clone());
    fs::create_dir(&staging)
        .with_context(|| format!("failed to create baseline staging directory {staging:?}"))?;

    let root_exists = match fs::symlink_metadata(root) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                bail!("baseline root {root:?} must be a real directory");
            }
            copy_directory(root, &staging)?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).context(format!("failed to inspect baseline root {root:?}"));
        }
    };

    let mut paths = BTreeSet::new();
    for update in updates {
        if !paths.insert(update.relative_path.clone()) {
            bail!("duplicate baseline update for {:?}", update.relative_path);
        }
        let path = staging.join(&update.relative_path);
        let directory = path
            .parent()
            .context("baseline update path has no parent directory")?;
        fs::create_dir_all(directory)
            .with_context(|| format!("failed to create staged baseline directory {directory:?}"))?;
        let source = yaml_serde::to_string(&update.document)
            .context("failed to serialize conformance baseline")?;
        fs::write(&path, source)
            .with_context(|| format!("failed to write staged conformance baseline {path:?}"))?;
        read_baseline(&path)?;
    }

    if root_exists {
        fs::rename(root, &backup).with_context(|| {
            format!("failed to move existing baseline root {root:?} to {backup:?}")
        })?;
        if let Err(error) = fs::rename(&staging, root) {
            let rollback = fs::rename(&backup, root);
            if rollback.is_err() {
                backup_guard.disarm();
            }
            return Err(anyhow!(error).context(format!(
                "failed to install staged baselines at {root:?}; rollback result: {rollback:?}"
            )));
        }
        if let Err(error) = fs::remove_dir_all(&backup) {
            let move_new = fs::rename(root, &staging);
            let restore_old = fs::rename(&backup, root);
            if move_new.is_err() || restore_old.is_err() {
                staging_guard.disarm();
            }
            if restore_old.is_err() {
                backup_guard.disarm();
            }
            return Err(anyhow!(error).context(format!(
                "failed to remove baseline backup after commit; rollback results: new={move_new:?}, old={restore_old:?}"
            )));
        }
        backup_guard.disarm();
    } else {
        fs::rename(&staging, root).with_context(|| {
            format!("failed to install staged conformance baselines at {root:?}")
        })?;
    }
    staging_guard.disarm();
    Ok(())
}

/// Rejects unknown statuses in official output and returns its scored findings.
pub(crate) fn validate_scored_results(results: &ConformanceResults) -> Result<()> {
    scored_findings(results).map(|_| ())
}

fn scored_findings(results: &ConformanceResults) -> Result<BTreeSet<ScoredFinding>> {
    let mut findings = BTreeSet::new();
    for (scenario, result) in &results.scenarios {
        for check in &result.checks {
            let name = check.name.as_deref().unwrap_or_default();
            let status = match &check.status {
                CheckStatus::Failure => Some(ScoredStatus::Failure),
                CheckStatus::Warning => Some(ScoredStatus::Warning),
                CheckStatus::Success | CheckStatus::Skipped | CheckStatus::Info => None,
                CheckStatus::Other(status) => {
                    bail!(
                        "unknown official check status {status:?} for scenario {scenario:?}, check {:?}",
                        check.id
                    );
                }
            };
            if let Some(status) = status {
                findings.insert(ScoredFinding {
                    scenario: scenario.clone(),
                    check: check.id.clone(),
                    name: name.to_owned(),
                    status,
                });
            }
        }
    }
    Ok(findings)
}

fn baseline_path(
    root: &Path,
    client_version: &str,
    server_era: ConformanceServerEra,
    lane: SemanticLane,
) -> PathBuf {
    root.join(baseline_relative_path(client_version, server_era, lane))
}

fn baseline_relative_path(
    client_version: &str,
    server_era: ConformanceServerEra,
    lane: SemanticLane,
) -> PathBuf {
    PathBuf::from(client_version)
        .join(server_era.label())
        .join(format!("{}.yml", lane.slug()))
}

fn read_baseline_optional(path: &Path) -> Result<Option<ConformanceBaseline>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                bail!("conformance baseline {path:?} must be a real file");
            }
            Ok(Some(read_baseline_with_metadata(path, &metadata)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).context(format!("failed to inspect conformance baseline {path:?}"))
        }
    }
}

fn read_baseline(path: &Path) -> Result<ConformanceBaseline> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect conformance baseline {path:?}"))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("conformance baseline {path:?} must be a real file");
    }
    read_baseline_with_metadata(path, &metadata)
}

fn read_baseline_with_metadata(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<ConformanceBaseline> {
    if metadata.len() > MAX_BASELINE_BYTES {
        bail!("conformance baseline {path:?} exceeds the {MAX_BASELINE_BYTES} byte safety limit");
    }
    let source =
        fs::read(path).with_context(|| format!("failed to read conformance baseline {path:?}"))?;
    let baseline: ConformanceBaseline = yaml_serde::from_slice(&source)
        .with_context(|| format!("failed to parse conformance baseline {path:?}"))?;
    if baseline.findings.windows(2).any(|pair| pair[0] >= pair[1]) {
        bail!("conformance baseline {path:?} findings must be strictly sorted without duplicates");
    }
    Ok(baseline)
}

fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    let mut entries = fs::read_dir(source)
        .with_context(|| format!("failed to read baseline directory {source:?}"))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to enumerate baseline directory {source:?}"))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect baseline entry {:?}", entry.path()))?;
        if file_type.is_symlink() {
            bail!("baseline tree must not contain symlink {:?}", entry.path());
        }
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            fs::create_dir(&target).with_context(|| {
                format!("failed to create staged baseline directory {target:?}")
            })?;
            copy_directory(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target).with_context(|| {
                format!(
                    "failed to copy baseline file {:?} to {target:?}",
                    entry.path()
                )
            })?;
        } else {
            bail!("unsupported baseline tree entry {:?}", entry.path());
        }
    }
    Ok(())
}

struct DirectoryGuard {
    path: PathBuf,
    armed: bool,
}

impl DirectoryGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DirectoryGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::results::{ConformanceCheck, ConformanceScenarioResult};

    fn check(id: &str, status: CheckStatus) -> ConformanceCheck {
        ConformanceCheck {
            id: id.to_owned(),
            name: None,
            description: None,
            status,
            timestamp: None,
            spec_references: Vec::new(),
            error_message: None,
            details: None,
            metadata: None,
            logs: None,
            extensions: BTreeMap::new(),
        }
    }

    fn results(scenario: &str, checks: Vec<ConformanceCheck>) -> ConformanceResults {
        ConformanceResults {
            scenarios: [(
                scenario.to_owned(),
                ConformanceScenarioResult {
                    scenario: scenario.to_owned(),
                    checks,
                    source: PathBuf::from("checks.json"),
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    fn write_document(
        root: &Path,
        client_version: &str,
        era: ConformanceServerEra,
        lane: SemanticLane,
        findings: Vec<ScoredFinding>,
    ) {
        let path = baseline_path(root, client_version, era, lane);
        fs::create_dir_all(path.parent().expect("baseline path parent"))
            .expect("baseline directory");
        fs::write(
            path,
            yaml_serde::to_string(&ConformanceBaseline { findings })
                .expect("baseline should serialize"),
        )
        .expect("baseline should be written");
    }

    #[test]
    fn routed_comparison_subtracts_findings_reproduced_by_direct_fixture() {
        let directory = tempfile::tempdir().expect("temporary baseline root");
        let root = directory.path();
        let client = "2026-07-28";
        let era = ConformanceServerEra::Modern;
        let shared = ScoredFinding {
            scenario: "tools-list".to_owned(),
            check: "shared".to_owned(),
            name: String::new(),
            status: ScoredStatus::Failure,
        };
        let routed = ScoredFinding {
            scenario: "tools-list".to_owned(),
            check: "routed".to_owned(),
            name: String::new(),
            status: ScoredStatus::Warning,
        };
        write_document(
            root,
            client,
            era,
            SemanticLane::FixtureDirect,
            vec![shared.clone()],
        );
        write_document(
            root,
            client,
            era,
            SemanticLane::ExternalDataPlane,
            vec![routed.clone()],
        );
        let actual = BTreeMap::from([
            (
                SemanticLane::FixtureDirect,
                results("tools-list", vec![check("shared", CheckStatus::Failure)]),
            ),
            (
                SemanticLane::ExternalDataPlane,
                results(
                    "tools-list",
                    vec![
                        check("shared", CheckStatus::Failure),
                        check("routed", CheckStatus::Warning),
                    ],
                ),
            ),
        ]);

        let evaluation = evaluate_baselines(
            &actual,
            &[SemanticLane::FixtureDirect, SemanticLane::ExternalDataPlane],
            root,
            client,
            era,
            false,
        )
        .expect("baselines should match after fixture subtraction");

        assert!(
            evaluation
                .comparisons
                .iter()
                .all(BaselineComparison::matches)
        );
        assert_eq!(evaluation.comparisons[1].actual, [routed]);
    }

    #[test]
    fn baseline_drift_reports_unexpected_and_stale_findings() {
        let directory = tempfile::tempdir().expect("temporary baseline root");
        let expected = ScoredFinding {
            scenario: "ping".to_owned(),
            check: "old".to_owned(),
            name: String::new(),
            status: ScoredStatus::Warning,
        };
        write_document(
            directory.path(),
            "2025-11-25",
            ConformanceServerEra::Legacy,
            SemanticLane::FixtureDirect,
            vec![expected.clone()],
        );
        let actual = BTreeMap::from([(
            SemanticLane::FixtureDirect,
            results("ping", vec![check("new", CheckStatus::Failure)]),
        )]);

        let evaluation = evaluate_baselines(
            &actual,
            &[SemanticLane::FixtureDirect],
            directory.path(),
            "2025-11-25",
            ConformanceServerEra::Legacy,
            false,
        )
        .expect("well-formed drift should be evaluated");

        let comparison = &evaluation.comparisons[0];
        assert!(!comparison.matches());
        assert_eq!(comparison.stale, [expected]);
        assert_eq!(comparison.unexpected[0].check, "new");
    }

    #[test]
    fn reused_specification_ids_are_disambiguated_by_check_name() {
        let mut first = check("shared-spec-id", CheckStatus::Failure);
        first.name = Some("FirstAssertion".to_owned());
        let mut second = check("shared-spec-id", CheckStatus::Failure);
        second.name = Some("SecondAssertion".to_owned());

        let findings = scored_findings(&results("header-validation", vec![first, second]))
            .expect("distinct named checks should be scoreable")
            .into_iter()
            .collect::<Vec<_>>();

        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].name, "FirstAssertion");
        assert_eq!(findings[1].name, "SecondAssertion");
    }

    #[test]
    fn repeated_official_checks_collapse_to_one_scored_finding() {
        let mut repeated = check("shared-spec-id", CheckStatus::Failure);
        repeated.name = Some("RepeatedAssertion".to_owned());

        let findings = scored_findings(&results(
            "header-validation",
            vec![repeated.clone(), repeated],
        ))
        .expect("repeated official checks should be scoreable")
        .into_iter()
        .collect::<Vec<_>>();

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].name, "RepeatedAssertion");
    }

    #[test]
    fn repository_contains_strict_baselines_for_every_default_matrix_lane() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/conformance/baselines");

        for era in [ConformanceServerEra::Legacy, ConformanceServerEra::Modern] {
            for lane in ALL_CONFORMANCE_LANES {
                let path = baseline_path(&root, "2026-07-28", era, lane);
                read_baseline(&path).unwrap_or_else(|error| {
                    panic!("default baseline {} must be valid: {error}", path.display())
                });
            }
        }
    }

    #[test]
    fn malformed_unsorted_and_unknown_status_baselines_fail_closed() {
        let directory = tempfile::tempdir().expect("temporary baseline root");
        let path = baseline_path(
            directory.path(),
            "2026-07-28",
            ConformanceServerEra::Modern,
            SemanticLane::FixtureDirect,
        );
        fs::create_dir_all(path.parent().expect("baseline parent")).expect("baseline directory");
        fs::write(
            &path,
            "findings:\n  - scenario: ping\n    check: z\n    name: z\n    status: WARNING\n  - scenario: ping\n    check: a\n    name: a\n    status: FAILURE\n",
        )
        .expect("malformed ordering should be written");
        assert!(read_baseline(&path).is_err());

        fs::write(
            &path,
            "findings:\n  - scenario: ping\n    check: a\n    name: a\n    status: SURPRISE\n",
        )
        .expect("unknown status should be written");
        assert!(read_baseline(&path).is_err());
    }

    #[test]
    fn unknown_official_status_and_missing_fixture_lane_fail_closed() {
        let unknown = BTreeMap::from([(
            SemanticLane::FixtureDirect,
            results(
                "ping",
                vec![check("future", CheckStatus::Other("FUTURE".to_owned()))],
            ),
        )]);
        let error = evaluate_baselines(
            &unknown,
            &[SemanticLane::FixtureDirect],
            Path::new("unused"),
            "2026-07-28",
            ConformanceServerEra::Dual,
            true,
        )
        .expect_err("unknown official statuses must fail")
        .to_string();
        assert!(error.contains("unknown official check status"));

        let routed = BTreeMap::from([(
            SemanticLane::ExternalDataPlane,
            results("ping", vec![check("ok", CheckStatus::Success)]),
        )]);
        let error = evaluate_baselines(
            &routed,
            &[SemanticLane::ExternalDataPlane],
            Path::new("unused"),
            "2026-07-28",
            ConformanceServerEra::Dual,
            true,
        )
        .expect_err("routed gating requires the direct fixture")
        .to_string();
        assert!(error.contains("missing fixture-direct lane"));
    }

    #[test]
    fn blessing_installs_version_era_lane_layout_and_preserves_other_files() {
        let directory = tempfile::tempdir().expect("temporary baseline parent");
        let root = directory.path().join("baselines");
        fs::create_dir(&root).expect("baseline root");
        fs::write(root.join("preserved.txt"), "keep\n").expect("preserved baseline file");
        let actual = BTreeMap::from([(
            SemanticLane::FixtureDirect,
            results("ping", vec![check("warning", CheckStatus::Warning)]),
        )]);
        let evaluation = evaluate_baselines(
            &actual,
            &[SemanticLane::FixtureDirect],
            &root,
            "2026-07-28",
            ConformanceServerEra::Legacy,
            true,
        )
        .expect("bless evaluation should allow a missing baseline");

        bless_baselines_transactionally(&root, &evaluation.updates)
            .expect("baseline transaction should commit");

        assert_eq!(
            fs::read_to_string(root.join("preserved.txt")).expect("preserved file"),
            "keep\n"
        );
        let installed = read_baseline(&baseline_path(
            &root,
            "2026-07-28",
            ConformanceServerEra::Legacy,
            SemanticLane::FixtureDirect,
        ))
        .expect("installed baseline should parse");
        assert_eq!(installed.findings[0].status, ScoredStatus::Warning);
    }

    #[test]
    fn failed_blessing_leaves_the_existing_tree_unchanged() {
        let directory = tempfile::tempdir().expect("temporary baseline parent");
        let root = directory.path().join("baselines");
        fs::create_dir(&root).expect("baseline root");
        fs::write(root.join("sentinel"), "original\n").expect("sentinel");
        let update = BaselineUpdate {
            relative_path: PathBuf::from("2026-07-28/dual/fixture-direct.yml"),
            document: ConformanceBaseline {
                findings: Vec::new(),
            },
        };

        let error = bless_baselines_transactionally(&root, &[update.clone(), update])
            .expect_err("duplicate updates must abort before commit")
            .to_string();

        assert!(error.contains("duplicate baseline update"));
        assert_eq!(
            fs::read_to_string(root.join("sentinel")).expect("original sentinel"),
            "original\n"
        );
        assert!(!root.join("2026-07-28").exists());
    }
}
