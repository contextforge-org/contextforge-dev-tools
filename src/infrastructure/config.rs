//! Environment loading and repository path resolution.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use uuid::Uuid;

use crate::infrastructure::assets::{contains_runtime_assets, materialize_runtime_assets};

const ROOT_OVERRIDE: &str = "CF_INTEGRATION_ROOT";
const LOCAL_SECRETS_FILE: &str = "secrets.env";
const REDACTED: &str = "<redacted>";

/// Environment values supplied without mutating the process environment.
pub(crate) type Environment = HashMap<OsString, OsString>;

/// Source used for a loaded configuration value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValueOrigin {
    /// Value supplied by the process environment.
    Process,
    /// Value loaded from the repository `.env` file.
    Dotenv,
    /// Value supplied by a configuration default.
    Default,
}

/// Environment value paired with its source.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SourcedValue {
    /// Raw environment value.
    pub(crate) value: OsString,
    /// Source that supplied the value.
    pub(crate) origin: ValueOrigin,
}

/// Environment values loaded from the process and optional `.env` file.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LoadedEnvironment {
    values: HashMap<OsString, SourcedValue>,
    warnings: Vec<String>,
}

/// Resolved container image and whether it is prebuilt.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ImageSetting {
    resolved: OsString,
    prebuilt: bool,
    tracks_main_revision: bool,
}

impl ImageSetting {
    /// Returns the image selected after applying shell-compatible fallbacks.
    #[must_use]
    pub(crate) fn resolved(&self) -> &OsStr {
        &self.resolved
    }

    /// Returns whether the selected image should be pulled instead of auto-built.
    #[must_use]
    pub(crate) fn is_prebuilt(&self) -> bool {
        self.prebuilt
    }

    /// Returns whether the image tag must be derived from the fetched main branch.
    #[must_use]
    pub(crate) fn tracks_main_revision(&self) -> bool {
        self.tracks_main_revision
    }
}

/// Derived configuration used by integration commands.
#[derive(Clone)]
pub(crate) struct AppConfig {
    workspace_root: PathBuf,
    asset_root: PathBuf,
    integration_dir: SourcedValue,
    controlplane_dir: SourcedValue,
    pub(crate) controlplane_repo: SourcedValue,
    pub(crate) controlplane_ref: SourcedValue,
    dataplane_dir: SourcedValue,
    pub(crate) dataplane_repo: SourcedValue,
    pub(crate) dataplane_ref: SourcedValue,
    pub(crate) integration_project: SourcedValue,
    pub(crate) controlplane_project: SourcedValue,
    pub(crate) jwt_secret_key: SourcedValue,
    pub(crate) auth_encryption_secret: SourcedValue,
    controlplane_image: ImageSetting,
    dataplane_image: ImageSetting,
    pub(crate) dataplane_platform: SourcedValue,
    pub(crate) compose_build: SourcedValue,
    pub(crate) fast_time_server_id: SourcedValue,
    pub(crate) fast_time_expected_image: SourcedValue,
    pub(crate) base_url: SourcedValue,
    pub(crate) platform_admin_email: SourcedValue,
    pub(crate) platform_admin_password: SourcedValue,
    pub(crate) key_file_password: SourcedValue,
    pub(crate) locust_users: SourcedValue,
    pub(crate) locust_spawn_rate: SourcedValue,
    pub(crate) locust_run_time: SourcedValue,
    environment: LoadedEnvironment,
}

/// Environment and workspace paths loaded before resolving an action.
#[derive(Debug, Clone)]
pub(crate) struct ConfigBootstrap {
    workspace_root: PathBuf,
    root_overridden: bool,
    environment: LoadedEnvironment,
}

/// Filesystem resources required by a resolved action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConfigRequirements {
    runtime: bool,
}

impl ConfigRequirements {
    /// Configuration for report and token operations that must not write files.
    pub(crate) const READ_ONLY: Self = Self { runtime: false };
    /// Configuration for operations backed by Compose or runtime scripts.
    pub(crate) const RUNTIME: Self = Self { runtime: true };
}

impl fmt::Debug for SourcedValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourcedValue")
            .field("value", &REDACTED)
            .field("origin", &self.origin)
            .finish()
    }
}

impl fmt::Debug for LoadedEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoadedEnvironment")
            .field("values", &self.values)
            .field("warnings", &self.warnings)
            .finish()
    }
}

impl fmt::Debug for ImageSetting {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageSetting")
            .field("resolved", &REDACTED)
            .field("prebuilt", &self.prebuilt)
            .field("tracks_main_revision", &self.tracks_main_revision)
            .finish()
    }
}

