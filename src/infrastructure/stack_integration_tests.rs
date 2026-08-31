use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;

use cf_integration::infrastructure::StackMode;
use cf_integration::infrastructure::compose::ComposeProject;
use cf_integration::infrastructure::stack::{
    BuildInputs, BuildMode, CleanupKind, FreshnessSnapshot, ServiceSnapshot, StackCommandPlan,
    StackFreshness, resolve_build,
};

fn project(mode: StackMode) -> ComposeProject {
    match mode {
        StackMode::Dataplane => ComposeProject::dataplane(
            Path::new("/repo"),
            Path::new("/cp"),
            OsString::from("integration"),
            false,
        ),
        StackMode::Controlplane => ComposeProject::controlplane(
            Path::new("/repo"),
            Path::new("/cp"),
            OsString::from("controlplane"),
            false,
        ),
    }
}

fn args(plan: StackCommandPlan) -> Vec<OsString> {
    plan.command().arguments().to_vec()
}

fn ends_with(actual: &[OsString], expected: &[&str]) -> bool {
    actual.ends_with(
        &expected
            .iter()
            .map(OsString::from)
            .collect::<Vec<OsString>>(),
    )
}

fn auto_inputs() -> BuildInputs {
    BuildInputs {
        controlplane_image_prebuilt: false,
        controlplane_image_present: true,
        controlplane_checkout_revision: Some("cp-head".to_owned()),
        controlplane_image_revision: Some("cp-head".to_owned()),
        include_dataplane: true,
        dataplane_source_ref: Some("main".to_owned()),
        dataplane_image_present: true,
        dataplane_checkout_revision: Some("dp-head".to_owned()),
        dataplane_image_revision: Some("dp-head".to_owned()),
    }
}

#[test]
fn build_mode_accepts_only_documented_values() {
    for value in ["true", "1"] {
        assert_eq!(
            value.parse::<BuildMode>().expect("true mode"),
            BuildMode::Always
        );
    }
    for value in ["false", "0"] {
        assert_eq!(
            value.parse::<BuildMode>().expect("false mode"),
            BuildMode::Never
        );
    }
    assert_eq!(
        "auto".parse::<BuildMode>().expect("auto mode"),
        BuildMode::Auto
    );
    assert!("yes".parse::<BuildMode>().is_err());
    assert!("".parse::<BuildMode>().is_err());
}

#[test]
fn explicit_build_modes_override_freshness() {
    let inputs = auto_inputs();
    assert!(resolve_build(BuildMode::Always, &inputs).build);
    assert!(!resolve_build(BuildMode::Never, &inputs).build);
}

#[test]
fn auto_builds_missing_or_stale_controlplane_unless_image_is_explicit() {
    let mut inputs = auto_inputs();
    assert!(!resolve_build(BuildMode::Auto, &inputs).build);

    inputs.controlplane_image_present = false;
    assert!(resolve_build(BuildMode::Auto, &inputs).build);

    inputs.controlplane_image_present = true;
    inputs.controlplane_image_revision = Some("old".to_owned());
    assert!(resolve_build(BuildMode::Auto, &inputs).build);

    inputs.controlplane_image_prebuilt = true;
    assert!(!resolve_build(BuildMode::Auto, &inputs).build);
}

#[test]
fn dataplane_freshness_is_considered_only_for_source_mode() {
    let mut inputs = auto_inputs();
    inputs.dataplane_image_revision = Some("old".to_owned());
    assert!(resolve_build(BuildMode::Auto, &inputs).build);

    inputs.dataplane_source_ref = Some(String::new());
    assert!(!resolve_build(BuildMode::Auto, &inputs).build);

    inputs.dataplane_source_ref = None;
    assert!(!resolve_build(BuildMode::Auto, &inputs).build);

    inputs.dataplane_source_ref = Some("main".to_owned());
    inputs.include_dataplane = false;
    assert!(!resolve_build(BuildMode::Auto, &inputs).build);
}

