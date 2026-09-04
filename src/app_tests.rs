use std::ffi::OsString;
use std::path::PathBuf;

use cf_integration::app::{
    Action, CiAction, ConformanceAction, DebugAction, ResolvedLoadArgs, StackAction, resolve_action,
};
use cf_integration::cli::{Cli, LaneSelection, LiveGroup, ProtocolVersion, TokenKind};
use cf_integration::conformance::results::{ConformanceServerEra, SemanticLane};
use cf_integration::infrastructure::StackMode;
use cf_integration::infrastructure::config::Environment;
use cf_integration::performance::LoadRequest;
use clap::Parser;

fn action(arguments: &[&str], environment: &[(&str, &str)]) -> Action {
    let cli = Cli::try_parse_from(arguments.iter().copied()).expect("CLI should parse");
    let environment = environment
        .iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect::<Environment>();
    resolve_action(cli, &environment).expect("action should resolve")
}

#[test]
fn every_subcommand_has_a_stable_progress_description() {
    let cases: &[(&[&str], &str)] = &[
        (&["cf-integration", "stack", "up"], "stack up"),
        (&["cf-integration", "stack", "down"], "stack down"),
        (&["cf-integration", "stack", "status"], "stack status"),
        (&["cf-integration", "stack", "logs"], "stack logs"),
        (&["cf-integration", "stack", "config"], "stack config"),
        (&["cf-integration", "probe"], "probe"),
        (&["cf-integration", "load"], "load test"),
        (&["cf-integration", "live"], "live tests"),
        (
            &["cf-integration", "conformance", "run"],
            "conformance tests",
        ),
        (
            &["cf-integration", "conformance", "report"],
            "conformance report",
        ),
        (&["cf-integration", "debug", "inspect"], "debug inspect"),
        (
            &["cf-integration", "debug", "token", "--kind", "admin"],
            "debug token",
        ),
    ];

    for (arguments, expected) in cases {
        assert_eq!(action(arguments, &[]).description(), *expected);
    }
}

#[test]
fn ci_image_preparation_is_read_only_until_execution() {
    let action = action(
        &[
            "cf-integration",
            "ci",
            "prepare-image",
            "--artifact",
            "contextforge-data-plane-conformance",
            "--binary",
            "contextforge-data-plane",
            "--image",
            "contextforge-data-plane:conformance",
        ],
        &[(
            "GITHUB_REPOSITORY",
            "contextforge-org/contextforge-data-plane",
        )],
    );

    assert!(matches!(&action, Action::Ci(CiAction::PrepareImage { .. })));
    assert_eq!(action.description(), "prepare prebuilt CI image");
    assert!(!action.requires_runtime_assets());
}

#[test]
fn ci_image_preparation_rejects_nested_artifact_paths() {
    let cli = Cli::try_parse_from([
        "cf-integration",
        "ci",
        "prepare-image",
        "--artifact",
        "artifact",
        "--binary",
        "nested/binary",
        "--image",
        "service:test",
        "--repository",
        "owner/repository",
    ])
    .expect("CLI syntax should parse before path validation");

    let error = resolve_action(cli, &Environment::new())
        .expect_err("artifact binary must remain inside its download root");

    assert_eq!(
        error.to_string(),
        "--binary must be one filename at the artifact root"
    );
}

#[test]
fn every_subcommand_reports_its_resolved_lane_at_startup() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["cf-integration", "stack", "up"],
            "Lane: external\nProtocol version: modern",
        ),
        (
            &["cf-integration", "stack", "down"],
            "Lane: builtin, external",
        ),
        (&["cf-integration", "stack", "status"], "Lane: external"),
        (&["cf-integration", "stack", "logs"], "Lane: external"),
        (&["cf-integration", "stack", "config"], "Lane: external"),
        (
            &["cf-integration", "probe"],
            "Lane: external\nProtocol version: modern",
        ),
        (
            &["cf-integration", "load"],
            "Lane: external\nProtocol version: modern",
        ),
        (
            &["cf-integration", "live"],
            "Lane: external\nProtocol version: modern",
        ),
        (
            &["cf-integration", "conformance", "run"],
            "Lane: fixture direct, builtin, external\nClient era: modern [2026-07-28]\nServer era: legacy [2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25]; modern [2026-07-28]",
        ),
        (
            &["cf-integration", "conformance", "report"],
            "Lane: recorded conformance results\nClient era: recorded conformance results\nServer era: recorded conformance results",
        ),
        (
            &["cf-integration", "debug", "inspect"],
            "Lane: external\nProtocol version: modern",
        ),
        (
            &["cf-integration", "debug", "token", "--kind", "admin"],
            "Lane: not applicable (token only)",
        ),
    ];

    for (arguments, expected) in cases {
        assert_eq!(action(arguments, &[]).startup_summary(), *expected);
    }
}

