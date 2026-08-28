//! Command-line argument model.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use crate::mcp::protocol::PROTOCOL_VERSION;
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

const RUN_TIME_ERROR: &str =
    "must be a positive Locust duration using h, m, and s at most once in that order";
const PROTOCOL_VERSION_ERROR: &str = "must use the MCP YYYY-MM-DD version format";

fn parse_positive_usize(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| String::from("must be an integer greater than zero"))?;
    if parsed == 0 {
        Err(String::from("must be an integer greater than zero"))
    } else {
        Ok(parsed)
    }
}

fn parse_positive_f64(value: &str) -> Result<f64, String> {
    let parsed = value
        .parse::<f64>()
        .map_err(|_| String::from("must be a finite number greater than zero"))?;
    if parsed.is_finite() && parsed > 0.0 {
        Ok(parsed)
    } else {
        Err(String::from("must be a finite number greater than zero"))
    }
}

fn parse_run_time(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut position = 0;
    let mut previous_unit = None;

    if bytes.is_empty() {
        return Err(String::from(RUN_TIME_ERROR));
    }

    while position < bytes.len() {
        let number_start = position;
        while position < bytes.len() && bytes[position].is_ascii_digit() {
            position += 1;
        }
        if number_start == position {
            return Err(String::from(RUN_TIME_ERROR));
        }

        let amount = value[number_start..position]
            .parse::<u64>()
            .map_err(|_| String::from(RUN_TIME_ERROR))?;
        if amount == 0 {
            return Err(String::from(RUN_TIME_ERROR));
        }

        let unit = match bytes.get(position) {
            Some(b'h') => 0,
            Some(b'm') => 1,
            Some(b's') => 2,
            _ => return Err(String::from(RUN_TIME_ERROR)),
        };
        if previous_unit.is_some_and(|previous| unit <= previous) {
            return Err(String::from(RUN_TIME_ERROR));
        }
        previous_unit = Some(unit);
        position += 1;
    }

    Ok(value.to_owned())
}

/// Orchestrates control-plane and dataplane integration workflows.
#[derive(Debug, Clone, PartialEq, Parser)]
#[command(name = "cf-integration", version, arg_required_else_help = true)]
pub(crate) struct Cli {
    /// Workflow to run.
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Top-level integration workflow.
#[derive(Debug, Clone, PartialEq, Subcommand)]
pub(crate) enum Command {
    /// Manage Compose stacks.
    Stack(StackArgs),
    /// Probe one public MCP route.
    Probe(RoutedWorkflowTargetArgs),
    /// Run an MCP load test.
    Load(LoadArgs),
    /// Run upstream live gateway tests.
    Live(LiveArgs),
    /// Run and report official MCP conformance.
    Conformance(ConformanceArgs),
    /// Run manual debugging utilities.
    Debug(DebugArgs),
}

/// Stack command selection.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct StackArgs {
    /// Stack operation to run.
    #[command(subcommand)]
    pub(crate) command: StackCommand,
}

/// Operation on one or more Compose stacks.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub(crate) enum StackCommand {
    /// Start one stack topology.
    Up(StackUpArgs),
    /// Stop one or both stack topologies.
    Down(StackDownArgs),
    /// Show services for one stack topology.
    Status(TopologyArgs),
    /// Follow logs for one stack topology.
    Logs(StackLogsArgs),
    /// Render the merged configuration for one stack topology.
    Config(TopologyArgs),
}

/// Options for starting one stack.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct StackUpArgs {
    /// Stack topology; defaults to CF_MCP_STACK_MODE, then dataplane.
    #[arg(long, value_enum)]
    pub(crate) topology: Option<CliTopology>,

    /// Remove existing stack volumes before starting.
    #[arg(long)]
    pub(crate) fresh: bool,
}

/// Options for stopping stacks.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct StackDownArgs {
    /// Stack topology; defaults to all.
    #[arg(long, value_enum)]
    pub(crate) topology: Option<TopologySelection>,

    /// Remove persistent volumes as well as containers and networks.
    #[arg(long)]
    pub(crate) volumes: bool,
}

/// A command targeting one stack topology.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct TopologyArgs {
    /// Stack topology; defaults to CF_MCP_STACK_MODE, then dataplane.
    #[arg(long, value_enum)]
    pub(crate) topology: Option<CliTopology>,
}

