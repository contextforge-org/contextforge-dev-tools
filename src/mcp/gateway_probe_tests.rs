use std::sync::{Arc, Mutex};

use crate::mcp::backend_identity::is_dataplane_endpoint;
use crate::mcp::probe::{ProbeRequest, ProbeResponse, ProbeTransport};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::any;

use super::{GatewayClient, MAX_RESPONSE_BODY_BYTES};
use serde_json::json;
use tokio::net::TcpListener;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<(HeaderMap, Bytes)>>>);

async fn json_handler(State(capture): State<Capture>, request: Request) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .expect("request body should fit");
    capture
        .0
        .lock()
        .expect("capture lock")
        .push((parts.headers, body));
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json; charset=utf-8")
        .header("mcp-session-id", "session-from-server")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#,
        ))
        .expect("response")
}

async fn server(router: Router) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("address");
    let (shutdown, receiver) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = receiver.await;
            })
            .await
            .expect("test server");
    });
    (format!("http://{address}/mcp"), shutdown)
}

fn request(url: String) -> ProbeRequest {
    ProbeRequest {
        url,
        payload: json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
        bearer_token: Some("test-secret-token".to_owned()),
        session_id: Some("client-session".to_owned()),
        protocol_version: Some("2025-11-25".to_owned()),
    }
}

async fn send(request: ProbeRequest) -> anyhow::Result<ProbeResponse> {
    let endpoint = url::Url::parse(&request.url)?;
    let mode = if is_dataplane_endpoint(&endpoint) {
        crate::mcp::GatewayTopology::Dataplane
    } else {
        crate::mcp::GatewayTopology::Direct
    };
    let server_id = endpoint
        .path_segments()
        .and_then(|mut segments| {
            (segments.next() == Some("servers"))
                .then(|| segments.next())
                .flatten()
        })
        .unwrap_or("test")
        .to_owned();
    let mut base_url = endpoint;
    base_url.set_path("/");
    base_url.set_query(None);
    base_url.set_fragment(None);
    let client = GatewayClient::new(mode, base_url.as_str(), &server_id, "test-secret-token")?;
    client.post(request).await
}

#[tokio::test]
async fn sends_exact_mcp_headers_and_parses_json_response() {
    let capture = Capture::default();
    let (url, shutdown) = server(
        Router::new()
            .route("/mcp", any(json_handler))
            .with_state(capture.clone()),
    )
    .await;

    let response = send(request(url)).await.expect("request should succeed");

    assert_eq!(response.status, 200);
    assert_eq!(response.session_id.as_deref(), Some("session-from-server"));
    assert_eq!(
        response.message,
        Some(json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}}))
    );
    let captured = capture.0.lock().expect("capture lock");
    let (headers, body) = &captured[0];
    assert_eq!(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer test-secret-token")
    );
    assert_eq!(
        headers.get("accept").and_then(|value| value.to_str().ok()),
        Some("application/json, text/event-stream")
    );
    assert_eq!(
        headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        headers
            .get("mcp-protocol-version")
            .and_then(|value| value.to_str().ok()),
        Some("2025-11-25")
    );
    assert_eq!(
        headers
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok()),
        Some("client-session")
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(body).expect("JSON body"),
        json!({"jsonrpc":"2.0","id":1,"method":"ping"})
    );
    let _ = shutdown.send(());
}

#[tokio::test]
async fn omits_optional_auth_session_and_protocol_headers() {
    let capture = Capture::default();
    let (url, shutdown) = server(
        Router::new()
            .route("/mcp", any(json_handler))
            .with_state(capture.clone()),
    )
    .await;
    let mut request = request(url);
    request.bearer_token = None;
    request.session_id = None;
    request.protocol_version = None;

    send(request).await.expect("request should succeed");

    let captured = capture.0.lock().expect("capture lock");
    assert!(captured[0].0.get("authorization").is_none());
    assert!(captured[0].0.get("mcp-session-id").is_none());
    assert!(captured[0].0.get("mcp-protocol-version").is_none());
    let _ = shutdown.send(());
}

#[tokio::test]
async fn stateless_requests_send_method_and_target_name_headers() {
    let capture = Capture::default();
    let (url, shutdown) = server(
        Router::new()
            .route("/mcp", any(json_handler))
            .with_state(capture.clone()),
    )
    .await;
    let mut request = request(url);
    request.payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "echo", "arguments": {}}
    });
    request.protocol_version = Some("2026-07-28".to_owned());
    request.session_id = None;

    send(request).await.expect("request should succeed");

    let captured = capture.0.lock().expect("capture lock");
    let headers = &captured[0].0;
    assert_eq!(
        headers
            .get("mcp-method")
            .and_then(|value| value.to_str().ok()),
        Some("tools/call")
    );
    assert_eq!(
        headers
            .get("mcp-name")
            .and_then(|value| value.to_str().ok()),
        Some("echo")
    );
    assert!(headers.get("mcp-session-id").is_none());
    let _ = shutdown.send(());
}

