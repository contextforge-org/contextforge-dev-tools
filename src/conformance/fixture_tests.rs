use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::State;
use axum::http::{Request, Response, StatusCode};
use axum::routing::any;
use cf_integration::conformance::fixture::{
    ConformanceFixtureClient, OFFICIAL_CONFORMANCE_BACKEND_URL, OFFICIAL_CONFORMANCE_GATEWAY_NAME,
    OFFICIAL_CONFORMANCE_SERVER_ID, ProvisionedConformanceFixture,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_stream::wrappers::ReceiverStream;

const TOKEN: &str = "fixture-admin-secret";
const GATEWAY_ID: &str = "gateway-new";

#[test]
fn official_fixture_uses_empty_slug_gateway_name() {
    assert_eq!(OFFICIAL_CONFORMANCE_GATEWAY_NAME, "_");
}

#[derive(Clone, Debug)]
struct ExpectedRequest {
    method: &'static str,
    path: String,
    status: StatusCode,
    response: String,
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Option<Value>,
}

#[derive(Clone, Default)]
struct ApiState {
    expected: Arc<Mutex<VecDeque<ExpectedRequest>>>,
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
}

struct FakeApi {
    base_url: String,
    state: ApiState,
}

impl FakeApi {
    async fn start(expected: Vec<ExpectedRequest>) -> Self {
        let state = ApiState {
            expected: Arc::new(Mutex::new(expected.into())),
            captured: Arc::default(),
        };
        let app = Router::new()
            .fallback(any(fake_api_handler))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake API");
        let address = listener.local_addr().expect("fake API address");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve fake API");
        });
        Self {
            base_url: format!("http://{address}"),
            state,
        }
    }

    fn requests(&self) -> Vec<CapturedRequest> {
        self.state.captured.lock().expect("captured lock").clone()
    }

    fn assert_complete(&self) {
        let remaining = self.state.expected.lock().expect("expected lock");
        assert!(remaining.is_empty(), "unconsumed requests: {remaining:?}");
    }
}

async fn fake_api_handler(State(state): State<ApiState>, request: Request<Body>) -> Response<Body> {
    let method = request.method().to_string();
    let path = request
        .uri()
        .path_and_query()
        .map_or_else(|| request.uri().path().to_owned(), ToString::to_string);
    let authorization = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = to_bytes(request.into_body(), 1024 * 1024)
        .await
        .expect("read request body");
    let body = if body.is_empty() {
        None
    } else {
        Some(serde_json::from_slice(&body).expect("request JSON"))
    };
    state
        .captured
        .lock()
        .expect("captured lock")
        .push(CapturedRequest {
            method: method.clone(),
            path: path.clone(),
            authorization,
            body,
        });

    let expected = state
        .expected
        .lock()
        .expect("expected lock")
        .pop_front()
        .expect("unexpected request");
    assert_eq!(method, expected.method);
    assert_eq!(path, expected.path);
    Response::builder()
        .status(expected.status)
        .header("content-type", "application/json")
        .body(Body::from(expected.response))
        .expect("fake response")
}

async fn never_responds() -> Response<Body> {
    std::future::pending().await
}

async fn stalled_json_handler(request: Request<Body>) -> Response<Body> {
    match (request.method().as_str(), request.uri().path()) {
        ("GET", "/servers") => json_http_response(StatusCode::OK, json!([])),
        ("GET", "/gateways") => {
            let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(1);
            tokio::spawn(async move {
                let _sender = sender;
                std::future::pending::<()>().await;
            });
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Body::from_stream(ReceiverStream::new(receiver)))
                .expect("stalled JSON response")
        }
        (method, path) => panic!("unexpected stalled JSON request: {method} {path}"),
    }
}

#[derive(Clone, Default)]
struct DelayedGatewayState {
    visible: Arc<AtomicBool>,
    deleted: Arc<AtomicBool>,
    gateway_gets: Arc<AtomicUsize>,
}

#[derive(Clone, Default)]
struct UnownedCatalogState {
    server_body: Arc<Mutex<Option<Value>>>,
}

fn json_http_response(status: StatusCode, body: Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("JSON HTTP response")
}

async fn unowned_catalog_handler(
    State(state): State<UnownedCatalogState>,
    request: Request<Body>,
) -> Response<Body> {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    match (method.as_str(), path.as_str()) {
        ("GET", "/servers") => json_http_response(StatusCode::OK, json!([])),
        ("DELETE", path) if path.starts_with("/gateways/") => {
            json_http_response(StatusCode::NO_CONTENT, json!({}))
        }
        ("GET", "/gateways") => json_http_response(StatusCode::OK, json!([])),
        ("POST", "/gateways") => {
            json_http_response(StatusCode::CREATED, gateway_record(GATEWAY_ID))
        }
        ("POST", path) if path == format!("/gateways/{GATEWAY_ID}/tools/refresh") => {
            json_http_response(StatusCode::OK, json!({}))
        }
        ("GET", "/tools") => json_http_response(
            StatusCode::OK,
            json!([
                {"id":"a2a-unowned", "name":"unrelated_a2a", "gatewayId":null},
                {"id":"missing-owner", "name":"unrelated_missing_owner"},
                {"id":"official-tool", "name":"test_simple_text", "gatewayId":GATEWAY_ID}
            ]),
        ),
        ("GET", "/resources") => json_http_response(
            StatusCode::OK,
            json!([{"id":"resource-1", "uri":"test://static-text", "gatewayId":GATEWAY_ID}]),
        ),
        ("GET", "/prompts") => json_http_response(
            StatusCode::OK,
            json!([{"id":"prompt-1", "name":"test_simple_prompt", "gatewayId":GATEWAY_ID}]),
        ),
        ("POST", "/servers") => {
            let body = to_bytes(request.into_body(), 1024 * 1024)
                .await
                .expect("server request body");
            let body = serde_json::from_slice(&body).expect("server request JSON");
            *state.server_body.lock().expect("server body lock") = Some(body);
            json_http_response(
                StatusCode::CREATED,
                json!({"id":OFFICIAL_CONFORMANCE_SERVER_ID}),
            )
        }
        _ => panic!("unexpected unowned-catalog request: {method} {path}"),
    }
}

