use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use cf_integration_platform::compose::{ComposeProject, validate_integration_contract};

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("platform crate should be nested under crates")
}
use serde_json::json;

const EXPECTED_IMAGE: &str = "ghcr.io/ibm/cfex-mcp-fast-time-server:latest";

fn valid_config() -> serde_json::Value {
    json!({
        "services": {
            "fast_time_server": {
                "image": EXPECTED_IMAGE,
                "labels": {"name": "cf-fast-time-server"}
            },
            "register_fast_time": {
                "labels": {"name": "cf-register-fast-time"},
                "command": [
                    "wait for http://fast_time_server:9080/health",
                    "register http://fast_time_server:9080/mcp"
                ]
            },
            "fast_test_server": {
                "image": "example/modern:latest",
                "labels": {"name": "cf-fast-test-server"},
                "profiles": ["testing"]
            },
            "register_fast_test": {
                "labels": {"name": "cf-register-fast-test"},
                "profiles": ["testing"]
            }
        }
    })
}

fn messages(config: &serde_json::Value) -> Vec<String> {
    validate_integration_contract(config, EXPECTED_IMAGE)
        .into_iter()
        .map(|violation| violation.to_string())
        .collect()
}

fn run_fixture_patch(source: &str) -> (std::process::ExitStatus, String) {
    let directory = tempfile::tempdir().expect("create patch test directory");
    let target = directory.path().join("everything-server.ts");
    fs::write(&target, source).expect("write patch test input");
    let script = workspace_root().join("docker/patch-mcp-conformance-hosts.mjs");

    let output = Command::new("node")
        .arg(script)
        .arg(&target)
        .output()
        .expect("run conformance host patch");
    let contents = fs::read_to_string(target).expect("read patch test output");
    (output.status, contents)
}

#[test]
fn readme_documents_the_official_conformance_fixture_contract() {
    let readme = fs::read_to_string(workspace_root().join("README.md")).expect("read README");
    let normalized = readme.split_whitespace().collect::<Vec<_>>().join(" ");

    for fact in [
        "official TypeScript fixture",
        "Fast Time remains",
        "runs fixture-direct, controlplane, and dataplane lanes",
        "c321dd32035556e6769d3724a8ee97d87c3faaac",
        "defaults to MCP `2026-07-28`",
        "loopback `MCP_CLI_BASE_URL`",
        "passes an empty expected-failure file",
        "records raw failures without suppression",
        "same official fixture",
        "--server-era dual",
        "fixture-direct lane is the expected incompatible baseline",
    ] {
        assert!(normalized.contains(fact), "README must document: {fact}");
    }
}

#[test]
fn dataplane_compose_files_are_in_override_order() {
    let project = ComposeProject::dataplane(
        Path::new("/repo"),
        Path::new("/checkout"),
        OsString::from("cf"),
        false,
    );

    assert_eq!(
        project.files(),
        [
            PathBuf::from("/checkout/docker-compose.yml"),
            PathBuf::from("/repo/docker/docker-compose.cf-controlplane-build-labels.yaml"),
            PathBuf::from("/repo/docker/docker-compose.cf-dataplane.yaml"),
            PathBuf::from("/repo/docker/docker-compose.cf-integration.yaml"),
        ]
    );
    assert!(project.profiles().is_empty());
    assert_eq!(
        project.command(["config", "--format", "json"]).arguments(),
        [
            "compose",
            "-p",
            "cf",
            "-f",
            "/checkout/docker-compose.yml",
            "-f",
            "/repo/docker/docker-compose.cf-controlplane-build-labels.yaml",
            "-f",
            "/repo/docker/docker-compose.cf-dataplane.yaml",
            "-f",
            "/repo/docker/docker-compose.cf-integration.yaml",
            "config",
            "--format",
            "json",
        ]
        .map(OsString::from)
    );
}

