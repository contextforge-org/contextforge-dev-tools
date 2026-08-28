//! Official MCP conformance result and comparison support.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::infrastructure::process::CommandSpec;

use crate::conformance::fixture::OFFICIAL_CONFORMANCE_SERVER_ID;
pub use crate::conformance::profile::{DEFAULT_MCP_SPEC_VERSION, OFFICIAL_CONFORMANCE_PACKAGE};
use crate::conformance::profile::{
    LEGACY_MCP_SPEC_VERSION, OFFICIAL_CONFORMANCE_REPOSITORY, OFFICIAL_CONFORMANCE_REVISION,
    STABLE_MCP_SPEC_VERSION,
};

/// Default official server scenario suite.
pub const DEFAULT_CONFORMANCE_SUITE: &str = "all";

/// Exact provenance for the backing server used by an official run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConformanceFixtureMetadata {
    /// Upstream fixture repository.
    pub repository: String,
    /// Immutable upstream fixture revision.
    pub revision: String,
    /// Provisioned virtual-server identity.
    pub server_id: String,
}

/// Reproducibility metadata stored beside one official result set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConformanceRunMetadata {
    /// Pinned official runner package.
    pub oracle: String,
    /// Stack label exercised by this artifact.
    pub target: String,
    /// MCP protocol revision used by the official conformance client.
    pub client_version: String,
    /// Protocol era exposed by the upstream fixture server.
    pub server_era: ConformanceServerEra,
    /// Official scenario suite label.
    pub suite: String,
    /// Backing fixture provenance, absent on historical or caller-managed runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixture: Option<ConformanceFixtureMetadata>,
}

/// Protocol behavior exposed by the pinned upstream fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConformanceServerEra {
    /// Serve both initialization-based and per-request protocol behavior.
    Dual,
    /// Serve only initialization-based protocol behavior.
    Legacy,
    /// Serve only per-request protocol behavior.
    Modern,
}

impl ConformanceServerEra {
    /// Stable CLI, environment, metadata, and report label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Dual => "dual",
            Self::Legacy => "legacy",
            Self::Modern => "modern",
        }
    }

    /// Parses a stable artifact path component.
    #[must_use]
    pub fn from_label(value: &str) -> Option<Self> {
        match value {
            "dual" => Some(Self::Dual),
            "legacy" => Some(Self::Legacy),
            "modern" => Some(Self::Modern),
            _ => None,
        }
    }
}

impl fmt::Display for ConformanceServerEra {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Endpoint topology exercised by one official server-conformance run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SemanticLane {
    /// Official oracle connected directly to the pinned TypeScript fixture.
    FixtureDirect,
    /// Official oracle routed through the Python built-in data plane.
    BuiltInDataPlane,
    /// Official oracle routed through the external Rust data plane.
    ExternalDataPlane,
}

impl SemanticLane {
    /// Stable metadata and report label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::FixtureDirect => "fixture direct",
            Self::BuiltInDataPlane => "built-in data-plane route",
            Self::ExternalDataPlane => "external data-plane route",
        }
    }

    /// Stable artifact and baseline path component.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::FixtureDirect => "fixture-direct",
            Self::BuiltInDataPlane => "built-in-data-plane",
            Self::ExternalDataPlane => "external-data-plane",
        }
    }
}

