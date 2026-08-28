use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, CONNECTION, CONTENT_TYPE, HOST, LOCATION};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::routing::any;
use cf_integration::mcp::auth_proxy::{AuthProxy, MAX_REQUEST_BODY_BYTES};
use reqwest::{Client, Method, Url};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;

const INBOUND_TOKEN: &str = "must-not-reach-upstream";
const INJECTED_TOKEN: &str = "injected-secret-token";
const SENSITIVE_UPSTREAM_PATH: &str = "private/session-sensitive/mcp";

#[derive(Clone, Debug)]
struct CapturedRequest {
    method: Method,
    headers: HeaderMap,
    body: Bytes,
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<CapturedRequest>>>);

impl Capture {
    fn take(&self) -> Vec<CapturedRequest> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .expect("captured request lock should not be poisoned"),
        )
    }
}

struct TestServer {
    url: Url,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl TestServer {
    async fn start(router: Router, path: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test upstream should bind");
        let address = listener
            .local_addr()
            .expect("bound test listener should have an address");
        let (shutdown, shutdown_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_receiver.await;
                })
                .await
                .expect("test upstream should serve");
        });
        let url = Url::parse(&format!("http://{address}/{path}"))
            .expect("test upstream URL should be valid");
        Self {
            url,
            shutdown: Some(shutdown),
            task,
        }
    }

    async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.await.expect("test upstream task should join");
    }
}

async fn capture_handler(State(capture): State<Capture>, request: Request) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, MAX_REQUEST_BODY_BYTES)
        .await
        .expect("test request body should fit");
    capture
        .0
        .lock()
        .expect("captured request lock should not be poisoned")
        .push(CapturedRequest {
            method: parts.method,
            headers: parts.headers,
            body,
        });

    Response::builder()
        .status(StatusCode::CREATED)
        .header(CONTENT_TYPE, "application/json")
        .header("mcp-session-id", "upstream-session")
        .header("mcp-protocol-version", "2025-11-25")
        .header("x-upstream-header", "preserved")
        .header(CONNECTION, "x-response-hop")
        .header("x-response-hop", "must-not-be-forwarded")
        .body(Body::from(r#"{"forwarded":true}"#))
        .expect("test response should be valid")
}

async fn start_capture_server() -> (TestServer, Capture) {
    let capture = Capture::default();
    let router = Router::new()
        .route(&format!("/{SENSITIVE_UPSTREAM_PATH}"), any(capture_handler))
        .with_state(capture.clone());
    (
        TestServer::start(router, SENSITIVE_UPSTREAM_PATH).await,
        capture,
    )
}

fn client() -> Client {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("test client should build")
}

#[tokio::test]
async fn injects_auth_and_preserves_mcp_request_and_response_contract() {
    let (upstream, capture) = start_capture_server().await;
    let proxy = AuthProxy::start(upstream.url.clone(), INJECTED_TOKEN)
        .await
        .expect("proxy should start");

    let response = client()
        .post(proxy.url().clone())
        .header(AUTHORIZATION, format!("Bearer {INBOUND_TOKEN}"))
        .header(CONTENT_TYPE, "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", "client-session")
        .header("mcp-protocol-version", "2025-11-25")
        .header("x-end-to-end", "preserve-me")
        .header("origin", proxy.url().origin().ascii_serialization())
        .header(CONNECTION, "x-remove-me")
        .header("x-remove-me", "must-not-be-forwarded")
        .body(r#"{"jsonrpc":"2.0","id":1}"#)
        .send()
        .await
        .expect("proxy request should succeed");

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response.headers().get(CONTENT_TYPE),
        Some(&HeaderValue::from_static("application/json"))
    );
    assert_eq!(
        response.headers().get("mcp-session-id"),
        Some(&HeaderValue::from_static("upstream-session"))
    );
    assert_eq!(
        response.headers().get("mcp-protocol-version"),
        Some(&HeaderValue::from_static("2025-11-25"))
    );
    assert_eq!(
        response.headers().get("x-upstream-header"),
        Some(&HeaderValue::from_static("preserved"))
    );
    assert!(response.headers().get("x-response-hop").is_none());
    assert_eq!(
        response.text().await.expect("response body should read"),
        r#"{"forwarded":true}"#
    );

    let requests = capture.take();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, Method::POST);
    assert_eq!(
        request.headers.get(AUTHORIZATION),
        Some(&HeaderValue::from_static("Bearer injected-secret-token"))
    );
    let upstream_authority = format!(
        "{}:{}",
        upstream.url.host_str().expect("upstream host"),
        upstream.url.port().expect("upstream port")
    );
    assert_eq!(
        request
            .headers
            .get(HOST)
            .and_then(|value| value.to_str().ok()),
        Some(upstream_authority.as_str()),
        "ordinary proxy Host must target the fixed upstream authority"
    );
    assert_eq!(
        request.headers.get(CONTENT_TYPE),
        Some(&HeaderValue::from_static("application/json"))
    );
    assert_eq!(
        request.headers.get("accept"),
        Some(&HeaderValue::from_static(
            "application/json, text/event-stream"
        ))
    );
    assert_eq!(
        request.headers.get("mcp-session-id"),
        Some(&HeaderValue::from_static("client-session"))
    );
    assert_eq!(
        request.headers.get("mcp-protocol-version"),
        Some(&HeaderValue::from_static("2025-11-25"))
    );
    assert_eq!(
        request.headers.get("x-end-to-end"),
        Some(&HeaderValue::from_static("preserve-me"))
    );
    assert_eq!(
        request
            .headers
            .get("origin")
            .and_then(|value| value.to_str().ok()),
        Some(upstream.url.origin().ascii_serialization().as_str())
    );
    assert!(request.headers.get("x-remove-me").is_none());
    assert_eq!(request.body, r#"{"jsonrpc":"2.0","id":1}"#);

    proxy.shutdown().await.expect("proxy should shut down");
    upstream.shutdown().await;
}

#[tokio::test]
async fn selected_protocol_version_rewrites_only_the_initialize_payload() {
    let (upstream, capture) = start_capture_server().await;
    let proxy = AuthProxy::start_with_protocol_version(
        upstream.url.clone(),
        INJECTED_TOKEN,
        Some("2025-06-18"),
    )
    .await
    .expect("proxy should start");
    let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"inspector","version":"1"}}}"#;

    client()
        .post(proxy.url().clone())
        .header(CONTENT_TYPE, "application/json")
        .body(initialize)
        .send()
        .await
        .expect("initialize request should complete");

    let requests = capture.take();
    let payload: Value =
        serde_json::from_slice(&requests[0].body).expect("forwarded body should remain JSON");
    assert_eq!(
        payload["params"]["protocolVersion"],
        Value::String("2025-06-18".to_owned())
    );

    proxy.shutdown().await.expect("proxy should shut down");
    upstream.shutdown().await;
}