#[test]
fn shared_metadata_overlay_clears_obsolete_fast_time_arguments() {
    let compose = fs::read_to_string(
        workspace_root().join("docker/docker-compose.cf-controlplane-build-labels.yaml"),
    )
    .expect("read shared control-plane metadata overlay");
    let overlay: serde_yaml::Value =
        serde_yaml::from_str(&compose).expect("parse shared control-plane metadata overlay");

    assert_eq!(
        overlay["services"]["fast_time_server"]["command"],
        serde_yaml::Value::Sequence(Vec::new())
    );
    let gateway_environment = overlay["services"]["gateway"]["environment"]
        .as_mapping()
        .expect("shared gateway environment must be a mapping");
    for key in [
        "PLATFORM_ADMIN_EMAIL",
        "PLATFORM_ADMIN_PASSWORD",
        "PASSWORD_CHANGE_ENFORCEMENT_ENABLED",
        "ADMIN_REQUIRE_PASSWORD_CHANGE_ON_BOOTSTRAP",
        "REQUIRE_PASSWORD_CHANGE_FOR_DEFAULT_PASSWORD",
    ] {
        assert!(
            gateway_environment.contains_key(serde_yaml::Value::String(key.to_owned())),
            "shared gateway environment must define {key}"
        );
    }
}

#[test]
fn compose_overlays_assign_short_container_display_names() {
    let shared = fs::read_to_string(
        workspace_root().join("docker/docker-compose.cf-controlplane-build-labels.yaml"),
    )
    .expect("read shared control-plane metadata overlay");
    let shared: serde_yaml::Value =
        serde_yaml::from_str(&shared).expect("parse shared control-plane metadata overlay");
    let dataplane =
        fs::read_to_string(workspace_root().join("docker/docker-compose.cf-dataplane.yaml"))
            .expect("read dataplane Compose overlay");
    let dataplane: serde_yaml::Value =
        serde_yaml::from_str(&dataplane).expect("parse dataplane Compose overlay");
    let conformance =
        fs::read_to_string(workspace_root().join("docker/docker-compose.cf-conformance.yaml"))
            .expect("read conformance Compose overlay");
    let conformance: serde_yaml::Value =
        serde_yaml::from_str(&conformance).expect("parse conformance Compose overlay");

    for (service, expected_name) in [
        ("gateway", "cf-controlplane"),
        ("migration", "cf-migration"),
        ("register_fast_time", "cf-register-fast-time"),
        ("fast_time_server", "cf-fast-time-server"),
        ("nginx", "cf-nginx"),
        ("postgres", "cf-postgres"),
        ("pgbouncer", "cf-pgbouncer"),
        ("redis", "cf-redis"),
        ("locust", "cf-locust"),
        ("locust_worker", "cf-locust-worker"),
        ("locust_token", "cf-locust-token"),
        ("fast_test_server", "cf-fast-test-server"),
        ("register_fast_test", "cf-register-fast-test"),
        ("a2a_echo_agent", "cf-a2a-echo-agent"),
        ("a2a_echo_agent_v0_3_0", "cf-a2a-echo-agent-v0-3-0"),
        ("register_a2a_echo", "cf-register-a2a-echo"),
        ("mcp_inspector", "cf-mcp-inspector"),
        ("keycloak", "cf-keycloak"),
    ] {
        assert_eq!(
            shared["services"][service]["labels"]["name"].as_str(),
            Some(expected_name),
            "{service} must expose a concise container display name"
        );
    }
    assert_eq!(
        dataplane["services"]["dataplane"]["labels"]["name"].as_str(),
        Some("cf-dataplane")
    );
    assert_eq!(
        conformance["services"]["mcp_conformance_server"]["labels"]["name"].as_str(),
        Some("cf-conformance-server")
    );
    assert_eq!(
        conformance["services"]["mcp_conformance_proxy"]["labels"]["name"].as_str(),
        Some("cf-conformance-proxy")
    );
}