impl fmt::Display for SemanticLane {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Semantic lane used by official conformance workflows.
pub type ConformanceTarget = SemanticLane;

/// Whether provenance identifies the exact pinned official TypeScript fixture.
#[must_use]
pub fn is_trusted_official_fixture(fixture: Option<&ConformanceFixtureMetadata>) -> bool {
    fixture.is_some_and(|fixture| {
        fixture.repository == OFFICIAL_CONFORMANCE_REPOSITORY
            && fixture.revision == OFFICIAL_CONFORMANCE_REVISION
            && fixture.server_id == OFFICIAL_CONFORMANCE_SERVER_ID
    })
}

// Exact server catalogs emitted by @modelcontextprotocol/conformance@0.2.0-alpha.11.
// Keep these coupled to OFFICIAL_CONFORMANCE_PACKAGE and verify the pin with
// the ignored package-backed test before updating either.
const SERVER_SCENARIOS_2025_06_18: [&str; 27] = [
    "server-initialize",
    "server-session-lifecycle",
    "logging-set-level",
    "ping",
    "completion-complete",
    "tools-list",
    "tools-call-simple-text",
    "tools-call-image",
    "tools-call-audio",
    "tools-call-embedded-resource",
    "tools-call-mixed-content",
    "tools-call-with-logging",
    "tools-call-error",
    "tools-call-with-progress",
    "tools-call-sampling",
    "tools-call-elicitation",
    "resources-list",
    "resources-read-text",
    "resources-read-binary",
    "resources-templates-read",
    "resources-subscribe",
    "resources-unsubscribe",
    "prompts-list",
    "prompts-get-simple",
    "prompts-get-with-args",
    "prompts-get-embedded-resource",
    "prompts-get-with-image",
];
const SERVER_ACTIVE_ADDITIONS_2025_11_25: [&str; 4] = [
    "elicitation-sep1034-defaults",
    "server-sse-multiple-streams",
    "elicitation-sep1330-enums",
    "dns-rebinding-protection",
];
const SERVER_PENDING_2025_11_25: [&str; 2] = ["json-schema-2020-12", "server-sse-polling"];
const SERVER_ACTIVE_2026_07_28: [&str; 20] = [
    "completion-complete",
    "dns-rebinding-protection",
    "prompts-get-embedded-resource",
    "prompts-get-simple",
    "prompts-get-with-args",
    "prompts-get-with-image",
    "prompts-list",
    "resources-list",
    "resources-read-binary",
    "resources-read-text",
    "resources-templates-read",
    "server-sse-multiple-streams",
    "tools-call-audio",
    "tools-call-embedded-resource",
    "tools-call-error",
    "tools-call-image",
    "tools-call-mixed-content",
    "tools-call-simple-text",
    "tools-call-with-progress",
    "tools-list",
];
const SERVER_ALL_ADDITIONS_2026_07_28: [&str; 20] = [
    "caching",
    "http-custom-header-server-validation",
    "http-header-validation",
    "input-required-result-basic-elicitation",
    "input-required-result-basic-list-roots",
    "input-required-result-basic-sampling",
    "input-required-result-capability-check",
    "input-required-result-ignore-extra-params",
    "input-required-result-missing-input-response",
    "input-required-result-multi-round",
    "input-required-result-multiple-input-requests",
    "input-required-result-non-tool-request",
    "input-required-result-request-state",
    "input-required-result-result-type",
    "input-required-result-tampered-state",
    "input-required-result-unsupported-methods",
    "input-required-result-validate-input",
    "json-schema-2020-12",
    "sep-2164-resource-not-found",
    "server-stateless",
];

const MAX_CHECKS_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Builds the exact official server-conformance invocation.
#[must_use = "a command specification does nothing until a process runner executes it"]
pub fn official_server_command(
    endpoint: &str,
    suite: &str,
    spec_version: &str,
    expected_failures: &Path,
    output_dir: &Path,
) -> CommandSpec {
    CommandSpec::new("npx")
        .clear_environment()
        .arg("-y")
        .arg(OFFICIAL_CONFORMANCE_PACKAGE)
        .arg("server")
        .arg("--url")
        .arg(endpoint)
        .arg("--suite")
        .arg(suite)
        .arg("--spec-version")
        .arg(spec_version)
        .arg("--expected-failures")
        .arg(expected_failures.as_os_str().to_owned())
        .arg("--output-dir")
        .arg(output_dir.as_os_str().to_owned())
        .arg("--verbose")
}

/// Typed official check status with forward-compatible preservation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CheckStatus {
    /// Required check passed.
    Success,
    /// Required check failed.
    Failure,
    /// Warning emitted by the official framework; compliant in summaries.
    Warning,
    /// Scenario or check was explicitly skipped.
    Skipped,
    /// Informational event; not applicable without a stronger status.
    Info,
    /// Status introduced by a newer official package.
    Other(String),
}

impl CheckStatus {
    fn from_wire(value: String) -> Self {
        match value.as_str() {
            "SUCCESS" => Self::Success,
            "FAILURE" => Self::Failure,
            "WARNING" => Self::Warning,
            "SKIPPED" => Self::Skipped,
            "INFO" => Self::Info,
            _ => Self::Other(value),
        }
    }

    fn as_wire(&self) -> &str {
        match self {
            Self::Success => "SUCCESS",
            Self::Failure => "FAILURE",
            Self::Warning => "WARNING",
            Self::Skipped => "SKIPPED",
            Self::Info => "INFO",
            Self::Other(value) => value,
        }
    }
}

impl Serialize for CheckStatus {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_wire())
    }
}

impl<'de> Deserialize<'de> for CheckStatus {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from_wire)
    }
}

