use std::ffi::OsString;
use std::path::PathBuf;

use cf_integration::app::{
    Action, ConformanceAction, DebugAction, LiveLane, ResolvedLoadArgs, StackAction, resolve_action,
};
use cf_integration::cli::{Cli, LiveGroup, ProtocolVersion, TokenKind, TopologySelection};
use cf_integration::compliance::conformance::{ConformanceServerEra, ConformanceTarget};
use cf_integration::load::LoadRequest;
use cf_integration::platform::StackMode;
use cf_integration::platform::config::Environment;
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
            lane: LiveLane::BuiltInDataPlane,
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
            lane: LiveLane::Fixture,
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
                ConformanceTarget::Fixture,
                ConformanceTarget::BuiltInDataPlane,
                ConformanceTarget::ExternalDataPlane,
            ],
            spec_version: "2026-07-28".to_owned(),
            server_era: ConformanceServerEra::Dual,
            results_dir: None,
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
                "--server-era",
                "modern",
                "--results-dir",
                "results",
            ],
            &[],
        ),
        Action::Conformance(ConformanceAction::Run {
            lanes: vec![
                ConformanceTarget::Fixture,
                ConformanceTarget::ExternalDataPlane,
            ],
            spec_version: "2025-06-18".to_owned(),
            server_era: ConformanceServerEra::Modern,
            results_dir: Some(PathBuf::from("results")),
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