#[test]
fn dataplane_overlays_track_the_current_image_build_and_environment_contract() {
    let root = workspace_root();
    let compose = fs::read_to_string(root.join("docker/docker-compose.cf-dataplane.yaml"))
        .expect("read dataplane Compose overlay");
    let compose: serde_yaml::Value =
        serde_yaml::from_str(&compose).expect("parse dataplane Compose overlay");
    let environment = compose["services"]["dataplane"]["environment"]
        .as_mapping()
        .expect("dataplane environment must be a mapping");

    for key in [
        "CONTEXTFORGE_DATA_PLANE_ADDRESS",
        "CONTEXTFORGE_DATA_PLANE_REDIS_HOSTNAME",
        "CONTEXTFORGE_DATA_PLANE_REDIS_PORT",
        "CONTEXTFORGE_DATA_PLANE_REDIS_CONNECTION_MODE",
        "CONTEXTFORGE_DATA_PLANE_TOKEN_SECRET",
        "CONTEXTFORGE_DATA_PLANE_TOKEN_VERIFICATION_PRIVATE_KEY",
        "CONTEXTFORGE_DATA_PLANE_UPSTREAM_CONNECTION_MODE",
        "CONTEXTFORGE_DATA_PLANE_USER_CONFIG_CACHE_EXPIRY_SECONDS",
        "CONTEXTFORGE_GATEWAY_RS_MCP_ALLOWED_HOSTS",
        "CONTEXTFORGE_GATEWAY_RS_MCP_ALLOWED_ORIGINS",
    ] {
        assert!(
            environment.contains_key(serde_yaml::Value::String(key.to_owned())),
            "dataplane environment must define {key}"
        );
    }
    assert_eq!(
        environment[serde_yaml::Value::String(
            "CONTEXTFORGE_DATA_PLANE_TOKEN_VERIFICATION_PRIVATE_KEY".to_owned()
        )]
        .as_str(),
        Some("/dev/null"),
        "the unused local-bootstrap signing key must not add a real private key to the harness"
    );
    assert!(
        environment
            [serde_yaml::Value::String("CONTEXTFORGE_GATEWAY_RS_MCP_ALLOWED_HOSTS".to_owned())]
        .as_str()
        .expect("MCP Host allowlist must be text")
        .contains(",nginx}"),
        "the default MCP Host allowlist must accept containerized Locust through nginx"
    );
    for obsolete in [
        "CONTEXTFORGE_GATEWAY_RS_ADDRESS",
        "CONTEXTFORGE_GATEWAY_RS_REDIS_HOSTNAME",
        "CONTEXTFORGE_GATEWAY_RS_TOKEN_SECRET",
        "CONTEXTFORGE_GATEWAY_RS_UPSTREAM_CONNECTION_MODE",
        "CONTEXTFORGE_GATEWAY_RS_USER_CONFIG_CACHE_EXPIRY_SECONDS",
    ] {
        assert!(
            !environment.contains_key(serde_yaml::Value::String(obsolete.to_owned())),
            "obsolete dataplane environment key must be absent: {obsolete}"
        );
    }

    let build = fs::read_to_string(root.join("docker/docker-compose.cf-dataplane-build.yaml"))
        .expect("read dataplane build overlay");
    let build: serde_yaml::Value =
        serde_yaml::from_str(&build).expect("parse dataplane build overlay");
    assert_eq!(
        build["services"]["dataplane"]["build"]["dockerfile"].as_str(),
        Some("docker/Dockerfile")
    );
}

#[test]
fn source_dataplane_adds_build_overlay_last() {
    let project = ComposeProject::dataplane(
        Path::new("/repo"),
        Path::new("/checkout"),
        OsString::from("project"),
        true,
    );

    assert_eq!(
        project.files().last().map(PathBuf::as_path),
        Some(Path::new(
            "/repo/docker/docker-compose.cf-dataplane-build.yaml"
        ))
    );
}