async fn delayed_gateway_handler(
    State(state): State<DelayedGatewayState>,
    request: Request<Body>,
) -> Response<Body> {
    match (request.method().as_str(), request.uri().path()) {
        ("GET", "/servers") => json_http_response(StatusCode::OK, json!([])),
        ("GET", "/gateways") => {
            state.gateway_gets.fetch_add(1, Ordering::SeqCst);
            let gateways = if state.visible.load(Ordering::SeqCst) {
                json!([gateway_record("gateway-delayed")])
            } else {
                json!([])
            };
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Body::from(gateways.to_string()))
                .expect("gateway list response")
        }
        ("POST", "/gateways") => {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(45)).await;
                state.visible.store(true, Ordering::SeqCst);
            });
            std::future::pending().await
        }
        ("DELETE", "/gateways/gateway-delayed") => {
            state.visible.store(false, Ordering::SeqCst);
            state.deleted.store(true, Ordering::SeqCst);
            Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::empty())
                .expect("gateway delete response")
        }
        (method, path) => panic!("unexpected delayed-gateway request: {method} {path}"),
    }
}

fn response(method: &'static str, path: impl Into<String>, response: Value) -> ExpectedRequest {
    ExpectedRequest {
        method,
        path: path.into(),
        status: StatusCode::OK,
        response: response.to_string(),
    }
}

fn status(method: &'static str, path: impl Into<String>, status: StatusCode) -> ExpectedRequest {
    ExpectedRequest {
        method,
        path: path.into(),
        status,
        response: json!({"detail": status.as_u16()}).to_string(),
    }
}

fn raw_response(
    method: &'static str,
    path: impl Into<String>,
    status: StatusCode,
    response: impl Into<String>,
) -> ExpectedRequest {
    ExpectedRequest {
        method,
        path: path.into(),
        status,
        response: response.into(),
    }
}

fn gateway_record(id: &str) -> Value {
    json!({
        "id": id,
        "name": OFFICIAL_CONFORMANCE_GATEWAY_NAME,
        "url": OFFICIAL_CONFORMANCE_BACKEND_URL,
        "transport": "STREAMABLEHTTP",
        "description": "Official MCP conformance fixture",
        "enabled": true,
        "reachable": true,
        "capabilities": {}
    })
}

fn server_record(id: &str) -> Value {
    json!({
        "id": id,
        "name": "Official MCP Conformance Server",
        "description": "Virtual server for the pinned official MCP conformance fixture."
    })
}

fn catalogs(gateway_key: &str, prompt: bool) -> [Value; 3] {
    [
        json!([
            {"id":"tool-1", "name":"test_simple_text", gateway_key:GATEWAY_ID},
            {"id":"foreign-tool", "name":"test_simple_text", gateway_key:"other-gateway"}
        ]),
        json!([
            {"id":"resource-1", "uri":"test://static-text", gateway_key:GATEWAY_ID},
            {"id":"foreign-resource", "uri":"test://static-text", gateway_key:"other-gateway"}
        ]),
        if prompt {
            json!([
                {"id":"prompt-1", "name":"test_simple_prompt", gateway_key:GATEWAY_ID},
                {"id":"foreign-prompt", "name":"test_simple_prompt", gateway_key:"other-gateway"}
            ])
        } else {
            json!([])
        },
    ]
}

fn provision_prefix(gateways: Value) -> Vec<ExpectedRequest> {
    vec![
        response("GET", "/servers", json!([])),
        response("GET", "/gateways", gateways),
    ]
}

#[test]
fn admin_client_builder_explicitly_disables_environment_proxies() {
    let source = include_str!("fixture.rs");
    let builder = source
        .split("let http = reqwest::Client::builder()")
        .nth(1)
        .expect("admin client builder");
    let builder = builder
        .split(".build()")
        .next()
        .expect("admin client build chain");

    assert!(builder.contains(".no_proxy()"), "{builder}");
}

