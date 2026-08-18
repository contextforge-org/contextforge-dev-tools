use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, Response, StatusCode};
use axum::routing::any;
use cf_integration_load::{GooseLoadConfig, GooseRunError, LoadEngine, LoadRequest, LoadSettings};
use cf_integration_mcp::mcp::{ACCEPT, PROTOCOL_VERSION, STATELESS_PROTOCOL_VERSION};
use cf_integration_platform::StackMode;
use cf_integration_platform::config::{AppConfig, Environment};
use serde_json::{Value, json};
use tempfile::TempDir;

const TOKEN: &str = "secret.goose.jwt";
const SESSION_ID: &str = "mock-session-id";

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

fn app_config(root: &Path, process: &Environment) -> AppConfig {
    AppConfig::load(process, &root.join("target/debug/cf-integration"), root)
        .expect("application config should load")
        .config
}

fn load_settings(config: &AppConfig, run_time: &str) -> LoadSettings {
    LoadSettings::resolve(
        config,
        &LoadRequest {
            engine: LoadEngine::Goose,
            smoke: false,
            users: Some(1),
            spawn_rate: Some(2.5),
            run_time: Some(run_time.to_owned()),
        },
    )
    .expect("load settings should resolve")
}

#[test]
fn dataplane_configuration_maps_settings_and_encodes_the_public_server_path() {
    let root = repository_root();
    let config = app_config(
        root.path(),
        &environment(&[("MCP_CLI_BASE_URL", "http://127.0.0.1:9321")]),
    );
    let settings = load_settings(&config, "1m250ms");

    let goose = GooseLoadConfig::new(
        &config,
        StackMode::Dataplane,
        &settings,
        TOKEN,
        Some("server/with space"),
    )
    .expect("Goose configuration should build");

    assert_eq!(goose.host(), "http://127.0.0.1:9321/");
    assert_eq!(goose.endpoint(), "/servers/server%2Fwith%20space/mcp");
    assert_eq!(goose.users(), 1);
    assert_eq!(goose.hatch_rate(), 2.5);
    assert_eq!(goose.run_time(), "61s");
    assert_eq!(
        goose.reports().html(),
        root.path()
            .join(".integration/reports/load/dataplane/goose/goose-report.html")
    );
    assert_eq!(
        goose.reports().json(),
        root.path()
            .join(".integration/reports/load/dataplane/goose/goose-report.json")
    );
    let debug = format!("{goose:?}");
    assert!(!debug.contains(TOKEN));
    assert!(debug.contains("[REDACTED]"));
}

#[test]
fn controlplane_configuration_targets_raw_mcp_and_needs_no_server_id() {
    let root = repository_root();
    let config = app_config(root.path(), &Environment::new());
    let settings = load_settings(&config, "1s");

    let goose = GooseLoadConfig::new(&config, StackMode::Controlplane, &settings, TOKEN, None)
        .expect("control-plane Goose configuration should build");

    assert_eq!(goose.endpoint(), "/mcp");
    assert!(
        goose.reports().html().is_file()
            || goose.reports().html().parent().is_some_and(Path::is_dir)
    );
}

#[test]
fn explicit_protocol_version_is_retained_by_goose_configuration() {
    let root = repository_root();
    let config = app_config(root.path(), &Environment::new());
    let settings = load_settings(&config, "1s");

    let goose = GooseLoadConfig::new_with_protocol_version(
        &config,
        StackMode::Controlplane,
        &settings,
        TOKEN,
        None,
        "2025-06-18",
    )
    .expect("control-plane Goose configuration should build");

    assert_eq!(goose.protocol_version(), "2025-06-18");
}