impl fmt::Debug for AppConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppConfig")
            .field("workspace_root", &REDACTED)
            .field("asset_root", &REDACTED)
            .field("controlplane_image", &self.controlplane_image)
            .field("dataplane_image", &self.dataplane_image)
            .field("environment", &self.environment)
            .finish_non_exhaustive()
    }
}

impl LoadedEnvironment {
    /// Returns the loaded value for `key`.
    #[must_use]
    pub(crate) fn get(&self, key: &OsStr) -> Option<&SourcedValue> {
        self.values.get(key)
    }

    /// Returns non-fatal `.env` parsing warnings.
    #[must_use]
    pub(crate) fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Iterates loaded values and their origins in unspecified order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&OsString, &SourcedValue)> {
        self.values.iter()
    }
}

impl ConfigBootstrap {
    /// Loads process values and an optional workspace `.env` without writing files.
    pub(crate) fn load(process: &Environment, cwd: &Path) -> Result<Self> {
        let override_value = process
            .get(OsStr::new(ROOT_OVERRIDE))
            .filter(|value| !value.is_empty());
        let workspace_root = override_value
            .map(|value| absolute_path(cwd, value))
            .unwrap_or_else(|| cwd.to_path_buf());
        let environment = load_environment(&workspace_root, process)?;
        Ok(Self {
            workspace_root,
            root_overridden: override_value.is_some(),
            environment,
        })
    }

    /// Returns the merged process and dotenv environment.
    #[must_use]
    pub(crate) fn environment(&self) -> &LoadedEnvironment {
        &self.environment
    }

    /// Returns non-fatal dotenv parsing warnings.
    #[must_use]
    pub(crate) fn warnings(&self) -> &[String] {
        self.environment.warnings()
    }
}

impl AppConfig {
    /// Derives action-specific configuration from a side-effect-free bootstrap.
    ///
    /// # Errors
    ///
    /// Returns an error when required runtime assets or local secrets cannot be
    /// prepared.
    pub(crate) fn load(
        bootstrap: ConfigBootstrap,
        requirements: ConfigRequirements,
    ) -> Result<Self> {
        let root = bootstrap.workspace_root;
        let environment = bootstrap.environment;

        let integration_dir = resolved_path(
            &root,
            shell_value(
                &environment,
                "CF_INTEGRATION_DIR",
                root.join(".integration").into_os_string(),
            ),
        );
        let asset_root = if requirements.runtime {
            if contains_runtime_assets(&root) {
                root.clone()
            } else if bootstrap.root_overridden {
                bail!(
                    "{ROOT_OVERRIDE}={} does not contain the complete runtime asset set",
                    root.display()
                );
            } else {
                materialize_runtime_assets(Path::new(&integration_dir.value))?
            }
        } else {
            root.clone()
        };
        let controlplane_dir = resolved_path(
            &root,
            shell_value(
                &environment,
                "CF_CONTROLPLANE_DIR",
                Path::new(&integration_dir.value)
                    .join("mcp-context-forge")
                    .into_os_string(),
            ),
        );
        let dataplane_dir = resolved_path(
            &root,
            shell_value(
                &environment,
                "CF_DATAPLANE_DIR",
                Path::new(&integration_dir.value)
                    .join("contextforge-data-plane")
                    .into_os_string(),
            ),
        );
        let controlplane_repo = shell_value(
            &environment,
            "CF_CONTROLPLANE_REPO",
            OsString::from("https://github.com/IBM/mcp-context-forge.git"),
        );
        let controlplane_ref =
            shell_value(&environment, "CF_CONTROLPLANE_REF", OsString::from("main"));
        let dataplane_repo = shell_value(
            &environment,
            "CF_DATAPLANE_REPO",
            OsString::from("https://github.com/contextforge-org/contextforge-data-plane.git"),
        );
        let dataplane_ref = shell_value(&environment, "CF_DATAPLANE_REF", OsString::new());
        let integration_project =
            shell_value(&environment, "CF_INTEGRATION_PROJECT", OsString::from("cf"));
        let controlplane_project = shell_value(
            &environment,
            "CF_CONTROLPLANE_PROJECT",
            OsString::from("cf-controlplane-only"),
        );
        let local_secrets = if requirements.runtime
            && (first_nonempty(&environment, "JWT_SECRET_KEY").is_none()
                || first_nonempty(&environment, "AUTH_ENCRYPTION_SECRET").is_none())
        {
            Some(load_or_create_local_secrets(Path::new(
                &integration_dir.value,
            ))?)
        } else {
            None
        };
        let jwt_secret_key = match first_nonempty(&environment, "JWT_SECRET_KEY") {
            Some(value) => value.clone(),
            None if !requirements.runtime => default_value(""),
            None => default_value(
                &local_secrets
                    .as_ref()
                    .context("local secrets were not loaded for JWT_SECRET_KEY")?
                    .jwt_secret_key,
            ),
        };
        let auth_encryption_secret = match first_nonempty(&environment, "AUTH_ENCRYPTION_SECRET") {
            Some(value) => value.clone(),
            None if !requirements.runtime => default_value(""),
            None => default_value(
                &local_secrets
                    .as_ref()
                    .context("local secrets were not loaded for AUTH_ENCRYPTION_SECRET")?
                    .auth_encryption_secret,
            ),
        };
        let controlplane_image = controlplane_image(&environment);
        let dataplane_image = dataplane_image(&environment, &dataplane_ref);
        let dataplane_platform = shell_value(
            &environment,
            "CF_DATAPLANE_PLATFORM",
            OsString::from("auto"),
        );
        let compose_build = shell_value(&environment, "CF_COMPOSE_BUILD", OsString::from("auto"));
        let fast_time_server_id = shell_value(
            &environment,
            "CF_FAST_TIME_SERVER_ID",
            OsString::from("9779b6698cbd4b4995ee04a4fab38737"),
        );
        let fast_time_expected_image = shell_value(
            &environment,
            "CF_FAST_TIME_EXPECTED_IMAGE",
            OsString::from("ghcr.io/ibm/cfex-mcp-fast-time-server:latest"),
        );
        let base_url = base_url(&environment);
        let platform_admin_email = shell_value(
            &environment,
            "PLATFORM_ADMIN_EMAIL",
            OsString::from("admin@example.com"),
        );
        let platform_admin_password = shell_value(
            &environment,
            "PLATFORM_ADMIN_PASSWORD",
            OsString::from("changeme"),
        );
        let key_file_password = shell_value(&environment, "KEY_FILE_PASSWORD", OsString::new());
        let locust_users = present_value(&environment, "LOCUST_USERS", "100");
        let locust_spawn_rate = present_value(&environment, "LOCUST_SPAWN_RATE", "10");
        let locust_run_time = present_value(&environment, "LOCUST_RUN_TIME", "5m");
        Ok(Self {
            workspace_root: root,
            asset_root,
            integration_dir,
            controlplane_dir,
            controlplane_repo,
            controlplane_ref,
            dataplane_dir,
            dataplane_repo,
            dataplane_ref,
            integration_project,
            controlplane_project,
            jwt_secret_key,
            auth_encryption_secret,
            controlplane_image,
            dataplane_image,
            dataplane_platform,
            compose_build,
            fast_time_server_id,
            fast_time_expected_image,
            base_url,
            platform_admin_email,
            platform_admin_password,
            key_file_password,
            locust_users,
            locust_spawn_rate,
            locust_run_time,
            environment,
        })
    }

