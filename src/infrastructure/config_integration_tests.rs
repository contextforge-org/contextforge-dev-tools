use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::Path;

use cf_integration::infrastructure::config::{
    AppConfig, ConfigBootstrap, ConfigRequirements, Environment, ValueOrigin, absolute_path,
    load_environment,
};

use tempfile::TempDir;

const ROOT_OVERRIDE: &str = "CF_INTEGRATION_ROOT";

fn environment(values: &[(&str, &str)]) -> Environment {
    values
        .iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect()
}

fn repository_root() -> TempDir {
    let root = tempfile::tempdir().expect("temporary repository root should be created");
    fs::write(root.path().join("Cargo.toml"), "[package]\n")
        .expect("temporary Cargo manifest should be written");
    fs::create_dir_all(root.path().join("docker"))
        .expect("temporary docker directory should be created");
    fs::write(
        root.path()
            .join("docker/docker-compose.cf-integration.yaml"),
        "services: {}\n",
    )
    .expect("temporary Compose file should be written");
    root
}

#[derive(Debug)]
struct TestConfigLoad {
    config: AppConfig,
    warnings: Vec<String>,
}

fn load_app_config(root: &Path, process: &Environment) -> TestConfigLoad {
    let bootstrap = ConfigBootstrap::load(process, root).expect("bootstrap should load");
    let warnings = bootstrap.warnings().to_vec();
    let config = AppConfig::load(bootstrap, ConfigRequirements::Runtime)
        .expect("application config should load");
    TestConfigLoad { config, warnings }
}

#[test]
fn process_values_override_dotenv_even_when_empty() {
    let root = tempfile::tempdir().expect("temporary directory should be created");
    fs::write(
        root.path().join(".env"),
        "PRESENT=from-dotenv\nEMPTY=from-dotenv\n",
    )
    .expect("dotenv should be written");
    let process = environment(&[("PRESENT", "from-process"), ("EMPTY", "")]);

    let loaded = load_environment(root.path(), &process).expect("environment should load");

    let present = loaded
        .get(OsStr::new("PRESENT"))
        .expect("process value should be present");
    assert_eq!(present.value, OsString::from("from-process"));
    assert_eq!(present.origin, ValueOrigin::Process);

    let empty = loaded
        .get(OsStr::new("EMPTY"))
        .expect("empty process value should be present");
    assert_eq!(empty.value, OsString::new());
    assert_eq!(empty.origin, ValueOrigin::Process);
}

#[test]
fn dotenv_supports_export_and_matching_outer_quotes() {
    let root = tempfile::tempdir().expect("temporary directory should be created");
    fs::write(
        root.path().join(".env"),
        "export DOUBLE=\"two words\"\nSINGLE='one word'\nPLAIN=value\n",
    )
    .expect("dotenv should be written");

    let loaded = load_environment(root.path(), &Environment::new())
        .expect("quoted dotenv values should load");

    for (key, expected) in [
        ("DOUBLE", "two words"),
        ("SINGLE", "one word"),
        ("PLAIN", "value"),
    ] {
        let sourced = loaded
            .get(OsStr::new(key))
            .expect("dotenv value should be present");
        assert_eq!(sourced.value, OsString::from(expected));
        assert_eq!(sourced.origin, ValueOrigin::Dotenv);
    }
    assert!(loaded.warnings().is_empty());
}

#[test]
fn invalid_dotenv_lines_and_keys_are_warnings() {
    let root = tempfile::tempdir().expect("temporary directory should be created");
    fs::write(
        root.path().join(".env"),
        "NO_EQUALS\n9INVALID=value\nHAS-DASH=value\n_VALID=kept\n",
    )
    .expect("dotenv should be written");

    let loaded =
        load_environment(root.path(), &Environment::new()).expect("warnings are non-fatal");

    assert_eq!(loaded.warnings().len(), 3);
    assert!(loaded.warnings()[0].contains("line 1"));
    assert!(loaded.warnings()[1].contains("line 2"));
    assert!(loaded.warnings()[2].contains("line 3"));
    assert_eq!(
        loaded
            .get(OsStr::new("_VALID"))
            .expect("valid line should still load")
            .value,
        OsString::from("kept")
    );
}