#[test]
fn configuration_rejects_missing_credentials_and_dataplane_server_id() {
    let root = repository_root();
    let config = app_config(root.path(), &Environment::new());
    let settings = load_settings(&config, "1s");

    let token_error = GooseLoadConfig::new(&config, StackMode::Controlplane, &settings, " ", None)
        .expect_err("empty bearer token should fail");
    assert!(token_error.to_string().contains("bearer token"));

    let server_error = GooseLoadConfig::new(&config, StackMode::Dataplane, &settings, TOKEN, None)
        .expect_err("missing dataplane server ID should fail");
    assert!(server_error.to_string().contains("server ID"));
}

#[test]
fn configuration_rejects_a_duration_goose_cannot_represent() {
    let root = repository_root();
    let config = app_config(root.path(), &Environment::new());
    let settings = load_settings(&config, "18446744073709551615d");

    let error = GooseLoadConfig::new(&config, StackMode::Controlplane, &settings, TOKEN, None)
        .expect_err("duration larger than Goose usize seconds should fail");

    assert!(error.to_string().contains("too large"));
}

#[derive(Clone, Debug)]
struct MockState {
    observations: Arc<Mutex<Vec<Observation>>>,
    omit_session: bool,
    reject_notification: bool,
    nonempty_notification: bool,
    backend_marker: Option<&'static str>,
    delete_status: StatusCode,
    initialize_result: Option<Value>,
    protocol_version: &'static str,
}

#[derive(Clone, Debug)]
struct Observation {
    http_method: Method,
    rpc_method: Option<String>,
    path: String,
    accept: Option<String>,
    authenticated: bool,
    session: Option<String>,
    protocol_version: Option<String>,
    routing_method: Option<String>,
    routing_name: Option<String>,
    metadata_protocol_version: Option<String>,
    initialize_protocol_version: Option<String>,
    called_tool: Option<String>,
}