#[test]
fn conformance_startup_reports_every_selected_client_and_server_protocol() {
    let resolved = action(
        &[
            "cf-integration",
            "conformance",
            "run",
            "--lane",
            "builtin",
            "--client-era",
            "legacy",
            "--client-era",
            "modern",
            "--server-era",
            "dual",
        ],
        &[],
    );

    assert_eq!(
        resolved.startup_summary(),
        "Lane: builtin\nClient era: legacy [2025-06-18, 2025-11-25]; modern [2026-07-28]\nServer era: dual [2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25, 2026-07-28]"
    );
}

#[test]
fn conformance_startup_labels_both_legacy_era_selections() {
    let resolved = action(
        &[
            "cf-integration",
            "conformance",
            "run",
            "--client-era",
            "legacy",
            "--server-era",
            "legacy",
        ],
        &[],
    );

    assert_eq!(
        resolved.startup_summary(),
        "Lane: fixture direct, builtin, external\nClient era: legacy [2025-06-18, 2025-11-25]\nServer era: legacy [2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25]"
    );
}

#[test]
fn multi_phase_commands_own_detailed_progress_while_simple_commands_use_global_progress() {
    assert!(!action(&["cf-integration", "stack", "up"], &[]).uses_global_activity());
    assert!(!action(&["cf-integration", "load"], &[]).uses_global_activity());
    assert!(!action(&["cf-integration", "conformance", "run"], &[]).uses_global_activity());
    assert!(action(&["cf-integration", "stack", "down"], &[]).uses_global_activity());
    assert!(action(&["cf-integration", "probe"], &[]).uses_global_activity());
}

#[test]
fn lane_precedence_is_cli_then_environment_then_external() {
    assert_eq!(
        action(&["cf-integration", "probe"], &[]),
        Action::Probe {
            topology: StackMode::Dataplane,
            protocol_version: ProtocolVersion::default(),
        }
    );
    assert_eq!(
        action(&["cf-integration", "probe"], &[("CF_MCP_LANE", "builtin")],),
        Action::Probe {
            topology: StackMode::Controlplane,
            protocol_version: ProtocolVersion::default(),
        }
    );
    assert_eq!(
        action(
            &[
                "cf-integration",
                "probe",
                "--lane",
                "external",
                "--protocol-version",
                "legacy",
            ],
            &[("CF_MCP_LANE", "invalid")],
        ),
        Action::Probe {
            topology: StackMode::Dataplane,
            protocol_version: ProtocolVersion::Legacy,
        }
    );
}

#[test]
fn invalid_environment_lane_is_rejected_when_used() {
    let cli = Cli::try_parse_from(["cf-integration", "probe"]).expect("CLI should parse");
    let environment = [(OsString::from("CF_MCP_LANE"), OsString::from("bad"))]
        .into_iter()
        .collect();
    let error = resolve_action(cli, &environment).expect_err("invalid lane must fail");
    assert!(error.to_string().contains("invalid CF_MCP_LANE"));
}

#[test]
fn date_based_protocol_environment_is_rejected() {
    let cli = Cli::try_parse_from(["cf-integration", "probe"]).expect("CLI should parse");
    let environment = [(
        OsString::from("MCP_PROTOCOL_VERSION"),
        OsString::from("2026-07-28"),
    )]
    .into_iter()
    .collect();
    let error = resolve_action(cli, &environment).expect_err("wire revisions must remain internal");

    assert_eq!(
        error.to_string(),
        "invalid MCP_PROTOCOL_VERSION: must be modern or legacy"
    );
}

#[test]
fn stack_actions_resolve_freshness_and_volume_cleanup() {
    assert_eq!(
        action(
            &[
                "cf-integration",
                "stack",
                "up",
                "--lane",
                "builtin",
                "--fresh",
            ],
            &[],
        ),
        Action::Stack(StackAction::Up {
            topology: StackMode::Controlplane,
            fresh: true,
        })
    );
    assert_eq!(
        action(&["cf-integration", "stack", "down", "--volumes"], &[],),
        Action::Stack(StackAction::Down {
            lane: LaneSelection::All,
            volumes: true,
        })
    );
}