#[test]
fn conformance_fixture_is_an_explicit_overlay_and_profile() {
    let default_project = ComposeProject::dataplane(
        Path::new("/repo"),
        Path::new("/checkout"),
        OsString::from("project"),
        false,
    );

    assert!(default_project.profiles().is_empty());
    assert!(
        !default_project
            .files()
            .iter()
            .any(|file| file.ends_with("docker-compose.cf-conformance.yaml"))
    );

    let overlay = default_project
        .clone()
        .with_profiles(["testing"])
        .with_conformance_overlay(Path::new("/repo"));
    assert_eq!(overlay.profiles(), ["testing"]);
    assert_eq!(
        &overlay.files()[..default_project.files().len()],
        default_project.files()
    );
    assert_eq!(
        overlay.files().last().map(PathBuf::as_path),
        Some(Path::new("/repo/docker/docker-compose.cf-conformance.yaml"))
    );

    let conformance = overlay.with_conformance_fixture(Path::new("/repo"));
    assert_eq!(conformance.profiles(), ["testing", "conformance"]);
    let deduplicated = conformance.with_conformance_fixture(Path::new("/repo"));
    assert_eq!(deduplicated.profiles(), ["testing", "conformance"]);
    assert_eq!(
        deduplicated
            .files()
            .iter()
            .filter(|file| file.ends_with("docker-compose.cf-conformance.yaml"))
            .count(),
        1
    );
}

#[test]
fn conformance_container_inputs_pin_the_runner_revision_and_protocol_fixture() {
    let root = workspace_root();
    let dockerfile = fs::read_to_string(root.join("docker/mcp-conformance-server.Dockerfile"))
        .expect("read conformance Dockerfile");
    let patch = fs::read_to_string(root.join("docker/patch-mcp-conformance-hosts.mjs"))
        .expect("read host patch script");
    let compose = fs::read_to_string(root.join("docker/docker-compose.cf-conformance.yaml"))
        .expect("read conformance Compose overlay");

    assert!(dockerfile.contains("FROM node:22-bookworm-slim"));
    assert!(
        dockerfile
            .contains("ARG MCP_CONFORMANCE_REVISION=c321dd32035556e6769d3724a8ee97d87c3faaac")
    );
    assert!(dockerfile.contains(
        "git clone https://github.com/modelcontextprotocol/conformance.git mcp-conformance"
    ));
    assert!(
        dockerfile
            .contains("git -C mcp-conformance checkout --detach \"${MCP_CONFORMANCE_REVISION}\"")
    );
    assert!(dockerfile.contains("WORKDIR /opt/mcp-conformance/examples/servers/typescript"));
    assert!(dockerfile.contains("npm ci"));
    assert!(
        dockerfile
            .contains("node /usr/local/bin/patch-mcp-conformance-hosts.mjs everything-server.ts")
    );
    assert!(dockerfile.contains(
        "git diff --exit-code -- . ':(exclude)examples/servers/typescript/everything-server.ts'"
    ));
    assert!(
        dockerfile.contains("git diff --check -- examples/servers/typescript/everything-server.ts")
    );
    assert!(dockerfile.contains(
        "grep -Fxc \"const app = createMcpExpressApp({ allowedHosts: ['mcp_conformance_server', 'localhost', '127.0.0.1', '::1'] });\" examples/servers/typescript/everything-server.ts"
    ));
    assert!(dockerfile.contains("ENV PORT=3000"));
    assert!(dockerfile.contains("EXPOSE 3000"));
    assert!(dockerfile.contains("CMD [\"npm\", \"start\"]"));

    let old = "const app = createMcpExpressApp();";
    let replacement = "const app = createMcpExpressApp({ allowedHosts: ['mcp_conformance_server', 'localhost', '127.0.0.1', '::1'] });";
    assert!(patch.contains(old));
    assert!(patch.contains(replacement));
    assert!(patch.contains("replacementCount !== 1"));
    assert!(patch.contains("process.argv[2]"));
    assert!(patch.contains("MCP_CONFORMANCE_SERVER_ERA"));
    assert!(patch.contains("isModernEraRequest"));
    assert!(patch.contains("UnsupportedProtocolVersionError"));

    let actual_compose: serde_yaml::Value =
        serde_yaml::from_str(&compose).expect("parse conformance Compose overlay");
    let expected_compose: serde_yaml::Value = serde_yaml::from_str(
        r#"
services:
  gateway:
    environment:
      GATEWAY_TOOL_NAME_SEPARATOR: "_"
  mcp_conformance_server:
    profiles: ["conformance"]
    image: cf-integration/mcp-conformance-server:0.2.0-alpha.11
    labels:
      name: cf-conformance-server
    build:
      context: ${CF_INTEGRATION_ROOT:?Set CF_INTEGRATION_ROOT to the integration harness root}
      dockerfile: docker/mcp-conformance-server.Dockerfile
    restart: "no"
    environment:
      PORT: "3000"
      MCP_CONFORMANCE_SERVER_ERA: ${CF_CONFORMANCE_SERVER_ERA:-dual}
    ports:
      - "127.0.0.1:${CF_CONFORMANCE_PORT:-0}:3000"
    networks:
      - mcpnet
    healthcheck:
      test:
        - CMD
        - node
        - -e
        - fetch('http://127.0.0.1:3000/mcp').then(response => { if (response.status !== 400) process.exit(1); }).catch(() => process.exit(1))
      interval: 2s
      timeout: 2s
      retries: 30
      start_period: 2s
  mcp_conformance_proxy:
    profiles: ["conformance"]
    image: nginx:1.30.4-alpine3.24
    labels:
      name: cf-conformance-proxy
    restart: "no"
    volumes:
      - ${CF_INTEGRATION_ROOT:?Set CF_INTEGRATION_ROOT to the integration harness root}/docker/nginx.cf-conformance-proxy.conf:/etc/nginx/conf.d/default.conf:ro
    networks:
      - mcpnet
    depends_on:
      mcp_conformance_server:
        condition: service_healthy
"#,
    )
    .expect("parse expected conformance Compose contract");
    assert_eq!(actual_compose, expected_compose);

    let proxy = fs::read_to_string(root.join("docker/nginx.cf-conformance-proxy.conf"))
        .expect("read conformance proxy config");
    assert!(proxy.contains("proxy_pass http://mcp_conformance_server:3000;"));
    assert!(proxy.contains("proxy_set_header Host localhost:3000;"));
}

