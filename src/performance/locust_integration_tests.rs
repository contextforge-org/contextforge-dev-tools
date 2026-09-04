use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::Path;

use cf_integration::infrastructure::StackMode;
use cf_integration::infrastructure::compose::ComposeProject;
use cf_integration::infrastructure::config::{
    AppConfig, ConfigBootstrap, ConfigRequirements, Environment,
};
use cf_integration::performance::{LoadRequest, LoadSettings, LocustCommand};
use tempfile::TempDir;

fn environment(values: &[(&str, &str)]) -> Environment {
    values
        .iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect()
}

fn repository_root(dotenv: Option<&str>) -> TempDir {
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
    if let Some(contents) = dotenv {
        fs::write(root.path().join(".env"), contents).expect("dotenv should be written");
    }
    root
}

fn config(root: &Path, process: &Environment) -> AppConfig {
    let bootstrap = ConfigBootstrap::load(process, root).expect("bootstrap should load");
    AppConfig::load(bootstrap, ConfigRequirements::RUNTIME).expect("application config should load")
}

fn args(smoke: bool) -> LoadRequest {
    LoadRequest {
        smoke,
        users: None,
        spawn_rate: None,
        run_time: None,
    }
}

fn project(config: &AppConfig, mode: StackMode) -> ComposeProject {
    match mode {
        StackMode::Dataplane => ComposeProject::dataplane(
            config.asset_root(),
            config.controlplane_dir(),
            config.integration_project().value.clone(),
            !config.dataplane_ref().value.is_empty(),
        ),
        StackMode::Controlplane => ComposeProject::controlplane(
            config.asset_root(),
            config.controlplane_dir(),
            config.controlplane_project().value.clone(),
            false,
        ),
    }
}

#[test]
fn full_load_uses_configured_defaults() {
    let root = repository_root(None);
    let settings = LoadSettings::resolve(&config(root.path(), &Environment::new()), &args(false))
        .expect("default load settings should resolve");

    assert_eq!(settings.users().get(), 100);
    assert_eq!(settings.spawn_rate(), 10.0);
    assert_eq!(settings.run_time(), "5m");
}

#[test]
fn command_line_values_override_process_environment() {
    let root = repository_root(None);
    let process = environment(&[
        ("LOCUST_USERS", "20"),
        ("LOCUST_SPAWN_RATE", "2.5"),
        ("LOCUST_RUN_TIME", "30s"),
    ]);
    let mut cli = args(true);
    cli.users = Some(3);
    cli.spawn_rate = Some(0.5);
    cli.run_time = Some(String::from("15s"));

    let settings = LoadSettings::resolve(&config(root.path(), &process), &cli)
        .expect("CLI settings should resolve");

    assert_eq!(settings.users().get(), 3);
    assert_eq!(settings.spawn_rate(), 0.5);
    assert_eq!(settings.run_time(), "15s");
}

#[test]
fn smoke_replaces_dotenv_and_default_values_but_preserves_process_values() {
    let root = repository_root(Some(
        "LOCUST_USERS=90\nLOCUST_SPAWN_RATE=9\nLOCUST_RUN_TIME=9m\n",
    ));
    let process = environment(&[("LOCUST_SPAWN_RATE", "2.5")]);

    let settings = LoadSettings::resolve(&config(root.path(), &process), &args(true))
        .expect("smoke settings should resolve");

    assert_eq!(settings.users().get(), 1);
    assert_eq!(settings.spawn_rate(), 2.5);
    assert_eq!(settings.run_time(), "10s");
}

#[test]
fn invalid_process_values_return_contextual_errors_including_empty_values() {
    for (key, value, expected) in [
        (
            "LOCUST_USERS",
            "",
            "LOCUST_USERS must be an integer greater than zero",
        ),
        (
            "LOCUST_SPAWN_RATE",
            "NaN",
            "LOCUST_SPAWN_RATE must be a finite number greater than zero",
        ),
        ("LOCUST_RUN_TIME", "forever", "positive Locust duration"),
    ] {
        let root = repository_root(None);
        let process = environment(&[(key, value)]);

        let error = LoadSettings::resolve(&config(root.path(), &process), &args(false))
            .expect_err("invalid load settings should fail");

        assert!(
            error.to_string().contains(expected),
            "unexpected error: {error:#}"
        );
    }
}