#[tokio::test]
async fn provision_refuses_foreign_reserved_gateway_without_deleting_it() {
    for foreign in [
        json!({
            "id":"foreign-url", "name":OFFICIAL_CONFORMANCE_GATEWAY_NAME,
            "url":"http://foreign.test/mcp", "transport":"STREAMABLEHTTP",
            "description":"Official MCP conformance fixture"
        }),
        json!({
            "id":"foreign-transport", "name":OFFICIAL_CONFORMANCE_GATEWAY_NAME,
            "url":OFFICIAL_CONFORMANCE_BACKEND_URL, "transport":"SSE",
            "description":"Official MCP conformance fixture"
        }),
        json!({
            "id":"foreign-description", "name":OFFICIAL_CONFORMANCE_GATEWAY_NAME,
            "url":OFFICIAL_CONFORMANCE_BACKEND_URL, "transport":"STREAMABLEHTTP",
            "description":"someone else's gateway"
        }),
        json!({
            "id":"null-description", "name":OFFICIAL_CONFORMANCE_GATEWAY_NAME,
            "url":OFFICIAL_CONFORMANCE_BACKEND_URL, "transport":"STREAMABLEHTTP",
            "description":null
        }),
        json!({
            "id":"missing-description", "name":OFFICIAL_CONFORMANCE_GATEWAY_NAME,
            "url":OFFICIAL_CONFORMANCE_BACKEND_URL, "transport":"STREAMABLEHTTP"
        }),
    ] {
        let api = FakeApi::start(vec![
            response("GET", "/servers", json!([])),
            response("GET", "/gateways", json!([foreign])),
        ])
        .await;

        let error = test_client(&api.base_url)
            .provision(OFFICIAL_CONFORMANCE_BACKEND_URL)
            .await
            .expect_err("foreign reserved-name gateway must fail closed");

        let message = format!("{error:#}");
        assert!(message.contains("not owned"), "{message}");
        assert!(!message.contains(TOKEN), "{message}");
        assert!(
            api.requests()
                .iter()
                .all(|request| request.method != "DELETE")
        );
        api.assert_complete();
    }
}

#[tokio::test]
async fn provision_refuses_foreign_deterministic_server_without_deleting_it() {
    for foreign in [
        json!({
            "id":OFFICIAL_CONFORMANCE_SERVER_ID,
            "name":"Foreign Server",
            "description":"not fixture owned"
        }),
        json!({
            "id":OFFICIAL_CONFORMANCE_SERVER_ID,
            "name":"Official MCP Conformance Server",
            "description":null
        }),
        json!({
            "id":OFFICIAL_CONFORMANCE_SERVER_ID,
            "name":"Official MCP Conformance Server"
        }),
    ] {
        let api = FakeApi::start(vec![response("GET", "/servers", json!([foreign]))]).await;

        let error = test_client(&api.base_url)
            .provision(OFFICIAL_CONFORMANCE_BACKEND_URL)
            .await
            .expect_err("foreign deterministic server must fail closed");

        let message = format!("{error:#}");
        assert!(message.contains("not owned"), "{message}");
        assert!(!message.contains(TOKEN), "{message}");
        assert!(
            api.requests()
                .iter()
                .all(|request| request.method != "DELETE")
        );
        api.assert_complete();
    }
}

#[tokio::test]
async fn unrelated_nullable_descriptions_do_not_block_owned_fixture_detection() {
    let mut expected = vec![
        response(
            "GET",
            "/servers",
            json!([
                {"id":"unrelated-null", "name":"Unrelated", "description":null},
                {"id":"unrelated-missing", "name":"Unrelated"},
                server_record(OFFICIAL_CONFORMANCE_SERVER_ID)
            ]),
        ),
        status(
            "DELETE",
            format!("/servers/{OFFICIAL_CONFORMANCE_SERVER_ID}"),
            StatusCode::NO_CONTENT,
        ),
        response(
            "GET",
            "/gateways",
            json!([
                {
                    "id":"unrelated-null", "name":"another-null",
                    "url":"http://example.test/mcp", "transport":"SSE", "description":null
                },
                {
                    "id":"unrelated-missing", "name":"another-missing",
                    "url":"http://example.test/mcp", "transport":"SSE"
                },
                gateway_record("owned-stale")
            ]),
        ),
        status("DELETE", "/gateways/owned-stale", StatusCode::NO_CONTENT),
    ];
    append_create_and_refresh(&mut expected);
    append_catalogs(&mut expected, "gatewayId", true);
    expected.push(response(
        "POST",
        "/servers",
        server_record(OFFICIAL_CONFORMANCE_SERVER_ID),
    ));
    let api = FakeApi::start(expected).await;

    test_client(&api.base_url)
        .provision(OFFICIAL_CONFORMANCE_BACKEND_URL)
        .await
        .expect("unrelated nullable descriptions must not block provisioning");

    api.assert_complete();
}

#[tokio::test]
async fn provision_deletes_exact_owned_stale_server_and_gateway() {
    let api = FakeApi::start(vec![
        response(
            "GET",
            "/servers",
            json!([server_record(OFFICIAL_CONFORMANCE_SERVER_ID)]),
        ),
        status(
            "DELETE",
            format!("/servers/{OFFICIAL_CONFORMANCE_SERVER_ID}"),
            StatusCode::NO_CONTENT,
        ),
        response("GET", "/gateways", json!([gateway_record("stale-owned")])),
        status("DELETE", "/gateways/stale-owned", StatusCode::NO_CONTENT),
        response("POST", "/gateways", gateway_record(GATEWAY_ID)),
        response(
            "POST",
            format!(
                "/gateways/{GATEWAY_ID}/tools/refresh?include_resources=true&include_prompts=true"
            ),
            json!({}),
        ),
        response("GET", "/tools", catalogs("gatewayId", true)[0].clone()),
        response("GET", "/resources", catalogs("gatewayId", true)[1].clone()),
        response("GET", "/prompts", catalogs("gatewayId", true)[2].clone()),
        response(
            "POST",
            "/servers",
            server_record(OFFICIAL_CONFORMANCE_SERVER_ID),
        ),
    ])
    .await;

    test_client(&api.base_url)
        .provision(OFFICIAL_CONFORMANCE_BACKEND_URL)
        .await
        .expect("owned stale resources should be deleted");

    api.assert_complete();
}

