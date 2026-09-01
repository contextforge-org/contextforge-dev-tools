use std::ffi::OsString;

use cf_integration::cli::{
    Cli, CliConformanceEra, CliLane, CliTopology, Command, ConformanceArgs, ConformanceCommand,
    DebugArgs, DebugCommand, LiveGroup, LoadArgs, ProtocolVersion, RoutedWorkflowTargetArgs,
    StackArgs, StackCommand, TokenKind, TopologySelection, WorkflowTargetArgs,
};
use clap::{CommandFactory, Parser, error::ErrorKind};

const REMOVED_COMMANDS: &[&str] = &["sync", "token", "test", "compliance", "inspect"];

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(args.iter().copied()).expect("command should parse")
}

fn rejected(args: &[&str]) {
    assert!(
        Cli::try_parse_from(args.iter().copied()).is_err(),
        "command unexpectedly parsed: {args:?}"
    );
}

fn command_at(path: &[&str]) -> clap::Command {
    let mut command = Cli::command();
    for name in path {
        command = command
            .find_subcommand(name)
            .cloned()
            .expect("help path should name a public command");
    }
    command
}

fn subcommands(path: &[&str]) -> Vec<String> {
    command_at(path)
        .get_subcommands()
        .filter(|command| command.get_name() != "help" && !command.is_hide_set())
        .map(|command| command.get_name().to_owned())
        .collect()
}

#[test]
fn hidden_ci_commands_parse_without_expanding_the_public_command_tree() {
    let cli = parse(&[
        "cf-integration",
        "ci",
        "prepare-image",
        "--artifact",
        "contextforge-data-plane-conformance",
        "--binary",
        "contextforge-data-plane",
        "--image",
        "contextforge-data-plane:conformance",
        "--repository",
        "contextforge-org/contextforge-data-plane",
    ]);

    assert!(matches!(cli.command, Command::Ci(_)));
    assert!(!subcommands(&[]).contains(&String::from("ci")));
}

#[test]
fn command_tree_contains_only_distinct_public_workflows() {
    assert_eq!(
        subcommands(&[]),
        ["stack", "probe", "load", "live", "conformance", "debug"]
    );
    assert_eq!(
        subcommands(&["stack"]),
        ["up", "down", "status", "logs", "config"]
    );
    assert_eq!(subcommands(&["conformance"]), ["run", "report"]);
    assert_eq!(subcommands(&["debug"]), ["inspect", "token"]);
}

#[test]
fn every_public_command_renders_help() {
    let paths: &[&[&str]] = &[
        &[],
        &["stack"],
        &["stack", "up"],
        &["stack", "down"],
        &["stack", "status"],
        &["stack", "logs"],
        &["stack", "config"],
        &["probe"],
        &["load"],
        &["live"],
        &["conformance"],
        &["conformance", "run"],
        &["conformance", "report"],
        &["debug"],
        &["debug", "inspect"],
        &["debug", "token"],
    ];

    for path in paths {
        let help = command_at(path).render_long_help().to_string();
        assert!(help.contains("Usage:"), "missing usage for {path:?}");
    }
}

#[test]
fn obsolete_root_commands_and_combined_workflows_are_rejected() {
    for command in REMOVED_COMMANDS {
        rejected(&["cf-integration", command]);
    }
    rejected(&["cf-integration", "stack", "reset"]);
    rejected(&["cf-integration", "conformance", "all"]);
    rejected(&["cf-integration", "conformance", "gateway"]);
}

#[test]
fn stack_up_and_down_make_destructive_behavior_explicit() {
    let Command::Stack(StackArgs {
        command: StackCommand::Up(up),
    }) = parse(&[
        "cf-integration",
        "stack",
        "up",
        "--topology",
        "dataplane",
        "--fresh",
    ])
    .command
    else {
        panic!("expected stack up")
    };
    assert_eq!(up.topology, Some(CliTopology::Dataplane));
    assert!(up.fresh);

    let Command::Stack(StackArgs {
        command: StackCommand::Down(down),
    }) = parse(&[
        "cf-integration",
        "stack",
        "down",
        "--topology",
        "all",
        "--volumes",
    ])
    .command
    else {
        panic!("expected stack down")
    };
    assert_eq!(down.topology, Some(TopologySelection::All));
    assert!(down.volumes);
}

#[test]
fn stack_logs_preserve_service_arguments() {
    let Command::Stack(StackArgs {
        command: StackCommand::Logs(args),
    }) = parse(&[
        "cf-integration",
        "stack",
        "logs",
        "--topology",
        "controlplane",
        "gateway",
        "worker",
    ])
    .command
    else {
        panic!("expected stack logs")
    };
    assert_eq!(args.topology, Some(CliTopology::Controlplane));
    assert_eq!(
        args.services,
        [OsString::from("gateway"), OsString::from("worker")]
    );
}