#[test]
fn run_time_rejects_units_and_group_orders_locust_does_not_support() {
    for run_time in ["1ms", "1d", "1s1s", "1s1m", "1m1h"] {
        let root = repository_root(None);
        let mut cli = args(false);
        cli.run_time = Some(run_time.to_owned());

        let error = LoadSettings::resolve(&config(root.path(), &Environment::new()), &cli)
            .expect_err("unsupported Locust run time must fail before process execution");

        assert!(
            error.to_string().contains("positive Locust duration"),
            "unexpected error for {run_time}: {error:#}"
        );
    }

    let root = repository_root(None);
    let mut cli = args(false);
    cli.run_time = Some("1h20m3s".to_owned());
    let settings = LoadSettings::resolve(&config(root.path(), &Environment::new()), &cli)
        .expect("ordered Locust h/m/s groups should resolve");
    assert_eq!(settings.run_time(), "1h20m3s");
}

#[test]
fn dataplane_locust_command_has_exact_compose_shape_and_environment() {
    let root = repository_root(None);
    let config = config(root.path(), &Environment::new());
    let settings = LoadSettings::resolve(&config, &args(false)).expect("settings should resolve");

    let run = LocustCommand::new_with_protocol_version(
        &config,
        project(&config, StackMode::Dataplane),
        StackMode::Dataplane,
        &settings,
        "scoped.jwt.value",
        Some("server-id"),
        "2025-11-25",
    )
    .expect("dataplane Locust command should build");

    let integration_dir = root.path().join(".integration");
    let asset_root = config.asset_root();
    let report_dir = integration_dir
        .join("reports")
        .join("load")
        .join("dataplane")
        .join("locust");
    assert_eq!(run.report_dir(), report_dir);
    assert!(run.report_dir().is_dir());
    let arguments = run.command().arguments();
    assert!(
        arguments
            .windows(3)
            .any(|values| values == ["run", "--rm", "--no-deps"])
    );
    assert!(arguments.contains(&volume_argument(&report_dir)));
    assert!(arguments.contains(&OsString::from("/mnt/locust-cf/locustfile_mcp.py")));
    assert!(
        arguments
            .windows(2)
            .any(|values| values == ["-e", "MCP_SKIP_TOOL_LIST"])
    );
    assert!(
        arguments
            .windows(2)
            .any(|values| values == ["--entrypoint", "locust"])
    );

    let expected_environment = HashMap::from([
        ("CF_INTEGRATION_DIR", integration_dir.as_os_str()),
        ("CF_INTEGRATION_ROOT", asset_root.as_os_str()),
        ("LOCUST_LOCUSTFILE", OsStr::new("locustfile_mcp.py")),
        ("LOCUST_MODE", OsStr::new("headless")),
        ("LOCUST_REQUEST_TIMEOUT_SECONDS", OsStr::new("60")),
        ("LOCUST_RUN_TIME", OsStr::new("5m")),
        ("LOCUST_SPAWN_RATE", OsStr::new("10")),
        ("LOCUST_USERS", OsStr::new("100")),
        ("MCPGATEWAY_BEARER_TOKEN", OsStr::new("scoped.jwt.value")),
        ("MCP_SERVER_ID", OsStr::new("server-id")),
        ("MCP_STACK_MODE", OsStr::new("dataplane")),
        ("MCP_PROTOCOL_VERSION", OsStr::new("2025-11-25")),
    ]);
    for (key, expected) in expected_environment {
        assert_eq!(
            run.command().environment().get(OsStr::new(key)),
            Some(&expected.to_owned()),
            "environment mismatch for {key}"
        );
    }
    assert_eq!(run.command().environment().len(), 12);
    assert!(
        !run.command()
            .environment()
            .contains_key(OsStr::new("COMPOSE_PROGRESS")),
        "Locust Compose runs must retain terminal-aware progress"
    );
}

