use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::mcp::protocol::{initialize_with_id_and_version, jsonrpc_with_id};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode};
use axum::routing::any;
use cf_integration::mcp::GatewayTopology;
use cf_integration::mcp::gateway::{
    DEFAULT_PROTOCOL_VERSION, GatewayClient, GatewayRequest, HeaderOverride, MCP_PROTOCOL_VERSION,
    MCP_SESSION_ID,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

#[derive(Clone, Debug)]
struct ObservedRequest {
    method: String,
    uri: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

#[derive(Clone, Debug)]
struct MockResponse {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: String,
}

impl MockResponse {
    fn json(status: StatusCode, body: Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: body.to_string(),
        }
    }

    fn sse(body: Value) -> Self {
        Self {
            status: StatusCode::OK,
            headers: vec![(
                "content-type".to_owned(),
                "text/event-stream; charset=utf-8".to_owned(),
            )],
            body: format!("event: message\ndata: {body}\n\n"),
        }
    }

    fn empty(status: StatusCode) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: String::new(),
        }
    }

    fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    fn dataplane(self) -> Self {
        self.with_header("x-cf-integration-backend", "dataplane")
    }
}

#[derive(Clone)]
struct MockState {
    observed: Arc<Mutex<Vec<ObservedRequest>>>,
    responses: Arc<Mutex<VecDeque<MockResponse>>>,
}

struct MockServer {
    base_url: String,
    observed: Arc<Mutex<Vec<ObservedRequest>>>,
    task: JoinHandle<()>,
}

impl MockServer {
    async fn start(responses: impl IntoIterator<Item = MockResponse>) -> Self {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let state = MockState {
            observed: Arc::clone(&observed),
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
        };
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock listener should bind");
        let address = listener
            .local_addr()
            .expect("mock listener should have an address");
        let app = Router::new().fallback(any(mock_handler)).with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock server should run");
        });

        Self {
            base_url: format!("http://{address}/ignored/base?discard=true#fragment"),
            observed,
            task,
        }
    }

    fn requests(&self) -> Vec<ObservedRequest> {
        self.observed
            .lock()
            .expect("observations should not be poisoned")
            .clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn mock_handler(State(state): State<MockState>, request: Request<Body>) -> Response<Body> {
    let method = request.method().to_string();
    let uri = request.uri().to_string();
    let headers = headers_to_map(request.headers());
    let body = to_bytes(request.into_body(), usize::MAX)
        .await
        .expect("mock request body should be readable");
    state
        .observed
        .lock()
        .expect("observations should not be poisoned")
        .push(ObservedRequest {
            method,
            uri,
            headers,
            body: body.to_vec(),
        });

    let response = state
        .responses
        .lock()
        .expect("responses should not be poisoned")
        .pop_front()
        .expect("every request should have a configured response");
    response_from(response)
}

fn response_from(spec: MockResponse) -> Response<Body> {
    let mut response = Response::new(Body::from(spec.body));
    *response.status_mut() = spec.status;
    for (name, value) in spec.headers {
        response.headers_mut().append(
            HeaderName::from_bytes(name.as_bytes()).expect("mock header name should be valid"),
            HeaderValue::from_str(&value).expect("mock header value should be valid"),
        );
    }
    response
}

fn headers_to_map(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value
                    .to_str()
                    .expect("test request headers should be visible ASCII")
                    .to_owned(),
            )
        })
        .collect()
}