async fn mcp_handler(State(state): State<MockState>, request: Request<Body>) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .expect("mock request body should be readable");
    let payload = if bytes.is_empty() {
        None
    } else {
        serde_json::from_slice::<Value>(&bytes).ok()
    };
    let rpc_method = payload
        .as_ref()
        .and_then(|value| value.get("method"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let called_tool = payload
        .as_ref()
        .and_then(|value| value.pointer("/params/name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let observation = Observation {
        http_method: parts.method.clone(),
        rpc_method: rpc_method.clone(),
        path: parts.uri.path().to_owned(),
        accept: header(&parts.headers, "accept"),
        authenticated: header(&parts.headers, "authorization").as_deref()
            == Some("Bearer secret.goose.jwt"),
        session: header(&parts.headers, "mcp-session-id"),
        protocol_version: header(&parts.headers, "mcp-protocol-version"),
        routing_method: header(&parts.headers, "mcp-method"),
        routing_name: header(&parts.headers, "mcp-name"),
        metadata_protocol_version: payload
            .as_ref()
            .and_then(|value| {
                value.pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion")
            })
            .and_then(Value::as_str)
            .map(str::to_owned),
        initialize_protocol_version: payload
            .as_ref()
            .and_then(|value| value.pointer("/params/protocolVersion"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        called_tool,
    };
    state
        .observations
        .lock()
        .expect("mock observation lock should not be poisoned")
        .push(observation);

    if header(&parts.headers, "authorization").as_deref() != Some("Bearer secret.goose.jwt")
        || header(&parts.headers, "accept").as_deref() != Some(ACCEPT)
    {
        return response(StatusCode::BAD_REQUEST, "text/plain", "bad common headers");
    }

    if parts.method == Method::DELETE {
        return if has_session_headers(&parts.headers, state.protocol_version) {
            response(state.delete_status, "text/plain", "")
        } else {
            response(StatusCode::BAD_REQUEST, "text/plain", "bad delete headers")
        };
    }

    let Some(payload) = payload else {
        return response(StatusCode::BAD_REQUEST, "text/plain", "missing JSON body");
    };
    let id = payload.get("id").cloned().unwrap_or(Value::Null);
    match rpc_method.as_deref() {
        Some("server/discover") => {
            if !has_stateless_headers(
                &parts.headers,
                &payload,
                "server/discover",
                None,
                state.protocol_version,
            ) {
                return response(
                    StatusCode::BAD_REQUEST,
                    "text/plain",
                    "bad discover request",
                );
            }
            json_response(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "supportedVersions": [state.protocol_version],
                    "capabilities": {"tools": {}},
                    "resultType": "complete",
                    "cacheScope": "private",
                    "ttlMs": 0
                }
            }))
        }
        Some("initialize") => {
            if header(&parts.headers, "mcp-session-id").is_some()
                || header(&parts.headers, "mcp-protocol-version").is_some()
            {
                return response(
                    StatusCode::BAD_REQUEST,
                    "text/plain",
                    "early session headers",
                );
            }
            let result = state.initialize_result.clone().unwrap_or_else(|| {
                json!({
                    "protocolVersion": state.protocol_version,
                    "capabilities": {},
                    "serverInfo": {"name": "mock", "version": "1"}
                })
            });
            let mut builder = Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream; charset=utf-8");
            if !state.omit_session {
                builder = builder.header("mcp-session-id", SESSION_ID);
            }
            builder
                .body(Body::from(format!(
                    "data: {}\n\n",
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result
                    })
                )))
                .expect("mock initialize response should build")
        }
        Some("notifications/initialized") => {
            if state.reject_notification {
                return response(StatusCode::INTERNAL_SERVER_ERROR, "application/json", "");
            }
            if has_session_headers(&parts.headers, state.protocol_version)
                && payload.get("id").is_none()
            {
                response(
                    StatusCode::ACCEPTED,
                    "application/json",
                    if state.nonempty_notification {
                        "{}"
                    } else {
                        ""
                    },
                )
            } else {
                response(StatusCode::BAD_REQUEST, "text/plain", "bad notification")
            }
        }
        Some("tools/list") => {
            let valid_headers = if state.protocol_version == STATELESS_PROTOCOL_VERSION {
                has_stateless_headers(
                    &parts.headers,
                    &payload,
                    "tools/list",
                    None,
                    state.protocol_version,
                )
            } else {
                has_session_headers(&parts.headers, state.protocol_version)
            };
            if !valid_headers {
                return response(StatusCode::BAD_REQUEST, "text/plain", "bad list headers");
            }
            json_response(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {"name": "delete_everything_echo", "inputSchema": {"type": "object"}},
                        {"name": "echo", "inputSchema": {"type": "object"}}
                    ]
                }
            }))
        }
        Some("tools/call") => {
            let valid_headers = if state.protocol_version == STATELESS_PROTOCOL_VERSION {
                has_stateless_headers(
                    &parts.headers,
                    &payload,
                    "tools/call",
                    Some("echo"),
                    state.protocol_version,
                )
            } else {
                has_session_headers(&parts.headers, state.protocol_version)
            };
            if !valid_headers
                || payload.pointer("/params/name").and_then(Value::as_str) != Some("echo")
                || payload
                    .pointer("/params/arguments/message")
                    .and_then(Value::as_str)
                    != Some("cf-integration")
            {
                return response(StatusCode::BAD_REQUEST, "text/plain", "unsafe call");
            }
            response(
                StatusCode::OK,
                "text/event-stream",
                &format!(
                    "data: {}\n\n",
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"content": [{"type": "text", "text": "ok"}], "isError": false}
                    })
                ),
            )
        }
        Some("ping") => {
            if !has_session_headers(&parts.headers, state.protocol_version) {
                return response(StatusCode::BAD_REQUEST, "text/plain", "bad ping headers");
            }
            json_response(json!({"jsonrpc": "2.0", "id": id, "result": {}}))
        }
        _ => response(StatusCode::BAD_REQUEST, "text/plain", "unknown method"),
    }
}