#[test]
fn conformance_fixture_patch_is_fail_closed_and_adds_server_era_routing() {
    let old = "const app = createMcpExpressApp();";
    let replacement = "const app = createMcpExpressApp({ allowedHosts: ['mcp_conformance_server', 'localhost', '127.0.0.1', '::1'] });";
    let versions = r#"const LEGACY_SESSION_PROTOCOL_VERSIONS = [
  '2024-11-05',
  '2025-03-26',
  '2025-06-18',
  '2025-11-25'
];"#;
    let classification = r#"  const isLegacySessionEraRequest =
    meta === undefined &&
    reqVersion !== undefined &&
    LEGACY_SESSION_PROTOCOL_VERSIONS.includes(reqVersion);

  if (!sessionId && (reqVersion || meta) && !isLegacySessionEraRequest) {"#;
    let source = format!("before\n{old}\n{versions}\n{classification}\nafter\n");

    let (status, patched) = run_fixture_patch(&source);
    assert!(status.success());
    assert!(patched.contains(replacement));
    assert!(patched.contains("process.env.MCP_CONFORMANCE_SERVER_ERA ?? 'dual'"));
    assert!(patched.contains("CONFORMANCE_SERVER_ERA === 'legacy' && isModernEraRequest"));
    assert!(patched.contains("CONFORMANCE_SERVER_ERA === 'modern'"));
    assert!(!patched.contains(old));

    for missing in [old, versions, classification] {
        let unchanged = source.replacen(missing, "missing patch target", 1);
        let (status, contents) = run_fixture_patch(&unchanged);
        assert!(!status.success());
        assert_eq!(contents, unchanged);
    }
}

#[test]
fn controlplane_default_excludes_optional_profiles_and_sso_is_explicit() {
    let without_sso = ComposeProject::controlplane(
        Path::new("/repo"),
        Path::new("/checkout"),
        OsString::from("cp"),
        false,
    );
    let with_sso = ComposeProject::controlplane(
        Path::new("/repo"),
        Path::new("/checkout"),
        OsString::from("cp"),
        true,
    );

    assert!(without_sso.profiles().is_empty());
    assert_eq!(with_sso.profiles(), ["sso"]);
    assert_eq!(
        without_sso.files(),
        [
            PathBuf::from("/checkout/docker-compose.yml"),
            PathBuf::from("/repo/docker/docker-compose.cf-controlplane-build-labels.yaml"),
        ]
    );
}