fn response(id: u64, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn initialize(id: Value) -> GatewayRequest {
    GatewayRequest::probe(initialize_with_id_and_version(id, DEFAULT_PROTOCOL_VERSION))
        .protocol_version(HeaderOverride::Omit)
}

fn rpc(method: &str, params: Option<Value>, id: Value) -> GatewayRequest {
    GatewayRequest::probe(jsonrpc_with_id(method, params, id))
}

#[tokio::test]
async fn initialize_uses_fixed_encoded_route_and_exact_required_headers() {
    let server = MockServer::start([MockResponse::json(
        StatusCode::OK,
        response(1, json!({"protocolVersion": DEFAULT_PROTOCOL_VERSION})),
    )
    .with_header(MCP_SESSION_ID, "session-one")
    .dataplane()])
    .await;
    let mut client = GatewayClient::new(
        GatewayTopology::Dataplane,
        &server.base_url,
        "tenant/a b?%\u{00fc}",
        "secret-token",
    )
    .expect("valid gateway client should build");

    let exchange = client
        .send(initialize(json!(1)))
        .await
        .expect("initialize should succeed");

    assert_eq!(
        client.endpoint().as_str(),
        format!(
            "{}/servers/tenant%2Fa%20b%3F%25%C3%BC/mcp",
            server
                .base_url
                .split('/')
                .take(3)
                .collect::<Vec<_>>()
                .join("/")
        )
    );
    assert_eq!(client.session_id(), Some("session-one"));
    assert_eq!(exchange.session_id(), Some("session-one"));
    assert_eq!(exchange.mode(), GatewayTopology::Dataplane);
    assert_eq!(exchange.status(), 200);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.uri, "/servers/tenant%2Fa%20b%3F%25%C3%BC/mcp");
    assert_eq!(request.headers["authorization"], "Bearer secret-token");
    assert_eq!(
        request.headers["accept"],
        "application/json, text/event-stream"
    );
    assert_eq!(request.headers["content-type"], "application/json");
    assert!(!request.headers.contains_key(MCP_PROTOCOL_VERSION));
    assert!(!request.headers.contains_key(MCP_SESSION_ID));
    let payload: Value =
        serde_json::from_slice(&request.body).expect("initialize body should be JSON");
    assert_eq!(payload["method"], "initialize");
    assert_eq!(
        payload["params"]["protocolVersion"],
        DEFAULT_PROTOCOL_VERSION
    );
}

#[tokio::test]
async fn response_session_is_propagated_to_notifications() {
    let server = MockServer::start([
        MockResponse::json(StatusCode::OK, response(7, json!({})))
            .with_header(MCP_SESSION_ID, "from-response"),
        MockResponse::empty(StatusCode::ACCEPTED),
    ])
    .await;
    let mut client = GatewayClient::new(
        GatewayTopology::Direct,
        &server.base_url,
        "server",
        "secret-token",
    )
    .expect("valid gateway client should build");

    client
        .send(initialize(json!(7)))
        .await
        .expect("initialize should succeed");
    let notification = client
        .send(GatewayRequest::probe(
            json!({"jsonrpc":"2.0", "method":"notifications/initialized"}),
        ))
        .await
        .expect("initialized notification should receive 202");

    assert_eq!(notification.status(), 202);
    let requests = server.requests();
    assert!(requests.iter().all(|request| request.uri == "/mcp"));
    assert_eq!(requests[1].headers[MCP_SESSION_ID], "from-response");
    let payload: Value =
        serde_json::from_slice(&requests[1].body).expect("initialized notification should be JSON");
    assert_eq!(payload["jsonrpc"], "2.0");
    assert_eq!(payload["method"], "notifications/initialized");
    assert!(payload.get("id").is_none());
}

#[tokio::test]
async fn post_requests_parse_json_and_sse() {
    let server = MockServer::start([
        MockResponse::json(StatusCode::OK, response(2, json!({"tools": []}))).dataplane(),
        MockResponse::sse(response(3, json!({"resources": []}))).dataplane(),
    ])
    .await;
    let mut client = GatewayClient::new(
        GatewayTopology::Dataplane,
        &server.base_url,
        "server",
        "secret-token",
    )
    .expect("valid gateway client should build");

    let json_exchange = client
        .send(rpc("tools/list", Some(json!({})), json!(2)))
        .await
        .expect("JSON response should validate");
    let sse_exchange = client
        .send(rpc("resources/list", Some(json!({})), json!(3)))
        .await
        .expect("SSE response should validate");

    assert_eq!(
        json_exchange.message(),
        Some(&response(2, json!({"tools": []})))
    );
    assert_eq!(
        sse_exchange.message(),
        Some(&response(3, json!({"resources": []})))
    );
}

#[tokio::test]
async fn structured_json_suffix_is_not_accepted_as_mcp_json() {
    let server = MockServer::start([MockResponse {
        status: StatusCode::OK,
        headers: vec![(
            "content-type".to_owned(),
            "application/problem+json".to_owned(),
        )],
        body: response(2, json!({"tools": []})).to_string(),
    }])
    .await;
    let mut client = GatewayClient::new(
        GatewayTopology::Direct,
        &server.base_url,
        "server",
        "secret-token",
    )
    .expect("valid gateway client should build");

    let exchange = client
        .send(rpc("tools/list", None, json!(2)))
        .await
        .expect("non-MCP media types remain available for workflow diagnostics");
    assert!(exchange.message().is_none());
    assert!(!exchange.body().is_empty());
}