async fn marked_mcp_handler(
    State(state): State<MockState>,
    request: Request<Body>,
) -> Response<Body> {
    let marker = state.backend_marker;
    let mut response = mcp_handler(State(state), request).await;
    if let Some(marker) = marker {
        response.headers_mut().insert(
            "x-cf-integration-backend",
            marker.parse().expect("test backend marker should be valid"),
        );
    }
    response
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn has_session_headers(headers: &HeaderMap, protocol_version: &str) -> bool {
    header(headers, "mcp-session-id").as_deref() == Some(SESSION_ID)
        && header(headers, "mcp-protocol-version").as_deref() == Some(protocol_version)
}

fn has_stateless_headers(
    headers: &HeaderMap,
    payload: &Value,
    method: &str,
    name: Option<&str>,
    protocol_version: &str,
) -> bool {
    header(headers, "mcp-session-id").is_none()
        && header(headers, "mcp-protocol-version").as_deref() == Some(protocol_version)
        && header(headers, "mcp-method").as_deref() == Some(method)
        && header(headers, "mcp-name").as_deref() == name
        && payload
            .pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion")
            .and_then(Value::as_str)
            == Some(protocol_version)
}

fn response(status: StatusCode, content_type: &str, body: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .body(Body::from(body.to_owned()))
        .expect("mock response should build")
}

fn json_response(value: Value) -> Response<Body> {
    response(
        StatusCode::OK,
        "application/json; charset=utf-8",
        &value.to_string(),
    )
}

async fn spawn_mock(
    omit_session: bool,
    reject_notification: bool,
    nonempty_notification: bool,
) -> (String, MockState, tokio::task::JoinHandle<()>) {
    spawn_mock_with_marker(
        omit_session,
        reject_notification,
        nonempty_notification,
        Some("dataplane"),
    )
    .await
}

async fn spawn_mock_with_marker(
    omit_session: bool,
    reject_notification: bool,
    nonempty_notification: bool,
    backend_marker: Option<&'static str>,
) -> (String, MockState, tokio::task::JoinHandle<()>) {
    spawn_mock_with_options(
        omit_session,
        reject_notification,
        nonempty_notification,
        backend_marker,
        StatusCode::NO_CONTENT,
        None,
        PROTOCOL_VERSION,
    )
    .await
}

async fn spawn_mock_with_options(
    omit_session: bool,
    reject_notification: bool,
    nonempty_notification: bool,
    backend_marker: Option<&'static str>,
    delete_status: StatusCode,
    initialize_result: Option<Value>,
    protocol_version: &'static str,
) -> (String, MockState, tokio::task::JoinHandle<()>) {
    let state = MockState {
        observations: Arc::new(Mutex::new(Vec::new())),
        omit_session,
        reject_notification,
        nonempty_notification,
        backend_marker,
        delete_status,
        initialize_result,
        protocol_version,
    };
    let app = Router::new()
        .route("/{*path}", any(marked_mcp_handler))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock listener should bind");
    let address = listener
        .local_addr()
        .expect("mock listener should have a local address");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("mock server should run");
    });
    (format!("http://{address}"), state, handle)
}

async fn spawn_mock_with_initialize_result(
    initialize_result: Value,
) -> (String, MockState, tokio::task::JoinHandle<()>) {
    spawn_mock_with_options(
        false,
        false,
        false,
        Some("dataplane"),
        StatusCode::NO_CONTENT,
        Some(initialize_result),
        PROTOCOL_VERSION,
    )
    .await
}

#[derive(Clone)]
struct RedirectState {
    location: String,
}

async fn redirect_handler(State(state): State<RedirectState>) -> Response<Body> {
    Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header("location", state.location)
        .body(Body::empty())
        .expect("redirect response should build")
}

async fn redirect_target_handler(State(hits): State<Arc<AtomicUsize>>) -> Response<Body> {
    hits.fetch_add(1, Ordering::SeqCst);
    response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "text/plain",
        "redirected",
    )
}