#[test]
fn empty_and_full_line_comments_are_ignored_after_leading_whitespace() {
    let root = tempfile::tempdir().expect("temporary directory should be created");
    fs::write(
        root.path().join(".env"),
        "\n   \n  # comment\n\t# another comment\n  KEY=value\n",
    )
    .expect("dotenv should be written");

    let loaded = load_environment(root.path(), &Environment::new()).expect("dotenv should load");

    assert!(loaded.warnings().is_empty());
    assert_eq!(
        loaded
            .get(OsStr::new("KEY"))
            .expect("indented assignment should load")
            .value,
        OsString::from("value")
    );
}

#[test]
fn dotenv_keeps_inline_comments_and_expansion_syntax_literal() {
    let root = tempfile::tempdir().expect("temporary directory should be created");
    fs::write(
        root.path().join(".env"),
        "INLINE=value # literal\nEXPANSION=${HOME}/data\nUNMATCHED='literal\n",
    )
    .expect("dotenv should be written");

    let loaded = load_environment(root.path(), &Environment::new()).expect("dotenv should load");

    for (key, expected) in [
        ("INLINE", "value # literal"),
        ("EXPANSION", "${HOME}/data"),
        ("UNMATCHED", "'literal"),
    ] {
        assert_eq!(
            loaded
                .get(OsStr::new(key))
                .expect("literal dotenv value should load")
                .value,
            OsString::from(expected)
        );
    }
}

#[test]
fn missing_dotenv_is_allowed() {
    let root = tempfile::tempdir().expect("temporary directory should be created");
    let process = environment(&[("PROCESS_ONLY", "set")]);

    let loaded = load_environment(root.path(), &process).expect("missing dotenv should be allowed");

    assert!(loaded.warnings().is_empty());
    assert_eq!(
        loaded
            .get(OsStr::new("PROCESS_ONLY"))
            .expect("process environment should be retained")
            .origin,
        ValueOrigin::Process
    );
}

#[test]
fn unreadable_dotenv_returns_contextual_error() {
    let root = tempfile::tempdir().expect("temporary directory should be created");
    fs::create_dir(root.path().join(".env")).expect("dotenv directory should be created");

    let error = load_environment(root.path(), &Environment::new())
        .expect_err("a dotenv directory should not be readable as a file");

    let message = format!("{error:#}");
    assert!(message.contains(".env"));
    assert!(message.contains("read"));
}

#[test]
fn absolute_path_joins_relative_values_to_root() {
    let root = Path::new("/tmp/integration-root");

    assert_eq!(
        absolute_path(root, OsStr::new("nested/file")),
        root.join("nested/file")
    );
}

#[test]
fn absolute_path_keeps_absolute_values_unchanged() {
    let absolute = Path::new("/tmp/already-absolute");

    assert_eq!(
        absolute_path(Path::new("/different/root"), absolute.as_os_str()),
        absolute
    );
}

#[test]
fn workspace_root_prefers_process_override() {
    let process_root = repository_root();
    let cwd_root = repository_root();
    let process = Environment::from([(
        OsString::from(ROOT_OVERRIDE),
        process_root.path().as_os_str().to_owned(),
    )]);

    let bootstrap =
        ConfigBootstrap::load(&process, cwd_root.path()).expect("bootstrap should load");
    let config = AppConfig::load(bootstrap, ConfigRequirements::ReadOnly)
        .expect("read-only config should resolve");

    assert_eq!(config.root(), process_root.path());
}

#[test]
fn workspace_root_defaults_to_cwd_without_writing_files() {
    let outside = tempfile::tempdir().expect("temporary directory should be created");
    let bootstrap =
        ConfigBootstrap::load(&Environment::new(), outside.path()).expect("bootstrap should load");
    let config = AppConfig::load(bootstrap, ConfigRequirements::ReadOnly)
        .expect("read-only config should resolve");

    assert_eq!(config.root(), outside.path());
    assert!(!outside.path().join(".integration").exists());
}

#[test]
fn invalid_explicit_runtime_root_fails_closed() {
    let invalid_root = tempfile::tempdir().expect("temporary directory should be created");
    let outside = tempfile::tempdir().expect("temporary directory should be created");
    let process = Environment::from([(
        OsString::from(ROOT_OVERRIDE),
        invalid_root.path().as_os_str().to_owned(),
    )]);

    let bootstrap = ConfigBootstrap::load(&process, outside.path()).expect("bootstrap should load");
    let error = AppConfig::load(bootstrap, ConfigRequirements::Runtime)
        .expect_err("an explicit root without assets must fail");

    assert!(error.to_string().contains(ROOT_OVERRIDE));
}