#[test]
fn load_keeps_validated_locust_settings() {
    let Command::Load(LoadArgs {
        target,
        users,
        spawn_rate,
        run_time,
        ..
    }) = parse(&[
        "cf-integration",
        "load",
        "--users",
        "2",
        "--spawn-rate",
        "0.5",
        "--run-time",
        "1m30s",
    ])
    .command
    else {
        panic!("expected load")
    };
    assert_eq!(target.lane, None);
    assert_eq!(target.protocol_version, None);
    assert_eq!(users, Some(2));
    assert_eq!(spawn_rate, Some(0.5));
    assert_eq!(run_time.as_deref(), Some("1m30s"));

    rejected(&["cf-integration", "load", "--users", "0"]);
    rejected(&["cf-integration", "load", "--run-time", "1ms"]);
    rejected(&["cf-integration", "load", "--run-time", "zero"]);
    rejected(&["cf-integration", "load", "--engine", "locust"]);
}

#[test]
fn live_defaults_to_all_and_accepts_the_main_harness_groups() {
    let Command::Live(defaults) = parse(&["cf-integration", "live"]).command else {
        panic!("expected live workflow")
    };
    assert_eq!(defaults.target.lane, None);
    assert_eq!(defaults.group, LiveGroup::All);
    assert_eq!(defaults.target.protocol_version, None);

    for (name, expected) in [
        ("mcp", LiveGroup::Mcp),
        ("rbac", LiveGroup::Rbac),
        ("protocol", LiveGroup::Protocol),
        ("all", LiveGroup::All),
    ] {
        let Command::Live(args) = parse(&[
            "cf-integration",
            "live",
            "--lane",
            "external-data-plane",
            "--group",
            name,
        ])
        .command
        else {
            panic!("expected live workflow")
        };
        assert_eq!(args.target.lane, Some(CliLane::ExternalDataPlane));
        assert_eq!(args.group, expected);
    }
}

#[test]
fn live_accepts_fixture_lane_and_explicit_protocol_version() {
    let Command::Live(args) = parse(&[
        "cf-integration",
        "live",
        "--lane",
        "fixture-direct",
        "--group",
        "protocol",
        "--protocol-version",
        "2025-06-18",
    ])
    .command
    else {
        panic!("expected live workflow")
    };

    assert_eq!(args.target.lane, Some(CliLane::FixtureDirect));
    assert_eq!(args.group, LiveGroup::Protocol);
    assert_eq!(
        args.target.protocol_version,
        Some(
            "2025-06-18"
                .parse::<ProtocolVersion>()
                .expect("valid protocol version")
        )
    );

    rejected(&["cf-integration", "live", "--protocol-version", "latest"]);

    rejected(&["cf-integration", "live", "--lane", "fixture"]);
}

#[test]
fn probe_rejects_removed_topology_alias() {
    rejected(&["cf-integration", "probe", "--topology", "dataplane"]);
}

#[test]
fn load_rejects_removed_topology_alias() {
    rejected(&["cf-integration", "load", "--topology", "dataplane"]);
}

#[test]
fn live_rejects_removed_topology_alias() {
    rejected(&[
        "cf-integration",
        "live",
        "--topology",
        "external-data-plane",
    ]);
}

#[test]
fn operational_workflows_share_canonical_lane_and_protocol_version_flags() {
    fn assert_routed_target(target: &RoutedWorkflowTargetArgs) {
        assert_eq!(target.lane, Some(CliTopology::Controlplane));
        assert_eq!(
            target.protocol_version,
            Some(
                "2025-06-18"
                    .parse::<ProtocolVersion>()
                    .expect("valid protocol version")
            )
        );
    }

    fn assert_fixture_target(target: &WorkflowTargetArgs) {
        assert_eq!(target.lane, Some(CliLane::BuiltInDataPlane));
        assert_eq!(
            target.protocol_version,
            Some(
                "2025-06-18"
                    .parse::<ProtocolVersion>()
                    .expect("valid protocol version")
            )
        );
    }

    let common = ["--lane", "controlplane", "--protocol-version", "2025-06-18"];
    let Command::Probe(probe) = parse(
        &["cf-integration", "probe"]
            .into_iter()
            .chain(common)
            .collect::<Vec<_>>(),
    )
    .command
    else {
        panic!("expected probe workflow")
    };
    assert_routed_target(&probe);

    let Command::Load(load) = parse(
        &["cf-integration", "load"]
            .into_iter()
            .chain(common)
            .collect::<Vec<_>>(),
    )
    .command
    else {
        panic!("expected load workflow")
    };
    assert_routed_target(&load.target);

    let Command::Live(live) = parse(&[
        "cf-integration",
        "live",
        "--lane",
        "built-in-data-plane",
        "--protocol-version",
        "2025-06-18",
    ])
    .command
    else {
        panic!("expected live workflow")
    };
    assert_fixture_target(&live.target);

    let Command::Debug(DebugArgs {
        command: DebugCommand::Inspect(inspect),
    }) = parse(
        &["cf-integration", "debug", "inspect"]
            .into_iter()
            .chain(common)
            .collect::<Vec<_>>(),
    )
    .command
    else {
        panic!("expected inspect workflow")
    };
    assert_routed_target(&inspect.target);
}

