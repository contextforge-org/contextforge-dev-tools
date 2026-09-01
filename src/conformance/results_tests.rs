use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use cf_integration::conformance::fixture::{
    OFFICIAL_CONFORMANCE_REPOSITORY, OFFICIAL_CONFORMANCE_REVISION, OFFICIAL_CONFORMANCE_SERVER_ID,
};
use cf_integration::conformance::results::{
    CheckStatus, ComparisonClassification, ComparisonReport, ConformanceCheck,
    ConformanceDirection, ConformanceFixtureMetadata, ConformanceResults, ConformanceRunMetadata,
    ConformanceScenarioResult, ConformanceServerEra, DEFAULT_CLIENT_CONFORMANCE_SCENARIOS,
    DEFAULT_CONFORMANCE_SUITE, DEFAULT_MCP_SPEC_VERSION, OFFICIAL_CONFORMANCE_PACKAGE,
    ScenarioComparison, ScenarioOutcome, SemanticLane, SpecReference, classify_outcomes,
    compare_result_sets, expected_client_scenarios, expected_server_scenarios,
    is_trusted_official_fixture, load_client_results, load_server_results, official_client_command,
    official_server_command, render_comparison_markdown, validate_client_scenario_set,
    validate_server_scenario_set, write_comparison_report,
};

const SPEC_REFERENCE: &str =
    "https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle#initialization";

#[test]
fn semantic_lanes_have_one_shared_stable_vocabulary() {
    assert_eq!(SemanticLane::FixtureDirect.label(), "fixture direct");
    assert_eq!(SemanticLane::BuiltInDataPlane.label(), "built-in dataplane");
    assert_eq!(
        SemanticLane::ExternalDataPlane.label(),
        "external dataplane"
    );
}