#[tokio::test]
async fn custom_protocol_version_sets_the_default_request_header() {
    let server = MockServer::start([MockResponse::json(
        StatusCode::OK,
        response(1, json!({"protocolVersion": "2099-01-01"})),
    )])
    .await;
    let mut client = GatewayClient::builder(
        GatewayTopology::Direct,
        &server.base_url,
        "server",
        "secret-token",
    )
    .protocol_version("2099-01-01")
    .build()
    .expect("custom-version client should build");

    client
        .send(rpc("ping", None, json!(1)))
        .await
        .expect("custom-version initialize should succeed");

    let request = &server.requests()[0];
    assert_eq!(request.headers[MCP_PROTOCOL_VERSION], "2099-01-01");
}

#[tokio::test]
async fn absent_response_session_is_never_synthesized_or_sent() {
    let server = MockServer::start([
        MockResponse::json(StatusCode::OK, response(1, json!({}))),
        MockResponse::empty(StatusCode::ACCEPTED),
    ])
    .await;
    let mut client = GatewayClient::new(
        GatewayTopology::Direct,
        &server.base_url,
        "server",
        "secret-token",
    )
    .expect("valid gateway client should build");

    client
        .send(initialize(json!(1)))
        .await
        .expect("initialize should succeed without a session header");
    client
        .send(GatewayRequest::probe(
            json!({"jsonrpc":"2.0", "method":"notifications/initialized"}),
        ))
        .await
        .expect("notification should succeed without a session header");

    assert_eq!(client.session_id(), None);
    assert!(
        server
            .requests()
            .iter()
            .all(|request| !request.headers.contains_key(MCP_SESSION_ID))
    );
}

#[tokio::test]
async fn assigned_session_must_be_nonempty_visible_ascii_without_spaces() {
    for invalid_session in ["", "session with spaces"] {
        let server =
            MockServer::start([MockResponse::json(StatusCode::OK, response(1, json!({})))
                .with_header(MCP_SESSION_ID, invalid_session)])
            .await;
        let mut client = GatewayClient::new(
            GatewayTopology::Direct,
            &server.base_url,
            "server",
            "secret-token",
        )
        .expect("valid gateway client should build");

        let error = client
            .send(initialize(json!(1)))
            .await
            .expect_err("invalid response session IDs must be rejected");

        assert!(error.to_string().contains("session header"));
        assert_eq!(client.session_id(), None);
    }
}

#[tokio::test]
async fn http_failure_diagnostics_capture_exchange_without_leaking_secrets_or_controls() {
    let token = "very-secret-token";
    let server = MockServer::start([MockResponse {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        headers: vec![("x-debug".to_owned(), format!("leaked-{token}"))],
        body: format!("failed {token}\nnext\u{0007}"),
    }
    .dataplane()])
    .await;
    let mut client = GatewayClient::new(
        GatewayTopology::Dataplane,
        &server.base_url,
        "server",
        token,
    )
    .expect("valid gateway client should build");

    let exchange = client
        .send(rpc("tools/list", None, json!(4)))
        .await
        .expect("HTTP errors are returned for workflow validation");
    let diagnostic = format!("{exchange:?}");
    assert_eq!(exchange.status(), 500);
    assert_eq!(exchange.request().mode(), GatewayTopology::Dataplane);
    assert!(diagnostic.contains("Dataplane"));
    assert!(diagnostic.contains("status: 500"));
    assert!(diagnostic.contains("<redacted>"));
    assert!(diagnostic.contains("\\n"));
    assert!(diagnostic.contains("\\u{0007}"));
    assert!(!diagnostic.contains(token));
    assert!(!diagnostic.contains('\u{0007}'));
    assert_eq!(exchange.request().headers()["authorization"], "<redacted>");
    assert!(
        exchange
            .request()
            .body()
            .expect("request body should be captured")
            .contains("tools/list")
    );
    assert_eq!(exchange.headers()["x-debug"], "leaked-<redacted>");
    assert_eq!(exchange.body(), "failed <redacted>\\nnext\\u{0007}");
}