#[tokio::test]
async fn known_cleanup_refuses_replaced_resources_and_attempts_both() {
    let fixture = ProvisionedConformanceFixture {
        gateway_id: GATEWAY_ID.to_owned(),
        server_id: OFFICIAL_CONFORMANCE_SERVER_ID.to_owned(),
    };
    let api = FakeApi::start(vec![
        response(
            "GET",
            "/servers",
            json!([{
                "id":OFFICIAL_CONFORMANCE_SERVER_ID,
                "name":"Replacement",
                "description":"foreign"
            }]),
        ),
        response(
            "GET",
            "/gateways",
            json!([{
                "id":GATEWAY_ID,
                "name":OFFICIAL_CONFORMANCE_GATEWAY_NAME,
                "url":"http://replacement.test/mcp",
                "transport":"STREAMABLEHTTP",
                "description":"Official MCP conformance fixture"
            }]),
        ),
    ])
    .await;

    let error = test_client(&api.base_url)
        .cleanup(Some(&fixture))
        .await
        .expect_err("replaced fixture resources must fail closed");

    let message = format!("{error:#}");
    assert!(message.contains("server cleanup failed"), "{message}");
    assert!(message.contains("gateway cleanup failed"), "{message}");
    assert!(!message.contains(TOKEN), "{message}");
    assert!(
        api.requests()
            .iter()
            .all(|request| request.method != "DELETE")
    );
    api.assert_complete();
}

#[tokio::test]
async fn known_cleanup_refuses_non_deterministic_server_id_even_with_fixture_metadata() {
    let fixture = ProvisionedConformanceFixture {
        gateway_id: GATEWAY_ID.to_owned(),
        server_id: "foreign-server-id".to_owned(),
    };
    let api = FakeApi::start(vec![
        response(
            "GET",
            "/servers",
            json!([server_record("foreign-server-id")]),
        ),
        response("GET", "/gateways", json!([])),
    ])
    .await;

    let error = test_client(&api.base_url)
        .cleanup(Some(&fixture))
        .await
        .expect_err("only the deterministic fixture server ID may be deleted");

    let message = format!("{error:#}");
    assert!(message.contains("not owned"), "{message}");
    assert!(
        api.requests()
            .iter()
            .all(|request| request.method != "DELETE")
    );
    api.assert_complete();
}

#[tokio::test]
async fn reconciliation_refuses_foreign_delayed_reserved_collision() {
    let mut expected = provision_prefix(json!([]));
    expected.extend([
        raw_response("POST", "/gateways", StatusCode::CREATED, "{"),
        response(
            "GET",
            "/gateways",
            json!([{
                "id":"delayed-foreign",
                "name":OFFICIAL_CONFORMANCE_GATEWAY_NAME,
                "url":"http://foreign.test/mcp",
                "transport":"STREAMABLEHTTP",
                "description":"Official MCP conformance fixture"
            }]),
        ),
    ]);
    let api = FakeApi::start(expected).await;

    let error = test_client(&api.base_url)
        .provision(OFFICIAL_CONFORMANCE_BACKEND_URL)
        .await
        .expect_err("foreign delayed collision must fail closed");

    let message = format!("{error:#}");
    assert!(message.contains("reconciliation failed"), "{message}");
    assert!(message.contains("not owned"), "{message}");
    assert!(!message.contains(TOKEN), "{message}");
    assert!(
        api.requests()
            .iter()
            .all(|request| request.method != "DELETE")
    );
    api.assert_complete();
}

#[tokio::test]
async fn provision_refuses_foreign_gateway_create_response() {
    let mut expected = provision_prefix(json!([]));
    expected.extend([
        response(
            "POST",
            "/gateways",
            json!({
                "id":GATEWAY_ID,
                "name":OFFICIAL_CONFORMANCE_GATEWAY_NAME,
                "url":"http://foreign.test/mcp",
                "transport":"STREAMABLEHTTP",
                "description":"Official MCP conformance fixture"
            }),
        ),
        response("GET", "/gateways", json!([])),
        response("GET", "/gateways", json!([])),
    ]);
    let api = FakeApi::start(expected).await;

    let error = test_client(&api.base_url)
        .provision(OFFICIAL_CONFORMANCE_BACKEND_URL)
        .await
        .expect_err("foreign gateway create response must fail closed");

    let message = format!("{error:#}");
    assert!(message.contains("not owned"), "{message}");
    assert!(!message.contains(TOKEN), "{message}");
    assert!(
        api.requests()
            .iter()
            .all(|request| request.method != "DELETE")
    );
    api.assert_complete();
}

#[tokio::test]
async fn provision_rejects_non_official_backend_before_admin_requests() {
    let api = FakeApi::start(vec![]).await;

    let error = test_client(&api.base_url)
        .provision("http://foreign.test/mcp")
        .await
        .expect_err("fixture backend must be the pinned official service");

    let message = format!("{error:#}");
    assert!(
        message.contains("official conformance backend URL"),
        "{message}"
    );
    assert!(!message.contains(TOKEN), "{message}");
    assert!(api.requests().is_empty());
    api.assert_complete();
}

fn append_create_and_refresh(expected: &mut Vec<ExpectedRequest>) {
    expected.push(response("POST", "/gateways", gateway_record(GATEWAY_ID)));
    expected.push(response(
        "POST",
        format!("/gateways/{GATEWAY_ID}/tools/refresh?include_resources=true&include_prompts=true"),
        json!({"gateway_id":GATEWAY_ID, "tools_added":1, "resources_added":1, "prompts_added":1}),
    ));
}

