//! CLI-to-runtime command resolution.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Result, bail};
use cf_integration_compliance::conformance::{ConformanceServerEra, ConformanceTarget};
use cf_integration_load::LoadRequest;
use cf_integration_platform::StackMode;
use cf_integration_platform::config::Environment;

use crate::cli::{
    Cli, CliLane, CliTopology, Command, ConformanceCommand, DebugCommand, LiveGroup,
    ProtocolVersion, StackCommand, TokenKind, TopologySelection,
};
const STACK_MODE_ENV: &str = "CF_MCP_STACK_MODE";
const PROTOCOL_VERSION_ENV: &str = "MCP_PROTOCOL_VERSION";

/// Fully resolved application operation.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Stack(StackAction),
    Probe {
        topology: StackMode,
        protocol_version: ProtocolVersion,
    },
    Load(ResolvedLoadArgs),
    Live {
        lane: LiveLane,
        group: LiveGroup,
        protocol_version: ProtocolVersion,
    },
    Conformance(ConformanceAction),
    Debug(DebugAction),
}

/// Fully resolved stack operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackAction {
    Up {
        topology: StackMode,
        fresh: bool,
    },
    Down {
        topology: TopologySelection,
        volumes: bool,
    },
    Status(StackMode),
    Logs {
        topology: StackMode,
        services: Vec<OsString>,
    },
    Config(StackMode),
}

/// Fully resolved load-test options.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedLoadArgs {
    pub topology: StackMode,
    pub protocol_version: ProtocolVersion,
    pub request: LoadRequest,
}

/// One upstream live-test execution path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveLane {
    Fixture,
    BuiltInDataPlane,
    ExternalDataPlane,
}

/// Fully resolved official conformance operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConformanceAction {
    Run {
        lanes: Vec<ConformanceTarget>,
        spec_version: String,
        server_era: ConformanceServerEra,
        results_dir: Option<PathBuf>,
    },
    Report {
        results_dir: Option<PathBuf>,
        output_dir: Option<PathBuf>,
    },
}

/// Fully resolved manual debugging operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebugAction {
    Inspect {
        topology: StackMode,
        protocol_version: ProtocolVersion,
        method: String,
        server_id: Option<String>,
    },
    Token {
        kind: TokenKind,
        server_id: Option<String>,
    },
}

/// Resolves a parsed CLI without starting child processes or mutating global state.
///
/// # Errors
///
/// Returns an error when a command needs `CF_MCP_STACK_MODE` and its value is
/// neither `controlplane` nor `dataplane`.
pub fn resolve_action(cli: Cli, environment: &Environment) -> Result<Action> {
    match cli.command {
        Command::Stack(args) => resolve_stack(args.command, environment).map(Action::Stack),
        Command::Probe(args) => {
            let topology = resolve_topology(args.lane, environment)?;
            Ok(Action::Probe {
                topology,
                protocol_version: resolve_protocol_version(
                    args.protocol_version,
                    environment,
                    ProtocolVersion::default(),
                )?,
            })
        }
        Command::Load(args) => {
            let topology = resolve_topology(args.target.lane, environment)?;
            Ok(Action::Load(ResolvedLoadArgs {
                topology,
                protocol_version: resolve_protocol_version(
                    args.target.protocol_version,
                    environment,
                    ProtocolVersion::default(),
                )?,
                request: LoadRequest {
                    smoke: args.smoke,
                    users: args.users,
                    spawn_rate: args.spawn_rate,
                    run_time: args.run_time,
                },
            }))
        }
        Command::Live(args) => {
            let lane = resolve_live_lane(args.target.lane, environment)?;
            if lane == LiveLane::Fixture && args.group != LiveGroup::Protocol {
                bail!("--lane fixture-direct requires --group protocol");
            }
            Ok(Action::Live {
                lane,
                group: args.group,
                protocol_version: resolve_protocol_version(
                    args.target.protocol_version,
                    environment,
                    ProtocolVersion::default(),
                )?,
            })
        }
        Command::Conformance(args) => Ok(Action::Conformance(match args.command {
            ConformanceCommand::Run(args) => ConformanceAction::Run {
                lanes: resolve_lanes(args.lane.into_iter().map(Into::into)),
                spec_version: args.protocol_version.to_string(),
                server_era: args.server_era.into(),
                results_dir: args.results_dir,
            },
            ConformanceCommand::Report(args) => ConformanceAction::Report {
                results_dir: args.results_dir,
                output_dir: args.output_dir,
            },
        })),
        Command::Debug(args) => Ok(Action::Debug(match args.command {
            DebugCommand::Inspect(args) => {
                let topology = resolve_topology(args.target.lane, environment)?;
                DebugAction::Inspect {
                    topology,
                    protocol_version: resolve_protocol_version(
                        args.target.protocol_version,
                        environment,
                        ProtocolVersion::default(),
                    )?,
                    method: args.method,
                    server_id: args.server_id,
                }
            }
            DebugCommand::Token(args) => {
                if args.kind == TokenKind::Admin && args.server_id.is_some() {
                    bail!("--server-id is only valid with --kind scoped");
                }
                DebugAction::Token {
                    kind: args.kind,
                    server_id: args.server_id,
                }
            }
        })),
    }
}