/// An official MCP specification reference. Unknown fields are retained verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpecReference {
    /// Reference identifier emitted by the official framework.
    pub id: String,
    /// Optional source URL emitted by the official framework.
    #[serde(default)]
    pub url: Option<String>,
    /// Forward-compatible fields from the official framework.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// One check from an official `checks.json` file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConformanceCheck {
    /// Stable official check identifier.
    pub id: String,
    /// Human-readable check name.
    #[serde(default)]
    pub name: Option<String>,
    /// Human-readable check description.
    #[serde(default)]
    pub description: Option<String>,
    /// Typed official status.
    pub status: CheckStatus,
    /// Framework timestamp, preserved without interpretation.
    #[serde(default)]
    pub timestamp: Option<String>,
    /// Specification references, preserved without normalization.
    #[serde(rename = "specReferences", default)]
    pub spec_references: Vec<SpecReference>,
    /// Failure text, if supplied by the official framework.
    #[serde(rename = "errorMessage", default)]
    pub error_message: Option<String>,
    /// Scenario-specific details.
    #[serde(default)]
    pub details: Option<Value>,
    /// Scenario-specific metadata.
    #[serde(default)]
    pub metadata: Option<Value>,
    /// Scenario logs in their original JSON shape.
    #[serde(default)]
    pub logs: Option<Value>,
    /// Forward-compatible fields from the official framework.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// Result and provenance for one official server scenario.
#[derive(Debug, Clone, PartialEq)]
pub struct ConformanceScenarioResult {
    /// Validated scenario name extracted from the official result directory.
    pub scenario: String,
    /// Typed checks parsed from `checks.json`.
    pub checks: Vec<ConformanceCheck>,
    /// Relative path beneath the caller-provided result root.
    pub source: PathBuf,
}

impl ConformanceScenarioResult {
    /// Reduces official check statuses into a scenario-level comparison outcome.
    #[must_use]
    pub fn outcome(&self) -> ScenarioOutcome {
        let mut has_success = false;
        let mut has_warning = false;
        let mut has_skipped = false;
        let mut has_info = false;
        let mut has_unknown = false;
        let mut failures = Vec::new();

        for check in &self.checks {
            match check.status {
                CheckStatus::Failure => failures.push(check),
                CheckStatus::Warning => has_warning = true,
                CheckStatus::Success => has_success = true,
                CheckStatus::Skipped => has_skipped = true,
                CheckStatus::Other(_) => has_unknown = true,
                CheckStatus::Info => has_info = true,
            }
        }

        if !failures.is_empty()
            && failures.iter().all(|check| {
                check
                    .error_message
                    .as_deref()
                    .is_some_and(is_missing_official_fixture)
            })
        {
            ScenarioOutcome::FixtureFailure
        } else if !failures.is_empty() {
            ScenarioOutcome::NonCompliant
        } else if has_unknown {
            ScenarioOutcome::Ambiguous
        } else if has_success || has_warning {
            ScenarioOutcome::Compliant
        } else if has_skipped || has_info {
            ScenarioOutcome::NotApplicable
        } else {
            ScenarioOutcome::Ambiguous
        }
    }

    /// Reduces the outcome using trusted pinned-fixture provenance.
    ///
    /// A fixture-shaped not-found result from a trusted fixture is attributed
    /// to the gateway path as a compliance failure. Unknown or caller-managed
    /// fixtures retain the historical fixture-failure outcome.
    #[must_use]
    pub fn outcome_with_trusted_fixture(&self, trusted_fixture: bool) -> ScenarioOutcome {
        match (self.outcome(), trusted_fixture) {
            (ScenarioOutcome::FixtureFailure, true) => ScenarioOutcome::NonCompliant,
            (outcome, _) => outcome,
        }
    }
}

fn is_missing_official_fixture(message: &str) -> bool {
    const EXACT_MARKERS: [&str; 8] = [
        "Tool not found: test_",
        "Tool 'json_schema_2020_12_tool' not found",
        "Prompt not found: test_",
        "Resource not found: test://",
        "Routing problem... wrong tool name",
        "Routing problem... wrong prompt name",
        "Routing problem... wrong resource name",
        "Routing problem... wrong completion reference",
    ];

    EXACT_MARKERS.iter().any(|marker| message.contains(marker))
}

/// Deterministically indexed official server results.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConformanceResults {
    /// Scenario name to parsed result.
    pub scenarios: BTreeMap<String, ConformanceScenarioResult>,
}