#[test]
fn load_preserves_explicit_locust_settings() {
    assert_eq!(
        action(
            &[
                "cf-integration",
                "load",
                "--lane",
                "builtin",
                "--protocol-version",
                "legacy",
                "--smoke",
                "--users",
                "2",
                "--spawn-rate",
                "0.5",
                "--run-time",
                "10s",
            ],
            &[],
        ),
        Action::Load(ResolvedLoadArgs {
            topology: StackMode::Controlplane,
            protocol_version: ProtocolVersion::Legacy,
            standalone: false,
            observability: false,
            request: LoadRequest {
                smoke: true,
                users: Some(2),
                spawn_rate: Some(0.5),
                run_time: Some("10s".to_owned()),
            },
        })
    );
}

#[test]
fn standalone_load_is_external_only() {
    let standalone = action(
        &[
            "cf-integration",
            "load",
            "--lane",
            "external",
            "--standalone",
        ],
        &[],
    );
    assert_eq!(
        standalone,
        Action::Load(ResolvedLoadArgs {
            topology: StackMode::Dataplane,
            protocol_version: ProtocolVersion::default(),
            standalone: true,
            observability: false,
            request: LoadRequest {
                smoke: false,
                users: None,
                spawn_rate: None,
                run_time: None,
            },
        })
    );
    assert_eq!(
        standalone.startup_summary(),
        "Lane: external\nProtocol version: modern\nControl plane: disabled during load"
    );

    let cli = Cli::try_parse_from([
        "cf-integration",
        "load",
        "--lane",
        "builtin",
        "--standalone",
    ])
    .expect("CLI syntax should parse before lane validation");
    let error = resolve_action(cli, &Environment::new())
        .expect_err("standalone mode must reject the built-in lane");
    assert_eq!(error.to_string(), "--standalone requires --lane external");
}

#[test]
fn load_enables_observability_only_when_requested() {
    let enabled = action(&["cf-integration", "load", "--observability"], &[]);
    let Action::Load(args) = &enabled else {
        panic!("expected load action")
    };

    assert!(args.observability);
    assert_eq!(
        enabled.startup_summary(),
        "Lane: external\nProtocol version: modern\nObservability: ClickStack enabled during load"
    );
}

#[test]
fn live_resolves_lane_group_and_protocol_version() {
    assert_eq!(
        action(
            &["cf-integration", "live", "--group", "mcp"],
            &[
                ("CF_MCP_LANE", "builtin"),
                ("MCP_PROTOCOL_VERSION", "legacy"),
            ],
        ),
        Action::Live {
            lane: SemanticLane::BuiltInDataPlane,
            group: LiveGroup::Mcp,
            protocol_version: ProtocolVersion::Legacy,
        }
    );
}

#[test]
fn live_fixture_lane_bypasses_topology_and_cli_version_wins() {
    assert_eq!(
        action(
            &[
                "cf-integration",
                "live",
                "--lane",
                "fixture-direct",
                "--group",
                "protocol",
                "--protocol-version",
                "modern",
            ],
            &[
                ("CF_MCP_LANE", "invalid"),
                ("MCP_PROTOCOL_VERSION", "legacy"),
            ],
        ),
        Action::Live {
            lane: SemanticLane::FixtureDirect,
            group: LiveGroup::Protocol,
            protocol_version: ProtocolVersion::Modern,
        }
    );
}

#[test]
fn live_fixture_lane_rejects_non_protocol_groups() {
    let cli = Cli::try_parse_from([
        "cf-integration",
        "live",
        "--lane",
        "fixture-direct",
        "--group",
        "mcp",
    ])
    .expect("CLI should parse before cross-field validation");
    let error = resolve_action(cli, &Environment::new())
        .expect_err("fixture lane should require protocol group");
    assert!(
        error
            .to_string()
            .contains("--lane fixture-direct requires --group protocol")
    );
}

#[test]
fn conformance_defaults_to_all_three_ordered_lanes() {
    assert_eq!(
        action(&["cf-integration", "conformance", "run"], &[]),
        Action::Conformance(ConformanceAction::Run {
            lanes: vec![
                SemanticLane::FixtureDirect,
                SemanticLane::BuiltInDataPlane,
                SemanticLane::ExternalDataPlane,
            ],
            standalone: false,
            client_eras: vec![ConformanceServerEra::Modern],
            client_versions: vec!["2026-07-28".to_owned()],
            server_eras: vec![ConformanceServerEra::Legacy, ConformanceServerEra::Modern],
            results_dir: None,
            baseline_dir: None,
            bless: false,
            output_dir: None,
        })
    );
}

