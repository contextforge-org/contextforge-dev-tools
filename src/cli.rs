//! Command-line argument model.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use crate::conformance::profile::DUAL_CLIENT_PROTOCOL_VERSIONS;
use crate::mcp::protocol::{LEGACY_PROTOCOL_VERSION, PROTOCOL_VERSION};
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

const RUN_TIME_ERROR: &str =
    "must be a positive Locust duration using h, m, and s at most once in that order";
const PROTOCOL_VERSION_ERROR: &str = "must be modern or legacy";

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

fn parse_memory_limit(value: &str) -> Result<String, String> {
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    let (amount, unit) = value.split_at(digits);
    let valid_amount = amount.parse::<u64>().is_ok_and(|amount| amount > 0);
    let valid_unit = matches!(unit.to_ascii_lowercase().as_str(), "b" | "k" | "m" | "g");
    if valid_amount && valid_unit {
        Ok(value.to_owned())
    } else {
        Err(String::from(
            "must be a positive Docker memory limit such as 16G",
        ))
    }
}

/// Orchestrates built-in and external dataplane integration workflows.
#[derive(Debug, Clone, PartialEq, Parser)]
#[command(name = "cf-integration", version, arg_required_else_help = true)]
pub(crate) struct Cli {
    /// Run the external dataplane with mocked Redis and no control plane.
    #[arg(short = 's', long, global = true)]
    pub(crate) standalone: bool,

    /// Workflow to run.
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Top-level integration workflow.
#[derive(Debug, Clone, PartialEq, Subcommand)]
pub(crate) enum Command {
    /// Manage Compose stacks.
    #[command(visible_alias = "s")]
    Stack(StackArgs),
    /// Probe one public MCP route.
    #[command(visible_alias = "p")]
    Probe(RoutedWorkflowTargetArgs),
    /// Run an MCP load test.
    #[command(visible_alias = "l")]
    Load(LoadArgs),
    /// Run upstream live gateway tests.
    #[command(visible_alias = "v")]
    Live(LiveArgs),
    /// Run and report official MCP conformance.
    #[command(visible_alias = "c")]
    Conformance(ConformanceArgs),
    /// Run manual debugging utilities.
    #[command(visible_alias = "d")]
    Debug(DebugArgs),
    /// Repository CI orchestration used by ContextForge workflows.
    #[command(hide = true)]
    Ci(CiArgs),
}

/// Internal CI command selection.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct CiArgs {
    /// CI operation to run.
    #[command(subcommand)]
    pub(crate) command: CiCommand,
}

/// Internal CI operations kept in the published binary instead of workflow scripts.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub(crate) enum CiCommand {
    /// Download an exact CI artifact and package it as a local Docker image.
    PrepareImage(CiPrepareImageArgs),
    /// Remove stale unpublished release state before release-plz runs.
    PrepareRelease,
    /// Select the release tag produced by or recoverable after release-plz.
    SelectRelease,
}

/// Options for packaging a prebuilt service binary from GitHub Actions.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct CiPrepareImageArgs {
    /// GitHub Actions artifact prefix; the exact checkout revision is appended.
    #[arg(long)]
    pub(crate) artifact: String,

    /// Binary filename at the root of the downloaded artifact.
    #[arg(long)]
    pub(crate) binary: PathBuf,

    /// Local Docker image tag to create.
    #[arg(long)]
    pub(crate) image: String,

    /// GitHub owner/repository; defaults to GITHUB_REPOSITORY.
    #[arg(long)]
    pub(crate) repository: Option<String>,

    /// Exact artifact revision; defaults to the current Git checkout.
    #[arg(long)]
    pub(crate) revision: Option<String>,

    /// Dockerfile containing the prebuilt image target.
    #[arg(long, default_value = "docker/Dockerfile")]
    pub(crate) dockerfile: PathBuf,

    /// Dockerfile target that copies from the prebuilt build context.
    #[arg(long, default_value = "conformance-prebuilt")]
    pub(crate) target: String,

    /// Generated artifact download directory.
    #[arg(long, default_value = ".integration/ci/prebuilt")]
    pub(crate) download_dir: PathBuf,
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
    /// Start one execution lane.
    #[command(visible_alias = "u")]
    Up(StackUpArgs),
    /// Stop one or both execution lanes.
    #[command(visible_alias = "d")]
    Down(StackDownArgs),
    /// Show services for one execution lane.
    #[command(visible_alias = "s")]
    Status(StackLaneArgs),
    /// Follow logs for one execution lane.
    #[command(visible_alias = "l")]
    Logs(StackLogsArgs),
    /// Render the merged configuration for one execution lane.
    #[command(visible_alias = "c")]
    Config(StackLaneArgs),
}