#[tokio::test]
async fn failure_exchange_redacts_session_ids_reflected_in_response_bodies() {
    let session = "private-session-id-43f7";
    let server = MockServer::start([
        MockResponse::json(StatusCode::OK, response(1, json!({})))
            .with_header(MCP_SESSION_ID, session)
            .dataplane(),
        MockResponse {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            headers: vec![("x-debug".to_owned(), format!("reflected {session}"))],
            body: format!("failed session {session}"),
        }
        .dataplane(),
    ])
    .await;
    let mut client = GatewayClient::new(
        GatewayTopology::Dataplane,
        &server.base_url,
        "server",
        "secret-token",
    )
    .expect("valid gateway client should build");
    client
        .send(initialize(json!(1)))
        .await
        .expect("initialize should assign a session");

    let exchange = client
        .send(rpc("tools/list", None, json!(2)))
        .await
        .expect("HTTP failure should retain its diagnostic capture");
    let diagnostic = format!("{exchange:?}");

    assert!(!exchange.body().contains(session));
    assert!(!exchange.headers()["x-debug"].contains(session));
    assert!(!diagnostic.contains(session));
    assert!(exchange.body().contains("<redacted>"));
}

#[tokio::test]
async fn debug_output_redacts_bearer_token_session_and_payload() {
    let token = "debug-secret-token";
    let server = MockServer::start([MockResponse::json(StatusCode::OK, response(1, json!({})))
        .with_header(MCP_SESSION_ID, "debug-secret-session")
        .dataplane()])
    .await;
    let mut client = GatewayClient::new(
        GatewayTopology::Dataplane,
        &server.base_url,
        "server",
        token,
    )
    .expect("valid gateway client should build");
    client
        .send(initialize(json!(1)))
        .await
        .expect("initialize should succeed");
    let request = GatewayRequest::probe(
        json!({"jsonrpc":"2.0", "method":"notifications/custom", "params":{"token":token}}),
    )
    .session(HeaderOverride::Value("debug-secret-session".to_owned()));

    let diagnostic = format!("{client:?}\n{request:?}");

    assert!(diagnostic.contains("<redacted>"));
    assert!(!diagnostic.contains(token));
    assert!(!diagnostic.contains("debug-secret-session"));
}

#[test]
fn builder_debug_redacts_token_even_when_repeated_in_other_inputs() {
    let token = "repeated-secret";
    let builder = GatewayClient::builder(
        GatewayTopology::Direct,
        &format!("http://example.test/base?token={token}"),
        token,
        token,
    )
    .protocol_version(token);

    let diagnostic = format!("{builder:?}");

    assert!(diagnostic.contains("<redacted>"));
    assert!(!diagnostic.contains(token));
}

#[tokio::test]
async fn dataplane_rejects_absent_fallback_forged_and_duplicate_backend_markers() {
    let token = "backend-marker-secret";
    let cases = [
        MockResponse::json(StatusCode::OK, response(1, json!({}))),
        MockResponse::json(StatusCode::OK, response(1, json!({})))
            .with_header("x-cf-integration-backend", "controlplane-fallback"),
        MockResponse::json(StatusCode::OK, response(1, json!({})))
            .with_header("x-cf-integration-backend", token),
        MockResponse::json(StatusCode::OK, response(1, json!({})))
            .with_header("x-cf-integration-backend", "dataplane")
            .with_header("x-cf-integration-backend", "dataplane"),
    ];

    for response in cases {
        let server = MockServer::start([response]).await;
        let mut client = GatewayClient::new(
            GatewayTopology::Dataplane,
            &server.base_url,
            "server",
            token,
        )
        .expect("valid gateway client should build");

        let error = client
            .send(initialize(json!(1)))
            .await
            .expect_err("dataplane responses must carry one exact backend marker");
        let diagnostic = format!("{error}\n{error:?}");

        assert!(diagnostic.contains("backend marker"), "{diagnostic}");
        assert!(!diagnostic.contains(token), "{diagnostic}");
        assert_eq!(client.session_id(), None);
    }
}

#[tokio::test]
async fn controlplane_does_not_require_the_harness_only_backend_marker() {
    let server =
        MockServer::start([MockResponse::json(StatusCode::OK, response(1, json!({})))]).await;
    let mut client = GatewayClient::new(
        GatewayTopology::Direct,
        &server.base_url,
        "server",
        "secret-token",
    )
    .expect("valid gateway client should build");

    client
        .send(initialize(json!(1)))
        .await
        .expect("stock controlplane nginx has no integration backend marker");
}