    /// Returns the directory used for dotenv, reports, and relative overrides.
    #[must_use]
    pub(crate) fn root(&self) -> &Path {
        &self.workspace_root
    }

    /// Returns the root containing Compose overlays and runtime scripts.
    #[must_use]
    pub(crate) fn asset_root(&self) -> &Path {
        &self.asset_root
    }

    /// Returns the resolved integration runtime directory.
    #[must_use]
    pub(crate) fn integration_dir(&self) -> &Path {
        Path::new(&self.integration_dir.value)
    }

    /// Returns the resolved control-plane checkout directory.
    #[must_use]
    pub(crate) fn controlplane_dir(&self) -> &Path {
        Path::new(&self.controlplane_dir.value)
    }

    /// Returns the resolved dataplane checkout directory.
    #[must_use]
    pub(crate) fn dataplane_dir(&self) -> &Path {
        Path::new(&self.dataplane_dir.value)
    }

    /// Returns the configured control-plane repository.
    #[must_use]
    pub(crate) fn controlplane_repo(&self) -> &SourcedValue {
        &self.controlplane_repo
    }

    /// Returns the configured control-plane revision.
    #[must_use]
    pub(crate) fn controlplane_ref(&self) -> &SourcedValue {
        &self.controlplane_ref
    }

    /// Returns the configured dataplane repository.
    #[must_use]
    pub(crate) fn dataplane_repo(&self) -> &SourcedValue {
        &self.dataplane_repo
    }

    /// Returns the configured dataplane revision.
    #[must_use]
    pub(crate) fn dataplane_ref(&self) -> &SourcedValue {
        &self.dataplane_ref
    }

    /// Returns the integration Compose project name.
    #[must_use]
    pub(crate) fn integration_project(&self) -> &SourcedValue {
        &self.integration_project
    }

    /// Returns the control-plane Compose project name.
    #[must_use]
    pub(crate) fn controlplane_project(&self) -> &SourcedValue {
        &self.controlplane_project
    }