/// Options for starting one stack.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct StackUpArgs {
    /// Routed lane and protocol-version selection.
    #[command(flatten)]
    pub(crate) target: RoutedWorkflowTargetArgs,

    /// Remove existing stack volumes before starting.
    #[arg(short = 'f', long)]
    pub(crate) fresh: bool,
}

/// Options for stopping stacks.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct StackDownArgs {
    /// Execution lane; defaults to all.
    #[arg(short = 'l', long, value_enum)]
    pub(crate) lane: Option<LaneSelection>,

    /// Remove persistent volumes as well as containers and networks.
    #[arg(short = 'v', long)]
    pub(crate) volumes: bool,
}

/// A command targeting one stack lane.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct StackLaneArgs {
    /// Execution lane; defaults to CF_MCP_LANE, then external.
    #[arg(short = 'l', long, value_enum)]
    pub(crate) lane: Option<CliRoutedLane>,
}

/// Target selection for routed MCP workflows.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct RoutedWorkflowTargetArgs {
    /// Execution lane; defaults to CF_MCP_LANE, then external.
    #[arg(short = 'l', long, value_enum)]
    pub(crate) lane: Option<CliRoutedLane>,

    /// MCP mode; defaults to MCP_PROTOCOL_VERSION, then modern.
    #[arg(short = 'p', long, value_enum)]
    pub(crate) protocol_version: Option<ProtocolVersion>,
}

/// Target selection for MCP workflows that support a direct fixture lane.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct WorkflowTargetArgs {
    /// Execution lane; defaults to CF_MCP_LANE, then external.
    #[arg(short = 'l', long, value_enum)]
    pub(crate) lane: Option<CliLane>,

    /// MCP mode; defaults to MCP_PROTOCOL_VERSION, then modern.
    #[arg(short = 'p', long, value_enum)]
    pub(crate) protocol_version: Option<ProtocolVersion>,
}

/// Options for following stack logs.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct StackLogsArgs {
    /// Execution lane; defaults to CF_MCP_LANE, then external.
    #[arg(short = 'l', long, value_enum)]
    pub(crate) lane: Option<CliRoutedLane>,

    /// Services whose logs to follow; all services when omitted.
    #[arg(value_name = "SERVICE")]
    pub(crate) services: Vec<OsString>,
}

/// A routed MCP execution lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum CliRoutedLane {
    /// Route through the Python built-in dataplane.
    Builtin,
    /// Route through the external Rust dataplane.
    External,
}

impl From<CliRoutedLane> for crate::infrastructure::StackMode {
    fn from(lane: CliRoutedLane) -> Self {
        match lane {
            CliRoutedLane::Builtin => Self::Controlplane,
            CliRoutedLane::External => Self::Dataplane,
        }
    }
}

/// One or both routed MCP execution lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum LaneSelection {
    /// Route through the Python built-in dataplane.
    Builtin,
    /// Route through the external Rust dataplane.
    External,
    /// Run the built-in and external lanes sequentially.
    All,
}

/// Load command selection.
#[derive(Debug, Clone, PartialEq, Args)]
pub(crate) struct LoadArgs {
    /// Load operation to run.
    #[command(subcommand)]
    pub(crate) command: LoadCommand,
}