#[test]
fn fixture_server_eras_report_their_exact_protocol_sets() {
    assert_eq!(
        ConformanceServerEra::Legacy.protocol_versions(),
        &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"]
    );
    assert_eq!(
        ConformanceServerEra::Modern.protocol_versions(),
        &["2026-07-28"]
    );
    assert_eq!(
        ConformanceServerEra::Dual.protocol_versions(),
        &[
            "2024-11-05",
            "2025-03-26",
            "2025-06-18",
            "2025-11-25",
            "2026-07-28"
        ]
    );
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture_metadata() -> ConformanceFixtureMetadata {
    ConformanceFixtureMetadata {
        repository: OFFICIAL_CONFORMANCE_REPOSITORY.to_owned(),
        revision: OFFICIAL_CONFORMANCE_REVISION.to_owned(),
        server_id: OFFICIAL_CONFORMANCE_SERVER_ID.to_owned(),
    }
}

fn check(id: &str, status: CheckStatus) -> ConformanceCheck {
    ConformanceCheck {
        id: id.to_owned(),
        name: Some(id.to_owned()),
        description: Some(format!("check {id}")),
        status,
        timestamp: Some("2026-07-10T12:34:56.000Z".to_owned()),
        spec_references: vec![SpecReference {
            id: "MCP-Lifecycle".to_owned(),
            url: Some(SPEC_REFERENCE.to_owned()),
            extensions: BTreeMap::new(),
        }],
        error_message: None,
        details: None,
        metadata: None,
        logs: None,
        extensions: BTreeMap::new(),
    }
}

fn result(
    scenario: &str,
    statuses: impl IntoIterator<Item = CheckStatus>,
) -> ConformanceScenarioResult {
    ConformanceScenarioResult {
        scenario: scenario.to_owned(),
        checks: statuses
            .into_iter()
            .enumerate()
            .map(|(index, status)| check(&format!("check-{index}"), status))
            .collect(),
        source: PathBuf::from(format!("server-{scenario}-timestamp/checks.json")),
    }
}

fn results(entries: impl IntoIterator<Item = ConformanceScenarioResult>) -> ConformanceResults {
    ConformanceResults {
        scenarios: entries
            .into_iter()
            .map(|entry| (entry.scenario.clone(), entry))
            .collect(),
    }
}

#[test]
fn metadata_roundtrips_exact_fixture_provenance() {
    let metadata = ConformanceRunMetadata {
        oracle: OFFICIAL_CONFORMANCE_PACKAGE.to_owned(),
        target: "control-plane".to_owned(),
        direction: ConformanceDirection::Server,
        client_version: DEFAULT_MCP_SPEC_VERSION.to_owned(),
        server_era: ConformanceServerEra::Legacy,
        suite: DEFAULT_CONFORMANCE_SUITE.to_owned(),
        fixture: fixture_metadata(),
    };

    let serialized = serde_json::to_vec(&metadata).expect("metadata should serialize");
    let roundtrip: ConformanceRunMetadata =
        serde_json::from_slice(&serialized).expect("metadata should deserialize");

    assert_eq!(roundtrip, metadata);
    assert_eq!(roundtrip.server_era, ConformanceServerEra::Legacy);
    assert!(is_trusted_official_fixture(&roundtrip.fixture));
}

#[test]
fn metadata_without_an_explicit_server_era_is_rejected() {
    let error = serde_json::from_value::<ConformanceRunMetadata>(serde_json::json!({
        "oracle": OFFICIAL_CONFORMANCE_PACKAGE,
        "target": "fixture direct",
        "direction": "server",
        "client_version": "2025-11-25",
        "suite": "all",
        "fixture": fixture_metadata()
    }))
    .expect_err("metadata must identify the server era")
    .to_string();

    assert!(error.contains("server_era"));
}

#[test]
fn metadata_without_an_explicit_direction_is_rejected() {
    let error = serde_json::from_value::<ConformanceRunMetadata>(serde_json::json!({
        "oracle": OFFICIAL_CONFORMANCE_PACKAGE,
        "target": "fixture direct",
        "client_version": "2026-07-28",
        "server_era": "modern",
        "suite": "all",
        "fixture": fixture_metadata()
    }))
    .expect_err("metadata must identify the conformance direction")
    .to_string();

    assert!(error.contains("direction"));
}

#[test]
fn metadata_without_fixture_provenance_is_rejected() {
    let error = serde_json::from_value::<ConformanceRunMetadata>(serde_json::json!({
        "oracle": OFFICIAL_CONFORMANCE_PACKAGE,
        "target": "fixture direct",
        "direction": "server",
        "client_version": "2025-11-25",
        "server_era": "legacy",
        "suite": "all"
    }))
    .expect_err("fixture provenance is mandatory")
    .to_string();

    assert!(error.contains("fixture"));
}

#[test]
fn fixture_trust_requires_every_pinned_identity() {
    let exact = fixture_metadata();
    assert!(is_trusted_official_fixture(&exact));
    for mismatch in [
        ConformanceFixtureMetadata {
            repository: "https://example.test/untrusted".to_owned(),
            ..exact.clone()
        },
        ConformanceFixtureMetadata {
            revision: "untrusted-revision".to_owned(),
            ..exact.clone()
        },
        ConformanceFixtureMetadata {
            server_id: "untrusted-server".to_owned(),
            ..exact
        },
    ] {
        assert!(!is_trusted_official_fixture(&mismatch));
    }
}

#[test]
fn pinned_server_scenario_catalog_has_exact_suite_differences() {
    let stable_active = expected_server_scenarios("active", "2025-11-25")
        .expect("active scenario catalog should be pinned");
    let stable_all = expected_server_scenarios("all", "2025-11-25")
        .expect("all scenario catalog should be pinned");
    let previous = expected_server_scenarios("all", "2025-06-18")
        .expect("previous stable scenario catalog should be pinned");
    let draft_active = expected_server_scenarios("active", "2026-07-28")
        .expect("draft active scenario catalog should be pinned");
    let draft_all = expected_server_scenarios("all", "2026-07-28")
        .expect("draft all scenario catalog should be pinned");

    assert_eq!(stable_active.len(), 31);
    assert_eq!(stable_all.len(), 33);
    assert_eq!(previous.len(), 27);
    assert_eq!(draft_active.len(), 20);
    assert_eq!(draft_all.len(), 40);
    assert_eq!(
        stable_all
            .difference(&stable_active)
            .copied()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["json-schema-2020-12", "server-sse-polling"])
    );
    assert!(draft_all.contains("server-stateless"));
    assert!(!draft_all.contains("server-initialize"));
}