#[test]
fn dataplane_up_always_removes_orphans_and_optionally_builds() {
    let without_build = args(StackCommandPlan::up(
        project(StackMode::Dataplane),
        StackMode::Dataplane,
        false,
        false,
        1,
    ));
    let with_build = args(StackCommandPlan::up(
        project(StackMode::Dataplane),
        StackMode::Dataplane,
        true,
        false,
        1,
    ));

    assert!(ends_with(&without_build, &["up", "-d", "--remove-orphans"]));
    assert!(ends_with(
        &with_build,
        &["up", "-d", "--remove-orphans", "--build"]
    ));
    assert!(
        with_build
            .iter()
            .all(|argument| argument != OsStr::new("--quiet-build"))
    );
}

#[test]
fn controlplane_up_does_not_activate_locust_profile_when_ui_is_disabled() {
    let disabled = args(StackCommandPlan::up(
        project(StackMode::Controlplane),
        StackMode::Controlplane,
        false,
        false,
        3,
    ));
    assert!(ends_with(&disabled, &["up", "-d"]));
    assert!(
        disabled
            .iter()
            .all(|argument| !argument.to_string_lossy().starts_with("locust"))
    );

    let enabled = args(StackCommandPlan::up(
        project(StackMode::Controlplane),
        StackMode::Controlplane,
        true,
        true,
        3,
    ));
    assert!(ends_with(
        &enabled,
        &["up", "-d", "--build", "--scale", "locust_worker=3"]
    ));
}

#[test]
fn cleanup_status_logs_and_config_use_typed_compose_commands() {
    let dataplane_project = project(StackMode::Dataplane);
    let down = StackCommandPlan::cleanup(dataplane_project.clone(), CleanupKind::Down);
    assert!(ends_with(
        &args(down.clone()),
        &["down", "--remove-orphans"]
    ));
    assert!(
        !down
            .command()
            .environment()
            .contains_key(OsStr::new("COMPOSE_PROGRESS")),
        "Compose must select interactive or plain progress from its actual terminal"
    );
    assert!(ends_with(
        &args(StackCommandPlan::cleanup(
            dataplane_project.clone(),
            CleanupKind::Reset
        )),
        &["down", "--volumes", "--remove-orphans"]
    ));
    assert!(ends_with(
        &args(StackCommandPlan::status(dataplane_project.clone())),
        &["ps"]
    ));
    assert!(ends_with(
        &args(StackCommandPlan::logs(
            dataplane_project.clone(),
            [
                OsString::from("cf-controlplane"),
                OsString::from("cf-migration"),
                OsString::from("cf-register-fast-time"),
                OsString::from("cf-fast-time-server"),
                OsString::from("cf-nginx"),
                OsString::from("cf-postgres"),
                OsString::from("cf-pgbouncer"),
                OsString::from("cf-redis"),
                OsString::from("cf-dataplane"),
                OsString::from("cf-locust"),
                OsString::from("cf-locust-worker"),
                OsString::from("cf-locust-token"),
                OsString::from("cf-a2a-echo-agent"),
                OsString::from("cf-a2a-echo-agent-v0-3-0"),
                OsString::from("cf-register-a2a-echo"),
                OsString::from("cf-mcp-inspector"),
                OsString::from("cf-keycloak"),
                OsString::from("cf-conformance-server"),
                OsString::from("cf-conformance-proxy"),
                OsString::from("custom-service"),
            ]
        )),
        &[
            "logs",
            "-f",
            "gateway",
            "migration",
            "register_fast_time",
            "fast_time_server",
            "nginx",
            "postgres",
            "pgbouncer",
            "redis",
            "dataplane",
            "locust",
            "locust_worker",
            "locust_token",
            "a2a_echo_agent",
            "a2a_echo_agent_v0_3_0",
            "register_a2a_echo",
            "mcp_inspector",
            "keycloak",
            "mcp_conformance_server",
            "mcp_conformance_proxy",
            "custom-service",
        ]
    ));
    assert!(ends_with(
        &args(StackCommandPlan::config(
            dataplane_project,
            StackMode::Dataplane,
        )),
        &[
            "--profile",
            "testing",
            "config",
            "--no-interpolate",
            "--no-env-resolution",
        ]
    ));
    assert!(ends_with(
        &args(StackCommandPlan::config(
            project(StackMode::Controlplane),
            StackMode::Controlplane,
        )),
        &["config", "--no-interpolate", "--no-env-resolution"]
    ));
}