/// MCP load workflows.
#[derive(Debug, Clone, PartialEq, Subcommand)]
pub(crate) enum LoadCommand {
    /// Run Locust through the selected public MCP route.
    #[command(visible_alias = "r")]
    Run(LoadRunArgs),
}

/// Load-test options.
#[derive(Debug, Clone, PartialEq, Args)]
pub(crate) struct LoadRunArgs {
    /// Execution lane; defaults to CF_MCP_LANE, then external.
    #[arg(short = 'l', long, value_enum)]
    pub(crate) lane: Option<CliRoutedLane>,

    /// Protocol era used by the load client; every lane uses Fast Time.
    #[arg(short = 'c', long, value_enum, default_value = "modern")]
    pub(crate) client_era: ProtocolVersion,

    /// Enable the ClickStack observability UI during the load test.
    #[arg(short = 'o', long)]
    pub(crate) observability: bool,

    /// Use smoke-test settings.
    #[arg(short = 'S', long)]
    pub(crate) smoke: bool,

    /// Concurrent users; must be greater than zero.
    #[arg(short = 'u', long, value_parser = parse_positive_usize)]
    pub(crate) users: Option<usize>,

    /// Users spawned per second; must be finite and greater than zero.
    #[arg(short = 'r', long, value_parser = parse_positive_f64)]
    pub(crate) spawn_rate: Option<f64>,

    /// Locust duration using positive h, m, and s groups, such as 1h30m.
    #[arg(short = 't', long, value_parser = parse_run_time)]
    pub(crate) run_time: Option<String>,

    /// Local Locust worker processes; must be greater than zero.
    #[arg(short = 'w', long, value_parser = parse_positive_usize)]
    pub(crate) workers: Option<usize>,

    /// Built-in gateway container memory limit, such as 16G.
    #[arg(short = 'm', long, value_parser = parse_memory_limit)]
    pub(crate) builtin_memory_limit: Option<String>,

    /// Split Docker CPUs evenly between the target and Locust.
    #[arg(short = 'i', long)]
    pub(crate) isolate_cpus: bool,
}

/// Upstream live-test options.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct LiveArgs {
    /// Shared lane and protocol-version selection.
    #[command(flatten)]
    pub(crate) target: WorkflowTargetArgs,

    /// Upstream live-test group.
    #[arg(short = 'g', long, value_enum, default_value = "all")]
    pub(crate) group: LiveGroup,
}

/// One MCP workflow execution lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum CliLane {
    /// Run directly against the workflow's reference fixture.
    FixtureDirect,
    /// Run the routed endpoint through the Python built-in dataplane.
    Builtin,
    /// Run the routed endpoint through the external Rust data plane.
    External,
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

/// Semantic MCP protocol mode shared by operational workflows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum ProtocolVersion {
    /// Use the latest per-request, stateless MCP revision.
    #[default]
    Modern,
    /// Use the latest initialization-based MCP revision.
    Legacy,
}

impl ProtocolVersion {
    /// Returns the exact MCP wire revision selected by this mode.
    #[must_use]
    pub(crate) const fn wire_version(self) -> &'static str {
        match self {
            Self::Modern => PROTOCOL_VERSION,
            Self::Legacy => LEGACY_PROTOCOL_VERSION,
        }
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Modern => "modern",
            Self::Legacy => "legacy",
        })
    }
}

impl FromStr for ProtocolVersion {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "modern" => Ok(Self::Modern),
            "legacy" => Ok(Self::Legacy),
            _ => Err(String::from(PROTOCOL_VERSION_ERROR)),
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
    #[command(visible_alias = "r")]
    Run(ConformanceRunArgs),
    /// Regenerate the three-lane comparison from existing artifacts.
    #[command(visible_alias = "p")]
    Report(ConformanceReportArgs),
}