#[tokio::test]
async fn forwards_host_and_origin_unchanged_for_gateway_dns_rebinding_checks() {
    let (upstream, capture) = start_capture_server().await;
    let proxy = AuthProxy::start(upstream.url.clone(), INJECTED_TOKEN)
        .await
        .expect("proxy should start");

    let response = client()
        .get(proxy.url().clone())
        .header(HOST, "evil.example.com")
        .header("origin", "https://attacker.invalid:4443")
        .send()
        .await
        .expect("proxy request should succeed");
    assert_eq!(response.status(), StatusCode::CREATED);

    let requests = capture.take();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, Method::GET);
    assert_eq!(
        requests[0].headers.get(HOST),
        Some(&HeaderValue::from_static("evil.example.com"))
    );
    assert_eq!(
        requests[0].headers.get("origin"),
        Some(&HeaderValue::from_static("https://attacker.invalid:4443"))
    );

    proxy.shutdown().await.expect("proxy should shut down");
    upstream.shutdown().await;
}

#[tokio::test]
async fn rejects_wrong_path_query_and_unsupported_methods() {
    let (upstream, capture) = start_capture_server().await;
    let proxy = AuthProxy::start(upstream.url.clone(), INJECTED_TOKEN)
        .await
        .expect("proxy should start");
    let client = client();

    let mut wrong_path = proxy.url().clone();
    wrong_path.set_path("/wrong");
    assert_eq!(
        client
            .get(wrong_path)
            .send()
            .await
            .expect("wrong path request should complete")
            .status(),
        StatusCode::NOT_FOUND
    );

    let mut query = proxy.url().clone();
    query.set_query(Some("redirect=elsewhere"));
    assert_eq!(
        client
            .get(query)
            .send()
            .await
            .expect("query request should complete")
            .status(),
        StatusCode::NOT_FOUND
    );

    for method in [Method::PUT, Method::PATCH, Method::OPTIONS, Method::CONNECT] {
        assert_eq!(
            client
                .request(method, proxy.url().clone())
                .send()
                .await
                .expect("unsupported method request should complete")
                .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
    assert!(capture.take().is_empty());

    proxy.shutdown().await.expect("proxy should shut down");
    upstream.shutdown().await;
}

#[tokio::test]
async fn supports_delete_and_does_not_follow_upstream_redirects() {
    async fn redirect() -> Response<Body> {
        Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header(LOCATION, "https://attacker.invalid/stolen")
            .header("mcp-session-id", "redirect-session")
            .body(Body::from("redirect-body"))
            .expect("test redirect should be valid")
    }

    let upstream =
        TestServer::start(Router::new().route("/redirect", any(redirect)), "redirect").await;
    let proxy = AuthProxy::start(upstream.url.clone(), INJECTED_TOKEN)
        .await
        .expect("proxy should start");

    let response = client()
        .delete(proxy.url().clone())
        .send()
        .await
        .expect("proxy request should succeed");
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        response.headers().get(LOCATION),
        Some(&HeaderValue::from_static("https://attacker.invalid/stolen"))
    );
    assert_eq!(
        response.headers().get("mcp-session-id"),
        Some(&HeaderValue::from_static("redirect-session"))
    );
    assert_eq!(
        response.text().await.expect("response body should read"),
        "redirect-body"
    );

    proxy.shutdown().await.expect("proxy should shut down");
    upstream.shutdown().await;
}

#[tokio::test]
async fn streams_sse_response_chunks_without_buffering() {
    async fn sse() -> Response<Body> {
        let (sender, receiver) = mpsc::channel::<Result<Bytes, Infallible>>(2);
        tokio::spawn(async move {
            sender
                .send(Ok(Bytes::from_static(b"data: first\n\n")))
                .await
                .expect("first SSE receiver should remain open");
            tokio::time::sleep(Duration::from_millis(400)).await;
            sender
                .send(Ok(Bytes::from_static(b"data: second\n\n")))
                .await
                .expect("second SSE receiver should remain open");
        });
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "text/event-stream")
            .header("mcp-session-id", "sse-session")
            .body(Body::from_stream(ReceiverStream::new(receiver)))
            .expect("test SSE response should be valid")
    }

    let upstream = TestServer::start(Router::new().route("/sse", any(sse)), "sse").await;
    let proxy = AuthProxy::start(upstream.url.clone(), INJECTED_TOKEN)
        .await
        .expect("proxy should start");

    let mut response = client()
        .get(proxy.url().clone())
        .send()
        .await
        .expect("proxy request should succeed");
    assert_eq!(
        response.headers().get(CONTENT_TYPE),
        Some(&HeaderValue::from_static("text/event-stream"))
    );
    let first = tokio::time::timeout(Duration::from_millis(200), response.chunk())
        .await
        .expect("first SSE chunk must arrive before the upstream stream ends")
        .expect("first SSE chunk should read")
        .expect("first SSE chunk should exist");
    assert_eq!(first, "data: first\n\n");
    let second = tokio::time::timeout(Duration::from_secs(1), response.chunk())
        .await
        .expect("second SSE chunk should arrive")
        .expect("second SSE chunk should read")
        .expect("second SSE chunk should exist");
    assert_eq!(second, "data: second\n\n");

    proxy.shutdown().await.expect("proxy should shut down");
    upstream.shutdown().await;
}