#[tokio::test]
async fn parses_blank_delimited_multiline_sse() {
    async fn sse() -> Response<Body> {
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream; charset=utf-8")
            .body(Body::from(concat!(
                ": heartbeat\n",
                "data: {\n",
                "data: \"jsonrpc\":\"2.0\",\n",
                "data: \"id\":1,\n",
                "data: \"result\":{\"ok\":true}\n",
                "data: }\n\n",
            )))
            .expect("SSE response")
    }
    let (url, shutdown) = server(Router::new().route("/mcp", any(sse))).await;

    let response = send(request(url)).await.expect("SSE should parse");

    assert_eq!(
        response.message,
        Some(json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}}))
    );
    let _ = shutdown.send(());
}

#[tokio::test]
async fn non_success_responses_are_returned_without_parsing_untrusted_bodies() {
    async fn unauthorized() -> Response<Body> {
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("content-type", "text/html")
            .body(Body::from("<secret>not JSON</secret>"))
            .expect("response")
    }
    let (url, shutdown) = server(Router::new().route("/mcp", any(unauthorized))).await;

    let response = send(request(url))
        .await
        .expect("HTTP status should be returned");

    assert_eq!(response.status, 401);
    assert_eq!(response.message, None);
    let _ = shutdown.send(());
}

#[tokio::test]
async fn successful_nonempty_response_requires_mcp_content_type() {
    async fn wrong_type() -> Response<Body> {
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/plain")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#))
            .expect("response")
    }
    let (url, shutdown) = server(Router::new().route("/mcp", any(wrong_type))).await;

    let error = send(request(url))
        .await
        .expect_err("wrong content type must fail");

    assert!(
        error
            .to_string()
            .contains("unsupported MCP response content type")
    );
    let _ = shutdown.send(());
}

#[tokio::test]
async fn response_body_is_bounded() {
    async fn oversized() -> Response<Body> {
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(vec![b'x'; MAX_RESPONSE_BODY_BYTES + 1]))
            .expect("response")
    }
    let (url, shutdown) = server(Router::new().route("/mcp", any(oversized))).await;

    let error = send(request(url))
        .await
        .expect_err("oversized response must fail");

    assert!(error.to_string().contains("safety limit"));
    let _ = shutdown.send(());
}

#[tokio::test]
async fn invalid_sensitive_headers_fail_without_leaking_values() {
    let secret = "token-secret\nforged";
    let mut request = request("http://127.0.0.1:9/mcp".to_owned());
    request.bearer_token = Some(secret.to_owned());

    let error = send(request)
        .await
        .expect_err("invalid token header must fail");

    assert!(!error.to_string().contains(secret));
    assert!(error.to_string().contains("Authorization"));
}

#[derive(Clone)]
struct BackendMarkers(Vec<&'static str>);

async fn backend_handler(State(markers): State<BackendMarkers>) -> Response<Body> {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json");
    for marker in markers.0 {
        builder = builder.header("x-cf-integration-backend", marker);
    }
    builder
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#,
        ))
        .expect("response")
}

fn dataplane_url(raw: &str) -> String {
    let mut url = url::Url::parse(raw).expect("test URL should parse");
    url.set_path("/servers/test/mcp");
    url.into()
}

#[tokio::test]
async fn dataplane_transport_accepts_one_exact_backend_marker() {
    let (url, shutdown) = server(
        Router::new()
            .route("/servers/test/mcp", any(backend_handler))
            .with_state(BackendMarkers(vec!["dataplane"])),
    )
    .await;

    let response = send(request(dataplane_url(&url)))
        .await
        .expect("exact dataplane marker should pass");

    assert_eq!(response.status, 200);
    let _ = shutdown.send(());
}

#[tokio::test]
async fn dataplane_transport_rejects_absent_fallback_forged_and_duplicate_markers_safely() {
    for markers in [
        vec![],
        vec!["controlplane-fallback"],
        vec!["private-forged-marker"],
        vec!["dataplane", "dataplane"],
    ] {
        let (url, shutdown) = server(
            Router::new()
                .route("/servers/test/mcp", any(backend_handler))
                .with_state(BackendMarkers(markers)),
        )
        .await;

        let error = send(request(dataplane_url(&url)))
            .await
            .expect_err("invalid dataplane identity must fail closed");
        let diagnostic = error.to_string();

        assert!(diagnostic.contains("backend marker"), "{diagnostic}");
        assert!(!diagnostic.contains("private-forged-marker"));
        let _ = shutdown.send(());
    }
}