#[tokio::test]
async fn goose_never_follows_redirects_with_a_bearer_credential() {
    let hits = Arc::new(AtomicUsize::new(0));
    let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("redirect target should bind");
    let target_address = target_listener.local_addr().expect("target address");
    let target_app = Router::new()
        .fallback(any(redirect_target_handler))
        .with_state(Arc::clone(&hits));
    let target = tokio::spawn(async move {
        axum::serve(target_listener, target_app)
            .await
            .expect("redirect target should run");
    });

    let origin_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("redirect origin should bind");
    let origin_address = origin_listener.local_addr().expect("origin address");
    let origin_app = Router::new()
        .fallback(any(redirect_handler))
        .with_state(RedirectState {
            location: format!("http://{target_address}/capture"),
        });
    let origin = tokio::spawn(async move {
        axum::serve(origin_listener, origin_app)
            .await
            .expect("redirect origin should run");
    });

    let host = format!("http://{origin_address}");
    let root = repository_root();
    let config = app_config(root.path(), &environment(&[("MCP_CLI_BASE_URL", &host)]));
    let settings = load_settings(&config, "1s");
    let error = GooseLoadConfig::new(&config, StackMode::Controlplane, &settings, TOKEN, None)
        .expect("Goose configuration should build")
        .execute()
        .await
        .expect_err("redirect response must fail closed");

    origin.abort();
    target.abort();
    assert!(matches!(error, GooseRunError::FailedMetrics { .. }));
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "redirect target was contacted"
    );
}

#[tokio::test]
async fn execute_runs_strict_session_flow_with_dynamic_safe_tools_and_reports() {
    let (host, state, server) = spawn_mock(false, false, false).await;
    let root = repository_root();
    let config = app_config(root.path(), &environment(&[("MCP_CLI_BASE_URL", &host)]));
    let settings = load_settings(&config, "1s");
    let goose = GooseLoadConfig::new(
        &config,
        StackMode::Dataplane,
        &settings,
        TOKEN,
        Some("server/with space"),
    )
    .expect("Goose configuration should build");

    let outcome = goose.execute().await.expect("strict MCP flow should pass");
    server.abort();

    assert_eq!(outcome.failed_requests(), 0);
    assert_eq!(outcome.failed_transactions(), 0);
    for report in [outcome.reports().html(), outcome.reports().json()] {
        let contents = fs::read_to_string(report).expect("Goose report should be readable");
        assert!(!contents.contains(TOKEN), "report leaked bearer token");
    }

    let observations = state
        .observations
        .lock()
        .expect("mock observation lock should not be poisoned")
        .clone();
    assert!(observations.len() >= 6, "observations: {observations:#?}");
    assert_eq!(observations[0].rpc_method.as_deref(), Some("initialize"));
    assert_eq!(
        observations[1].rpc_method.as_deref(),
        Some("notifications/initialized")
    );
    assert_eq!(
        observations.last().map(|value| &value.http_method),
        Some(&Method::DELETE)
    );
    assert!(observations.iter().all(|value| value.authenticated));
    assert!(
        observations
            .iter()
            .all(|value| value.accept.as_deref() == Some(ACCEPT))
    );
    assert!(
        observations
            .iter()
            .all(|value| value.path == "/servers/server%2Fwith%20space/mcp")
    );
    assert!(
        observations
            .iter()
            .any(|value| value.rpc_method.as_deref() == Some("tools/list"))
    );
    assert!(
        observations
            .iter()
            .any(|value| value.rpc_method.as_deref() == Some("tools/call"))
    );
    assert!(
        observations
            .iter()
            .any(|value| value.rpc_method.as_deref() == Some("ping"))
    );
    assert!(
        observations
            .iter()
            .filter_map(|value| value.called_tool.as_deref())
            .all(|name| name == "echo")
    );
    assert!(observations[0].session.is_none());
    assert!(observations[0].protocol_version.is_none());
    assert_eq!(
        observations[0].initialize_protocol_version.as_deref(),
        Some(PROTOCOL_VERSION)
    );
    assert!(
        observations[1..]
            .iter()
            .all(|value| value.session.as_deref() == Some(SESSION_ID))
    );
    assert!(
        observations[1..]
            .iter()
            .all(|value| value.protocol_version.as_deref() == Some(PROTOCOL_VERSION))
    );
}

