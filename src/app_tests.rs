use std::ffi::OsString;
use std::path::PathBuf;

use cf_integration::app::{
    Action, ConformanceAction, DebugAction, ResolvedLoadArgs, StackAction, resolve_action,
};
use cf_integration::cli::{Cli, LiveGroup, ProtocolVersion, TokenKind, TopologySelection};
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
fn every_subcommand_reports_its_resolved_topology_at_startup() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["cf-integration", "stack", "up"],
            "Topology: external dataplane\nProtocol version: 2026-07-28",
        ),
        (
            &["cf-integration", "stack", "down"],
            "Topology: built-in dataplane, external dataplane",
        ),
        (
            &["cf-integration", "stack", "status"],
            "Topology: external dataplane",
        ),
        (
            &["cf-integration", "stack", "logs"],
            "Topology: external dataplane",
        ),
        (
            &["cf-integration", "stack", "config"],
            "Topology: external dataplane",
        ),
        (
            &["cf-integration", "probe"],
            "Topology: external dataplane\nProtocol version: 2026-07-28",
        ),
        (
            &["cf-integration", "load"],
            "Topology: external dataplane\nProtocol version: 2026-07-28",
        ),
        (
            &["cf-integration", "live"],
            "Topology: external dataplane\nProtocol version: 2026-07-28",
        ),
        (
            &["cf-integration", "conformance", "run"],
            "Topology: fixture direct, built-in dataplane, external dataplane\nClient protocol versions: 2026-07-28\nServer protocol versions: legacy [2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25]; modern [2026-07-28]",
        ),
        (
            &["cf-integration", "conformance", "report"],
            "Topology: recorded conformance results\nClient protocol versions: recorded conformance results\nServer protocol versions: recorded conformance results",
        ),
        (
            &["cf-integration", "debug", "inspect"],
            "Topology: external dataplane\nProtocol version: 2026-07-28",
        ),
        (
            &["cf-integration", "debug", "token", "--kind", "admin"],
            "Topology: not applicable (token only)",
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
            "built-in-data-plane",
            "--protocol-version",
            "2025-11-25",
            "--protocol-version",
            "2026-07-28",
            "--server-era",
            "dual",
        ],
        &[],
    );

    assert_eq!(
        resolved.startup_summary(),
        "Topology: built-in dataplane\nClient protocol versions: 2025-11-25, 2026-07-28\nServer protocol versions: dual [2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25, 2026-07-28]"
    );
}

#[test]
fn stack_up_owns_its_detailed_progress_while_other_commands_use_global_progress() {
    assert!(!action(&["cf-integration", "stack", "up"], &[]).uses_global_activity());
    assert!(action(&["cf-integration", "stack", "down"], &[]).uses_global_activity());
    assert!(action(&["cf-integration", "probe"], &[]).uses_global_activity());
}

#[test]
fn topology_precedence_is_cli_then_environment_then_dataplane() {
    assert_eq!(
        action(&["cf-integration", "probe"], &[]),
        Action::Probe {
            topology: StackMode::Dataplane,
            protocol_version: ProtocolVersion::default(),
        }
    );
    assert_eq!(
        action(
            &["cf-integration", "probe"],
            &[("CF_MCP_STACK_MODE", "controlplane")],
        ),
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
                "dataplane",
                "--protocol-version",
                "2025-06-18",
            ],
            &[("CF_MCP_STACK_MODE", "invalid")],
        ),
        Action::Probe {
            topology: StackMode::Dataplane,
            protocol_version: "2025-06-18"
                .parse::<ProtocolVersion>()
                .expect("valid protocol version"),
        }
    );
}

#[test]
fn invalid_environment_topology_is_rejected_when_used() {
    let cli = Cli::try_parse_from(["cf-integration", "probe"]).expect("CLI should parse");
    let environment = [(OsString::from("CF_MCP_STACK_MODE"), OsString::from("bad"))]
        .into_iter()
        .collect();
    let error = resolve_action(cli, &environment).expect_err("invalid topology must fail");
    assert!(error.to_string().contains("invalid CF_MCP_STACK_MODE"));
}

#[test]
fn stack_actions_resolve_freshness_and_volume_cleanup() {
    assert_eq!(
        action(
            &[
                "cf-integration",
                "stack",
                "up",
                "--topology",
                "controlplane",
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
            topology: TopologySelection::All,
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
                "controlplane",
                "--protocol-version",
                "2025-06-18",
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
            protocol_version: "2025-06-18"
                .parse::<ProtocolVersion>()
                .expect("valid protocol version"),
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
fn live_resolves_lane_group_and_protocol_version() {
    assert_eq!(
        action(
            &["cf-integration", "live", "--group", "mcp"],
            &[
                ("CF_MCP_STACK_MODE", "controlplane"),
                ("MCP_PROTOCOL_VERSION", "2025-06-18"),
            ],
        ),
        Action::Live {
            lane: SemanticLane::BuiltInDataPlane,
            group: LiveGroup::Mcp,
            protocol_version: "2025-06-18"
                .parse::<ProtocolVersion>()
                .expect("valid protocol version"),
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
                "2025-03-26",
            ],
            &[
                ("CF_MCP_STACK_MODE", "invalid"),
                ("MCP_PROTOCOL_VERSION", "2025-06-18"),
            ],
        ),
        Action::Live {
            lane: SemanticLane::FixtureDirect,
            group: LiveGroup::Protocol,
            protocol_version: "2025-03-26"
                .parse::<ProtocolVersion>()
                .expect("valid protocol version"),
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
                "external-data-plane",
                "--lane",
                "fixture-direct",
                "--lane",
                "external-data-plane",
                "--protocol-version",
                "2025-06-18",
                "--protocol-version",
                "2025-11-25",
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
            client_versions: vec!["2025-06-18".to_owned(), "2025-11-25".to_owned()],
            server_eras: vec![ConformanceServerEra::Modern, ConformanceServerEra::Legacy],
            results_dir: Some(PathBuf::from("results")),
            baseline_dir: Some(PathBuf::from("baselines")),
            bless: true,
            output_dir: Some(PathBuf::from("reports")),
        })
    );
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
        &[
            "cf-integration",
            "stack",
            "status",
            "--topology",
            "dataplane",
        ],
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
                "controlplane",
                "--protocol-version",
                "2025-06-18",
                "--method",
                "prompts/list",
            ],
            &[],
        ),
        Action::Debug(DebugAction::Inspect {
            topology: StackMode::Controlplane,
            protocol_version: "2025-06-18"
                .parse::<ProtocolVersion>()
                .expect("valid protocol version"),
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
