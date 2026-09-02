//! CLI-to-runtime command resolution.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::{Component, PathBuf};
use std::str::FromStr;

use crate::conformance::profile::{
    DUAL_CLIENT_PROTOCOL_VERSIONS, LEGACY_CLIENT_PROTOCOL_VERSIONS, MODERN_CLIENT_PROTOCOL_VERSIONS,
};
use crate::conformance::results::{ConformanceServerEra, SemanticLane};
use crate::infrastructure::StackMode;
use crate::infrastructure::config::Environment;
use crate::performance::LoadRequest;
use anyhow::{Result, bail};

use crate::cli::{
    CiCommand, Cli, CliLane, CliRoutedLane, Command, ConformanceCommand, DebugCommand,
    LaneSelection, LiveGroup, ProtocolVersion, StackCommand, TokenKind,
};
const LANE_ENV: &str = "CF_MCP_LANE";
const PROTOCOL_VERSION_ENV: &str = "MCP_PROTOCOL_VERSION";

/// Fully resolved application operation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Action {
    Stack(StackAction),
    Probe {
        topology: StackMode,
        protocol_version: ProtocolVersion,
    },
    Load(ResolvedLoadArgs),
    Live {
        lane: SemanticLane,
        group: LiveGroup,
        protocol_version: ProtocolVersion,
    },
    Conformance(ConformanceAction),
    Debug(DebugAction),
    Ci(CiAction),
}

impl Action {
    /// Stable command path used by lifecycle output.
    #[must_use]
    pub(crate) const fn description(&self) -> &'static str {
        match self {
            Self::Stack(StackAction::Up { .. }) => "stack up",
            Self::Stack(StackAction::Down { .. }) => "stack down",
            Self::Stack(StackAction::Status(_)) => "stack status",
            Self::Stack(StackAction::Logs { .. }) => "stack logs",
            Self::Stack(StackAction::Config(_)) => "stack config",
            Self::Probe { .. } => "probe",
            Self::Load(_) => "load test",
            Self::Live { .. } => "live tests",
            Self::Conformance(ConformanceAction::Run { .. }) => "conformance tests",
            Self::Conformance(ConformanceAction::Report { .. }) => "conformance report",
            Self::Debug(DebugAction::Inspect { .. }) => "debug inspect",
            Self::Debug(DebugAction::Token { .. }) => "debug token",
            Self::Ci(CiAction::PrepareImage { .. }) => "prepare prebuilt CI image",
            Self::Ci(CiAction::PrepareRelease) => "prepare release state",
            Self::Ci(CiAction::SelectRelease) => "select release tag",
        }
    }

    /// Resolved execution context printed before the command starts.
    #[must_use]
    pub(crate) fn startup_summary(&self) -> String {
        match self {
            Self::Stack(action) => action.startup_summary(),
            Self::Probe {
                topology,
                protocol_version,
            }
            | Self::Debug(DebugAction::Inspect {
                topology,
                protocol_version,
                ..
            }) => lane_and_protocol(*topology, protocol_version),
            Self::Load(args) => lane_and_protocol(args.topology, &args.protocol_version),
            Self::Live {
                lane,
                protocol_version,
                ..
            } => format!(
                "Lane: {}\nProtocol version: {protocol_version}",
                lane.label()
            ),
            Self::Conformance(ConformanceAction::Run {
                lanes,
                client_eras,
                server_eras,
                ..
            }) => format!(
                "Lane: {}\nClient era: {}\nServer era: {}",
                join_lane_labels(lanes),
                join_client_eras(client_eras),
                join_server_eras(server_eras),
            ),
            Self::Conformance(ConformanceAction::Report { .. }) => String::from(
                "Lane: recorded conformance results\nClient era: recorded conformance results\nServer era: recorded conformance results",
            ),
            Self::Debug(DebugAction::Token { .. }) => {
                String::from("Lane: not applicable (token only)")
            }
            Self::Ci(CiAction::PrepareImage { .. }) => {
                String::from("CI operation: prepare prebuilt image")
            }
            Self::Ci(CiAction::PrepareRelease) => {
                String::from("CI operation: prepare release state")
            }
            Self::Ci(CiAction::SelectRelease) => String::from("CI operation: select release tag"),
        }
    }

    /// Returns whether the dispatcher should own one command-wide activity line.
    #[must_use]
    pub(crate) const fn uses_global_activity(&self) -> bool {
        !matches!(
            self,
            Self::Stack(StackAction::Up { .. })
                | Self::Load(_)
                | Self::Conformance(ConformanceAction::Run { .. })
        )
    }

    /// Returns whether this operation needs Compose overlays or runtime scripts.
    #[must_use]
    pub(crate) const fn requires_runtime_assets(&self) -> bool {
        !matches!(
            self,
            Self::Conformance(ConformanceAction::Report { .. })
                | Self::Debug(DebugAction::Token { .. })
                | Self::Ci(_)
        )
    }
}