/// Returns the exact pinned server scenario set for one supported suite/spec pair.
///
/// # Errors
///
/// Returns an error for a suite or specification revision without a catalog
/// verified against [`OFFICIAL_CONFORMANCE_PACKAGE`].
pub fn expected_server_scenarios(
    suite: &str,
    spec_version: &str,
) -> Result<BTreeSet<&'static str>> {
    if !matches!(suite, "active" | "all") {
        bail!("unsupported official server suite {suite:?}; expected active or all");
    }
    match spec_version {
        LEGACY_MCP_SPEC_VERSION => Ok(SERVER_SCENARIOS_2025_06_18.into_iter().collect()),
        STABLE_MCP_SPEC_VERSION => {
            let mut scenarios = SERVER_SCENARIOS_2025_06_18
                .into_iter()
                .collect::<BTreeSet<_>>();
            scenarios.extend(SERVER_ACTIVE_ADDITIONS_2025_11_25);
            if suite == "all" {
                scenarios.extend(SERVER_PENDING_2025_11_25);
            }
            Ok(scenarios)
        }
        DEFAULT_MCP_SPEC_VERSION => {
            let mut scenarios = SERVER_ACTIVE_2026_07_28
                .into_iter()
                .collect::<BTreeSet<_>>();
            if suite == "all" {
                scenarios.extend(SERVER_ALL_ADDITIONS_2026_07_28);
            }
            Ok(scenarios)
        }
        _ => Err(anyhow::anyhow!(
            "no verified {OFFICIAL_CONFORMANCE_PACKAGE} server scenario catalog for specification {spec_version:?}"
        )),
    }
}

/// Requires parsed results to exactly cover the pinned suite/spec scenario set.
///
/// # Errors
///
/// Returns an error listing missing or unexpected scenarios when a child run
/// stopped early or the pinned package catalog changed.
pub fn validate_server_scenario_set(
    results: &ConformanceResults,
    suite: &str,
    spec_version: &str,
) -> Result<()> {
    let expected = expected_server_scenarios(suite, spec_version)?;
    let actual = results
        .scenarios
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let missing = expected.difference(&actual).copied().collect::<Vec<_>>();
    let unexpected = actual.difference(&expected).copied().collect::<Vec<_>>();
    let empty_checks = results
        .scenarios
        .iter()
        .filter_map(|(scenario, result)| result.checks.is_empty().then_some(scenario.as_str()))
        .collect::<Vec<_>>();
    if !missing.is_empty() || !unexpected.is_empty() || !empty_checks.is_empty() {
        bail!(
            "official conformance scenario set is incomplete for suite {suite:?} and specification {spec_version:?}; missing={missing:?}; unexpected={unexpected:?}; empty_checks={empty_checks:?}"
        );
    }
    Ok(())
}

/// Recursively parses official `server-*/checks.json` result files.
///
/// Symlinks are never followed, scenario directory names are strictly validated,
/// and stored provenance is relative to `root`.
pub fn load_server_results(root: &Path) -> Result<ConformanceResults> {
    let root = fs::canonicalize(root)
        .with_context(|| format!("failed to resolve conformance results root {root:?}"))?;
    if !root.is_dir() {
        bail!("conformance results root {root:?} is not a directory");
    }

    let mut check_files = Vec::new();
    collect_check_files(&root, &mut check_files)?;
    check_files.sort();

    let mut scenarios = BTreeMap::new();
    for path in check_files {
        let parent = path
            .parent()
            .context("checks.json result has no parent directory")?;
        let directory_name = parent
            .file_name()
            .and_then(OsStr::to_str)
            .context("official result directory name is not valid UTF-8")?;
        if !directory_name.starts_with("server-") {
            continue;
        }
        let scenario = scenario_from_result_directory(directory_name)?;
        let metadata = fs::metadata(&path)
            .with_context(|| format!("failed to inspect official result file {path:?}"))?;
        if metadata.len() > MAX_CHECKS_FILE_BYTES {
            bail!(
                "official result file {path:?} exceeds the {} byte safety limit",
                MAX_CHECKS_FILE_BYTES
            );
        }
        let source_bytes = fs::read(&path)
            .with_context(|| format!("failed to read official result file {path:?}"))?;
        let checks: Vec<ConformanceCheck> = serde_json::from_slice(&source_bytes)
            .with_context(|| format!("failed to parse official result file {path:?}"))?;
        let source = path
            .strip_prefix(&root)
            .context("official result escaped the result root")?
            .to_owned();
        let result = ConformanceScenarioResult {
            scenario: scenario.clone(),
            checks,
            source,
        };
        if scenarios.insert(scenario.clone(), result).is_some() {
            bail!("duplicate official result for scenario {scenario:?}");
        }
    }

    Ok(ConformanceResults { scenarios })
}