fn append_catalogs(expected: &mut Vec<ExpectedRequest>, gateway_key: &str, prompt: bool) {
    let [tools, resources, prompts] = catalogs(gateway_key, prompt);
    expected.extend([
        response("GET", "/tools", tools),
        response("GET", "/resources", resources),
        response("GET", "/prompts", prompts),
    ]);
}

fn test_client(base_url: &str) -> ConformanceFixtureClient {
    ConformanceFixtureClient::builder(base_url, TOKEN)
        .poll_interval(Duration::ZERO)
        .max_attempts(2)
        .reconciliation_interval(Duration::ZERO)
        .build()
        .expect("valid fixture client")
}

#[tokio::test]
async fn provision_uses_authenticated_admin_api_in_exact_order() {
    let mut expected = provision_prefix(json!([]));
    append_create_and_refresh(&mut expected);
    append_catalogs(&mut expected, "gatewayId", true);
    expected.push(response(
        "POST",
        "/servers",
        json!({"id":OFFICIAL_CONFORMANCE_SERVER_ID, "name":"Official MCP Conformance Server"}),
    ));
    let api = FakeApi::start(expected).await;

    let fixture = test_client(&api.base_url)
        .provision(OFFICIAL_CONFORMANCE_BACKEND_URL)
        .await
        .expect("provision fixture");

    assert_eq!(
        fixture,
        ProvisionedConformanceFixture {
            gateway_id: GATEWAY_ID.to_owned(),
            server_id: OFFICIAL_CONFORMANCE_SERVER_ID.to_owned(),
        }
    );
    let requests = api.requests();
    assert!(
        requests
            .iter()
            .all(|request| request.authorization.as_deref() == Some("Bearer fixture-admin-secret"))
    );
    let gateway_body = requests[2].body.as_ref().expect("gateway body");
    assert_eq!(
        gateway_body,
        &json!({
            "name":OFFICIAL_CONFORMANCE_GATEWAY_NAME,
            "url":OFFICIAL_CONFORMANCE_BACKEND_URL,
            "transport":"STREAMABLEHTTP",
            "authType":"authheaders",
            "authHeaders":[{"key":"Host", "value":"localhost:3000"}],
            "description":"Official MCP conformance fixture"
        })
    );
    let server = requests.last().expect("server request");
    assert_eq!(server.method, "POST");
    assert_eq!(server.path, "/servers");
    assert_eq!(
        server.body.as_ref().expect("server body"),
        &json!({
            "server": {
                "id": OFFICIAL_CONFORMANCE_SERVER_ID,
                "name": "Official MCP Conformance Server",
                "description": "Virtual server for the pinned official MCP conformance fixture.",
                "associated_tools": ["tool-1"],
                "associated_resources": ["resource-1"],
                "associated_prompts": ["prompt-1"]
            }
        })
    );
    api.assert_complete();
}

#[tokio::test]
async fn provision_deletes_every_stale_reserved_gateway_before_creation() {
    let mut expected = provision_prefix(json!([
        gateway_record("stale-one"),
        {"id":"unrelated", "name":"another-gateway", "url":"http://example.test/mcp", "transport":"SSE", "description":"unrelated"},
        gateway_record("stale-two")
    ]));
    expected.extend([
        status("DELETE", "/gateways/stale-one", StatusCode::NO_CONTENT),
        status("DELETE", "/gateways/stale-two", StatusCode::OK),
    ]);
    append_create_and_refresh(&mut expected);
    append_catalogs(&mut expected, "gatewayId", true);
    expected.push(response(
        "POST",
        "/servers",
        json!({"id":OFFICIAL_CONFORMANCE_SERVER_ID}),
    ));
    let api = FakeApi::start(expected).await;

    test_client(&api.base_url)
        .provision(OFFICIAL_CONFORMANCE_BACKEND_URL)
        .await
        .expect("provision fixture");

    api.assert_complete();
}

#[tokio::test]
async fn provision_accepts_snake_case_gateway_ids() {
    let mut expected = provision_prefix(json!([]));
    append_create_and_refresh(&mut expected);
    append_catalogs(&mut expected, "gateway_id", true);
    expected.push(response(
        "POST",
        "/servers",
        json!({"id":OFFICIAL_CONFORMANCE_SERVER_ID}),
    ));
    let api = FakeApi::start(expected).await;

    test_client(&api.base_url)
        .provision(OFFICIAL_CONFORMANCE_BACKEND_URL)
        .await
        .expect("snake-case catalog IDs");

    let server = api.requests().pop().expect("server request");
    assert_eq!(
        server.body.expect("server body")["server"]["associated_tools"][0],
        "tool-1"
    );
    api.assert_complete();
}

#[tokio::test]
async fn provision_ignores_catalog_entries_with_null_or_missing_gateway_id() {
    let state = UnownedCatalogState::default();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind unowned catalog API");
    let address = listener.local_addr().expect("unowned catalog API address");
    let app = Router::new()
        .fallback(any(unowned_catalog_handler))
        .with_state(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve unowned catalog API");
    });

    test_client(&format!("http://{address}"))
        .provision(OFFICIAL_CONFORMANCE_BACKEND_URL)
        .await
        .expect("unowned catalog rows must not block provisioning");

    let body = state
        .server_body
        .lock()
        .expect("server body lock")
        .clone()
        .expect("server request");
    assert_eq!(body["server"]["associated_tools"], json!(["official-tool"]));
}