#[test]
fn incomplete_scenario_catalog_is_rejected() {
    let partial = results([result("server-initialize", [CheckStatus::Failure])]);
    let error = validate_server_scenario_set(&partial, "active", "2025-11-25")
        .expect_err("one parsed scenario cannot prove the suite completed");
    assert!(error.to_string().contains("missing="));
}

#[test]
fn official_command_is_pinned_complete_and_ordered() {
    let spec = official_server_command(
        "http://127.0.0.1:49152/mcp",
        DEFAULT_CONFORMANCE_SUITE,
        DEFAULT_MCP_SPEC_VERSION,
        Path::new("expected-failures.yml"),
        Path::new("results"),
    );

    assert_eq!(
        OFFICIAL_CONFORMANCE_PACKAGE,
        "@modelcontextprotocol/conformance@0.2.0-alpha.11"
    );
    assert_eq!(DEFAULT_MCP_SPEC_VERSION, "2026-07-28");
    assert!(!spec.inherits_environment());
    assert_eq!(
        spec.arguments(),
        &[
            OsString::from("-y"),
            OsString::from(OFFICIAL_CONFORMANCE_PACKAGE),
            OsString::from("server"),
            OsString::from("--url"),
            OsString::from("http://127.0.0.1:49152/mcp"),
            OsString::from("--suite"),
            OsString::from("all"),
            OsString::from("--spec-version"),
            OsString::from("2026-07-28"),
            OsString::from("--expected-failures"),
            OsString::from("expected-failures.yml"),
            OsString::from("--output-dir"),
            OsString::from("results"),
            OsString::from("--verbose"),
        ]
    );
}

#[test]
fn official_client_command_is_scoped_complete_and_ordered() {
    let spec = official_client_command(
        "cf-integration __client-conformance",
        "tools_call",
        DEFAULT_MCP_SPEC_VERSION,
        Path::new("expected-failures.yml"),
        Path::new("results"),
    );

    assert!(!spec.inherits_environment());
    assert_eq!(
        spec.arguments(),
        &[
            OsString::from("-y"),
            OsString::from(OFFICIAL_CONFORMANCE_PACKAGE),
            OsString::from("client"),
            OsString::from("--command"),
            OsString::from("cf-integration __client-conformance"),
            OsString::from("--scenario"),
            OsString::from("tools_call"),
            OsString::from("--spec-version"),
            OsString::from(DEFAULT_MCP_SPEC_VERSION),
            OsString::from("--expected-failures"),
            OsString::from("expected-failures.yml"),
            OsString::from("--timeout"),
            OsString::from("60000"),
            OsString::from("--output-dir"),
            OsString::from("results"),
            OsString::from("--verbose"),
        ]
    );
}

#[test]
fn scoped_client_catalog_is_exact_and_versioned() {
    assert_eq!(
        expected_client_scenarios(DEFAULT_MCP_SPEC_VERSION)
            .expect("current client scenario catalog should be pinned"),
        DEFAULT_CLIENT_CONFORMANCE_SCENARIOS
            .iter()
            .copied()
            .collect()
    );
    assert!(expected_client_scenarios("2025-11-25").is_err());
}