fn collect_check_files(directory: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(directory)
        .with_context(|| format!("failed to read conformance result directory {directory:?}"))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| {
            format!("failed to enumerate conformance result directory {directory:?}")
        })?;
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let file_type = entry.file_type().with_context(|| {
            format!(
                "failed to inspect conformance result entry {:?}",
                entry.path()
            )
        })?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_check_files(&entry.path(), output)?;
        } else if file_type.is_file() && entry.file_name() == OsStr::new("checks.json") {
            output.push(entry.path());
        }
    }
    Ok(())
}

fn scenario_from_result_directory(directory: &str) -> Result<String> {
    let rest = directory
        .strip_prefix("server-")
        .context("official result directory must begin with server-")?;
    if rest.len() <= 25 {
        bail!("official result directory {directory:?} has no scenario or timestamp");
    }
    let split = rest.len() - 25;
    if rest.as_bytes().get(split) != Some(&b'-') {
        bail!("official result directory {directory:?} has an invalid timestamp separator");
    }
    let scenario = &rest[..split];
    let timestamp = &rest[split + 1..];
    if !is_normalized_iso_timestamp(timestamp) {
        bail!("official result directory {directory:?} has an invalid normalized timestamp");
    }
    validate_scenario_name(scenario)
        .with_context(|| format!("invalid scenario in official result directory {directory:?}"))?;
    Ok(scenario.to_owned())
}

fn is_normalized_iso_timestamp(value: &str) -> bool {
    if value.len() != 24 || !value.is_ascii() {
        return false;
    }
    let bytes = value.as_bytes();
    for (index, expected) in [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b'-'),
        (16, b'-'),
        (19, b'-'),
        (23, b'Z'),
    ] {
        if bytes[index] != expected {
            return false;
        }
    }
    bytes.iter().enumerate().all(|(index, byte)| {
        matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
    })
}

fn validate_scenario_name(scenario: &str) -> Result<()> {
    if scenario.is_empty() {
        bail!("scenario name is empty");
    }
    if scenario.contains("..") {
        bail!("scenario name contains a parent-directory segment");
    }
    if scenario.starts_with(['-', '.', '/', '_'])
        || scenario.ends_with(['-', '.', '/', '_'])
        || scenario.contains("//")
    {
        bail!("scenario name has an invalid boundary or path segment");
    }
    if !scenario
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
    {
        bail!("scenario name contains unsupported characters");
    }
    Ok(())
}

/// Scenario-level outcome used for direct and routed comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScenarioOutcome {
    /// No failures or unknown statuses; at least one success or warning was observed.
    Compliant,
    /// At least one official failure was observed.
    NonCompliant,
    /// Fixture setup failed; implementation compliance is not established.
    FixtureFailure,
    /// Checks only skipped or described a not-applicable scenario.
    NotApplicable,
    /// Results contain an unknown status or no checks.
    Ambiguous,
    /// No result was produced for this side.
    Missing,
}

impl ScenarioOutcome {
    /// Stable report label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Compliant => "compliant",
            Self::NonCompliant => "failure",
            Self::FixtureFailure => "fixture failure",
            Self::NotApplicable => "not applicable",
            Self::Ambiguous => "ambiguous",
            Self::Missing => "missing",
        }
    }
}

/// Required three-way comparison report classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ComparisonClassification {
    /// Direct and both routed paths are compliant.
    AllCompliant,
    /// Only the direct fixture run fails.
    FixtureOnlyFailure,
    /// Only the built-in data-plane route fails.
    BuiltInDataPlaneOnlyFailure,
    /// Only the external data-plane route fails.
    ExternalDataPlaneOnlyFailure,
    /// The direct fixture and built-in data-plane route fail.
    FixtureAndBuiltInDataPlaneFailure,
    /// The direct fixture and external data-plane route fail.
    FixtureAndExternalDataPlaneFailure,
    /// Both gateway paths fail while the direct fixture passes.
    GatewaysOnlyFailure,
    /// Direct and both routed paths fail.
    SharedFailure,
    /// Fixture setup prevented a compliance result.
    FixtureFailure,
    /// Neither path applies.
    NotApplicable,
    /// Missing, unknown, or internally inconsistent evidence.
    Ambiguous,
}

impl ComparisonClassification {
    const ALL: [Self; 11] = [
        Self::AllCompliant,
        Self::FixtureOnlyFailure,
        Self::BuiltInDataPlaneOnlyFailure,
        Self::ExternalDataPlaneOnlyFailure,
        Self::FixtureAndBuiltInDataPlaneFailure,
        Self::FixtureAndExternalDataPlaneFailure,
        Self::GatewaysOnlyFailure,
        Self::SharedFailure,
        Self::FixtureFailure,
        Self::NotApplicable,
        Self::Ambiguous,
    ];