#[tokio::test]
async fn provision_retries_an_incomplete_catalog_then_succeeds() {
    let mut expected = provision_prefix(json!([]));
    append_create_and_refresh(&mut expected);
    append_catalogs(&mut expected, "gatewayId", false);
    append_catalogs(&mut expected, "gatewayId", true);
    expected.push(response(
        "POST",
        "/servers",
        json!({"id":OFFICIAL_CONFORMANCE_SERVER_ID}),
    ));
    let api = FakeApi::start(expected).await;

    test_client(&api.base_url)
        .provision(OFFICIAL_CONFORMANCE_BACKEND_URL)
        .await
        .expect("second catalog attempt succeeds");

    assert_eq!(
        api.requests()
            .iter()
            .filter(|request| request.path == "/tools")
            .count(),
        2
    );
    api.assert_complete();
}

#[tokio::test]
async fn provision_reports_missing_identity_and_cleans_partial_fixture() {
    let mut expected = provision_prefix(json!([]));
    append_create_and_refresh(&mut expected);
    append_catalogs(&mut expected, "gatewayId", false);
    append_catalogs(&mut expected, "gatewayId", false);
    expected.extend([
        response("GET", "/servers", json!([])),
        response("GET", "/gateways", json!([gateway_record(GATEWAY_ID)])),
        status(
            "DELETE",
            format!("/gateways/{GATEWAY_ID}"),
            StatusCode::NO_CONTENT,
        ),
    ]);
    let api = FakeApi::start(expected).await;

    let error = test_client(&api.base_url)
        .provision(OFFICIAL_CONFORMANCE_BACKEND_URL)
        .await
        .expect_err("missing prompt must fail");

    let message = format!("{error:#}");
    assert!(message.contains("test_simple_prompt"), "{message}");
    assert!(message.contains(GATEWAY_ID), "{message}");
    api.assert_complete();
}

#[tokio::test]
async fn provision_rejects_prefixed_catalog_names_instead_of_weakening_identity() {
    let prefixed_catalogs = [
        json!([{"id":"tool-1", "name":"fixture_test_simple_text", "gatewayId":GATEWAY_ID}]),
        json!([{"id":"resource-1", "uri":"test://static-text", "gatewayId":GATEWAY_ID}]),
        json!([{"id":"prompt-1", "name":"fixture_test_simple_prompt", "gatewayId":GATEWAY_ID}]),
    ];
    let mut expected = provision_prefix(json!([]));
    append_create_and_refresh(&mut expected);
    for _ in 0..2 {
        expected.extend([
            response("GET", "/tools", prefixed_catalogs[0].clone()),
            response("GET", "/resources", prefixed_catalogs[1].clone()),
            response("GET", "/prompts", prefixed_catalogs[2].clone()),
        ]);
    }
    expected.extend([
        response("GET", "/servers", json!([])),
        response("GET", "/gateways", json!([gateway_record(GATEWAY_ID)])),
        status(
            "DELETE",
            format!("/gateways/{GATEWAY_ID}"),
            StatusCode::NO_CONTENT,
        ),
    ]);
    let api = FakeApi::start(expected).await;

    let error = test_client(&api.base_url)
        .provision(OFFICIAL_CONFORMANCE_BACKEND_URL)
        .await
        .expect_err("prefixed identities must remain a setup error");

    let message = format!("{error:#}");
    assert!(message.contains("test_simple_text"), "{message}");
    assert!(message.contains("test_simple_prompt"), "{message}");
    api.assert_complete();
}

#[tokio::test]
async fn cleanup_deletes_server_before_gateway_and_accepts_not_found() {
    let api = FakeApi::start(vec![
        response(
            "GET",
            "/servers",
            json!([server_record(OFFICIAL_CONFORMANCE_SERVER_ID)]),
        ),
        status(
            "DELETE",
            format!("/servers/{OFFICIAL_CONFORMANCE_SERVER_ID}"),
            StatusCode::NOT_FOUND,
        ),
        response("GET", "/gateways", json!([gateway_record("gateway-old")])),
        status("DELETE", "/gateways/gateway-old", StatusCode::NOT_FOUND),
    ])
    .await;
    let fixture = ProvisionedConformanceFixture {
        gateway_id: "gateway-old".to_owned(),
        server_id: OFFICIAL_CONFORMANCE_SERVER_ID.to_owned(),
    };

    test_client(&api.base_url)
        .cleanup(Some(&fixture))
        .await
        .expect("idempotent cleanup");

    api.assert_complete();
}