#[tokio::test]
async fn caps_buffered_request_bodies_before_forwarding() {
    let (upstream, capture) = start_capture_server().await;
    let proxy = AuthProxy::start(upstream.url.clone(), INJECTED_TOKEN)
        .await
        .expect("proxy should start");

    let response = client()
        .post(proxy.url().clone())
        .body(vec![b'x'; MAX_REQUEST_BODY_BYTES + 1])
        .send()
        .await
        .expect("oversized request should receive a response");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(capture.take().is_empty());

    proxy.shutdown().await.expect("proxy should shut down");
    upstream.shutdown().await;
}

#[tokio::test]
async fn endpoint_is_unguessable_and_debug_and_errors_do_not_leak_secrets() {
    let (upstream, _capture) = start_capture_server().await;
    let proxy_a = AuthProxy::start(upstream.url.clone(), INJECTED_TOKEN)
        .await
        .expect("first proxy should start");
    let proxy_b = AuthProxy::start(upstream.url.clone(), INJECTED_TOKEN)
        .await
        .expect("second proxy should start");

    assert_ne!(proxy_a.url().path(), proxy_b.url().path());
    assert!(proxy_a.url().path().len() >= 32);
    assert_eq!(proxy_a.url().host_str(), Some("127.0.0.1"));
    assert!(proxy_a.url().port().is_some_and(|port| port != 0));
    let debug = format!("{proxy_a:?}");
    for secret in [
        INJECTED_TOKEN,
        proxy_a.url().path(),
        SENSITIVE_UPSTREAM_PATH,
    ] {
        assert!(!debug.contains(secret));
    }
    assert!(debug.contains("<redacted>"));

    let invalid_token = "sensitive-token\ninvalid";
    let error = AuthProxy::start(upstream.url.clone(), invalid_token)
        .await
        .expect_err("invalid Authorization header should be rejected");
    let display = error.to_string();
    let debug = format!("{error:?}");
    assert!(!display.contains(invalid_token));
    assert!(!debug.contains(invalid_token));

    proxy_a.shutdown().await.expect("proxy should shut down");
    proxy_b.shutdown().await.expect("proxy should shut down");
    upstream.shutdown().await;
}