    /// Returns the JWT signing secret setting.
    #[must_use]
    pub(crate) fn jwt_secret_key(&self) -> &SourcedValue {
        &self.jwt_secret_key
    }

    /// Returns the control-plane credential-encryption secret setting.
    #[must_use]
    pub(crate) fn auth_encryption_secret(&self) -> &SourcedValue {
        &self.auth_encryption_secret
    }

    /// Returns the resolved control-plane image setting.
    #[must_use]
    pub(crate) fn controlplane_image(&self) -> &ImageSetting {
        &self.controlplane_image
    }

    /// Returns the resolved dataplane image setting.
    #[must_use]
    pub(crate) fn dataplane_image(&self) -> &ImageSetting {
        &self.dataplane_image
    }

    /// Returns the configured dataplane container platform.
    #[must_use]
    pub(crate) fn dataplane_platform(&self) -> &SourcedValue {
        &self.dataplane_platform
    }

    /// Returns the Compose build-mode setting.
    #[must_use]
    pub(crate) fn compose_build(&self) -> &SourcedValue {
        &self.compose_build
    }

    /// Returns the Fast Time server identifier setting.
    #[must_use]
    pub(crate) fn fast_time_server_id(&self) -> &SourcedValue {
        &self.fast_time_server_id
    }

    /// Returns the expected Fast Time image setting.
    #[must_use]
    pub(crate) fn fast_time_expected_image(&self) -> &SourcedValue {
        &self.fast_time_expected_image
    }

    /// Returns the public integration base URL setting.
    #[must_use]
    pub(crate) fn base_url(&self) -> &SourcedValue {
        &self.base_url
    }

    /// Returns the platform administrator email setting.
    #[must_use]
    pub(crate) fn platform_admin_email(&self) -> &SourcedValue {
        &self.platform_admin_email
    }

    /// Returns the bootstrap platform administrator password setting.
    #[must_use]
    pub(crate) fn platform_admin_password(&self) -> &SourcedValue {
        &self.platform_admin_password
    }

    /// Returns the private-key password setting.
    #[must_use]
    pub(crate) fn key_file_password(&self) -> &SourcedValue {
        &self.key_file_password
    }

    /// Returns the Locust user-count setting.
    #[must_use]
    pub(crate) fn locust_users(&self) -> &SourcedValue {
        &self.locust_users
    }

    /// Returns the Locust spawn-rate setting.
    #[must_use]
    pub(crate) fn locust_spawn_rate(&self) -> &SourcedValue {
        &self.locust_spawn_rate
    }

    /// Returns the Locust run-time setting.
    #[must_use]
    pub(crate) fn locust_run_time(&self) -> &SourcedValue {
        &self.locust_run_time
    }

    /// Returns the environment loaded before deriving fallback values.
    #[must_use]
    pub(crate) fn environment(&self) -> &LoadedEnvironment {
        &self.environment
    }
}

fn shell_value(environment: &LoadedEnvironment, key: &str, default: OsString) -> SourcedValue {
    first_nonempty(environment, key)
        .cloned()
        .unwrap_or(SourcedValue {
            value: default,
            origin: ValueOrigin::Default,
        })
}

fn present_value(environment: &LoadedEnvironment, key: &str, default: &str) -> SourcedValue {
    environment
        .get(OsStr::new(key))
        .cloned()
        .unwrap_or_else(|| default_value(default))
}

fn first_nonempty<'a>(environment: &'a LoadedEnvironment, key: &str) -> Option<&'a SourcedValue> {
    environment
        .get(OsStr::new(key))
        .filter(|sourced| !sourced.value.is_empty())
}

fn default_value(value: &str) -> SourcedValue {
    SourcedValue {
        value: OsString::from(value),
        origin: ValueOrigin::Default,
    }
}

fn resolved_path(root: &Path, mut value: SourcedValue) -> SourcedValue {
    value.value = absolute_path(root, &value.value).into_os_string();
    value
}

fn is_configured_value(environment: &LoadedEnvironment, key: &str) -> bool {
    environment.get(OsStr::new(key)).is_some()
}

fn prefixed_value(prefix: &str, suffix: &OsStr) -> OsString {
    let mut value = OsString::from(prefix);
    value.push(suffix);
    value
}

fn controlplane_image(environment: &LoadedEnvironment) -> ImageSetting {
    let (resolved, tracks_main_revision) =
        if let Some(image) = first_nonempty(environment, "CF_CONTROLPLANE_IMAGE") {
            (image.value.clone(), false)
        } else {
            let version = shell_value(
                environment,
                "CF_CONTROLPLANE_VERSION",
                OsString::from("main"),
            );
            let tracks_main_revision = version.value == OsStr::new("main");
            (
                prefixed_value("ghcr.io/ibm/mcp-context-forge:", &version.value),
                tracks_main_revision,
            )
        };

    ImageSetting {
        resolved,
        prebuilt: true,
        tracks_main_revision,
    }
}