#[test]
fn valid_contract_has_no_violations() {
    assert!(messages(&valid_config()).is_empty());
}

#[test]
fn contract_reports_missing_or_wrong_fast_time_image() {
    let mut missing = valid_config();
    missing["services"]
        .as_object_mut()
        .expect("services object")
        .remove("fast_time_server");
    assert_eq!(
        messages(&missing),
        ["fast_time_server is missing from the integration compose config"]
    );

    let mut wrong = valid_config();
    wrong["services"]["fast_time_server"]["image"] = json!("wrong/image:tag");
    assert_eq!(
        messages(&wrong),
        [format!(
            "fast_time_server image is \"wrong/image:tag\"; expected \"{EXPECTED_IMAGE}\""
        )]
    );
}

#[test]
fn contract_reports_incorrect_container_display_names() {
    let mut config = valid_config();
    config["services"]["register_fast_time"]["labels"]["name"] = json!("wrong");
    config["services"]["fast_time_server"]
        .as_object_mut()
        .expect("service object")
        .remove("labels");

    assert_eq!(
        messages(&config),
        [
            "register_fast_time display name is \"wrong\"; expected \"cf-register-fast-time\"",
            "fast_time_server display name is \"\"; expected \"cf-fast-time-server\"",
        ]
    );
}

#[test]
fn fast_test_services_must_stay_behind_profiles() {
    let mut config = valid_config();
    config["services"]["fast_test_server"]
        .as_object_mut()
        .expect("service object")
        .remove("profiles");
    config["services"]["register_fast_test"]["profiles"] = json!([]);

    assert_eq!(
        messages(&config),
        [
            "fast_test_server is active in the base integration stack; keep fast-test behind an explicit profile",
            "register_fast_test is active in the base integration stack; keep fast-test behind an explicit profile",
        ]
    );
}

#[test]
fn every_legacy_image_prefix_is_rejected_in_sorted_service_order() {
    let mut config = valid_config();
    let services = config["services"].as_object_mut().expect("services object");
    services.insert(
        "zulu".to_owned(),
        json!({"image": "ghcr.io/ibm/fast-time-server:old"}),
    );
    services.insert(
        "alpha".to_owned(),
        json!({"image": "mcpgateway/fast-test-server@sha256:abc"}),
    );

    assert_eq!(
        messages(&config),
        [
            "alpha uses legacy fast-test/time image \"mcpgateway/fast-test-server@sha256:abc\"",
            "zulu uses legacy fast-test/time image \"ghcr.io/ibm/fast-time-server:old\"",
        ]
    );
}

#[test]
fn registration_must_use_health_and_streamable_http_urls() {
    let mut config = valid_config();
    config["services"]["register_fast_time"]["command"] = json!("wrong command");

    assert_eq!(
        messages(&config),
        [
            "register_fast_time does not wait for fast_time_server on port 9080",
            "register_fast_time does not register the streamable HTTP endpoint at /mcp",
        ]
    );
}

#[test]
fn malformed_rendered_config_returns_stable_diagnostics_instead_of_panicking() {
    assert_eq!(
        messages(&json!({"services": []})),
        ["rendered Compose config has no services object"]
    );
    assert_eq!(
        messages(&json!({})),
        ["rendered Compose config has no services object"]
    );
}

#[test]
fn multiple_violations_have_stable_contract_order() {
    let config = json!({
        "services": {
            "register_fast_time": {
                "command": [],
                "labels": {"name": "cf-register-fast-time"}
            },
            "fast_test_server": {
                "image": "ghcr.io/ibm/fast-time-server:old",
                "labels": {"name": "cf-fast-test-server"}
            }
        }
    });

    assert_eq!(
        messages(&config),
        [
            "fast_time_server is missing from the integration compose config",
            "fast_test_server is active in the base integration stack; keep fast-test behind an explicit profile",
            "fast_test_server uses legacy fast-test/time image \"ghcr.io/ibm/fast-time-server:old\"",
            "register_fast_time does not wait for fast_time_server on port 9080",
            "register_fast_time does not register the streamable HTTP endpoint at /mcp",
        ]
    );
}