impl StackAction {
    fn startup_summary(&self) -> String {
        let lane = match self {
            Self::Up { topology, .. }
            | Self::Status(topology)
            | Self::Logs { topology, .. }
            | Self::Config(topology) => topology.lane_label().to_owned(),
            Self::Down { lane, .. } => match lane {
                LaneSelection::Builtin => StackMode::Controlplane.lane_label().to_owned(),
                LaneSelection::External => StackMode::Dataplane.lane_label().to_owned(),
                LaneSelection::All => format!(
                    "{}, {}",
                    StackMode::Controlplane.lane_label(),
                    StackMode::Dataplane.lane_label()
                ),
            },
        };
        if matches!(self, Self::Up { .. }) {
            format!(
                "Lane: {lane}\nProtocol version: {}",
                ProtocolVersion::default()
            )
        } else {
            format!("Lane: {lane}")
        }
    }
}

fn lane_and_protocol(topology: StackMode, protocol_version: &ProtocolVersion) -> String {
    format!(
        "Lane: {}\nProtocol version: {protocol_version}",
        topology.lane_label()
    )
}

fn join_lane_labels(lanes: &[SemanticLane]) -> String {
    lanes
        .iter()
        .map(|lane| lane.label())
        .collect::<Vec<_>>()
        .join(", ")
}