#[test]
fn routed_workflows_reject_the_fixture_lane_during_parsing() {
    for arguments in [
        vec!["cf-integration", "probe", "--lane", "fixture-direct"],
        vec!["cf-integration", "load", "--lane", "fixture-direct"],
        vec![
            "cf-integration",
            "debug",
            "inspect",
            "--lane",
            "fixture-direct",
        ],
    ] {
        let error = Cli::try_parse_from(arguments).expect_err("routed lane should be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
    }
}

#[test]
fn conformance_defaults_to_all_lanes_and_july_revision_at_resolution_time() {
    let Command::Conformance(ConformanceArgs {
        command: ConformanceCommand::Run(args),
    }) = parse(&["cf-integration", "conformance", "run"]).command
    else {
        panic!("expected conformance run")
    };
    assert!(args.lane.is_empty());
    assert!(args.client_era.is_empty());
    assert!(args.server_era.is_empty());
    assert!(args.results_dir.is_none());
    assert!(args.baseline_dir.is_none());
    assert!(!args.bless);
    assert!(args.output_dir.is_none());
}

#[test]
fn conformance_accepts_repeatable_exact_lanes_and_protocol_eras() {
    let Command::Conformance(ConformanceArgs {
        command: ConformanceCommand::Run(args),
    }) = parse(&[
        "cf-integration",
        "conformance",
        "run",
        "--lane",
        "fixture-direct",
        "--lane",
        "external-data-plane",
        "--client-era",
        "legacy",
        "--client-era",
        "dual",
        "--server-era",
        "legacy",
        "--server-era",
        "dual",
        "--baseline-dir",
        "baselines",
        "--output-dir",
        "reports",
        "--bless",
    ])
    .command
    else {
        panic!("expected conformance run")
    };
    assert_eq!(
        args.lane,
        [CliLane::FixtureDirect, CliLane::ExternalDataPlane]
    );
    assert_eq!(
        args.client_era,
        [CliConformanceEra::Legacy, CliConformanceEra::Dual]
    );
    assert_eq!(
        args.server_era,
        [CliConformanceEra::Legacy, CliConformanceEra::Dual]
    );
    assert_eq!(args.baseline_dir, Some("baselines".into()));
    assert_eq!(args.output_dir, Some("reports".into()));
    assert!(args.bless);
    rejected(&[
        "cf-integration",
        "conformance",
        "run",
        "--client-protocol-version",
        "2025-11-25",
    ]);
    rejected(&[
        "cf-integration",
        "conformance",
        "run",
        "--protocol-version",
        "2025-11-25",
    ]);
    rejected(&[
        "cf-integration",
        "conformance",
        "run",
        "--spec-version",
        "2025-11-25",
    ]);
    rejected(&[
        "cf-integration",
        "conformance",
        "run",
        "--client-version",
        "2025-11-25",
    ]);
    rejected(&["cf-integration", "conformance", "run", "--suite", "active"]);
    rejected(&[
        "cf-integration",
        "conformance",
        "run",
        "--baseline",
        "known.yml",
    ]);
}

#[test]
fn root_version_flag_reports_the_package_version() {
    let error = Cli::try_parse_from(["cf-integration", "--version"])
        .expect_err("version should short-circuit parsing");

    assert_eq!(error.kind(), ErrorKind::DisplayVersion);
    assert_eq!(
        error.to_string().trim(),
        format!("cf-integration {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn debug_token_requires_an_explicit_privilege_kind() {
    let error = Cli::try_parse_from(["cf-integration", "debug", "token"])
        .expect_err("token kind should be explicit");
    assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);

    let Command::Debug(DebugArgs {
        command: DebugCommand::Token(args),
    }) = parse(&[
        "cf-integration",
        "debug",
        "token",
        "--kind",
        "scoped",
        "--server-id",
        "server-1",
    ])
    .command
    else {
        panic!("expected debug token")
    };
    assert_eq!(args.kind, TokenKind::Scoped);
    assert_eq!(args.server_id.as_deref(), Some("server-1"));
}

#[test]
fn help_and_version_style_flags_reject_unexpected_positionals() {
    rejected(&["cf-integration", "probe", "unexpected"]);
    let error = Cli::try_parse_from(["cf-integration"])
        .expect_err("root without a workflow should show help");
    assert_eq!(
        error.kind(),
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    );
}
