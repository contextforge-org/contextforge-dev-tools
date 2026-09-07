//! Stack lifecycle decisions and Docker Compose commands.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::str::FromStr;

use crate::infrastructure::StackMode;
use crate::infrastructure::compose::{ComposeProject, SERVICE_DISPLAY_NAMES};
use crate::infrastructure::process::CommandSpec;

/// User-selected Compose image build policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuildMode {
    /// Decide from image availability and checkout revision labels.
    Auto,
    /// Always ask Compose to build.
    Always,
    /// Never ask Compose to build.
    Never,
}

impl FromStr for BuildMode {
    type Err = BuildModeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "true" | "1" => Ok(Self::Always),
            "false" | "0" => Ok(Self::Never),
            _ => Err(BuildModeParseError(value.to_owned())),
        }
    }
}

/// Invalid `CF_COMPOSE_BUILD` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuildModeParseError(String);

impl fmt::Display for BuildModeParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid CF_COMPOSE_BUILD={}; use auto, true, or false",
            self.0
        )
    }
}

impl std::error::Error for BuildModeParseError {}

/// Runtime facts needed to resolve automatic image builds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuildInputs {
    pub(crate) controlplane_image_prebuilt: bool,
    pub(crate) controlplane_image_present: bool,
    pub(crate) controlplane_checkout_revision: Option<String>,
    pub(crate) controlplane_image_revision: Option<String>,
    pub(crate) include_dataplane: bool,
    pub(crate) dataplane_source_ref: Option<String>,
    pub(crate) dataplane_image_present: bool,
    pub(crate) dataplane_checkout_revision: Option<String>,
    pub(crate) dataplane_image_revision: Option<String>,
}

/// Resolved Compose build decision and stable operator diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuildDecision {
    pub(crate) build: bool,
    pub(crate) reasons: Vec<String>,
}

/// Resolves `CF_COMPOSE_BUILD` without running Docker or Git.
#[must_use]
pub(crate) fn resolve_build(mode: BuildMode, inputs: &BuildInputs) -> BuildDecision {
    match mode {
        BuildMode::Always => BuildDecision {
            build: true,
            reasons: vec!["explicit build enabled".to_owned()],
        },
        BuildMode::Never => BuildDecision {
            build: false,
            reasons: vec!["explicit build disabled".to_owned()],
        },
        BuildMode::Auto => resolve_auto_build(inputs),
    }
}

fn resolve_auto_build(inputs: &BuildInputs) -> BuildDecision {
    let mut reasons = Vec::new();
    if inputs.controlplane_image_prebuilt {
        reasons.push("prebuilt cf-controlplane image selected".to_owned());
    } else if !inputs.controlplane_image_present {
        reasons.push("cf-controlplane image missing".to_owned());
    } else if !matching_revision(
        inputs.controlplane_checkout_revision.as_deref(),
        inputs.controlplane_image_revision.as_deref(),
    ) {
        reasons.push("cf-controlplane image revision is stale".to_owned());
    }

    let controlplane_build = !inputs.controlplane_image_prebuilt
        && (!inputs.controlplane_image_present
            || !matching_revision(
                inputs.controlplane_checkout_revision.as_deref(),
                inputs.controlplane_image_revision.as_deref(),
            ));

    // This condition intentionally depends on the actual configured source
    // ref. The shell predecessor once treated a function name as a truthy
    // string and incorrectly required source-image freshness in published mode.
    let dataplane_source_enabled = inputs.include_dataplane
        && inputs
            .dataplane_source_ref
            .as_deref()
            .is_some_and(|reference| !reference.is_empty());
    let dataplane_build = dataplane_source_enabled
        && (!inputs.dataplane_image_present
            || !matching_revision(
                inputs.dataplane_checkout_revision.as_deref(),
                inputs.dataplane_image_revision.as_deref(),
            ));
    if dataplane_source_enabled {
        if !inputs.dataplane_image_present {
            reasons.push("cf-dataplane image missing".to_owned());
        } else if dataplane_build {
            reasons.push("cf-dataplane image revision is stale".to_owned());
        }
    }

    if reasons.is_empty() {
        reasons.push("all source images match their checkouts".to_owned());
    }
    BuildDecision {
        build: controlplane_build || dataplane_build,
        reasons,
    }
}

fn matching_revision(checkout: Option<&str>, image: Option<&str>) -> bool {
    matches!((checkout, image), (Some(checkout), Some(image)) if !checkout.is_empty() && checkout == image)
}

/// Destructive scope of a Compose cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CleanupKind {
    /// Remove containers and networks while retaining volumes.
    Down,
    /// Remove containers, networks, and volumes.
    Reset,
}

/// Builds a mode-specific Compose `up` command.
pub(crate) fn stack_up_command(
    project: ComposeProject,
    mode: StackMode,
    build: bool,
    start_locust_ui: bool,
    locust_workers: usize,
) -> CommandSpec {
    let mut command = project.command(["up", "-d", "--remove-orphans"]);
    if build {
        command = command.arg("--build");
    }
    if mode == StackMode::Controlplane && start_locust_ui {
        command = command.args(["--scale", &format!("locust_worker={locust_workers}")]);
    }
    command
}