    /// Stable report label matching the compliance-report vocabulary.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::AllCompliant => "all compliant",
            Self::FixtureOnlyFailure => "fixture-only failure",
            Self::BuiltInDataPlaneOnlyFailure => "built-in data-plane only failure",
            Self::ExternalDataPlaneOnlyFailure => "external data-plane only failure",
            Self::FixtureAndBuiltInDataPlaneFailure => "fixture + built-in data-plane failure",
            Self::FixtureAndExternalDataPlaneFailure => "fixture + external data-plane failure",
            Self::GatewaysOnlyFailure => "both gateways only failure",
            Self::SharedFailure => "shared failure",
            Self::FixtureFailure => "fixture failure",
            Self::NotApplicable => "not applicable",
            Self::Ambiguous => "ambiguous",
        }
    }
}

/// Classifies direct-fixture, built-in, and external data-plane route outcomes.
#[must_use]
pub fn classify_outcomes(
    fixture: ScenarioOutcome,
    built_in_data_plane: ScenarioOutcome,
    external_data_plane: ScenarioOutcome,
) -> ComparisonClassification {
    use ScenarioOutcome::{Ambiguous, FixtureFailure, Missing, NonCompliant, NotApplicable};

    if matches!(fixture, FixtureFailure)
        || matches!(built_in_data_plane, FixtureFailure)
        || matches!(external_data_plane, FixtureFailure)
    {
        return ComparisonClassification::FixtureFailure;
    }
    if matches!(fixture, Ambiguous | Missing)
        || matches!(built_in_data_plane, Ambiguous | Missing)
        || matches!(external_data_plane, Ambiguous | Missing)
    {
        return ComparisonClassification::Ambiguous;
    }
    if fixture == NotApplicable
        && built_in_data_plane == NotApplicable
        && external_data_plane == NotApplicable
    {
        return ComparisonClassification::NotApplicable;
    }

    match (
        fixture == NonCompliant,
        built_in_data_plane == NonCompliant,
        external_data_plane == NonCompliant,
    ) {
        (false, false, false) => ComparisonClassification::AllCompliant,
        (true, false, false) => ComparisonClassification::FixtureOnlyFailure,
        (false, true, false) => ComparisonClassification::BuiltInDataPlaneOnlyFailure,
        (false, false, true) => ComparisonClassification::ExternalDataPlaneOnlyFailure,
        (true, true, false) => ComparisonClassification::FixtureAndBuiltInDataPlaneFailure,
        (true, false, true) => ComparisonClassification::FixtureAndExternalDataPlaneFailure,
        (false, true, true) => ComparisonClassification::GatewaysOnlyFailure,
        (true, true, true) => ComparisonClassification::SharedFailure,
    }
}

/// One scenario row in the deterministic comparison report.
#[derive(Debug, Clone, PartialEq)]
pub struct ScenarioComparison {
    /// Official scenario name.
    pub scenario: String,
    /// Direct official fixture result.
    pub fixture: ScenarioOutcome,
    /// Raw failed checks in the direct fixture result.
    pub fixture_failed_checks: usize,
    /// Built-in data-plane result.
    pub built_in_data_plane: ScenarioOutcome,
    /// Raw failed checks in the built-in data-plane result.
    pub built_in_data_plane_failed_checks: usize,
    /// External data-plane result.
    pub external_data_plane: ScenarioOutcome,
    /// Raw failed checks in the external data-plane result.
    pub external_data_plane_failed_checks: usize,
    /// Reduced report classification.
    pub classification: ComparisonClassification,
    /// Raw official references from both result sets.
    pub spec_references: Vec<SpecReference>,
}

/// Per-target provenance trust used when reducing fixture-shaped failures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ComparisonFixtureTrust {
    /// Direct fixture provenance is the exact pinned source revision.
    pub fixture: bool,
    /// Built-in data-plane fixture provenance is the exact pinned source revision.
    pub built_in_data_plane: bool,
    /// External data-plane fixture provenance is the exact pinned source revision.
    pub external_data_plane: bool,
}

