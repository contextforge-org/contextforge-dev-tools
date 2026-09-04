//! Docker Compose command construction and rendered-contract validation.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::infrastructure::process::CommandSpec;

const LEGACY_FAST_TIME_IMAGE_PREFIXES: &[&str] = &[
    "ghcr.io/ibm/fast-time-server:",
    "ghcr.io/ibm/fast-time-server@",
];

/// Compose service keys and their public container display names.
pub(crate) const SERVICE_DISPLAY_NAMES: &[(&str, &str)] = &[
    ("auth_keygen", "cf-dataplane-auth-keygen"),
    ("gateway", "cf-controlplane"),
    ("migration", "cf-migration"),
    ("register_fast_time", "cf-register-fast-time"),
    ("fast_time_server", "cf-fast-time-server"),
    ("nginx", "cf-nginx"),
    ("postgres", "cf-postgres"),
    ("pgbouncer", "cf-pgbouncer"),
    ("redis", "cf-redis"),
    ("dataplane", "cf-dataplane"),
    ("config_writer", "cf-dataplane-config-writer"),
    ("locust", "cf-locust"),
    ("locust_worker", "cf-locust-worker"),
    ("locust_token", "cf-locust-token"),
    ("a2a_echo_agent", "cf-a2a-echo-agent"),
    ("a2a_echo_agent_v0_3_0", "cf-a2a-echo-agent-v0-3-0"),
    ("register_a2a_echo", "cf-register-a2a-echo"),
    ("mcp_inspector", "cf-mcp-inspector"),
    ("keycloak", "cf-keycloak"),
    ("mcp_conformance_server", "cf-conformance-server"),
    ("mcp_conformance_proxy", "cf-conformance-proxy"),
    ("clickstack", "cf-clickstack"),
];

/// Immutable Compose project invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeProject {
    project_name: OsString,
    files: Vec<PathBuf>,
    profiles: Vec<OsString>,
}

impl ComposeProject {
    /// Builds the standalone official MCP conformance fixture project.
    #[must_use]
    pub(crate) fn conformance_fixture(repository_root: &Path, project_name: OsString) -> Self {
        Self {
            project_name,
            files: vec![
                repository_root
                    .join("docker")
                    .join("docker-compose.cf-conformance-fixture.yaml"),
            ],
            profiles: vec![OsString::from("conformance")],
        }
    }

    /// Builds the independently managed local ClickStack project.
    #[must_use]
    pub(crate) fn observability(repository_root: &Path, project_name: OsString) -> Self {
        Self {
            project_name,
            files: vec![
                repository_root
                    .join("docker")
                    .join("docker-compose.cf-telemetry.yaml"),
            ],
            profiles: Vec::new(),
        }
    }

    /// Builds the control-plane plus dataplane overlay project.
    #[must_use]
    pub(crate) fn dataplane(
        repository_root: &Path,
        controlplane_checkout: &Path,
        project_name: OsString,
        build_dataplane: bool,
    ) -> Self {
        let mut files = vec![
            controlplane_checkout.join("docker-compose.yml"),
            repository_root
                .join("docker")
                .join("docker-compose.cf-controlplane-build-labels.yaml"),
            repository_root
                .join("docker")
                .join("docker-compose.cf-dataplane.yaml"),
            repository_root
                .join("docker")
                .join("docker-compose.cf-integration.yaml"),
            repository_root
                .join("docker")
                .join("docker-compose.cf-dataplane-config.yaml"),
        ];
        if build_dataplane {
            files.push(
                repository_root
                    .join("docker")
                    .join("docker-compose.cf-dataplane-build.yaml"),
            );
        }
        Self {
            project_name,
            files,
            profiles: Vec::new(),
        }
    }

    /// Builds the minimal external-dataplane project used by standalone workflows.
    #[must_use]
    pub(crate) fn standalone_dataplane(
        repository_root: &Path,
        project_name: OsString,
        build_dataplane: bool,
    ) -> Self {
        let mut files = vec![
            repository_root
                .join("docker")
                .join("docker-compose.cf-dataplane-standalone.yaml"),
            repository_root
                .join("docker")
                .join("docker-compose.cf-dataplane-config.yaml"),
        ];
        if build_dataplane {
            files.push(
                repository_root
                    .join("docker")
                    .join("docker-compose.cf-dataplane-build.yaml"),
            );
        }
        Self {
            project_name,
            files,
            profiles: Vec::new(),
        }
    }

    /// Builds the stock control-plane-only project.
    #[must_use]
    pub(crate) fn controlplane(
        repository_root: &Path,
        controlplane_checkout: &Path,
        project_name: OsString,
        enable_sso: bool,
    ) -> Self {
        let mut profiles = Vec::new();
        if enable_sso {
            profiles.push(OsString::from("sso"));
        }
        Self {
            project_name,
            files: vec![
                controlplane_checkout.join("docker-compose.yml"),
                repository_root
                    .join("docker")
                    .join("docker-compose.cf-controlplane-build-labels.yaml"),
            ],
            profiles,
        }
    }