#[tokio::test]
async fn cleanup_rejects_non_not_found_failure() {
    let api = FakeApi::start(vec![
        response(
            "GET",
            "/servers",
            json!([server_record(OFFICIAL_CONFORMANCE_SERVER_ID)]),
        ),
        status(
            "DELETE",
            format!("/servers/{OFFICIAL_CONFORMANCE_SERVER_ID}"),
            StatusCode::NO_CONTENT,
        ),
        response("GET", "/gateways", json!([gateway_record("gateway-old")])),
        status(
            "DELETE",
            "/gateways/gateway-old",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ])
    .await;
    let fixture = ProvisionedConformanceFixture {
        gateway_id: "gateway-old".to_owned(),
        server_id: OFFICIAL_CONFORMANCE_SERVER_ID.to_owned(),
    };

    let error = test_client(&api.base_url)
        .cleanup(Some(&fixture))
        .await
        .expect_err("cleanup status must fail");

    assert!(format!("{error:#}").contains("500"));
    api.assert_complete();
}

#[tokio::test]
async fn cleanup_still_attempts_gateway_after_server_delete_failure() {
    let api = FakeApi::start(vec![
        response(
            "GET",
            "/servers",
            json!([server_record(OFFICIAL_CONFORMANCE_SERVER_ID)]),
        ),
        status(
            "DELETE",
            format!("/servers/{OFFICIAL_CONFORMANCE_SERVER_ID}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        response("GET", "/gateways", json!([gateway_record("gateway-old")])),
        status("DELETE", "/gateways/gateway-old", StatusCode::NO_CONTENT),
    ])
    .await;
    let fixture = ProvisionedConformanceFixture {
        gateway_id: "gateway-old".to_owned(),
        server_id: OFFICIAL_CONFORMANCE_SERVER_ID.to_owned(),
    };

    let error = test_client(&api.base_url)
        .cleanup(Some(&fixture))
        .await
        .expect_err("server cleanup status must fail");

    assert!(format!("{error:#}").contains("500"));
    api.assert_complete();
}

#[tokio::test]
async fn provision_combines_primary_and_cleanup_failures_in_that_order() {
    let mut expected = provision_prefix(json!([]));
    expected.push(response("POST", "/gateways", gateway_record(GATEWAY_ID)));
    expected.push(status(
        "POST",
        format!("/gateways/{GATEWAY_ID}/tools/refresh?include_resources=true&include_prompts=true"),
        StatusCode::BAD_GATEWAY,
    ));
    expected.push(response("GET", "/servers", json!([])));
    expected.push(response(
        "GET",
        "/gateways",
        json!([gateway_record(GATEWAY_ID)]),
    ));
    expected.push(status(
        "DELETE",
        format!("/gateways/{GATEWAY_ID}"),
        StatusCode::INTERNAL_SERVER_ERROR,
    ));
    let api = FakeApi::start(expected).await;

    let error = test_client(&api.base_url)
        .provision(OFFICIAL_CONFORMANCE_BACKEND_URL)
        .await
        .expect_err("primary and cleanup must fail");

    let message = format!("{error:#}");
    let primary = message.find("502").expect("primary status");
    let cleanup = message.find("cleanup").expect("cleanup diagnostic");
    let cleanup_status = message.rfind("500").expect("cleanup status");
    assert!(primary < cleanup && cleanup < cleanup_status, "{message}");
    api.assert_complete();
}

#[tokio::test]
async fn malformed_gateway_create_response_reconciles_reserved_gateway() {
    let mut expected = provision_prefix(json!([]));
    expected.extend([
        raw_response("POST", "/gateways", StatusCode::CREATED, "{"),
        response(
            "GET",
            "/gateways",
            json!([gateway_record("gateway-committed")]),
        ),
        status(
            "DELETE",
            "/gateways/gateway-committed",
            StatusCode::NO_CONTENT,
        ),
        response("GET", "/gateways", json!([])),
        response("GET", "/gateways", json!([])),
    ]);
    let api = FakeApi::start(expected).await;

    let error = test_client(&api.base_url)
        .provision(OFFICIAL_CONFORMANCE_BACKEND_URL)
        .await
        .expect_err("malformed gateway response must fail");

    assert!(format!("{error:#}").contains("invalid JSON"));
    api.assert_complete();
}

#[tokio::test]
async fn gateway_create_and_reconciliation_failures_are_combined_primary_first() {
    let mut expected = provision_prefix(json!([]));
    expected.extend([
        raw_response("POST", "/gateways", StatusCode::CREATED, "{"),
        status("GET", "/gateways", StatusCode::INTERNAL_SERVER_ERROR),
    ]);
    let api = FakeApi::start(expected).await;

    let error = test_client(&api.base_url)
        .provision(OFFICIAL_CONFORMANCE_BACKEND_URL)
        .await
        .expect_err("create and reconciliation must fail");

    let message = format!("{error:#}");
    let primary = message.find("invalid JSON").expect("primary diagnostic");
    let reconciliation = message
        .find("reconciliation")
        .expect("reconciliation diagnostic");
    let reconciliation_status = message.rfind("500").expect("reconciliation status");
    assert!(
        primary < reconciliation && reconciliation < reconciliation_status,
        "{message}"
    );
    api.assert_complete();
}

#[tokio::test]
async fn gateway_create_timeout_reconciles_a_delayed_committed_gateway() {
    let state = DelayedGatewayState::default();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind delayed gateway API");
    let address = listener.local_addr().expect("delayed gateway API address");
    let app = Router::new()
        .fallback(any(delayed_gateway_handler))
        .with_state(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve delayed gateway API");
    });
    let client = ConformanceFixtureClient::builder(format!("http://{address}"), TOKEN)
        .request_timeout(Duration::from_millis(20))
        .reconciliation_attempts(5)
        .reconciliation_interval(Duration::from_millis(35))
        .build()
        .expect("valid fixture client");

    let error = tokio::time::timeout(
        Duration::from_secs(1),
        client.provision(OFFICIAL_CONFORMANCE_BACKEND_URL),
    )
    .await
    .expect("reconciliation must remain bounded")
    .expect_err("gateway creation must retain timeout failure");

    let message = format!("{error:#}");
    assert!(message.contains("timed out"), "{message}");
    assert!(!message.contains("reconciliation failed"), "{message}");
    assert!(state.deleted.load(Ordering::SeqCst));
    assert!(state.gateway_gets.load(Ordering::SeqCst) >= 4);
}

#[test]
fn builder_rejects_invalid_base_url_and_zero_attempts() {
    let invalid_url = ConformanceFixtureClient::builder("not a URL", TOKEN)
        .build()
        .expect_err("invalid URL");
    let zero_attempts = ConformanceFixtureClient::builder("http://localhost", TOKEN)
        .max_attempts(0)
        .build()
        .expect_err("zero attempts");

    assert!(format!("{invalid_url:#}").contains("base URL"));
    assert!(format!("{zero_attempts:#}").contains("max_attempts"));
}

#[test]
fn builder_accepts_only_http_origins_with_root_path() {
    for base_url in [
        "http://localhost/api",
        "http://localhost/?scope=fixture",
        "https://localhost/#fixture",
    ] {
        let error = ConformanceFixtureClient::builder(base_url, TOKEN)
            .build()
            .expect_err("non-origin base URL");
        assert!(format!("{error:#}").contains("base URL"));
    }
    ConformanceFixtureClient::builder("https://localhost/", TOKEN)
        .build()
        .expect("root HTTPS origin");
}

#[test]
fn builder_accepts_rfc6750_b64token_and_jwt_characters() {
    for token in [
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJmaXh0dXJlIn0.signature_-~",
        "abcDEF012-._~+/==",
    ] {
        ConformanceFixtureClient::builder("http://localhost", token)
            .build()
            .expect("valid RFC 6750 bearer token");
    }
}

#[test]
fn builder_rejects_zero_request_timeout() {
    let error = ConformanceFixtureClient::builder("http://localhost", TOKEN)
        .request_timeout(Duration::ZERO)
        .build()
        .expect_err("zero request timeout");

    assert!(format!("{error:#}").contains("request_timeout"));
}

#[test]
fn builder_rejects_reconciliation_without_two_quiet_attempts() {
    for attempts in [0, 1] {
        let error = ConformanceFixtureClient::builder("http://localhost", TOKEN)
            .reconciliation_attempts(attempts)
            .build()
            .expect_err("insufficient reconciliation attempts");

        assert!(format!("{error:#}").contains("reconciliation_attempts"));
    }
}

#[tokio::test]
async fn request_timeout_bounds_a_non_responding_admin_api() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind hanging API");
    let address = listener.local_addr().expect("hanging API address");
    tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(any(never_responds)))
            .await
            .expect("serve hanging API");
    });
    let client = ConformanceFixtureClient::builder(format!("http://{address}"), TOKEN)
        .request_timeout(Duration::from_millis(25))
        .build()
        .expect("valid fixture client");

    let error = tokio::time::timeout(Duration::from_secs(1), client.cleanup(None))
        .await
        .expect("client request timeout must bound the operation")
        .expect_err("hanging request must fail");

    let message = format!("{error:#}");
    assert!(message.contains("timed out"), "{message}");
    assert!(!message.contains(TOKEN), "{message}");
}