/// Compares results while independently attributing trusted fixture failures.
///
/// A trusted side converts fixture-shaped not-found results into ordinary
/// implementation failures. An untrusted side preserves historical behavior.
#[must_use]
pub fn compare_result_sets_with_fixture_trust(
    fixture: &ConformanceResults,
    built_in_data_plane: &ConformanceResults,
    external_data_plane: &ConformanceResults,
    trust: ComparisonFixtureTrust,
) -> Vec<ScenarioComparison> {
    let mut scenarios: BTreeSet<_> = fixture.scenarios.keys().cloned().collect();
    scenarios.extend(built_in_data_plane.scenarios.keys().cloned());
    scenarios.extend(external_data_plane.scenarios.keys().cloned());

    scenarios
        .into_iter()
        .map(|scenario| {
            let fixture_result = fixture.scenarios.get(&scenario);
            let built_in_result = built_in_data_plane.scenarios.get(&scenario);
            let external_result = external_data_plane.scenarios.get(&scenario);
            let fixture_outcome = fixture_result
                .map(|result| result.outcome_with_trusted_fixture(trust.fixture))
                .unwrap_or(ScenarioOutcome::Missing);
            let built_in_outcome = built_in_result
                .map(|result| result.outcome_with_trusted_fixture(trust.built_in_data_plane))
                .unwrap_or(ScenarioOutcome::Missing);
            let external_outcome = external_result
                .map(|result| result.outcome_with_trusted_fixture(trust.external_data_plane))
                .unwrap_or(ScenarioOutcome::Missing);

            let mut spec_references = Vec::new();
            if let Some(result) = fixture_result {
                append_references(&mut spec_references, result);
            }
            if let Some(result) = built_in_result {
                append_references(&mut spec_references, result);
            }
            if let Some(result) = external_result {
                append_references(&mut spec_references, result);
            }
            sort_and_deduplicate_references(&mut spec_references);

            ScenarioComparison {
                scenario,
                fixture: fixture_outcome,
                fixture_failed_checks: failed_check_count(fixture_result),
                built_in_data_plane: built_in_outcome,
                built_in_data_plane_failed_checks: failed_check_count(built_in_result),
                external_data_plane: external_outcome,
                external_data_plane_failed_checks: failed_check_count(external_result),
                classification: classify_outcomes(
                    fixture_outcome,
                    built_in_outcome,
                    external_outcome,
                ),
                spec_references,
            }
        })
        .collect()
}

fn failed_check_count(result: Option<&ConformanceScenarioResult>) -> usize {
    result.map_or(0, |result| {
        result
            .checks
            .iter()
            .filter(|check| check.status == CheckStatus::Failure)
            .count()
    })
}

fn append_references(output: &mut Vec<SpecReference>, result: &ConformanceScenarioResult) {
    for check in &result.checks {
        output.extend(check.spec_references.iter().cloned());
    }
}

fn sort_and_deduplicate_references(references: &mut Vec<SpecReference>) {
    references.sort_by_cached_key(reference_key);
    references.dedup();
}

fn reference_key(reference: &SpecReference) -> String {
    serde_json::to_string(reference).unwrap_or_else(|_| {
        format!(
            "{}\u{0}{}",
            reference.id,
            reference.url.as_deref().unwrap_or_default()
        )
    })
}

/// Inputs for a deterministic Markdown comparison report.
#[derive(Debug, Clone, PartialEq)]
pub struct ComparisonReport {
    /// MCP protocol version used by the official client in all result sets.
    pub client_version: String,
    /// Protocol era exposed by the upstream fixture in all result sets.
    pub server_era: ConformanceServerEra,
    /// Official scenario suite exercised by all result sets.
    pub suite: String,
    /// Exact official fixture provenance when recorded by the run.
    pub fixture: Option<ConformanceFixtureMetadata>,
    /// Scenario comparisons in any order; rendering sorts them.
    pub scenarios: Vec<ScenarioComparison>,
}