#[test]
fn controlplane_uses_the_same_harness_mcp_adapter_and_does_not_require_server_id() {
    let root = repository_root(None);
    let process = environment(&[
        ("LOCUST_REQUEST_TIMEOUT_SECONDS", "2.5"),
        ("MCP_PROTOCOL_VERSION", "2025-11-25"),
        ("MCP_TOOL_NAMES", "safe_time,safe_echo"),
    ]);
    let config = config(root.path(), &process);
    let settings = LoadSettings::resolve(&config, &args(true)).expect("settings should resolve");

    let run = LocustCommand::new_with_protocol_version(
        &config,
        project(&config, StackMode::Controlplane),
        StackMode::Controlplane,
        &settings,
        "admin.jwt.value",
        None,
        "2025-06-18",
    )
    .expect("control-plane Locust command should build");

    assert_eq!(
        run.report_dir(),
        root.path()
            .join(".integration/reports/load/controlplane/locust")
    );
    assert_eq!(
        run.command()
            .environment()
            .get(OsStr::new("LOCUST_LOCUSTFILE")),
        Some(&OsString::from("locustfile_mcp.py"))
    );
    assert_eq!(
        run.command()
            .environment()
            .get(OsStr::new("MCP_STACK_MODE")),
        Some(&OsString::from("controlplane"))
    );
    assert_eq!(
        run.command()
            .environment()
            .get(OsStr::new("LOCUST_REQUEST_TIMEOUT_SECONDS")),
        Some(&OsString::from("2.5"))
    );
    assert_eq!(
        run.command()
            .environment()
            .get(OsStr::new("MCP_PROTOCOL_VERSION")),
        Some(&OsString::from("2025-06-18"))
    );
    assert_eq!(
        run.command()
            .environment()
            .get(OsStr::new("MCP_TOOL_NAMES")),
        Some(&OsString::from("safe_time,safe_echo"))
    );
    assert_eq!(
        run.command().environment().get(OsStr::new("MCP_SERVER_ID")),
        Some(&OsString::new())
    );
    let arguments = run.command().arguments();
    let entrypoint = arguments
        .windows(2)
        .find(|pair| pair[0] == "--entrypoint")
        .expect("control-plane command should replace the shell entrypoint");
    assert_eq!(entrypoint[1], "locust");
    assert!(
        arguments
            .windows(2)
            .any(|pair| pair == [OsString::from("-e"), OsString::from("MCP_STACK_MODE")]),
        "control-plane command must propagate MCP_STACK_MODE into the container"
    );
    for name in [
        "LOCUST_REQUEST_TIMEOUT_SECONDS",
        "MCP_PROTOCOL_VERSION",
        "MCP_TOOL_NAMES",
    ] {
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == [OsString::from("-e"), OsString::from(name)]),
            "control-plane command must propagate {name} into the container"
        );
    }
    assert!(arguments.contains(&OsString::from("/mnt/locust-cf/locustfile_mcp.py")));
    let mut expected_adapter_mount = config
        .asset_root()
        .join("scripts")
        .join("locustfile_mcp.py")
        .into_os_string();
    expected_adapter_mount.push(":/mnt/locust-cf/locustfile_mcp.py:ro");
    let adapter_mount = arguments
        .windows(2)
        .find(|pair| pair[0] == "--volume" && pair[1] == expected_adapter_mount)
        .expect("control-plane command should mount the harness MCP adapter");
    assert_eq!(adapter_mount[0], "--volume");
    assert!(arguments.contains(&OsString::from("--only-summary")));
    assert!(!arguments.contains(&OsString::from("/bin/sh")));
    assert!(!arguments.contains(&OsString::from("-c")));
}

#[test]
fn locust_request_timeout_rejects_empty_non_finite_and_non_positive_values() {
    for value in ["", "NaN", "inf", "0", "-1"] {
        let root = repository_root(None);
        let process = environment(&[("LOCUST_REQUEST_TIMEOUT_SECONDS", value)]);
        let config = config(root.path(), &process);
        let settings =
            LoadSettings::resolve(&config, &args(false)).expect("load settings should resolve");

        let error = LocustCommand::new_with_protocol_version(
            &config,
            project(&config, StackMode::Controlplane),
            StackMode::Controlplane,
            &settings,
            "token",
            None,
            "2025-11-25",
        )
        .expect_err("invalid request timeout should fail before launch");

        assert!(
            error.to_string().contains(
                "LOCUST_REQUEST_TIMEOUT_SECONDS must be a finite number greater than zero"
            ),
            "unexpected error for {value:?}: {error:#}"
        );
    }
}

#[test]
fn dataplane_requires_nonempty_server_id_and_all_modes_require_a_token() {
    let root = repository_root(None);
    let config = config(root.path(), &Environment::new());
    let settings = LoadSettings::resolve(&config, &args(false)).expect("settings should resolve");

    let missing_server = LocustCommand::new_with_protocol_version(
        &config,
        project(&config, StackMode::Dataplane),
        StackMode::Dataplane,
        &settings,
        "token",
        None,
        "2025-11-25",
    )
    .expect_err("dataplane server ID should be required");
    assert!(missing_server.to_string().contains("server ID"));

    let missing_token = LocustCommand::new_with_protocol_version(
        &config,
        project(&config, StackMode::Controlplane),
        StackMode::Controlplane,
        &settings,
        "",
        None,
        "2025-11-25",
    )
    .expect_err("bearer token should be required");
    assert!(missing_token.to_string().contains("bearer token"));
}

fn volume_argument(report_dir: &Path) -> OsString {
    let mut argument = report_dir.as_os_str().to_owned();
    argument.push(":/mnt/reports");
    argument
}
