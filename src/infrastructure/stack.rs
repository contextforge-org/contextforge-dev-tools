//! Pure stack lifecycle decisions and Docker Compose command plans.

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

/// One immutable stack command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StackCommandPlan {
    command: CommandSpec,
}

impl StackCommandPlan {
    /// Builds a mode-specific Compose `up` command.
    #[must_use]
    pub(crate) fn up(
        project: ComposeProject,
        mode: StackMode,
        build: bool,
        start_locust_ui: bool,
        locust_workers: usize,
    ) -> Self {
        let mut arguments = vec![
            OsString::from("up"),
            OsString::from("-d"),
            OsString::from("--remove-orphans"),
        ];
        if build {
            arguments.push(OsString::from("--build"));
        }
        if mode == StackMode::Controlplane && start_locust_ui {
            arguments.push(OsString::from("--scale"));
            arguments.push(OsString::from(format!("locust_worker={locust_workers}")));
        }
        Self {
            command: project.command(arguments),
        }
    }

    /// Builds a Compose command that stops one service without removing it.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn stop_service(project: ComposeProject, service: &str) -> Self {
        Self {
            command: project.command(["stop", "--timeout", "5", service]),
        }
    }

    /// Builds a Compose command that restarts one previously stopped service.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn start_service(project: ComposeProject, service: &str) -> Self {
        Self {
            command: project.command(["start", service]),
        }
    }

    /// Builds a Compose command that restarts one service without its dependencies.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn restart_service(project: ComposeProject, service: &str) -> Self {
        Self {
            command: project.command(["restart", "--timeout", "5", service]),
        }
    }

    /// Builds a Compose cleanup command.
    #[must_use]
    pub(crate) fn cleanup(project: ComposeProject, kind: CleanupKind) -> Self {
        let mut arguments = vec![OsString::from("down")];
        if kind == CleanupKind::Reset {
            arguments.push(OsString::from("--volumes"));
        }
        arguments.push(OsString::from("--remove-orphans"));
        Self {
            command: project.command(arguments),
        }
    }

    /// Builds a Compose service-status command.
    #[must_use]
    pub(crate) fn status(project: ComposeProject) -> Self {
        Self {
            command: project.command(["ps"]),
        }
    }

    /// Builds a Compose log-follow command, translating the public control-plane service name.
    #[must_use]
    pub(crate) fn logs<I>(project: ComposeProject, services: I) -> Self
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut arguments = vec![OsString::from("logs"), OsString::from("-f")];
        arguments.extend(services.into_iter().map(compose_service_name));
        Self {
            command: project.command(arguments),
        }
    }

    /// Builds a Compose rendered-config command.
    #[must_use]
    pub(crate) fn config(project: ComposeProject, mode: StackMode) -> Self {
        let arguments = if mode == StackMode::Dataplane {
            vec![
                OsString::from("--profile"),
                OsString::from("testing"),
                OsString::from("config"),
                OsString::from("--no-interpolate"),
                OsString::from("--no-env-resolution"),
            ]
        } else {
            vec![
                OsString::from("config"),
                OsString::from("--no-interpolate"),
                OsString::from("--no-env-resolution"),
            ]
        };
        Self {
            command: project.command(arguments),
        }
    }

    /// Returns the executable process specification.
    pub(crate) fn command(&self) -> &CommandSpec {
        &self.command
    }
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