fn join_client_eras(client_eras: &[ConformanceServerEra]) -> String {
    client_eras
        .iter()
        .map(|era| {
            let versions = match era {
                ConformanceServerEra::Dual => DUAL_CLIENT_PROTOCOL_VERSIONS,
                ConformanceServerEra::Legacy => LEGACY_CLIENT_PROTOCOL_VERSIONS,
                ConformanceServerEra::Modern => MODERN_CLIENT_PROTOCOL_VERSIONS,
            };
            format!("{} [{}]", era.label(), versions.join(", "))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn join_server_eras(server_eras: &[ConformanceServerEra]) -> String {
    server_eras
        .iter()
        .map(|era| format!("{} [{}]", era.label(), era.protocol_versions_label()))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Fully resolved stack operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StackAction {
    Up {
        topology: StackMode,
        fresh: bool,
    },
    Down {
        lane: LaneSelection,
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
pub(crate) struct ResolvedLoadArgs {
    pub(crate) topology: StackMode,
    pub(crate) protocol_version: ProtocolVersion,
    pub(crate) request: LoadRequest,
}

/// Fully resolved official conformance operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConformanceAction {
    Run {
        lanes: Vec<SemanticLane>,
        client_eras: Vec<ConformanceServerEra>,
        client_versions: Vec<String>,
        server_eras: Vec<ConformanceServerEra>,
        results_dir: Option<PathBuf>,
        baseline_dir: Option<PathBuf>,
        bless: bool,
        output_dir: Option<PathBuf>,
    },
    Report {
        results_dir: Option<PathBuf>,
        output_dir: Option<PathBuf>,
    },
}

/// Fully resolved manual debugging operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DebugAction {
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

/// Repository CI operation executed by the published CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CiAction {
    PrepareImage {
        artifact: String,
        binary: PathBuf,
        image: String,
        repository: String,
        revision: Option<String>,
        dockerfile: PathBuf,
        target: String,
        download_dir: PathBuf,
    },
    PrepareRelease,
    SelectRelease,
}

/// Resolves a parsed CLI without starting child processes or mutating global state.
///
/// # Errors
///
/// Returns an error when a command needs `CF_MCP_LANE` and its value is neither
/// `builtin` nor `external`.
pub(crate) fn resolve_action(cli: Cli, environment: &Environment) -> Result<Action> {
    match cli.command {
        Command::Stack(args) => resolve_stack(args.command, environment).map(Action::Stack),
        Command::Probe(args) => {
            let topology = resolve_lane(args.lane, environment)?;
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
            let topology = resolve_lane(args.target.lane, environment)?;
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
            if lane == SemanticLane::FixtureDirect && args.group != LiveGroup::Protocol {
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
            ConformanceCommand::Run(args) => {
                let (client_eras, client_versions) = resolve_client_eras(args.client_era);
                ConformanceAction::Run {
                    lanes: resolve_lanes(args.lane.into_iter().map(Into::into)),
                    client_eras,
                    client_versions,
                    server_eras: resolve_server_eras(args.server_era),
                    results_dir: args.results_dir,
                    baseline_dir: args.baseline_dir,
                    bless: args.bless,
                    output_dir: args.output_dir,
                }
            }
            ConformanceCommand::Report(args) => ConformanceAction::Report {
                results_dir: args.results_dir,
                output_dir: args.output_dir,
            },
        })),
        Command::Debug(args) => Ok(Action::Debug(match args.command {
            DebugCommand::Inspect(args) => {
                let topology = resolve_lane(args.target.lane, environment)?;
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
        Command::Ci(args) => Ok(Action::Ci(match args.command {
            CiCommand::PrepareImage(args) => {
                let mut components = args.binary.components();
                if !matches!(components.next(), Some(Component::Normal(_)))
                    || components.next().is_some()
                {
                    bail!("--binary must be one filename at the artifact root");
                }
                let repository = args
                    .repository
                    .or_else(|| environment_utf8(environment, "GITHUB_REPOSITORY"))
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("set --repository or GITHUB_REPOSITORY"))?;
                CiAction::PrepareImage {
                    artifact: args.artifact,
                    binary: args.binary,
                    image: args.image,
                    repository,
                    revision: args.revision,
                    dockerfile: args.dockerfile,
                    target: args.target,
                    download_dir: args.download_dir,
                }
            }
            CiCommand::PrepareRelease => CiAction::PrepareRelease,
            CiCommand::SelectRelease => CiAction::SelectRelease,
        })),
    }
}

fn environment_utf8(environment: &Environment, key: &str) -> Option<String> {
    environment
        .get(std::ffi::OsStr::new(key))
        .and_then(|value| value.to_str())
        .map(str::to_owned)
}

fn resolve_live_lane(lane: Option<CliLane>, environment: &Environment) -> Result<SemanticLane> {
    Ok(match lane {
        Some(CliLane::FixtureDirect) => SemanticLane::FixtureDirect,
        Some(CliLane::Builtin) => SemanticLane::BuiltInDataPlane,
        Some(CliLane::External) => SemanticLane::ExternalDataPlane,
        None => match resolve_lane(None, environment)? {
            StackMode::Controlplane => SemanticLane::BuiltInDataPlane,
            StackMode::Dataplane => SemanticLane::ExternalDataPlane,
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
            topology: resolve_lane(args.lane, environment)?,
            fresh: args.fresh,
        }),
        StackCommand::Down(args) => Ok(StackAction::Down {
            lane: args.lane.unwrap_or(LaneSelection::All),
            volumes: args.volumes,
        }),
        StackCommand::Status(args) => {
            Ok(StackAction::Status(resolve_lane(args.lane, environment)?))
        }
        StackCommand::Logs(args) => Ok(StackAction::Logs {
            topology: resolve_lane(args.lane, environment)?,
            services: args.services,
        }),
        StackCommand::Config(args) => {
            Ok(StackAction::Config(resolve_lane(args.lane, environment)?))
        }
    }
}

fn resolve_lanes(lanes: impl IntoIterator<Item = SemanticLane>) -> Vec<SemanticLane> {
    let selected = lanes.into_iter().collect::<BTreeSet<_>>();
    let all = [
        SemanticLane::FixtureDirect,
        SemanticLane::BuiltInDataPlane,
        SemanticLane::ExternalDataPlane,
    ];
    if selected.is_empty() {
        all.into_iter().collect()
    } else {
        all.into_iter()
            .filter(|lane| selected.contains(lane))
            .collect()
    }
}

fn resolve_client_eras(
    eras: Vec<crate::cli::CliConformanceEra>,
) -> (Vec<ConformanceServerEra>, Vec<String>) {
    let eras = if eras.is_empty() {
        vec![crate::cli::CliConformanceEra::Modern]
    } else {
        eras
    };
    let mut seen_eras = BTreeSet::new();
    let eras = eras
        .into_iter()
        .map(Into::into)
        .filter(|era| seen_eras.insert(*era))
        .collect::<Vec<ConformanceServerEra>>();
    let mut seen_versions = BTreeSet::new();
    let versions = eras
        .iter()
        .flat_map(|era| match era {
            ConformanceServerEra::Dual => DUAL_CLIENT_PROTOCOL_VERSIONS,
            ConformanceServerEra::Legacy => LEGACY_CLIENT_PROTOCOL_VERSIONS,
            ConformanceServerEra::Modern => MODERN_CLIENT_PROTOCOL_VERSIONS,
        })
        .map(|version| (*version).to_owned())
        .filter(|version| seen_versions.insert(version.clone()))
        .collect();
    (eras, versions)
}

fn resolve_server_eras(eras: Vec<crate::cli::CliConformanceEra>) -> Vec<ConformanceServerEra> {
    let eras = if eras.is_empty() {
        vec![
            crate::cli::CliConformanceEra::Legacy,
            crate::cli::CliConformanceEra::Modern,
        ]
    } else {
        eras
    };
    let mut seen = BTreeSet::new();
    eras.into_iter()
        .map(Into::into)
        .filter(|era| seen.insert(*era))
        .collect()
}

fn resolve_lane(explicit: Option<CliRoutedLane>, environment: &Environment) -> Result<StackMode> {
    if let Some(lane) = explicit {
        return Ok(lane.into());
    }
    Ok(environment_lane(environment)?.unwrap_or(StackMode::Dataplane))
}

fn environment_lane(environment: &Environment) -> Result<Option<StackMode>> {
    let Some(value) = environment.get(OsStr::new(LANE_ENV)) else {
        return Ok(None);
    };
    match value.to_str() {
        Some("builtin") => Ok(Some(StackMode::Controlplane)),
        Some("external") => Ok(Some(StackMode::Dataplane)),
        _ => bail!(
            "invalid {LANE_ENV}; expected builtin or external (got {:?})",
            value
        ),
    }
}

/// Converts a CLI lane selection into its ordered stack modes.
pub(crate) fn selected_topologies(selection: LaneSelection) -> Vec<StackMode> {
    match selection {
        LaneSelection::Builtin => vec![StackMode::Controlplane],
        LaneSelection::External => vec![StackMode::Dataplane],
        LaneSelection::All => vec![StackMode::Controlplane, StackMode::Dataplane],
    }
}

/// Converts one concrete stack mode into a CLI lane selection.
pub(crate) const fn topology_selection(topology: StackMode) -> LaneSelection {
    match topology {
        StackMode::Controlplane => LaneSelection::Builtin,
        StackMode::Dataplane => LaneSelection::External,
    }
}