#[tokio::test]
async fn execute_emits_the_selected_protocol_version_in_body_and_headers() {
    const SELECTED_VERSION: &str = "2025-06-18";
    let (host, state, server) = spawn_mock_with_options(
        false,
        false,
        false,
        Some("dataplane"),
        StatusCode::NO_CONTENT,
        None,
        SELECTED_VERSION,
    )
    .await;
    let root = repository_root();
    let config = app_config(root.path(), &environment(&[("MCP_CLI_BASE_URL", &host)]));
    let settings = load_settings(&config, "1s");
    let goose = GooseLoadConfig::new_with_protocol_version(
        &config,
        StackMode::Controlplane,
        &settings,
        TOKEN,
        None,
        SELECTED_VERSION,
    )
    .expect("Goose configuration should build");

    let outcome = goose
        .execute()
        .await
        .expect("selected protocol flow should pass");
    server.abort();
    assert_eq!(outcome.failed_requests(), 0);

    let observations = state
        .observations
        .lock()
        .expect("mock observation lock should not be poisoned");
    assert_eq!(
        observations[0].initialize_protocol_version.as_deref(),
        Some(SELECTED_VERSION)
    );
    assert!(
        observations[1..]
            .iter()
            .all(|value| value.protocol_version.as_deref() == Some(SELECTED_VERSION))
    );
}

#[tokio::test]
async fn execute_runs_the_stateless_dataplane_lifecycle_with_routing_metadata() {
    let (host, state, server) = spawn_mock_with_options(
        true,
        false,
        false,
        Some("dataplane"),
        StatusCode::NO_CONTENT,
        None,
        STATELESS_PROTOCOL_VERSION,
    )
    .await;
    let root = repository_root();
    let config = app_config(root.path(), &environment(&[("MCP_CLI_BASE_URL", &host)]));
    let settings = load_settings(&config, "1s");
    let goose = GooseLoadConfig::new_with_protocol_version(
        &config,
        StackMode::Dataplane,
        &settings,
        TOKEN,
        Some("server"),
        STATELESS_PROTOCOL_VERSION,
    )
    .expect("stateless Goose configuration should build");

    let outcome = goose
        .execute()
        .await
        .expect("stateless dataplane flow should pass");
    server.abort();
    assert_eq!(outcome.failed_requests(), 0);
    assert_eq!(outcome.failed_transactions(), 0);

    let observations = state
        .observations
        .lock()
        .expect("mock observation lock should not be poisoned");
    assert_eq!(
        observations[0].rpc_method.as_deref(),
        Some("server/discover")
    );
    assert!(observations.iter().all(|value| value.session.is_none()));
    assert!(observations.iter().all(|value| {
        value.protocol_version.as_deref() == Some(STATELESS_PROTOCOL_VERSION)
            && value.routing_method == value.rpc_method
            && value.metadata_protocol_version.as_deref() == Some(STATELESS_PROTOCOL_VERSION)
    }));
    assert!(observations.iter().all(|value| {
        value.routing_name.as_deref()
            == if value.rpc_method.as_deref() == Some("tools/call") {
                Some("echo")
            } else {
                None
            }
    }));
    assert!(
        observations
            .iter()
            .all(|value| value.http_method == Method::POST)
    );
    assert!(
        observations
            .iter()
            .all(|value| value.rpc_method.as_deref() != Some("ping"))
    );
}