fn current_snapshot() -> FreshnessSnapshot {
    let running = [
        ("gateway", "cp-image", Some("cp-head")),
        ("dataplane", "dp-image", Some("dp-head")),
        ("nginx", "nginx", None),
        ("postgres", "postgres", None),
        ("pgbouncer", "pgbouncer", None),
        ("redis", "redis", None),
        ("fast_time_server", "fast-image", None),
    ]
    .into_iter()
    .map(|(name, image, revision)| {
        (
            name.to_owned(),
            ServiceSnapshot {
                running: true,
                completed_successfully: false,
                configured_image: Some(image.to_owned()),
                running_image_matches_configured: true,
                image_revision: revision.map(str::to_owned),
            },
        )
    });
    let completed = ["migration", "register_fast_time"].into_iter().map(|name| {
        (
            name.to_owned(),
            ServiceSnapshot {
                running: false,
                completed_successfully: true,
                configured_image: None,
                running_image_matches_configured: true,
                image_revision: None,
            },
        )
    });
    FreshnessSnapshot {
        services: running.chain(completed).collect::<BTreeMap<_, _>>(),
        controlplane_checkout_revision: Some("cp-head".to_owned()),
        dataplane_checkout_revision: Some("dp-head".to_owned()),
        controlplane_image_prebuilt: false,
        dataplane_source_enabled: true,
        expected_controlplane_image: "cp-image".to_owned(),
        expected_dataplane_image: "dp-image".to_owned(),
        expected_fast_time_image: "fast-image".to_owned(),
    }
}

#[test]
fn current_dataplane_stack_requires_services_images_and_setup_jobs() {
    assert_eq!(current_snapshot().evaluate(), StackFreshness::Current);

    let mut snapshot = current_snapshot();
    snapshot.services.get_mut("redis").expect("redis").running = false;
    assert_eq!(
        snapshot.evaluate(),
        StackFreshness::Stale("service is not running: redis".to_owned())
    );

    let mut snapshot = current_snapshot();
    snapshot
        .services
        .get_mut("register_fast_time")
        .expect("registration")
        .completed_successfully = false;
    assert_eq!(
        snapshot.evaluate(),
        StackFreshness::Stale(
            "setup service did not complete successfully: register_fast_time".to_owned()
        )
    );
}

#[test]
fn current_stack_rejects_wrong_fast_time_image() {
    let mut snapshot = current_snapshot();
    snapshot
        .services
        .get_mut("fast_time_server")
        .expect("fast time")
        .configured_image = Some("wrong".to_owned());
    assert_eq!(
        snapshot.evaluate(),
        StackFreshness::Stale("fast_time_server image differs".to_owned())
    );
}

#[test]
fn revision_checks_are_conditional_on_image_source() {
    let mut snapshot = current_snapshot();
    snapshot
        .services
        .get_mut("gateway")
        .expect("gateway")
        .image_revision = Some("old".to_owned());
    assert_eq!(
        snapshot.evaluate(),
        StackFreshness::Stale("cf-controlplane branch revision differs".to_owned())
    );
    snapshot.controlplane_image_prebuilt = true;
    assert_eq!(snapshot.evaluate(), StackFreshness::Current);

    let mut snapshot = current_snapshot();
    snapshot
        .services
        .get_mut("dataplane")
        .expect("dataplane")
        .image_revision = Some("old".to_owned());
    assert_eq!(
        snapshot.evaluate(),
        StackFreshness::Stale("cf-dataplane branch revision differs".to_owned())
    );
    snapshot.dataplane_source_enabled = false;
    assert_eq!(snapshot.evaluate(), StackFreshness::Current);
}

#[test]
fn stack_up_preserves_compose_auto_progress_without_shell_fragments() {
    let plan = StackCommandPlan::up(
        project(StackMode::Dataplane),
        StackMode::Dataplane,
        false,
        false,
        1,
    );
    let command = plan.command();
    assert_eq!(command.program(), OsStr::new("docker"));
    assert!(command.environment().is_empty());
    assert!(
        command
            .arguments()
            .iter()
            .all(|arg| !arg.to_string_lossy().contains("sh -c"))
    );
}