#[tokio::test]
async fn shutdown_stops_accepting_connections() {
    let (upstream, _capture) = start_capture_server().await;
    let proxy = AuthProxy::start(upstream.url.clone(), INJECTED_TOKEN)
        .await
        .expect("proxy should start");
    let endpoint = proxy.url().clone();

    proxy.shutdown().await.expect("proxy should shut down");

    let result = tokio::time::timeout(Duration::from_secs(1), client().get(endpoint).send())
        .await
        .expect("connection refusal should not hang");
    assert!(result.is_err());
    upstream.shutdown().await;
}

#[derive(Clone)]
struct BackendMarkerResponse {
    markers: Vec<&'static str>,
}

async fn backend_marker_handler(State(state): State<BackendMarkerResponse>) -> Response<Body> {
    let mut builder = Response::builder().status(StatusCode::OK);
    for marker in state.markers {
        builder = builder.header("x-cf-integration-backend", marker);
    }
    builder
        .body(Body::from("private-upstream-body"))
        .expect("marker response should build")
}

#[tokio::test]
async fn dataplane_proxy_requires_one_exact_backend_marker_before_forwarding() {
    let accepted = TestServer::start(
        Router::new()
            .route("/servers/test/mcp", any(backend_marker_handler))
            .with_state(BackendMarkerResponse {
                markers: vec!["dataplane"],
            }),
        "servers/test/mcp",
    )
    .await;
    let proxy = AuthProxy::start(accepted.url.clone(), INJECTED_TOKEN)
        .await
        .expect("dataplane proxy should start");
    let response = client()
        .get(proxy.url().clone())
        .send()
        .await
        .expect("valid dataplane marker should be forwarded");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.expect("body should read"),
        "private-upstream-body"
    );
    proxy.shutdown().await.expect("proxy should shut down");
    accepted.shutdown().await;

    for markers in [
        vec![],
        vec!["controlplane-fallback"],
        vec!["private-forged-marker"],
        vec!["dataplane", "dataplane"],
    ] {
        let upstream = TestServer::start(
            Router::new()
                .route("/servers/test/mcp", any(backend_marker_handler))
                .with_state(BackendMarkerResponse { markers }),
            "servers/test/mcp",
        )
        .await;
        let proxy = AuthProxy::start(upstream.url.clone(), INJECTED_TOKEN)
            .await
            .expect("dataplane proxy should start");

        let response = client()
            .get(proxy.url().clone())
            .send()
            .await
            .expect("invalid backend identity should fail closed locally");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(response.headers().get("x-cf-integration-backend").is_none());
        assert!(
            response
                .text()
                .await
                .expect("failure body should read")
                .is_empty(),
            "the proxy must not expose an invalid upstream response body"
        );

        proxy.shutdown().await.expect("proxy should shut down");
        upstream.shutdown().await;
    }
}

#[tokio::test]
async fn builtin_data_plane_proxy_allows_a_routed_controlplane_response() {
    let upstream = TestServer::start(
        Router::new()
            .route("/servers/test/mcp", any(backend_marker_handler))
            .with_state(BackendMarkerResponse {
                markers: vec!["controlplane"],
            }),
        "servers/test/mcp",
    )
    .await;
    let proxy = AuthProxy::start_builtin_data_plane(upstream.url.clone(), INJECTED_TOKEN)
        .await
        .expect("built-in data-plane proxy should start");

    let response = client()
        .get(proxy.url().clone())
        .send()
        .await
        .expect("routed built-in response should be forwarded");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.text().await.expect("body should read"),
        "private-upstream-body"
    );

    proxy.shutdown().await.expect("proxy should shut down");
    upstream.shutdown().await;
}