fn resolve_live_lane(lane: Option<CliLane>, environment: &Environment) -> Result<LiveLane> {
    Ok(match lane {
        Some(CliLane::FixtureDirect) => LiveLane::Fixture,
        Some(CliLane::BuiltInDataPlane) => LiveLane::BuiltInDataPlane,
        Some(CliLane::ExternalDataPlane) => LiveLane::ExternalDataPlane,
        None => match resolve_topology(None, environment)? {
            StackMode::Controlplane => LiveLane::BuiltInDataPlane,
            StackMode::Dataplane => LiveLane::ExternalDataPlane,
        },
    })
}

fn resolve_protocol_version(
    explicit: Option<ProtocolVersion>,
    environment: &Environment,
    fallback: ProtocolVersion,
) -> Result<ProtocolVersion> {
    if let Some(version) = explicit {
        return Ok(version);
    }
    let Some(value) = environment.get(OsStr::new(PROTOCOL_VERSION_ENV)) else {
        return Ok(fallback);
    };
    let value = value
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("{PROTOCOL_VERSION_ENV} must be UTF-8"))?;
    if value.is_empty() {
        return Ok(fallback);
    }
    ProtocolVersion::from_str(value)
        .map_err(|error| anyhow::anyhow!("invalid {PROTOCOL_VERSION_ENV}: {error}"))
}

fn resolve_stack(command: StackCommand, environment: &Environment) -> Result<StackAction> {
    match command {
        StackCommand::Up(args) => Ok(StackAction::Up {
            topology: resolve_topology(args.topology, environment)?,
            fresh: args.fresh,
        }),
        StackCommand::Down(args) => Ok(StackAction::Down {
            topology: args.topology.unwrap_or(TopologySelection::All),
            volumes: args.volumes,
        }),
        StackCommand::Status(args) => Ok(StackAction::Status(resolve_topology(
            args.topology,
            environment,
        )?)),
        StackCommand::Logs(args) => Ok(StackAction::Logs {
            topology: resolve_topology(args.topology, environment)?,
            services: args.services,
        }),
        StackCommand::Config(args) => Ok(StackAction::Config(resolve_topology(
            args.topology,
            environment,
        )?)),
    }
}

fn resolve_lanes(lanes: impl IntoIterator<Item = ConformanceTarget>) -> Vec<ConformanceTarget> {
    let selected = lanes.into_iter().collect::<BTreeSet<_>>();
    let all = [
        ConformanceTarget::Fixture,
        ConformanceTarget::BuiltInDataPlane,
        ConformanceTarget::ExternalDataPlane,
    ];
    if selected.is_empty() {
        all.into_iter().collect()
    } else {
        all.into_iter()
            .filter(|lane| selected.contains(lane))
            .collect()
    }
}

fn resolve_topology(explicit: Option<CliTopology>, environment: &Environment) -> Result<StackMode> {
    if let Some(topology) = explicit {
        return Ok(topology.into());
    }
    Ok(environment_topology(environment)?.unwrap_or(StackMode::Dataplane))
}

fn environment_topology(environment: &Environment) -> Result<Option<StackMode>> {
    let Some(value) = environment.get(OsStr::new(STACK_MODE_ENV)) else {
        return Ok(None);
    };
    match value.to_str() {
        Some("controlplane") => Ok(Some(StackMode::Controlplane)),
        Some("dataplane") => Ok(Some(StackMode::Dataplane)),
        _ => bail!(
            "invalid {STACK_MODE_ENV}; expected controlplane or dataplane (got {:?})",
            value
        ),
    }
}

/// Converts a CLI topology selection into its ordered stack modes.
pub(crate) fn selected_topologies(selection: TopologySelection) -> Vec<StackMode> {
    match selection {
        TopologySelection::Controlplane => vec![StackMode::Controlplane],
        TopologySelection::Dataplane => vec![StackMode::Dataplane],
        TopologySelection::All => vec![StackMode::Controlplane, StackMode::Dataplane],
    }
}

/// Converts one concrete stack mode into a CLI topology selection.
pub(crate) const fn topology_selection(topology: StackMode) -> TopologySelection {
    match topology {
        StackMode::Controlplane => TopologySelection::Controlplane,
        StackMode::Dataplane => TopologySelection::Dataplane,
    }
}