fn dataplane_image(environment: &LoadedEnvironment, dataplane_ref: &SourcedValue) -> ImageSetting {
    let explicitly_set = is_configured_value(environment, "CF_DATAPLANE_IMAGE");
    let resolved = if let Some(image) = first_nonempty(environment, "CF_DATAPLANE_IMAGE") {
        image.value.clone()
    } else if !dataplane_ref.value.is_empty() {
        shell_value(
            environment,
            "CF_DATAPLANE_LOCAL_IMAGE",
            OsString::from("contextforge-org/contextforge-data-plane:local"),
        )
        .value
    } else {
        let version = shell_value(
            environment,
            "CF_DATAPLANE_VERSION",
            OsString::from("latest"),
        );
        prefixed_value(
            "ghcr.io/contextforge-org/contextforge-data-plane:",
            &version.value,
        )
    };

    ImageSetting {
        resolved,
        prebuilt: explicitly_set || dataplane_ref.value.is_empty(),
        tracks_main_revision: false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalSecrets {
    jwt_secret_key: String,
    auth_encryption_secret: String,
}

fn load_or_create_local_secrets(integration_dir: &Path) -> Result<LocalSecrets> {
    let path = integration_dir.join(LOCAL_SECRETS_FILE);
    match fs::read_to_string(&path) {
        Ok(contents) => return parse_local_secrets(&path, &contents),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read local secrets file {}", path.display()));
        }
    }

    fs::create_dir_all(integration_dir).with_context(|| {
        format!(
            "failed to create integration directory {}",
            integration_dir.display()
        )
    })?;
    let generated = LocalSecrets {
        jwt_secret_key: random_secret(),
        auth_encryption_secret: random_secret(),
    };
    let contents = format!(
        "JWT_SECRET_KEY={}\nAUTH_ENCRYPTION_SECRET={}\n",
        generated.jwt_secret_key, generated.auth_encryption_secret
    );
    match create_private_file(&path) {
        Ok(mut file) => {
            file.write_all(contents.as_bytes()).with_context(|| {
                format!("failed to write local secrets file {}", path.display())
            })?;
            Ok(generated)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let contents = fs::read_to_string(&path)
                .with_context(|| format!("failed to read local secrets file {}", path.display()))?;
            parse_local_secrets(&path, &contents)
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed to create local secrets file {}", path.display())),
    }
}

fn random_secret() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<fs::File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn parse_local_secrets(path: &Path, contents: &str) -> Result<LocalSecrets> {
    let mut jwt_secret_key = None;
    let mut auth_encryption_secret = None;
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("JWT_SECRET_KEY=") {
            jwt_secret_key = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("AUTH_ENCRYPTION_SECRET=") {
            auth_encryption_secret = Some(value.to_owned());
        }
    }
    match (jwt_secret_key, auth_encryption_secret) {
        (Some(jwt_secret_key), Some(auth_encryption_secret))
            if jwt_secret_key.len() >= 32 && auth_encryption_secret.len() >= 32 =>
        {
            Ok(LocalSecrets {
                jwt_secret_key,
                auth_encryption_secret,
            })
        }
        _ => bail!(
            "invalid local secrets file {}; remove it and rerun the command",
            path.display()
        ),
    }
}

fn base_url(environment: &LoadedEnvironment) -> SourcedValue {
    if let Some(url) = first_nonempty(environment, "MCP_CLI_BASE_URL") {
        return url.clone();
    }

    let port = shell_value(environment, "NGINX_PORT", OsString::from("8080"));
    SourcedValue {
        value: prefixed_value("http://127.0.0.1:", &port.value),
        origin: port.origin,
    }
}

/// Loads process values and supplements missing keys from `root/.env`.
///
/// # Errors
///
/// Returns an error when an existing `.env` file cannot be read.
pub(crate) fn load_environment(root: &Path, process: &Environment) -> Result<LoadedEnvironment> {
    let mut values = process
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                SourcedValue {
                    value: value.clone(),
                    origin: ValueOrigin::Process,
                },
            )
        })
        .collect();
    let mut warnings = Vec::new();
    let dotenv_path = root.join(".env");

    let contents = match fs::read_to_string(&dotenv_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(LoadedEnvironment { values, warnings });
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read dotenv file {}", dotenv_path.display()));
        }
    };

    for (index, raw_line) in contents.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let assignment = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = assignment.split_once('=') else {
            warnings.push(format!(
                "invalid .env line {line_number}: expected KEY=value"
            ));
            continue;
        };

        if !is_valid_key(key) {
            warnings.push(format!("invalid .env key on line {line_number}: {key}"));
            continue;
        }

        if values.contains_key(OsStr::new(key)) {
            continue;
        }

        values.insert(
            OsString::from(key),
            SourcedValue {
                value: OsString::from(strip_outer_quotes(value)),
                origin: ValueOrigin::Dotenv,
            },
        );
    }

    Ok(LoadedEnvironment { values, warnings })
}