/// Target selection for routed MCP workflows.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct RoutedWorkflowTargetArgs {
    /// Execution lane; defaults to CF_MCP_STACK_MODE, then dataplane.
    #[arg(long, value_enum)]
    pub(crate) lane: Option<CliTopology>,

    /// MCP version; defaults to MCP_PROTOCOL_VERSION, then 2026-07-28.
    #[arg(long)]
    pub(crate) protocol_version: Option<ProtocolVersion>,
}

/// Target selection for MCP workflows that support a direct fixture lane.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct WorkflowTargetArgs {
    /// Execution lane; defaults to CF_MCP_STACK_MODE, then dataplane.
    #[arg(long, value_enum)]
    pub(crate) lane: Option<CliLane>,

    /// MCP version; defaults to MCP_PROTOCOL_VERSION, then 2026-07-28.
    #[arg(long)]
    pub(crate) protocol_version: Option<ProtocolVersion>,
}

/// Options for following stack logs.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct StackLogsArgs {
    /// Stack topology; defaults to CF_MCP_STACK_MODE, then dataplane.
    #[arg(long, value_enum)]
    pub(crate) topology: Option<CliTopology>,

    /// Services whose logs to follow; all services when omitted.
    #[arg(value_name = "SERVICE")]
    pub(crate) services: Vec<OsString>,
}

/// A live stack topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum CliTopology {
    /// Python control plane only.
    Controlplane,
    /// Python control plane routed through the Rust dataplane.
    Dataplane,
}

impl From<CliTopology> for crate::infrastructure::StackMode {
    fn from(topology: CliTopology) -> Self {
        match topology {
            CliTopology::Controlplane => Self::Controlplane,
            CliTopology::Dataplane => Self::Dataplane,
        }
    }
}

/// One or both stack topologies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum TopologySelection {
    /// Python control plane only.
    Controlplane,
    /// Python control plane routed through the Rust dataplane.
    Dataplane,
    /// Run controlplane and dataplane sequentially.
    All,
}

/// Load-test options.
#[derive(Debug, Clone, PartialEq, Args)]
pub(crate) struct LoadArgs {
    /// Routed lane and protocol-version selection.
    #[command(flatten)]
    pub(crate) target: RoutedWorkflowTargetArgs,

    /// Use smoke-test settings.
    #[arg(long)]
    pub(crate) smoke: bool,

    /// Concurrent users; must be greater than zero.
    #[arg(long, value_parser = parse_positive_usize)]
    pub(crate) users: Option<usize>,

    /// Users spawned per second; must be finite and greater than zero.
    #[arg(long, value_parser = parse_positive_f64)]
    pub(crate) spawn_rate: Option<f64>,

    /// Locust duration using positive h, m, and s groups, such as 1h30m.
    #[arg(long, value_parser = parse_run_time)]
    pub(crate) run_time: Option<String>,
}

/// Upstream live-test options.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct LiveArgs {
    /// Shared lane and protocol-version selection.
    #[command(flatten)]
    pub(crate) target: WorkflowTargetArgs,

    /// Upstream live-test group.
    #[arg(long, value_enum, default_value = "all")]
    pub(crate) group: LiveGroup,
}

/// One MCP workflow execution lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum CliLane {
    /// Run directly against the workflow's reference fixture.
    FixtureDirect,
    /// Run the routed endpoint through the Python built-in dataplane.
    BuiltInDataPlane,
    /// Run the routed endpoint through the external Rust data plane.
    ExternalDataPlane,
}

/// Upstream live-test group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum LiveGroup {
    /// MCP route tests backed by Fast Time.
    Mcp,
    /// Authorization and multi-transport tests.
    Rbac,
    /// Protocol-specific gateway tests.
    Protocol,
    /// Run the MCP, RBAC, and protocol groups.
    All,
}

/// A syntactically valid date-based MCP protocol version shared by workflows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProtocolVersion(String);

impl ProtocolVersion {
    /// Returns the exact selected MCP protocol version.
    #[must_use]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ProtocolVersion {
    fn default() -> Self {
        Self(PROTOCOL_VERSION.to_owned())
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ProtocolVersion {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let bytes = value.as_bytes();
        let valid = bytes.len() == 10
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes
                .iter()
                .enumerate()
                .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit());
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(String::from(PROTOCOL_VERSION_ERROR))
        }
    }
}