#[tokio::test]
async fn dataplane_goose_rejects_absent_fallback_forged_and_duplicate_backend_markers() {
    for marker in [
        None,
        Some("controlplane-fallback"),
        Some("private-forged-marker"),
        Some("dataplane, dataplane"),
    ] {
        let (host, _state, server) = spawn_mock_with_marker(false, false, false, marker).await;
        let root = repository_root();
        let config = app_config(root.path(), &environment(&[("MCP_CLI_BASE_URL", &host)]));
        let settings = load_settings(&config, "1s");
        let goose = GooseLoadConfig::new(
            &config,
            StackMode::Dataplane,
            &settings,
            TOKEN,
            Some("server"),
        )
        .expect("Goose configuration should build");

        let error = goose
            .execute()
            .await
            .expect_err("invalid dataplane identity must fail the load run");
        server.abort();

        let diagnostic = format!("{error}\n{error:?}");
        assert!(matches!(error, GooseRunError::FailedMetrics { .. }));
        assert!(!diagnostic.contains("private-forged-marker"));
    }
}

#[tokio::test]
async fn goose_accepts_absent_or_unsupported_session_delete_responses() {
    for delete_status in [StatusCode::NOT_FOUND, StatusCode::METHOD_NOT_ALLOWED] {
        let (host, _state, server) = spawn_mock_with_options(
            false,
            false,
            false,
            Some("dataplane"),
            delete_status,
            None,
            PROTOCOL_VERSION,
        )
        .await;
        let root = repository_root();
        let config = app_config(root.path(), &environment(&[("MCP_CLI_BASE_URL", &host)]));
        let settings = load_settings(&config, "1s");
        let goose = GooseLoadConfig::new(
            &config,
            StackMode::Dataplane,
            &settings,
            TOKEN,
            Some("server"),
        )
        .expect("Goose configuration should build");

        let outcome = goose
            .execute()
            .await
            .expect("404 and 405 are valid session-delete outcomes");
        server.abort();

        assert_eq!(outcome.failed_requests(), 0);
        assert_eq!(outcome.failed_transactions(), 0);
    }
}

#[tokio::test]
async fn goose_rejects_malformed_initialize_capabilities_and_server_info() {
    let malformed_results = [
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": [],
            "serverInfo": {"name": "mock", "version": "1"}
        }),
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
        }),
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "serverInfo": {"name": "", "version": "1"}
        }),
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "serverInfo": {"name": "mock", "version": 1}
        }),
    ];

    for result in malformed_results {
        let (host, _state, server) = spawn_mock_with_initialize_result(result).await;
        let root = repository_root();
        let config = app_config(root.path(), &environment(&[("MCP_CLI_BASE_URL", &host)]));
        let settings = load_settings(&config, "1s");
        let goose = GooseLoadConfig::new(
            &config,
            StackMode::Dataplane,
            &settings,
            TOKEN,
            Some("server"),
        )
        .expect("Goose configuration should build");

        let error = goose
            .execute()
            .await
            .expect_err("malformed initialize metadata must fail the load run");
        server.abort();

        assert!(matches!(error, GooseRunError::FailedMetrics { .. }));
    }
}

#[tokio::test]
async fn execute_fails_closed_when_initialize_has_no_real_session_id() {
    let (host, _state, server) = spawn_mock(true, false, false).await;
    let root = repository_root();
    let config = app_config(root.path(), &environment(&[("MCP_CLI_BASE_URL", &host)]));
    let settings = load_settings(&config, "1s");
    let goose = GooseLoadConfig::new(&config, StackMode::Controlplane, &settings, TOKEN, None)
        .expect("Goose configuration should build");

    let error = goose
        .execute()
        .await
        .expect_err("missing MCP session ID should fail the load run");
    server.abort();

    match error {
        GooseRunError::FailedMetrics {
            failed_requests,
            failed_transactions,
            metrics,
            reports,
            ..
        } => {
            assert!(failed_requests > 0);
            assert_eq!(failed_transactions, 0);
            assert!(!metrics.requests.is_empty());
            assert!(!metrics.transactions.is_empty());
            assert!(reports.html().is_file());
            assert!(reports.json().is_file());
        }
        other => panic!("unexpected error: {other:#}"),
    }
}