#[tokio::test]
async fn stalled_json_body_is_reported_as_a_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stalled JSON API");
    let address = listener.local_addr().expect("stalled JSON API address");
    tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(any(stalled_json_handler)))
            .await
            .expect("serve stalled JSON API");
    });
    let client = ConformanceFixtureClient::builder(format!("http://{address}"), TOKEN)
        .request_timeout(Duration::from_millis(25))
        .build()
        .expect("valid fixture client");

    let error = tokio::time::timeout(
        Duration::from_secs(1),
        client.provision(OFFICIAL_CONFORMANCE_BACKEND_URL),
    )
    .await
    .expect("stalled response body must remain bounded")
    .expect_err("stalled JSON body must fail");

    let message = format!("{error:#}");
    assert!(message.contains("timed out"), "{message}");
    assert!(!message.contains("invalid JSON"), "{message}");
    assert!(!message.contains(TOKEN), "{message}");
}

#[test]
fn builder_rejects_empty_whitespace_and_internal_equals_without_echoing_token() {
    for token in ["", "token with space", "abc=def"] {
        let error = ConformanceFixtureClient::builder("http://localhost", token)
            .build()
            .expect_err("malformed bearer token");
        let message = format!("{error:#}");
        if !token.is_empty() {
            assert!(!message.contains(token), "token leaked in: {message}");
        }
    }
}

#[tokio::test]
async fn token_is_absent_from_debug_and_errors() {
    let builder = ConformanceFixtureClient::builder("http://127.0.0.1:9", TOKEN);
    assert!(!format!("{builder:?}").contains(TOKEN));
    let client = builder.build().expect("valid client");
    assert!(!format!("{client:?}").contains(TOKEN));

    let error = client
        .cleanup(None)
        .await
        .expect_err("closed port must fail");

    assert!(!format!("{error:?}").contains(TOKEN));
    assert!(!format!("{error:#}").contains(TOKEN));
}

#[tokio::test]
async fn token_is_redacted_when_it_matches_a_fixture_id() {
    let token = OFFICIAL_CONFORMANCE_SERVER_ID;
    let api = FakeApi::start(vec![
        response(
            "GET",
            "/servers",
            json!([server_record(OFFICIAL_CONFORMANCE_SERVER_ID)]),
        ),
        status(
            "DELETE",
            format!("/servers/{OFFICIAL_CONFORMANCE_SERVER_ID}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        response("GET", "/gateways", json!([gateway_record("gateway-old")])),
        status("DELETE", "/gateways/gateway-old", StatusCode::NO_CONTENT),
    ])
    .await;
    let client = ConformanceFixtureClient::builder(&api.base_url, token)
        .build()
        .expect("valid client");
    let fixture = ProvisionedConformanceFixture {
        gateway_id: "gateway-old".to_owned(),
        server_id: token.to_owned(),
    };

    let error = client
        .cleanup(Some(&fixture))
        .await
        .expect_err("cleanup status must fail");

    assert!(!format!("{error:#}").contains(token));
    api.assert_complete();
}