#[test]
fn client_results_parse_and_require_every_scoped_scenario() {
    let directory = tempfile::tempdir().expect("temporary result directory");
    for (index, scenario) in DEFAULT_CLIENT_CONFORMANCE_SCENARIOS.iter().enumerate() {
        let result_directory = directory
            .path()
            .join(format!("{scenario}-2026-08-28T22-20-5{index}-000Z"));
        fs::create_dir(&result_directory).expect("client result directory");
        fs::write(
            result_directory.join("checks.json"),
            format!(r#"[{{"id":"{scenario}-check","status":"SUCCESS"}}]"#),
        )
        .expect("client checks should be written");
    }

    let parsed = load_client_results(directory.path()).expect("client results should parse");
    validate_client_scenario_set(&parsed, DEFAULT_MCP_SPEC_VERSION)
        .expect("complete scoped client result set should validate");
    assert_eq!(parsed.scenarios.len(), 4);

    fs::remove_dir_all(directory.path().join("tools_call-2026-08-28T22-20-50-000Z"))
        .expect("one client result should be removed");
    let partial = load_client_results(directory.path()).expect("partial results should parse");
    assert!(validate_client_scenario_set(&partial, DEFAULT_MCP_SPEC_VERSION).is_err());
}

#[test]
fn fixture_results_parse_recursively_with_forward_compatible_fields() {
    let parsed = load_server_results(&workspace_root().join("tests/fixtures/conformance/results"))
        .expect("fixture results should parse");

    assert_eq!(
        parsed
            .scenarios
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["server-initialize", "tools-list"]
    );
    let initialize = &parsed.scenarios["server-initialize"];
    assert_eq!(initialize.checks[0].status, CheckStatus::Success);
    assert_eq!(initialize.checks[1].status, CheckStatus::Info);
    assert_eq!(
        initialize.checks[0].extensions["futureCheckField"],
        "preserved"
    );
    assert_eq!(
        parsed.scenarios["tools-list"].checks[0].status,
        CheckStatus::Warning
    );
}

#[test]
fn scenario_outcomes_preserve_failure_warning_and_unknown_precedence() {
    assert_eq!(
        result("failure", [CheckStatus::Success, CheckStatus::Failure]).outcome(),
        ScenarioOutcome::NonCompliant
    );
    assert_eq!(
        result("warning", [CheckStatus::Success, CheckStatus::Warning]).outcome(),
        ScenarioOutcome::Compliant
    );
    assert_eq!(
        result("info", [CheckStatus::Skipped, CheckStatus::Info]).outcome(),
        ScenarioOutcome::NotApplicable
    );
    assert_eq!(
        result("future", [CheckStatus::Other("FUTURE".to_owned())]).outcome(),
        ScenarioOutcome::Ambiguous
    );
}

#[test]
fn pinned_fixture_turns_fixture_shaped_gateway_failures_into_gateway_failures() {
    let mut missing = check("missing", CheckStatus::Failure);
    missing.error_message = Some("Tool not found: test_simple_tool".to_owned());
    let result = ConformanceScenarioResult {
        scenario: "tools-list".to_owned(),
        checks: vec![missing],
        source: PathBuf::from("server-tools-list/checks.json"),
    };

    assert_eq!(result.outcome(), ScenarioOutcome::FixtureFailure);
    assert_eq!(result.gated_outcome(), ScenarioOutcome::NonCompliant);
}

#[test]
fn classifications_cover_all_three_lane_failure_combinations() {
    use ScenarioOutcome::{Compliant, NonCompliant};
    for (fixture, controlplane, dataplane, expected) in [
        (
            Compliant,
            Compliant,
            Compliant,
            ComparisonClassification::AllCompliant,
        ),
        (
            NonCompliant,
            Compliant,
            Compliant,
            ComparisonClassification::FixtureOnlyFailure,
        ),
        (
            Compliant,
            NonCompliant,
            Compliant,
            ComparisonClassification::BuiltInDataPlaneOnlyFailure,
        ),
        (
            Compliant,
            Compliant,
            NonCompliant,
            ComparisonClassification::ExternalDataPlaneOnlyFailure,
        ),
        (
            NonCompliant,
            NonCompliant,
            Compliant,
            ComparisonClassification::FixtureAndBuiltInDataPlaneFailure,
        ),
        (
            NonCompliant,
            Compliant,
            NonCompliant,
            ComparisonClassification::FixtureAndExternalDataPlaneFailure,
        ),
        (
            Compliant,
            NonCompliant,
            NonCompliant,
            ComparisonClassification::GatewaysOnlyFailure,
        ),
        (
            NonCompliant,
            NonCompliant,
            NonCompliant,
            ComparisonClassification::SharedFailure,
        ),
    ] {
        assert_eq!(
            classify_outcomes(fixture, controlplane, dataplane),
            expected
        );
    }
}

#[test]
fn comparison_counts_raw_failures_without_expected_failure_suppression() {
    let fixture = results([result("scenario", [CheckStatus::Success])]);
    let controlplane = results([result(
        "scenario",
        [CheckStatus::Failure, CheckStatus::Failure],
    )]);
    let dataplane = results([result("scenario", [CheckStatus::Failure])]);

    let compared = compare_result_sets(&fixture, &controlplane, &dataplane);

    assert_eq!(compared.len(), 1);
    assert_eq!(
        compared[0].classification,
        ComparisonClassification::GatewaysOnlyFailure
    );
    assert_eq!(compared[0].built_in_data_plane_failed_checks, 2);
    assert_eq!(compared[0].external_data_plane_failed_checks, 1);
}

#[test]
fn comparison_gates_fixture_shaped_failures_for_every_lane() {
    let mut missing = check("missing", CheckStatus::Failure);
    missing.error_message = Some("Tool not found: test_simple_tool".to_owned());
    let controlplane = results([ConformanceScenarioResult {
        scenario: "scenario".to_owned(),
        checks: vec![missing],
        source: PathBuf::from("server-scenario/checks.json"),
    }]);
    let fixture = results([result("scenario", [CheckStatus::Success])]);
    let dataplane = results([result("scenario", [CheckStatus::Success])]);

    let compared = compare_result_sets(&fixture, &controlplane, &dataplane);

    assert_eq!(
        compared[0].classification,
        ComparisonClassification::BuiltInDataPlaneOnlyFailure
    );
}

#[test]
fn report_renders_raw_counts_and_no_expected_failure_column() {
    let scenario = ScenarioComparison {
        scenario: "server|stateless".to_owned(),
        fixture: ScenarioOutcome::Compliant,
        fixture_failed_checks: 0,
        built_in_data_plane: ScenarioOutcome::NonCompliant,
        built_in_data_plane_failed_checks: 27,
        external_data_plane: ScenarioOutcome::NonCompliant,
        external_data_plane_failed_checks: 28,
        classification: ComparisonClassification::GatewaysOnlyFailure,
        spec_references: vec![SpecReference {
            id: "MCP|Transport".to_owned(),
            url: Some(SPEC_REFERENCE.to_owned()),
            extensions: BTreeMap::new(),
        }],
    };
    let report = ComparisonReport {
        client_version: DEFAULT_MCP_SPEC_VERSION.to_owned(),
        server_era: ConformanceServerEra::Modern,
        suite: DEFAULT_CONFORMANCE_SUITE.to_owned(),
        fixture: fixture_metadata(),
        scenarios: vec![scenario],
    };

    let markdown = render_comparison_markdown(&report);

    assert!(markdown.contains("- Client specification: `2026-07-28`"));
    assert!(markdown.contains("- Upstream server era: `modern`"));
    assert!(markdown.contains("| Built-in data-plane route | 0 | 1 | 27 |"));
    assert!(markdown.contains("| External data-plane route | 0 | 1 | 28 |"));
    assert!(markdown.contains("server\\|stateless"));
    assert!(!markdown.contains("Expected by"));

    let directory = tempfile::tempdir().expect("temporary output directory");
    let path = directory.path().join("nested/report.md");
    write_comparison_report(&path, &report).expect("report should be written");
    assert_eq!(
        fs::read_to_string(path).expect("report should be readable"),
        markdown
    );
}

#[cfg(unix)]
#[test]
fn result_loader_does_not_follow_symlinked_directories() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let outside = tempfile::tempdir().expect("outside directory should be created");
    let result_directory = outside
        .path()
        .join("server-malicious-2026-07-10T12-34-56-000Z");
    fs::create_dir(&result_directory).expect("outside result directory should be created");
    fs::write(
        result_directory.join("checks.json"),
        r#"[{"id":"malicious","status":"SUCCESS"}]"#,
    )
    .expect("outside checks should be written");
    symlink(&result_directory, directory.path().join("linked"))
        .expect("test symlink should be created");

    let parsed =
        load_server_results(directory.path()).expect("symlinked content should be ignored safely");
    assert!(parsed.scenarios.is_empty());
}