    /// Ordered Compose override files.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// Explicitly enabled Compose profiles.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn profiles(&self) -> &[OsString] {
        &self.profiles
    }

    /// Replaces the enabled profile set, primarily for exhaustive cleanup.
    #[must_use]
    pub(crate) fn with_profiles<I, S>(mut self, profiles: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.profiles = profiles.into_iter().map(Into::into).collect();
        self
    }

    /// Applies the official MCP conformance fixture's Compose overrides.
    ///
    /// This is separate from enabling the fixture profile so services affected
    /// by the overlay start with the required configuration before the
    /// profile-gated fixture itself is launched.
    #[must_use]
    pub(crate) fn with_conformance_overlay(mut self, repository_root: &Path) -> Self {
        let fixture = repository_root
            .join("docker")
            .join("docker-compose.cf-conformance-fixture.yaml");
        if !self.files.contains(&fixture) {
            self.files.push(fixture);
        }
        let overlay = repository_root
            .join("docker")
            .join("docker-compose.cf-conformance.yaml");
        if !self.files.contains(&overlay) {
            self.files.push(overlay);
        }
        self
    }

    /// Applies conformance-only settings to the control-plane service.
    #[must_use]
    pub(crate) fn with_controlplane_conformance_overlay(mut self, repository_root: &Path) -> Self {
        let overlay = repository_root
            .join("docker")
            .join("docker-compose.cf-conformance-controlplane.yaml");
        if !self.files.contains(&overlay) {
            self.files.push(overlay);
        }
        self
    }

    /// Applies the control-plane runtime settings used by conformance runs.
    #[must_use]
    pub(crate) fn with_conformance_runtime(mut self, repository_root: &Path) -> Self {
        let overlay = repository_root
            .join("docker")
            .join("docker-compose.cf-conformance-runtime.yaml");
        if !self.files.contains(&overlay) {
            self.files.push(overlay);
        }
        self
    }

    /// Enables the local ClickStack instance and OTLP exporters for routed services.
    #[must_use]
    pub(crate) fn with_observability(
        mut self,
        repository_root: &Path,
        include_dataplane: bool,
    ) -> Self {
        let controlplane = repository_root
            .join("docker")
            .join("docker-compose.cf-controlplane-observability.yaml");
        if !self.files.contains(&controlplane) {
            self.files.push(controlplane);
        }
        if include_dataplane {
            let overlay = repository_root
                .join("docker")
                .join("docker-compose.cf-dataplane-observability.yaml");
            if !self.files.contains(&overlay) {
                self.files.push(overlay);
            }
        }
        self
    }

    /// Enables only the external dataplane OTLP exporters.
    #[must_use]
    pub(crate) fn with_dataplane_observability(mut self, repository_root: &Path) -> Self {
        let overlay = repository_root
            .join("docker")
            .join("docker-compose.cf-dataplane-observability.yaml");
        if !self.files.contains(&overlay) {
            self.files.push(overlay);
        }
        self
    }

    /// Enables the isolated official MCP conformance server fixture.
    #[must_use]
    pub(crate) fn with_conformance_fixture(self, repository_root: &Path) -> Self {
        let mut project = self.with_conformance_overlay(repository_root);

        let profile = OsString::from("conformance");
        if !project.profiles.contains(&profile) {
            project.profiles.push(profile);
        }

        project
    }

    /// Creates a `docker compose` command with project, files, and profiles.
    pub(crate) fn command<I, S>(&self, arguments: I) -> CommandSpec
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut command = CommandSpec::new("docker")
            .arg("compose")
            .arg("-p")
            .arg(self.project_name.clone());
        for file in &self.files {
            command = command.arg("-f").arg(file.as_os_str().to_owned());
        }
        for profile in &self.profiles {
            command = command.arg("--profile").arg(profile.clone());
        }
        command.args(arguments)
    }
}

/// One deterministic integration Compose contract violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContractViolation(String);

impl fmt::Display for ContractViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Validates the rendered integration Compose configuration.
///
/// The order of returned violations is stable and intended for operator-facing
/// diagnostics and regression tests.
#[must_use]
pub(crate) fn validate_integration_contract(
    rendered: &Value,
    expected_fast_time_image: &str,
) -> Vec<ContractViolation> {
    let Some(services) = rendered.get("services").and_then(Value::as_object) else {
        return vec![violation("rendered Compose config has no services object")];
    };
    let mut violations = Vec::new();

    for &(service_name, expected_name) in SERVICE_DISPLAY_NAMES {
        let Some(service) = services.get(service_name) else {
            continue;
        };
        let actual_name = service
            .get("labels")
            .and_then(|labels| labels.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if actual_name != expected_name {
            violations.push(violation(format!(
                "{service_name} display name is {actual_name:?}; expected {expected_name:?}"
            )));
        }
    }

    match services.get("fast_time_server") {
        None => violations.push(violation(
            "fast_time_server is missing from the integration compose config",
        )),
        Some(service) => {
            let image = service.get("image").and_then(Value::as_str).unwrap_or("");
            if image != expected_fast_time_image {
                violations.push(violation(format!(
                    "fast_time_server image is {image:?}; expected {expected_fast_time_image:?}"
                )));
            }
        }
    }

    let mut service_names = services.keys().collect::<Vec<_>>();
    service_names.sort_unstable();
    for service_name in service_names {
        let image = services[service_name]
            .get("image")
            .and_then(Value::as_str)
            .unwrap_or("");
        if LEGACY_FAST_TIME_IMAGE_PREFIXES
            .iter()
            .any(|prefix| image.starts_with(prefix))
        {
            violations.push(violation(format!(
                "{service_name} uses legacy Fast Time image {image:?}"
            )));
        }
    }

    let registration_command = services
        .get("register_fast_time")
        .and_then(|service| service.get("command"))
        .map(command_text)
        .unwrap_or_default();
    if !registration_command.contains("http://fast_time_server:9080/health") {
        violations.push(violation(
            "register_fast_time does not wait for fast_time_server on port 9080",
        ));
    }
    if !registration_command.contains("http://fast_time_server:9080/mcp") {
        violations.push(violation(
            "register_fast_time does not register the streamable HTTP endpoint at /mcp",
        ));
    }

    violations
}

fn command_text(command: &Value) -> String {
    match command {
        Value::String(command) => command.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|part| {
                part.as_str()
                    .map_or_else(|| part.to_string(), str::to_owned)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn violation(message: impl Into<String>) -> ContractViolation {
    ContractViolation(message.into())
}