pub(crate) fn stack_cleanup_command(project: ComposeProject, kind: CleanupKind) -> CommandSpec {
    let mut command = project.command(["down"]);
    if kind == CleanupKind::Reset {
        command = command.arg("--volumes");
    }
    command.arg("--remove-orphans")
}

/// Translates public display names to Compose service names.
pub(crate) fn stack_logs_command(
    project: ComposeProject,
    services: impl IntoIterator<Item = OsString>,
) -> CommandSpec {
    project
        .command(["logs", "-f"])
        .args(services.into_iter().map(compose_service_name))
}

pub(crate) fn stack_config_command(project: ComposeProject, mode: StackMode) -> CommandSpec {
    let mut command = project.command(std::iter::empty::<&str>());
    if mode == StackMode::Dataplane {
        command = command.args(["--profile", "testing"]);
    }
    command.args(["config", "--no-interpolate", "--no-env-resolution"])
}

fn compose_service_name(service: OsString) -> OsString {
    let Some(display_name) = service.to_str() else {
        return service;
    };
    SERVICE_DISPLAY_NAMES
        .iter()
        .find_map(|&(compose_name, public_name)| {
            (display_name == public_name).then(|| OsString::from(compose_name))
        })
        .unwrap_or(service)
}

/// Captured runtime state for one Compose service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServiceSnapshot {
    pub(crate) running: bool,
    pub(crate) completed_successfully: bool,
    pub(crate) configured_image: Option<String>,
    pub(crate) running_image_matches_configured: bool,
    pub(crate) image_revision: Option<String>,
}

/// Facts used to decide whether a running dataplane stack is current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FreshnessSnapshot {
    pub(crate) services: BTreeMap<String, ServiceSnapshot>,
    pub(crate) controlplane_checkout_revision: Option<String>,
    pub(crate) dataplane_checkout_revision: Option<String>,
    pub(crate) controlplane_image_prebuilt: bool,
    pub(crate) dataplane_source_enabled: bool,
    pub(crate) expected_controlplane_image: String,
    pub(crate) expected_dataplane_image: String,
    pub(crate) expected_fast_time_image: String,
}

/// Result of evaluating a running dataplane stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StackFreshness {
    /// Every required service, image, and revision is current.
    Current,
    /// The first deterministic freshness failure.
    Stale(String),
}

impl FreshnessSnapshot {
    /// Evaluates the shell-compatible stack freshness contract.
    #[must_use]
    pub(crate) fn evaluate(&self) -> StackFreshness {
        for service in [
            "gateway",
            "dataplane",
            "nginx",
            "postgres",
            "pgbouncer",
            "redis",
            "fast_time_server",
        ] {
            if !self
                .services
                .get(service)
                .is_some_and(|snapshot| snapshot.running)
            {
                return stale(format!("service is not running: {service}"));
            }
        }
        for service in ["migration", "register_fast_time"] {
            if !self
                .services
                .get(service)
                .is_some_and(|snapshot| snapshot.completed_successfully)
            {
                return stale(format!(
                    "setup service did not complete successfully: {service}"
                ));
            }
        }

        for (service, expected, label) in [
            (
                "gateway",
                self.expected_controlplane_image.as_str(),
                "cf-controlplane",
            ),
            (
                "dataplane",
                self.expected_dataplane_image.as_str(),
                "cf-dataplane",
            ),
            (
                "fast_time_server",
                self.expected_fast_time_image.as_str(),
                "fast_time_server",
            ),
        ] {
            let matches = self.services.get(service).is_some_and(|snapshot| {
                snapshot.configured_image.as_deref() == Some(expected)
                    && snapshot.running_image_matches_configured
            });
            if !matches {
                return stale(format!("{label} image differs"));
            }
        }

        if !self.controlplane_image_prebuilt
            && !service_revision_matches(
                &self.services,
                "gateway",
                self.controlplane_checkout_revision.as_deref(),
            )
        {
            return stale("cf-controlplane branch revision differs");
        }
        if self.dataplane_source_enabled
            && !service_revision_matches(
                &self.services,
                "dataplane",
                self.dataplane_checkout_revision.as_deref(),
            )
        {
            return stale("cf-dataplane branch revision differs");
        }

        StackFreshness::Current
    }
}

fn service_revision_matches(
    services: &BTreeMap<String, ServiceSnapshot>,
    service: &str,
    checkout_revision: Option<&str>,
) -> bool {
    services
        .get(service)
        .and_then(|snapshot| snapshot.image_revision.as_deref())
        .zip(checkout_revision)
        .is_some_and(|(image, checkout)| !checkout.is_empty() && image == checkout)
}

fn stale(message: impl Into<String>) -> StackFreshness {
    StackFreshness::Stale(message.into())
}