/// Conformance command selection.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct ConformanceArgs {
    /// Conformance operation to run.
    #[command(subcommand)]
    pub(crate) command: ConformanceCommand,
}

/// Official MCP conformance workflows.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub(crate) enum ConformanceCommand {
    /// Run the pinned official oracle and TypeScript fixture.
    Run(ConformanceRunArgs),
    /// Regenerate the three-lane comparison from existing artifacts.
    Report(ConformanceReportArgs),
}

/// Official conformance run options.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct ConformanceRunArgs {
    /// Lane to run; repeat to select multiple lanes, defaults to all three.
    #[arg(long, value_enum, action = ArgAction::Append)]
    pub(crate) lane: Vec<CliLane>,

    /// MCP protocol version used by the official client; repeat for a matrix.
    #[arg(long = "protocol-version", action = ArgAction::Append)]
    pub(crate) protocol_version: Vec<ProtocolVersion>,

    /// Protocol era exposed by the fixture; repeat for a matrix.
    #[arg(long, value_enum, action = ArgAction::Append)]
    pub(crate) server_era: Vec<CliConformanceServerEra>,

    /// Result artifact root; defaults below CF_INTEGRATION_DIR.
    #[arg(long)]
    pub(crate) results_dir: Option<PathBuf>,

    /// Baseline root; defaults to tests/conformance/baselines.
    #[arg(long)]
    pub(crate) baseline_dir: Option<PathBuf>,

    /// Replace selected baselines atomically after every run succeeds.
    #[arg(long)]
    pub(crate) bless: bool,

    /// Report root; defaults to the repository reports directory.
    #[arg(long)]
    pub(crate) output_dir: Option<PathBuf>,
}

impl From<CliLane> for crate::conformance::results::SemanticLane {
    fn from(lane: CliLane) -> Self {
        match lane {
            CliLane::FixtureDirect => Self::FixtureDirect,
            CliLane::BuiltInDataPlane => Self::BuiltInDataPlane,
            CliLane::ExternalDataPlane => Self::ExternalDataPlane,
        }
    }
}

/// Protocol behavior exposed by the pinned upstream fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub(crate) enum CliConformanceServerEra {
    /// Accept both initialization-based and per-request clients.
    Dual,
    /// Accept only initialization-based clients.
    Legacy,
    /// Accept only per-request clients.
    Modern,
}

impl From<CliConformanceServerEra> for crate::conformance::results::ConformanceServerEra {
    fn from(era: CliConformanceServerEra) -> Self {
        match era {
            CliConformanceServerEra::Dual => Self::Dual,
            CliConformanceServerEra::Legacy => Self::Legacy,
            CliConformanceServerEra::Modern => Self::Modern,
        }
    }
}

/// Report-only options.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct ConformanceReportArgs {
    /// Existing result artifact root.
    #[arg(long)]
    pub(crate) results_dir: Option<PathBuf>,

    /// Markdown report directory; defaults to the repository reports directory.
    #[arg(long)]
    pub(crate) output_dir: Option<PathBuf>,
}

/// Debug command selection.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct DebugArgs {
    /// Debugging utility to run.
    #[command(subcommand)]
    pub(crate) command: DebugCommand,
}

/// Manual debugging utilities that are not compliance gates.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub(crate) enum DebugCommand {
    /// Debug a live endpoint with the official MCP Inspector.
    Inspect(InspectArgs),
    /// Request and print a token from a running control plane.
    Token(TokenArgs),
}

/// Official Inspector options.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct InspectArgs {
    /// Routed lane and protocol-version selection.
    #[command(flatten)]
    pub(crate) target: RoutedWorkflowTargetArgs,

    /// Inspector method such as tools/list.
    #[arg(long, default_value = "tools/list")]
    pub(crate) method: String,

    /// Existing virtual server ID; uses the configured/default fixture when omitted.
    #[arg(long)]
    pub(crate) server_id: Option<String>,
}

/// Token generation options.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct TokenArgs {
    /// Token privilege level.
    #[arg(long, value_enum)]
    pub(crate) kind: TokenKind,

    /// Virtual server restriction for a scoped token.
    #[arg(long)]
    pub(crate) server_id: Option<String>,
}

/// Token privilege level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum TokenKind {
    /// Catalog token with the minimum scopes needed by public MCP tests.
    Scoped,
    /// Authenticated platform-admin session token.
    Admin,
}