/// Official conformance run options.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct ConformanceRunArgs {
    /// Lane to run; repeat to select multiple lanes, defaults to all three.
    #[arg(short = 'l', long, value_enum, action = ArgAction::Append)]
    pub(crate) lane: Vec<CliLane>,

    /// Protocol era used by the official client; repeat for a matrix.
    #[arg(short = 'c', long, value_enum, action = ArgAction::Append)]
    pub(crate) client_era: Vec<CliConformanceEra>,

    /// Exact client protocol revision; repeat to select a matrix instead of an era.
    #[arg(
        short = 'C', long,
        value_parser = clap::builder::PossibleValuesParser::new(DUAL_CLIENT_PROTOCOL_VERSIONS.iter().copied()),
        conflicts_with = "client_era",
        action = ArgAction::Append
    )]
    pub(crate) client_version: Vec<String>,

    /// Protocol era exposed by the fixture; repeat for a matrix.
    #[arg(short = 'e', long, value_enum, action = ArgAction::Append)]
    pub(crate) server_era: Vec<CliConformanceEra>,

    /// Result artifact root; defaults below CF_INTEGRATION_DIR.
    #[arg(short = 'r', long)]
    pub(crate) results_dir: Option<PathBuf>,

    /// Baseline root; defaults to tests/conformance/baselines.
    #[arg(short = 'b', long)]
    pub(crate) baseline_dir: Option<PathBuf>,

    /// Replace selected baselines atomically after every run succeeds.
    #[arg(short = 'B', long)]
    pub(crate) bless: bool,

    /// Report root; defaults to the repository reports directory.
    #[arg(short = 'o', long)]
    pub(crate) output_dir: Option<PathBuf>,
}

impl From<CliLane> for crate::conformance::results::SemanticLane {
    fn from(lane: CliLane) -> Self {
        match lane {
            CliLane::FixtureDirect => Self::FixtureDirect,
            CliLane::Builtin => Self::BuiltInDataPlane,
            CliLane::External => Self::ExternalDataPlane,
        }
    }
}

/// Protocol behavior selected for one side of the conformance matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub(crate) enum CliConformanceEra {
    /// Select initialization-based and per-request protocol revisions.
    Dual,
    /// Select only initialization-based protocol revisions.
    Legacy,
    /// Select only per-request protocol revisions.
    Modern,
}

impl From<CliConformanceEra> for crate::conformance::results::ConformanceServerEra {
    fn from(era: CliConformanceEra) -> Self {
        match era {
            CliConformanceEra::Dual => Self::Dual,
            CliConformanceEra::Legacy => Self::Legacy,
            CliConformanceEra::Modern => Self::Modern,
        }
    }
}

/// Report-only options.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct ConformanceReportArgs {
    /// Existing result artifact root.
    #[arg(short = 'r', long)]
    pub(crate) results_dir: Option<PathBuf>,

    /// Markdown report directory; defaults to the repository reports directory.
    #[arg(short = 'o', long)]
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
    #[command(visible_alias = "i")]
    Inspect(InspectArgs),
    /// Request and print a token from a running control plane.
    #[command(visible_alias = "t")]
    Token(TokenArgs),
}

/// Official Inspector options.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct InspectArgs {
    /// Routed lane and protocol-version selection.
    #[command(flatten)]
    pub(crate) target: RoutedWorkflowTargetArgs,

    /// Inspector method such as tools/list.
    #[arg(short = 'm', long, default_value = "tools/list")]
    pub(crate) method: String,

    /// Existing virtual server ID; uses the configured/default fixture when omitted.
    #[arg(short = 'i', long)]
    pub(crate) server_id: Option<String>,
}

/// Token generation options.
#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub(crate) struct TokenArgs {
    /// Token privilege level.
    #[arg(short = 'k', long, value_enum)]
    pub(crate) kind: TokenKind,

    /// Virtual server restriction for a scoped token.
    #[arg(short = 'i', long)]
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