#[test]
fn conformance_lanes_are_deduplicated_and_normalized() {
    assert_eq!(
        action(
            &[
                "cf-integration",
                "conformance",
                "run",
                "--lane",
                "external",
                "--lane",
                "fixture-direct",
                "--lane",
                "external",
                "--client-era",
                "legacy",
                "--client-era",
                "modern",
                "--server-era",
                "modern",
                "--server-era",
                "legacy",
                "--server-era",
                "modern",
                "--results-dir",
                "results",
                "--baseline-dir",
                "baselines",
                "--output-dir",
                "reports",
                "--bless",
            ],
            &[],
        ),
        Action::Conformance(ConformanceAction::Run {
            lanes: vec![SemanticLane::FixtureDirect, SemanticLane::ExternalDataPlane,],
            standalone: false,
            client_eras: vec![ConformanceServerEra::Legacy, ConformanceServerEra::Modern],
            client_versions: vec![
                "2025-06-18".to_owned(),
                "2025-11-25".to_owned(),
                "2026-07-28".to_owned(),
            ],
            server_eras: vec![ConformanceServerEra::Modern, ConformanceServerEra::Legacy],
            results_dir: Some(PathBuf::from("results")),
            baseline_dir: Some(PathBuf::from("baselines")),
            bless: true,
            output_dir: Some(PathBuf::from("reports")),
        })
    );
}

#[test]
fn standalone_conformance_is_external_only() {
    let standalone = action(
        &[
            "cf-integration",
            "conformance",
            "run",
            "--lane",
            "external",
            "--standalone",
        ],
        &[],
    );
    let Action::Conformance(ConformanceAction::Run {
        lanes,
        standalone: enabled,
        ..
    }) = &standalone
    else {
        panic!("expected conformance run");
    };
    assert_eq!(lanes, &[SemanticLane::ExternalDataPlane]);
    assert!(enabled);
    assert!(
        standalone
            .startup_summary()
            .contains("Control plane: disabled; Redis config: mocked")
    );

    for arguments in [
        vec!["cf-integration", "conformance", "run", "--standalone"],
        vec![
            "cf-integration",
            "conformance",
            "run",
            "--lane",
            "builtin",
            "--standalone",
        ],
    ] {
        let cli = Cli::try_parse_from(arguments).expect("CLI should parse standalone mode");
        let error = resolve_action(cli, &Environment::new())
            .expect_err("standalone conformance must reject non-external lane selections");
        assert_eq!(
            error.to_string(),
            "--standalone requires --lane external as the only lane"
        );
    }
}

#[test]
fn conformance_report_is_official_only() {
    assert_eq!(
        action(
            &[
                "cf-integration",
                "conformance",
                "report",
                "--results-dir",
                "results",
                "--output-dir",
                "reports",
            ],
            &[],
        ),
        Action::Conformance(ConformanceAction::Report {
            results_dir: Some(PathBuf::from("results")),
            output_dir: Some(PathBuf::from("reports")),
        })
    );
}

#[test]
fn only_report_and_token_actions_skip_runtime_assets() {
    let report = action(&["cf-integration", "conformance", "report"], &[]);
    let token = action(
        &["cf-integration", "debug", "token", "--kind", "admin"],
        &[],
    );
    let stack = action(
        &["cf-integration", "stack", "status", "--lane", "external"],
        &[],
    );

    assert!(!report.requires_runtime_assets());
    assert!(!token.requires_runtime_assets());
    assert!(stack.requires_runtime_assets());
}

#[test]
fn debug_token_and_inspector_remain_explicit_non_gate_operations() {
    assert_eq!(
        action(
            &[
                "cf-integration",
                "debug",
                "token",
                "--kind",
                "scoped",
                "--server-id",
                "server-1",
            ],
            &[],
        ),
        Action::Debug(DebugAction::Token {
            kind: TokenKind::Scoped,
            server_id: Some("server-1".to_owned()),
        })
    );
    assert_eq!(
        action(
            &[
                "cf-integration",
                "debug",
                "inspect",
                "--lane",
                "builtin",
                "--protocol-version",
                "legacy",
                "--method",
                "prompts/list",
            ],
            &[],
        ),
        Action::Debug(DebugAction::Inspect {
            topology: StackMode::Controlplane,
            protocol_version: ProtocolVersion::Legacy,
            method: "prompts/list".to_owned(),
            server_id: None,
        })
    );
}

#[test]
fn admin_token_rejects_a_server_scope() {
    let cli = Cli::try_parse_from([
        "cf-integration",
        "debug",
        "token",
        "--kind",
        "admin",
        "--server-id",
        "server-1",
    ])
    .expect("CLI syntax should parse before semantic validation");
    let error = resolve_action(cli, &Environment::new())
        .expect_err("admin token server restriction must not be discarded");
    assert!(error.to_string().contains("only valid with --kind scoped"));
}