#[test]
fn app_config_exposes_resolved_paths_and_image_settings() {
    let root = repository_root();
    let absolute_dataplane = root.path().join("external/dataplane");
    let process = Environment::from([
        (
            OsString::from("CF_INTEGRATION_DIR"),
            OsString::from("runtime"),
        ),
        (
            OsString::from("CF_CONTROLPLANE_DIR"),
            OsString::from("checkouts/controlplane"),
        ),
        (
            OsString::from("CF_DATAPLANE_DIR"),
            absolute_dataplane.as_os_str().to_owned(),
        ),
        (
            OsString::from("CF_CONTROLPLANE_IMAGE"),
            OsString::from("example/controlplane:test"),
        ),
    ]);

    let loaded = load_app_config(root.path(), &process);

    assert_eq!(loaded.config.root(), root.path());
    assert_eq!(loaded.config.integration_dir(), root.path().join("runtime"));
    assert_eq!(
        loaded.config.controlplane_dir(),
        root.path().join("checkouts/controlplane")
    );
    assert_eq!(loaded.config.dataplane_dir(), absolute_dataplane);
    assert_eq!(
        loaded.config.controlplane_image().resolved(),
        OsStr::new("example/controlplane:test")
    );
    assert!(!loaded.config.controlplane_image().tracks_main_revision());
}

#[test]
fn explicit_empty_process_images_use_fallbacks_but_remain_explicit() {
    let root = repository_root();
    let process = environment(&[
        ("CF_CONTROLPLANE_IMAGE", ""),
        ("CF_CONTROLPLANE_VERSION", "test"),
        ("CF_DATAPLANE_IMAGE", ""),
        ("CF_DATAPLANE_REF", "main"),
    ]);

    let loaded = load_app_config(root.path(), &process);

    assert_eq!(
        loaded.config.controlplane_image().resolved(),
        OsStr::new("ghcr.io/ibm/mcp-context-forge:test")
    );
    assert!(loaded.config.controlplane_image().is_prebuilt());
    assert!(!loaded.config.controlplane_image().tracks_main_revision());
    assert_eq!(
        loaded.config.dataplane_image().resolved(),
        OsStr::new("contextforge-org/contextforge-data-plane:local")
    );
}

#[test]
fn app_config_propagates_warnings_and_retains_loaded_environment() {
    let root = repository_root();
    fs::write(
        root.path().join(".env"),
        "INVALID\nCF_CONTROLPLANE_REPO=from-dotenv\n",
    )
    .expect("dotenv should be written");

    let loaded = load_app_config(root.path(), &Environment::new());

    assert_eq!(loaded.warnings.len(), 1);
    assert_eq!(loaded.warnings, loaded.config.environment().warnings());
    let repository = loaded
        .config
        .environment()
        .get(OsStr::new("CF_CONTROLPLANE_REPO"))
        .expect("dotenv repository should be retained");
    assert_eq!(repository.value, OsString::from("from-dotenv"));
    assert_eq!(repository.origin, ValueOrigin::Dotenv);
}

#[test]
fn debug_output_redacts_derived_and_loaded_environment_values() {
    const JWT_SECRET: &str = "unique-debug-jwt-secret-7f4090";
    const KEY_PASSWORD: &str = "unique-debug-key-password-ea218c";
    const UNRELATED_SECRET: &str = "unique-debug-unrelated-secret-b463ad";

    let root = repository_root();
    let process = environment(&[
        ("JWT_SECRET_KEY", JWT_SECRET),
        ("KEY_FILE_PASSWORD", KEY_PASSWORD),
        ("UNRELATED_SECRET", UNRELATED_SECRET),
    ]);

    let loaded = load_app_config(root.path(), &process);
    let sourced = loaded
        .config
        .environment()
        .get(OsStr::new("UNRELATED_SECRET"))
        .expect("unrelated process value should be retained");
    let debug_outputs = [
        ("ConfigLoad", format!("{loaded:?}")),
        ("AppConfig", format!("{:?}", loaded.config)),
        (
            "LoadedEnvironment",
            format!("{:?}", loaded.config.environment()),
        ),
        ("SourcedValue", format!("{sourced:?}")),
    ];

    for (type_name, output) in debug_outputs {
        for secret in [JWT_SECRET, KEY_PASSWORD, UNRELATED_SECRET] {
            assert!(
                !output.contains(secret),
                "{type_name} Debug output leaked {secret}"
            );
        }
    }
}