#[tokio::test]
async fn failed_initialized_notification_disables_the_user_without_invalid_followups() {
    let (host, state, server) = spawn_mock(false, true, false).await;
    let root = repository_root();
    let config = app_config(root.path(), &environment(&[("MCP_CLI_BASE_URL", &host)]));
    let settings = load_settings(&config, "1s");
    let goose = GooseLoadConfig::new(&config, StackMode::Controlplane, &settings, TOKEN, None)
        .expect("Goose configuration should build");

    let error = goose
        .execute()
        .await
        .expect_err("rejected initialized notification should fail the load run");
    server.abort();

    assert!(matches!(error, GooseRunError::FailedMetrics { .. }));
    let observations = state
        .observations
        .lock()
        .expect("mock observation lock should not be poisoned");
    assert_eq!(observations.len(), 2, "observations: {observations:#?}");
    assert_eq!(observations[0].rpc_method.as_deref(), Some("initialize"));
    assert_eq!(
        observations[1].rpc_method.as_deref(),
        Some("notifications/initialized")
    );
}

#[tokio::test]
async fn initialized_notification_rejects_a_nonempty_202_response() {
    let (host, _state, server) = spawn_mock(false, false, true).await;
    let root = repository_root();
    let config = app_config(root.path(), &environment(&[("MCP_CLI_BASE_URL", &host)]));
    let settings = load_settings(&config, "1s");
    let goose = GooseLoadConfig::new(&config, StackMode::Controlplane, &settings, TOKEN, None)
        .expect("Goose configuration should build");

    let error = goose
        .execute()
        .await
        .expect_err("a notification response body violates Streamable HTTP");
    server.abort();

    assert!(matches!(error, GooseRunError::FailedMetrics { .. }));
}

#[test]
fn goose_configuration_uses_programmatic_values_and_both_report_files() {
    let root = repository_root();
    let config = app_config(root.path(), &Environment::new());
    let settings = load_settings(&config, "2m3s");
    let goose = GooseLoadConfig::new(&config, StackMode::Controlplane, &settings, TOKEN, None)
        .expect("Goose configuration should build");

    let mapped = goose.goose_configuration();
    assert_eq!(mapped.host, goose.host());
    assert_eq!(mapped.users, Some(1));
    assert_eq!(mapped.increase_rate.as_deref(), Some("2.5"));
    assert_eq!(mapped.run_time, "123s");
    assert!(mapped.no_reset_metrics);
    assert!(mapped.no_telnet);
    assert!(mapped.no_websocket);
    assert_eq!(
        mapped.report_file,
        vec![
            goose.reports().html().to_string_lossy().into_owned(),
            goose.reports().json().to_string_lossy().into_owned(),
        ]
    );
}

#[test]
fn report_directory_is_mode_and_engine_specific() {
    let root = repository_root();
    let config = app_config(root.path(), &Environment::new());
    let settings = load_settings(&config, "1s");
    let expected = [
        (StackMode::Controlplane, "controlplane"),
        (StackMode::Dataplane, "dataplane"),
    ];

    for (mode, mode_name) in expected {
        let server_id = (mode == StackMode::Dataplane).then_some("server");
        let goose = GooseLoadConfig::new(&config, mode, &settings, TOKEN, server_id)
            .expect("Goose configuration should build");
        let expected_dir = root
            .path()
            .join(format!(".integration/reports/load/{mode_name}/goose"));
        assert_eq!(
            goose.reports().html().parent(),
            Some(expected_dir.as_path())
        );
    }
}