/// Renders a deterministic, untrusted-input-safe Markdown comparison report.
#[must_use]
pub fn render_comparison_markdown(report: &ComparisonReport) -> String {
    let mut output = String::new();
    output.push_str("# MCP Conformance Comparison\n\n");
    output.push_str(&format!(
        "- Official oracle: `{}`\n- Client specification: `{}`\n- Upstream server era: `{}`\n- Suite: `{}`\n",
        OFFICIAL_CONFORMANCE_PACKAGE,
        markdown_code(&report.client_version),
        report.server_era,
        markdown_code(&report.suite)
    ));
    if let Some(fixture) = report.fixture.as_ref() {
        output.push_str(&format!(
            "- Fixture source: `{}` at `{}`\n",
            markdown_code(&fixture.repository),
            markdown_code(&fixture.revision)
        ));
    }
    output.push('\n');

    let mut counts = BTreeMap::new();
    for scenario in &report.scenarios {
        *counts.entry(scenario.classification).or_insert(0_usize) += 1;
    }
    output.push_str("## Target outcomes\n\n");
    output.push_str("| Target | Compliant scenarios | Failed scenarios | Failed checks | Fixture failures | Not applicable | Ambiguous | Missing |\n");
    output.push_str("|---|---:|---:|---:|---:|---:|---:|---:|\n");
    type Outcome = fn(&ScenarioComparison) -> ScenarioOutcome;
    type FailedChecks = fn(&ScenarioComparison) -> usize;
    let target_outcomes: [(&str, Outcome, FailedChecks); 3] = [
        (
            "Fixture direct",
            |scenario: &ScenarioComparison| scenario.fixture,
            |scenario: &ScenarioComparison| scenario.fixture_failed_checks,
        ),
        (
            "Built-in data-plane route",
            |scenario: &ScenarioComparison| scenario.built_in_data_plane,
            |scenario: &ScenarioComparison| scenario.built_in_data_plane_failed_checks,
        ),
        (
            "External data-plane route",
            |scenario: &ScenarioComparison| scenario.external_data_plane,
            |scenario: &ScenarioComparison| scenario.external_data_plane_failed_checks,
        ),
    ];
    for (label, outcome, failed_checks) in target_outcomes {
        let mut counts = BTreeMap::new();
        for scenario in &report.scenarios {
            *counts.entry(outcome(scenario)).or_insert(0_usize) += 1;
        }
        let failed_checks = report.scenarios.iter().map(failed_checks).sum::<usize>();
        output.push_str(&format!(
            "| {label} | {} | {} | {failed_checks} | {} | {} | {} | {} |\n",
            counts
                .get(&ScenarioOutcome::Compliant)
                .copied()
                .unwrap_or_default(),
            counts
                .get(&ScenarioOutcome::NonCompliant)
                .copied()
                .unwrap_or_default(),
            counts
                .get(&ScenarioOutcome::FixtureFailure)
                .copied()
                .unwrap_or_default(),
            counts
                .get(&ScenarioOutcome::NotApplicable)
                .copied()
                .unwrap_or_default(),
            counts
                .get(&ScenarioOutcome::Ambiguous)
                .copied()
                .unwrap_or_default(),
            counts
                .get(&ScenarioOutcome::Missing)
                .copied()
                .unwrap_or_default(),
        ));
    }

    output.push_str("\n## Comparison summary\n\n");
    output.push_str("| Classification | Scenarios |\n|---|---:|\n");
    for classification in ComparisonClassification::ALL {
        output.push_str(&format!(
            "| {} | {} |\n",
            classification.label(),
            counts.get(&classification).copied().unwrap_or_default()
        ));
    }

    output.push_str("\n## Scenarios\n\n");
    output.push_str(
        "| Scenario | Fixture direct | Built-in data-plane route | External data-plane route | Classification | Specification references |\n",
    );
    output.push_str("|---|---|---|---|---|---|\n");
    let mut scenarios: Vec<_> = report.scenarios.iter().collect();
    scenarios.sort_by(|left, right| left.scenario.cmp(&right.scenario));
    for scenario in scenarios {
        let references = if scenario.spec_references.is_empty() {
            "—".to_owned()
        } else {
            scenario
                .spec_references
                .iter()
                .map(render_reference)
                .collect::<Vec<_>>()
                .join("<br>")
        };
        output.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            markdown_cell(&scenario.scenario),
            scenario.fixture.label(),
            scenario.built_in_data_plane.label(),
            scenario.external_data_plane.label(),
            scenario.classification.label(),
            references
        ));
    }
    output
}

/// Writes a deterministic Markdown comparison report.
pub fn write_comparison_report(path: &Path, report: &ComparisonReport) -> Result<()> {
    create_parent_directory(path)?;
    fs::write(path, render_comparison_markdown(report))
        .with_context(|| format!("failed to write conformance comparison report {path:?}"))
}

fn render_reference(reference: &SpecReference) -> String {
    let id = markdown_cell(&reference.id);
    match reference.url.as_deref() {
        Some(url) if is_safe_markdown_url(url) => format!("[{id}]({url})"),
        Some(_) => format!("{id} (unsafe URL omitted)"),
        None => id,
    }
}

fn is_safe_markdown_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && !url
            .bytes()
            .any(|byte| byte.is_ascii_control() || matches!(byte, b'<' | b'>' | b'(' | b')' | b'|'))
}

fn markdown_cell(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace('\r', "")
        .replace('\n', "<br>")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn markdown_code(value: &str) -> String {
    value.replace('`', "\\`").replace(['\r', '\n'], " ")
}

fn create_parent_directory(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent directory {parent:?}"))?;
    }
    Ok(())
}