/// Returns `raw` unchanged when absolute, otherwise joined to `root`.
#[must_use]
pub(crate) fn absolute_path(root: &Path, raw: &OsStr) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn is_valid_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn strip_outer_quotes(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && matches!(
            (bytes.first(), bytes.last()),
            (Some(b'\''), Some(b'\'')) | (Some(b'"'), Some(b'"'))
        )
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(values: &[(&str, &str)]) -> Environment {
        values
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect()
    }

    fn repository_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("temporary repository root should be created")
    }

    fn load_app_config(root: &Path, process: &Environment) -> AppConfig {
        let bootstrap = ConfigBootstrap::load(process, root).expect("bootstrap should load");
        AppConfig::load(bootstrap, ConfigRequirements::RUNTIME)
            .expect("application config should load")
    }

    fn assert_sourced(actual: &SourcedValue, expected: &OsStr, origin: ValueOrigin) {
        assert_eq!(actual.value, expected);
        assert_eq!(actual.origin, origin);
    }

    #[test]
    fn read_only_config_does_not_create_runtime_state() {
        let outside = tempfile::tempdir().expect("temporary directory should be created");
        let bootstrap = ConfigBootstrap::load(&Environment::new(), outside.path())
            .expect("bootstrap should load");
        let config = AppConfig::load(bootstrap, ConfigRequirements::READ_ONLY)
            .expect("read-only config should resolve");

        assert_eq!(config.root(), outside.path());
        assert!(!outside.path().join(".integration").exists());
    }

    #[test]
    fn app_config_uses_documented_defaults() {
        let root = repository_root();

        let config = load_app_config(root.path(), &Environment::new());

        assert_eq!(config.workspace_root, root.path());
        assert_sourced(
            &config.integration_dir,
            root.path().join(".integration").as_os_str(),
            ValueOrigin::Default,
        );
        assert_sourced(
            &config.controlplane_dir,
            root.path()
                .join(".integration")
                .join("mcp-context-forge")
                .as_os_str(),
            ValueOrigin::Default,
        );
        assert_sourced(
            &config.controlplane_repo,
            OsStr::new("https://github.com/IBM/mcp-context-forge.git"),
            ValueOrigin::Default,
        );
        assert_sourced(
            &config.controlplane_ref,
            OsStr::new("main"),
            ValueOrigin::Default,
        );
        assert_sourced(
            &config.dataplane_dir,
            root.path()
                .join(".integration")
                .join("contextforge-data-plane")
                .as_os_str(),
            ValueOrigin::Default,
        );
        assert_sourced(
            &config.dataplane_repo,
            OsStr::new("https://github.com/contextforge-org/contextforge-data-plane.git"),
            ValueOrigin::Default,
        );
        assert_sourced(&config.dataplane_ref, OsStr::new(""), ValueOrigin::Default);
        assert_sourced(
            &config.integration_project,
            OsStr::new("cf"),
            ValueOrigin::Default,
        );
        assert_sourced(
            &config.controlplane_project,
            OsStr::new("cf-controlplane-only"),
            ValueOrigin::Default,
        );
        assert_eq!(config.jwt_secret_key.origin, ValueOrigin::Default);
        assert_eq!(config.jwt_secret_key.value.len(), 64);
        assert_eq!(config.auth_encryption_secret.origin, ValueOrigin::Default);
        assert_eq!(config.auth_encryption_secret.value.len(), 64);
        assert_eq!(
            config.controlplane_image.resolved,
            OsStr::new("ghcr.io/ibm/mcp-context-forge:main")
        );
        assert!(config.controlplane_image.prebuilt);
        assert!(config.controlplane_image.tracks_main_revision);
        assert_eq!(
            config.dataplane_image.resolved,
            OsStr::new("ghcr.io/contextforge-org/contextforge-data-plane:latest")
        );
        assert_sourced(
            &config.dataplane_platform,
            OsStr::new("auto"),
            ValueOrigin::Default,
        );
        assert_sourced(
            &config.compose_build,
            OsStr::new("auto"),
            ValueOrigin::Default,
        );
        assert_sourced(
            &config.fast_time_server_id,
            OsStr::new("9779b6698cbd4b4995ee04a4fab38737"),
            ValueOrigin::Default,
        );
        assert_sourced(
            &config.fast_time_expected_image,
            OsStr::new("ghcr.io/ibm/cfex-mcp-fast-time-server:latest"),
            ValueOrigin::Default,
        );
        assert_sourced(
            &config.base_url,
            OsStr::new("http://127.0.0.1:8080"),
            ValueOrigin::Default,
        );
        assert_sourced(
            &config.platform_admin_email,
            OsStr::new("admin@example.com"),
            ValueOrigin::Default,
        );
        assert_sourced(
            &config.platform_admin_password,
            OsStr::new("changeme"),
            ValueOrigin::Default,
        );
        assert_sourced(
            &config.key_file_password,
            OsStr::new(""),
            ValueOrigin::Default,
        );
        assert_sourced(
            &config.locust_users,
            OsStr::new("100"),
            ValueOrigin::Default,
        );
        assert_sourced(
            &config.locust_spawn_rate,
            OsStr::new("10"),
            ValueOrigin::Default,
        );
        assert_sourced(
            &config.locust_run_time,
            OsStr::new("5m"),
            ValueOrigin::Default,
        );
    }

    #[test]
    fn app_config_preserves_process_precedence_and_dotenv_origins() {
        let root = repository_root();
        fs::write(
            root.path().join(".env"),
            concat!(
                "CF_CONTROLPLANE_REPO=dotenv-controlplane\n",
                "CF_CONTROLPLANE_REF=dotenv-ref\n",
                "CF_DATAPLANE_REPO=dotenv-dataplane\n",
                "CF_INTEGRATION_PROJECT=dotenv-project\n",
                "CF_CONTROLPLANE_IMAGE=dotenv/image:tag\n",
            ),
        )
        .expect("dotenv should be written");
        let process = environment(&[
            ("CF_CONTROLPLANE_REPO", "process-controlplane"),
            ("CF_CONTROLPLANE_REF", ""),
        ]);

        let config = load_app_config(root.path(), &process);

        assert_sourced(
            &config.controlplane_repo,
            OsStr::new("process-controlplane"),
            ValueOrigin::Process,
        );
        assert_sourced(
            &config.controlplane_ref,
            OsStr::new("main"),
            ValueOrigin::Default,
        );
        assert_sourced(
            &config.dataplane_repo,
            OsStr::new("dotenv-dataplane"),
            ValueOrigin::Dotenv,
        );
        assert_sourced(
            &config.integration_project,
            OsStr::new("dotenv-project"),
            ValueOrigin::Dotenv,
        );
        assert_eq!(
            config.controlplane_image.resolved,
            OsStr::new("dotenv/image:tag")
        );
    }

    #[test]
    fn controlplane_image_uses_canonical_override_and_ignores_image_local() {
        let root = repository_root();
        let primary = environment(&[
            ("CF_CONTROLPLANE_IMAGE", "primary/image:tag"),
            ("IMAGE_LOCAL", "legacy/image:tag"),
        ]);
        let image_local = environment(&[
            ("IMAGE_LOCAL", "legacy/image:tag"),
            ("CF_CONTROLPLANE_VERSION", "edge"),
        ]);

        let primary_config = load_app_config(root.path(), &primary);
        let local_config = load_app_config(root.path(), &image_local);

        assert_eq!(
            primary_config.controlplane_image.resolved,
            OsStr::new("primary/image:tag")
        );
        assert_eq!(
            local_config.controlplane_image.resolved,
            OsStr::new("ghcr.io/ibm/mcp-context-forge:edge")
        );
        assert!(local_config.controlplane_image.prebuilt);
    }

    #[test]
    fn generated_local_secrets_are_stable_across_config_loads() {
        let root = repository_root();

        let first = load_app_config(root.path(), &Environment::new());
        let second = load_app_config(root.path(), &Environment::new());

        assert_eq!(first.jwt_secret_key, second.jwt_secret_key);
        assert_eq!(first.auth_encryption_secret, second.auth_encryption_secret);
        assert_eq!(first.jwt_secret_key.value.len(), 64);
        assert_eq!(first.auth_encryption_secret.value.len(), 64);
        assert!(
            root.path()
                .join(".integration")
                .join("secrets.env")
                .is_file()
        );
    }

    #[test]
    fn configured_secrets_override_generated_local_secrets() {
        let root = repository_root();
        let process = environment(&[
            (
                "JWT_SECRET_KEY",
                "configured-jwt-secret-12345678901234567890",
            ),
            (
                "AUTH_ENCRYPTION_SECRET",
                "configured-auth-secret-1234567890123456789",
            ),
        ]);

        let config = load_app_config(root.path(), &process);

        assert_sourced(
            &config.jwt_secret_key,
            OsStr::new("configured-jwt-secret-12345678901234567890"),
            ValueOrigin::Process,
        );
        assert_sourced(
            &config.auth_encryption_secret,
            OsStr::new("configured-auth-secret-1234567890123456789"),
            ValueOrigin::Process,
        );
        assert!(
            !root
                .path()
                .join(".integration")
                .join("secrets.env")
                .exists()
        );
    }

    #[test]
    fn dataplane_image_switches_between_source_and_published_defaults() {
        let root = repository_root();
        let source = environment(&[("CF_DATAPLANE_REF", "feature")]);
        let local = environment(&[
            ("CF_DATAPLANE_REF", "feature"),
            ("CF_DATAPLANE_LOCAL_IMAGE", "local/image:tag"),
        ]);
        let published = environment(&[("CF_DATAPLANE_VERSION", "2.0.0")]);
        let explicit = environment(&[("CF_DATAPLANE_IMAGE", "direct/image:tag")]);

        let source_config = load_app_config(root.path(), &source);
        let local_config = load_app_config(root.path(), &local);
        let published_config = load_app_config(root.path(), &published);
        let explicit_config = load_app_config(root.path(), &explicit);

        assert_eq!(
            source_config.dataplane_image.resolved,
            OsStr::new("contextforge-org/contextforge-data-plane:local")
        );
        assert_eq!(
            local_config.dataplane_image.resolved,
            OsStr::new("local/image:tag")
        );
        assert_eq!(
            published_config.dataplane_image.resolved,
            OsStr::new("ghcr.io/contextforge-org/contextforge-data-plane:2.0.0")
        );
        assert_eq!(
            explicit_config.dataplane_image.resolved,
            OsStr::new("direct/image:tag")
        );
    }

    #[test]
    fn fast_time_image_uses_canonical_override_and_ignores_legacy_input() {
        let root = repository_root();
        let expected = environment(&[
            ("CF_FAST_TIME_EXPECTED_IMAGE", "expected/image:tag"),
            ("FAST_TIME_IMAGE", "legacy/image:tag"),
        ]);
        let legacy = environment(&[("FAST_TIME_IMAGE", "legacy/image:tag")]);

        let expected_config = load_app_config(root.path(), &expected);
        let legacy_config = load_app_config(root.path(), &legacy);

        assert_sourced(
            &expected_config.fast_time_expected_image,
            OsStr::new("expected/image:tag"),
            ValueOrigin::Process,
        );
        assert_sourced(
            &legacy_config.fast_time_expected_image,
            OsStr::new("ghcr.io/ibm/cfex-mcp-fast-time-server:latest"),
            ValueOrigin::Default,
        );
    }

    #[test]
    fn base_url_uses_direct_value_then_port_and_admin_email_uses_subject() {
        let root = repository_root();
        let direct = environment(&[
            ("MCP_CLI_BASE_URL", "https://example.test"),
            ("NGINX_PORT", "9191"),
        ]);
        let port_and_admin = environment(&[
            ("MCP_CLI_BASE_URL", ""),
            ("NGINX_PORT", "9191"),
            ("PLATFORM_ADMIN_EMAIL", "operator@example.test"),
            ("PLATFORM_ADMIN_PASSWORD", "integration-password"),
        ]);

        let direct_config = load_app_config(root.path(), &direct);
        let fallback_config = load_app_config(root.path(), &port_and_admin);

        assert_sourced(
            &direct_config.base_url,
            OsStr::new("https://example.test"),
            ValueOrigin::Process,
        );
        assert_sourced(
            &fallback_config.base_url,
            OsStr::new("http://127.0.0.1:9191"),
            ValueOrigin::Process,
        );
        assert_sourced(
            &fallback_config.platform_admin_email,
            OsStr::new("operator@example.test"),
            ValueOrigin::Process,
        );
        assert_sourced(
            &fallback_config.platform_admin_password,
            OsStr::new("integration-password"),
            ValueOrigin::Process,
        );
    }

    #[test]
    fn locust_values_preserve_process_empty_and_dotenv_origins() {
        let root = repository_root();
        fs::write(root.path().join(".env"), "LOCUST_SPAWN_RATE=25\n")
            .expect("dotenv should be written");
        let process = environment(&[("LOCUST_USERS", "")]);

        let config = load_app_config(root.path(), &process);

        assert_sourced(&config.locust_users, OsStr::new(""), ValueOrigin::Process);
        assert_sourced(
            &config.locust_spawn_rate,
            OsStr::new("25"),
            ValueOrigin::Dotenv,
        );
        assert_sourced(
            &config.locust_run_time,
            OsStr::new("5m"),
            ValueOrigin::Default,
        );
        assert_sourced(
            config
                .environment
                .get(OsStr::new("LOCUST_USERS"))
                .expect("raw process value should remain loaded"),
            OsStr::new(""),
            ValueOrigin::Process,
        );
    }
}
