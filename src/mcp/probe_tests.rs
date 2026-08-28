use std::collections::VecDeque;
use std::future;
use std::io::{self, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::OutputStyle;
use crate::mcp::GatewayTopology;
use crate::mcp::backend_identity::BackendIdentity;

use super::{ProbeConfig, ProbeRequest, ProbeResponse, ProbeTransport, run_probe};

const INITIALIZE_ID: u64 = 1;
const TOOLS_LIST_ID: u64 = 2;
const TOOL_CALL_ID: u64 = 3;
const NEGOTIATED_VERSION: &str = "2025-11-25";

enum Reply {
    Ready(Result<ProbeResponse>),
    Pending,
}

struct FakeTransport {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<ProbeRequest>>,
}

impl FakeTransport {
    fn new(responses: impl IntoIterator<Item = ProbeResponse>) -> Self {
        Self::with_replies(
            responses
                .into_iter()
                .map(|response| response.with_backend_identity(BackendIdentity::Dataplane))
                .map(|response| Reply::Ready(Ok(response))),
        )
    }

    fn with_replies(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ProbeRequest> {
        self.requests
            .lock()
            .expect("captured probe requests lock should not be poisoned")
            .clone()
    }
}

impl ProbeTransport for FakeTransport {
    async fn post(&self, request: ProbeRequest) -> Result<ProbeResponse> {
        self.requests
            .lock()
            .expect("captured probe requests lock should not be poisoned")
            .push(request);
        let reply = self
            .replies
            .lock()
            .expect("scripted probe responses lock should not be poisoned")
            .pop_front()
            .unwrap_or_else(|| Reply::Ready(Err(anyhow!("unexpected probe request"))));
        match reply {
            Reply::Ready(response) => response,
            Reply::Pending => future::pending().await,
        }
    }
}

fn config() -> ProbeConfig {
    ProbeConfig {
        mode: GatewayTopology::Dataplane,
        base_url: "http://127.0.0.1:8080/".to_owned(),
        server_id: "server-123".to_owned(),
        bearer_token: "secret-token".to_owned(),
        config_timeout: Duration::ZERO,
        retry_interval: Duration::ZERO,
        request_timeout: Duration::from_secs(1),
        protocol_version: "2025-11-25".to_owned(),
        output_style: OutputStyle::plain(),
    }
}

#[tokio::test]
async fn controlplane_mode_uses_the_public_raw_mcp_route() {
    let transport = FakeTransport::new([
        ProbeResponse::new(401, None, None),
        initialize_success(Some("session-controlplane")),
        initialized_success(),
        tools_success(json!([{"name": "custom_tool"}])),
    ]);
    let mut configured = config();
    configured.mode = GatewayTopology::Direct;
    configured.output_style = OutputStyle::colored();
    let mut output = Vec::new();

    run_probe(&transport, &configured, &mut output)
        .await
        .expect("control-plane probe should succeed");

    assert!(
        transport
            .requests()
            .iter()
            .all(|request| request.url == "http://127.0.0.1:8080/mcp")
    );
    let output = String::from_utf8(output).expect("probe output should be UTF-8");
    assert!(output.contains("\x1b[32m        PASS\x1b[0m auth_negative"));
    assert!(output.contains("\x1b[33m        SKIP\x1b[0m tool_call"));
}

#[tokio::test]
async fn dataplane_probe_rejects_a_response_without_backend_identity() {
    let transport = FakeTransport::with_replies(
        [
            ProbeResponse::new(401, None, None),
            initialize_success(Some("session-abc")),
            initialized_success(),
            tools_success(json!([{"name": "custom_tool"}])),
        ]
        .into_iter()
        .map(|response| Reply::Ready(Ok(response))),
    );
    let mut output = Vec::new();

    let error = run_probe(&transport, &config(), &mut output)
        .await
        .expect_err("every dataplane probe response must identify the dataplane backend");

    assert!(error.to_string().contains("backend marker"), "{error}");
}

fn response(status: u16, session_id: Option<&str>, message: Value) -> ProbeResponse {
    ProbeResponse::new(status, session_id.map(str::to_owned), Some(message))
}

fn initialize_success(session_id: Option<&str>) -> ProbeResponse {
    initialize_success_with_version(session_id, NEGOTIATED_VERSION)
}

fn initialize_success_with_version(
    session_id: Option<&str>,
    protocol_version: &str,
) -> ProbeResponse {
    response(
        200,
        session_id,
        json!({
            "jsonrpc": "2.0",
            "id": INITIALIZE_ID,
            "result": {"protocolVersion": protocol_version}
        }),
    )
}

fn initialized_success() -> ProbeResponse {
    ProbeResponse::new(202, None, None)
}

fn tools_success(tools: Value) -> ProbeResponse {
    response(
        200,
        None,
        json!({"jsonrpc": "2.0", "id": TOOLS_LIST_ID, "result": {"tools": tools}}),
    )
}

fn call_success() -> ProbeResponse {
    response(
        200,
        None,
        json!({
            "jsonrpc": "2.0",
            "id": TOOL_CALL_ID,
            "result": {"content": [], "isError": false}
        }),
    )
}

fn discover_success() -> ProbeResponse {
    response(
        200,
        None,
        json!({
            "jsonrpc": "2.0",
            "id": INITIALIZE_ID,
            "result": {
                "supportedVersions": ["2026-07-28"],
                "capabilities": {"tools": {}},
                "resultType": "complete",
                "cacheScope": "private",
                "ttlMs": 0
            }
        }),
    )
}

#[tokio::test]
async fn happy_path_uses_public_route_auth_session_and_deterministic_ids() {
    let transport = FakeTransport::new([
        ProbeResponse::new(401, None, None),
        initialize_success(Some("session-abc")),
        initialized_success(),
        tools_success(json!([{"name": "fast_time_echo"}])),
        call_success(),
    ]);
    let mut output = Vec::new();

    run_probe(&transport, &config(), &mut output)
        .await
        .expect("happy probe flow should succeed");

    let requests = transport.requests();
    assert_eq!(requests.len(), 5);
    assert!(
        requests
            .iter()
            .all(|request| request.url == "http://127.0.0.1:8080/servers/server-123/mcp")
    );
    assert_eq!(requests[0].payload["method"], "initialize");
    assert_eq!(requests[0].payload["id"], INITIALIZE_ID);
    assert_eq!(
        requests[0].payload["params"]["protocolVersion"],
        "2025-11-25"
    );
    assert_eq!(requests[0].bearer_token, None);
    assert_eq!(requests[0].session_id, None);
    assert_eq!(requests[0].protocol_version, None);
    assert_eq!(requests[1].payload["id"], INITIALIZE_ID);
    assert_eq!(requests[1].bearer_token.as_deref(), Some("secret-token"));
    assert_eq!(requests[1].session_id, None);
    assert_eq!(requests[1].protocol_version, None);
    assert_eq!(requests[2].payload["method"], "notifications/initialized");
    assert!(requests[2].payload.get("id").is_none());
    assert_eq!(requests[2].session_id.as_deref(), Some("session-abc"));
    assert_eq!(
        requests[2].protocol_version.as_deref(),
        Some(NEGOTIATED_VERSION)
    );
    assert_eq!(requests[3].payload["method"], "tools/list");
    assert_eq!(requests[3].payload["id"], TOOLS_LIST_ID);
    assert_eq!(requests[3].session_id.as_deref(), Some("session-abc"));
    assert_eq!(requests[4].payload["method"], "tools/call");
    assert_eq!(requests[4].payload["id"], TOOL_CALL_ID);
    assert_eq!(
        requests[4].payload["params"],
        json!({
            "name": "fast_time_echo",
            "arguments": {"message": "cf-integration"}
        })
    );
    assert_eq!(requests[4].session_id.as_deref(), Some("session-abc"));
    assert!(
        requests[2..]
            .iter()
            .all(|request| request.protocol_version.as_deref() == Some(NEGOTIATED_VERSION))
    );

    let output = String::from_utf8(output).expect("probe output should be UTF-8");
    assert!(output.contains("probe url: http://127.0.0.1:8080/servers/server-123/mcp"));
    assert!(output.contains("PASS auth_negative status=401"));
    assert!(output.contains("PASS initialize status=200 session=present"));
    assert!(output.contains("PASS initialized status=202"));
    assert!(!output.contains("session-abc"));
    assert!(output.contains("PASS tools_list count=1"));
    assert!(output.contains("tool=fast_time_echo"));
    assert!(output.contains("PASS tool_call tool=fast_time_echo"));
}

#[tokio::test]
async fn forbidden_unauthenticated_response_is_accepted_as_auth_rejection() {
    let transport = FakeTransport::new([
        ProbeResponse::new(403, None, None),
        initialize_success(Some("session-forbidden")),
        initialized_success(),
        tools_success(json!([{"name": "custom_tool"}])),
    ]);
    let mut output = Vec::new();

    run_probe(&transport, &config(), &mut output)
        .await
        .expect("403 should be accepted as an unauthenticated rejection");

    let output = String::from_utf8(output).expect("probe output should be UTF-8");
    assert!(output.contains("PASS auth_negative status=403"));
}

#[tokio::test]
async fn stateless_happy_path_uses_discovery_request_metadata_and_no_session() {
    let transport = FakeTransport::new([
        ProbeResponse::new(401, None, None),
        discover_success(),
        tools_success(json!([{"name": "fast_time_echo"}])),
        call_success(),
    ]);
    let mut configured = config();
    configured.protocol_version = "2026-07-28".to_owned();
    let mut output = Vec::new();

    run_probe(&transport, &configured, &mut output)
        .await
        .expect("stateless probe flow should succeed");

    let requests = transport.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].payload["method"], "server/discover");
    assert_eq!(requests[1].payload["method"], "server/discover");
    assert_eq!(requests[2].payload["method"], "tools/list");
    assert_eq!(requests[3].payload["method"], "tools/call");
    assert_eq!(requests[0].bearer_token, None);
    assert!(requests.iter().all(|request| request.session_id.is_none()
        && request.protocol_version.as_deref() == Some("2026-07-28")
        && request.payload["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"]
            == "2026-07-28"));
    assert_eq!(requests[3].payload["params"]["name"], "fast_time_echo");
    let output = String::from_utf8(output).expect("probe output should be UTF-8");
    assert!(output.contains("PASS server_discover status=200 lifecycle=stateless"));
    assert!(!output.contains("PASS initialize"));
    assert!(!output.contains("PASS initialized"));
}

#[tokio::test]
async fn requested_version_drives_initialize_payload_and_negotiated_version_drives_headers() {
    let transport = FakeTransport::new([
        ProbeResponse::new(401, None, None),
        initialize_success_with_version(Some("session-version"), "2025-11-25"),
        initialized_success(),
        tools_success(json!([{"name": "custom_tool"}])),
    ]);
    let mut configured = config();
    configured.protocol_version = "2025-06-18".to_owned();
    let mut output = Vec::new();

    run_probe(&transport, &configured, &mut output)
        .await
        .expect("explicit supported protocol version should be used");

    let requests = transport.requests();
    assert_eq!(
        requests[0].payload["params"]["protocolVersion"],
        "2025-06-18"
    );
    assert_eq!(requests[0].protocol_version, None);
    assert_eq!(requests[1].protocol_version, None);
    assert!(
        requests[2..]
            .iter()
            .all(|request| request.protocol_version.as_deref() == Some("2025-11-25"))
    );
}

#[tokio::test]
async fn authenticated_initialize_retries_transient_statuses_until_success() {
    let transport = FakeTransport::new([
        ProbeResponse::new(401, None, None),
        ProbeResponse::new(404, None, None),
        ProbeResponse::new(503, None, None),
        initialize_success(Some("session-retry")),
        initialized_success(),
        tools_success(json!([{"name": "unknown_tool"}])),
    ]);
    let mut retrying_config = config();
    retrying_config.config_timeout = Duration::from_secs(1);
    let mut output = Vec::new();

    run_probe(&transport, &retrying_config, &mut output)
        .await
        .expect("initialize should retry until the scripted success");

    assert_eq!(transport.requests().len(), 6);
    let output = String::from_utf8(output).expect("probe output should be UTF-8");
    assert_eq!(output.matches("RETRY initialize").count(), 2);
}

#[tokio::test]
async fn zero_config_timeout_still_runs_one_authenticated_initialize_attempt() {
    let transport = FakeTransport::new([
        ProbeResponse::new(401, None, None),
        ProbeResponse::new(503, None, None),
        initialize_success(Some("must-not-be-used")),
    ]);
    let mut output = Vec::new();

    let error = run_probe(&transport, &config(), &mut output)
        .await
        .expect_err("zero config timeout should not retry a failed initialize");

    assert!(error.to_string().contains("initialize"));
    assert!(error.to_string().contains("status=503"));
    assert_eq!(transport.requests().len(), 2);
}

struct AlwaysUnavailableTransport {
    attempts: AtomicUsize,
}

impl ProbeTransport for AlwaysUnavailableTransport {
    async fn post(&self, request: ProbeRequest) -> Result<ProbeResponse> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if request.bearer_token.is_none() {
            Ok(ProbeResponse::new(401, None, None)
                .with_backend_identity(BackendIdentity::Dataplane))
        } else {
            Ok(ProbeResponse::new(503, None, None)
                .with_backend_identity(BackendIdentity::Dataplane))
        }
    }
}

#[tokio::test]
async fn zero_retry_interval_does_not_busy_loop() {
    let transport = AlwaysUnavailableTransport {
        attempts: AtomicUsize::new(0),
    };
    let mut bounded_config = config();
    bounded_config.config_timeout = Duration::from_millis(35);
    bounded_config.request_timeout = Duration::from_millis(100);
    let mut output = Vec::new();

    let error = run_probe(&transport, &bounded_config, &mut output)
        .await
        .expect_err("unavailable initialize should reach the config deadline");

    assert!(error.to_string().contains("status=503"));
    let attempts = transport.attempts.load(Ordering::SeqCst);
    assert!(
        (2..=8).contains(&attempts),
        "unexpected attempt count: {attempts}"
    );
}

#[tokio::test]
async fn every_transport_step_is_bounded_by_the_request_timeout() {
    let steps = [
        "auth_negative",
        "initialize",
        "initialized",
        "tools_list",
        "tool_call",
    ];

    for (pending_index, expected_step) in steps.into_iter().enumerate() {
        let mut replies = vec![
            Reply::Ready(Ok(ProbeResponse::new(401, None, None)
                .with_backend_identity(BackendIdentity::Dataplane))),
            Reply::Ready(Ok(initialize_success(Some("session-timeout"))
                .with_backend_identity(BackendIdentity::Dataplane))),
            Reply::Ready(Ok(
                initialized_success().with_backend_identity(BackendIdentity::Dataplane)
            )),
            Reply::Ready(Ok(tools_success(json!([{"name": "echo"}]))
                .with_backend_identity(BackendIdentity::Dataplane))),
            Reply::Ready(Ok(
                call_success().with_backend_identity(BackendIdentity::Dataplane)
            )),
        ];
        replies[pending_index] = Reply::Pending;
        let transport = FakeTransport::with_replies(replies);
        let mut timeout_config = config();
        timeout_config.config_timeout = Duration::from_secs(1);
        timeout_config.request_timeout = Duration::from_millis(5);
        let mut output = Vec::new();

        let guarded = tokio::time::timeout(
            Duration::from_millis(100),
            run_probe(&transport, &timeout_config, &mut output),
        )
        .await;

        let result = guarded.expect("probe must enforce its own per-request timeout");
        let error = result.expect_err("a pending transport call must time out");
        assert!(
            error.to_string().contains(expected_step),
            "unexpected timeout error: {error}"
        );
        assert!(error.to_string().contains("timed out"));
        assert_eq!(transport.requests().len(), pending_index + 1);
    }
}

#[tokio::test]
async fn unauthenticated_initialize_must_return_authentication_rejection() {
    let transport = FakeTransport::new([ProbeResponse::new(200, None, None)]);
    let mut output = Vec::new();

    let error = run_probe(&transport, &config(), &mut output)
        .await
        .expect_err("an unprotected public route should fail the probe");

    assert!(error.to_string().contains("expected 401 or 403"));
    assert!(error.to_string().contains("got 200"));
    assert_eq!(transport.requests().len(), 1);
}

#[tokio::test]
async fn authenticated_initialize_requires_a_session_id() {
    let transport = FakeTransport::new([
        ProbeResponse::new(401, None, None),
        initialize_success(None),
    ]);
    let mut output = Vec::new();

    let error = run_probe(&transport, &config(), &mut output)
        .await
        .expect_err("a missing MCP session ID should fail the probe");

    assert!(error.to_string().contains("Mcp-Session-Id"));
    assert_eq!(transport.requests().len(), 2);
}

#[tokio::test]
async fn initialize_requires_a_nonempty_negotiated_protocol_version() {
    for protocol_version in [Value::Null, json!(""), json!("   "), json!(7)] {
        let transport = FakeTransport::new([
            ProbeResponse::new(401, None, None),
            response(
                200,
                Some("session-version"),
                json!({
                    "jsonrpc": "2.0",
                    "id": INITIALIZE_ID,
                    "result": {"protocolVersion": protocol_version}
                }),
            ),
        ]);
        let mut output = Vec::new();

        let error = run_probe(&transport, &config(), &mut output)
            .await
            .expect_err("a missing or malformed negotiated version must fail initialize");

        assert!(error.to_string().contains("negotiated protocolVersion"));
        assert_eq!(transport.requests().len(), 2);
    }
}

#[tokio::test]
async fn initialized_notification_requires_http_202() {
    let transport = FakeTransport::new([
        ProbeResponse::new(401, None, None),
        initialize_success(Some("session-initialized-status")),
        ProbeResponse::new(200, None, None),
    ]);
    let mut output = Vec::new();

    let error = run_probe(&transport, &config(), &mut output)
        .await
        .expect_err("an initialized notification response other than 202 must fail");

    assert!(error.to_string().contains("initialized=FAIL status=200"));
    assert_eq!(transport.requests().len(), 3);
}

#[tokio::test]
async fn initialized_notification_requires_an_empty_response() {
    let transport = FakeTransport::new([
        ProbeResponse::new(401, None, None),
        initialize_success(Some("session-initialized-body")),
        response(
            202,
            None,
            json!({"jsonrpc": "2.0", "id": 99, "result": {"secret": "not-logged"}}),
        ),
    ]);
    let mut output = Vec::new();

    let error = run_probe(&transport, &config(), &mut output)
        .await
        .expect_err("an initialized notification response body must fail");
    let error = error.to_string();

    assert!(error.contains("initialized=FAIL expected an empty response"));
    assert!(!error.contains("not-logged"));
    assert_eq!(transport.requests().len(), 3);
}

#[tokio::test]
async fn json_rpc_version_must_be_exactly_2_0() {
    let transport = FakeTransport::new([
        ProbeResponse::new(401, None, None),
        response(
            200,
            Some("session-version"),
            json!({"jsonrpc": "1.0", "id": INITIALIZE_ID, "result": {}}),
        ),
    ]);
    let mut output = Vec::new();

    let error = run_probe(&transport, &config(), &mut output)
        .await
        .expect_err("a response with the wrong JSON-RPC version must fail");

    assert!(error.to_string().contains("invalid JSON-RPC version"));
}

#[tokio::test]
async fn json_rpc_response_id_must_match_the_request() {
    let transport = FakeTransport::new([
        ProbeResponse::new(401, None, None),
        response(
            200,
            Some("session-id"),
            json!({"jsonrpc": "2.0", "id": 999, "result": {}}),
        ),
    ]);
    let mut output = Vec::new();

    let error = run_probe(&transport, &config(), &mut output)
        .await
        .expect_err("a response with the wrong ID must fail");

    assert!(error.to_string().contains("response ID mismatch"));
}

#[tokio::test]
async fn json_rpc_errors_do_not_leak_response_payloads() {
    let transport = FakeTransport::new([
        ProbeResponse::new(401, None, None),
        response(
            200,
            Some("session-error"),
            json!({
                "jsonrpc": "2.0",
                "id": INITIALIZE_ID,
                "error": {"code": -32600, "message": "secret\nforged=PASS"}
            }),
        ),
    ]);
    let mut output = Vec::new();

    let error = run_probe(&transport, &config(), &mut output)
        .await
        .expect_err("a JSON-RPC error response should fail initialize");
    let error = error.to_string();

    assert!(error.contains("initialize"));
    assert!(error.contains("JSON-RPC error"));
    assert!(!error.contains("secret"));
    assert!(!error.contains('\n'));
}

#[tokio::test]
async fn tools_list_requires_at_least_one_tool() {
    let transport = FakeTransport::new([
        ProbeResponse::new(401, None, None),
        initialize_success(Some("session-empty")),
        initialized_success(),
        tools_success(json!([])),
    ]);
    let mut output = Vec::new();

    let error = run_probe(&transport, &config(), &mut output)
        .await
        .expect_err("an empty tools list should fail the probe");

    assert!(error.to_string().contains("no tools returned"));
}

#[tokio::test]
async fn every_tool_entry_requires_a_nonempty_string_name() {
    for tools in [
        json!([{}]),
        json!([{"name": ""}]),
        json!([{"name": "   "}]),
        json!([7]),
    ] {
        let transport = FakeTransport::new([
            ProbeResponse::new(401, None, None),
            initialize_success(Some("session-invalid-tool")),
            initialized_success(),
            tools_success(tools),
        ]);
        let mut output = Vec::new();

        let error = run_probe(&transport, &config(), &mut output)
            .await
            .expect_err("a malformed tool entry must fail the probe");

        assert!(error.to_string().contains("nonempty name"));
    }
}

#[tokio::test]
async fn tool_call_result_is_error_fails_without_leaking_content() {
    let transport = FakeTransport::new([
        ProbeResponse::new(401, None, None),
        initialize_success(Some("session-tool-error")),
        initialized_success(),
        tools_success(json!([{"name": "get_system_time"}])),
        response(
            200,
            None,
            json!({
                "jsonrpc": "2.0",
                "id": TOOL_CALL_ID,
                "result": {
                    "content": [{"type": "text", "text": "secret\nforged=PASS"}],
                    "isError": true
                }
            }),
        ),
    ]);
    let mut output = Vec::new();

    let error = run_probe(&transport, &config(), &mut output)
        .await
        .expect_err("a tool-level error should fail the probe");
    let error = error.to_string();

    assert!(error.contains("tool returned error"));
    assert!(!error.contains("secret"));
    assert!(!error.contains('\n'));
}

#[tokio::test]
async fn tool_call_result_requires_a_content_array() {
    for result in [json!({}), json!({"content": "not-an-array"})] {
        let transport = FakeTransport::new([
            ProbeResponse::new(401, None, None),
            initialize_success(Some("session-content")),
            initialized_success(),
            tools_success(json!([{"name": "echo"}])),
            response(
                200,
                None,
                json!({"jsonrpc": "2.0", "id": TOOL_CALL_ID, "result": result}),
            ),
        ]);
        let mut output = Vec::new();

        let error = run_probe(&transport, &config(), &mut output)
            .await
            .expect_err("missing or malformed content must fail the probe");

        assert!(error.to_string().contains("content array"));
    }
}

#[tokio::test]
async fn tool_call_is_error_must_be_boolean_when_present() {
    let transport = FakeTransport::new([
        ProbeResponse::new(401, None, None),
        initialize_success(Some("session-is-error")),
        initialized_success(),
        tools_success(json!([{"name": "echo"}])),
        response(
            200,
            None,
            json!({
                "jsonrpc": "2.0",
                "id": TOOL_CALL_ID,
                "result": {"content": [], "isError": "false"}
            }),
        ),
    ]);
    let mut output = Vec::new();

    let error = run_probe(&transport, &config(), &mut output)
        .await
        .expect_err("a non-boolean isError value must fail the probe");

    assert!(error.to_string().contains("isError must be a boolean"));
}

#[tokio::test]
async fn malicious_suffix_tool_is_not_called() {
    let transport = FakeTransport::new([
        ProbeResponse::new(401, None, None),
        initialize_success(Some("session-skip")),
        initialized_success(),
        tools_success(json!([{"name": "delete_everything_echo"}])),
    ]);
    let mut output = Vec::new();

    run_probe(&transport, &config(), &mut output)
        .await
        .expect("an untrusted suffix match should only skip the optional call");

    assert_eq!(transport.requests().len(), 4);
    let output = String::from_utf8(output).expect("probe output should be UTF-8");
    assert!(output.contains("SKIP tool_call no echo/get_system_time tool available"));
}

#[tokio::test]
async fn server_id_is_percent_encoded_as_one_url_path_segment() {
    let transport = FakeTransport::new([ProbeResponse::new(200, None, None)]);
    let mut encoded_config = config();
    encoded_config.server_id = "tenant/server?x# y".to_owned();
    let mut output = Vec::new();

    run_probe(&transport, &encoded_config, &mut output)
        .await
        .expect_err("the scripted authentication response should stop the probe");

    assert_eq!(
        transport.requests()[0].url,
        "http://127.0.0.1:8080/servers/tenant%2Fserver%3Fx%23%20y/mcp"
    );
}

#[tokio::test]
async fn tool_names_are_sanitized_before_writing_output() {
    let transport = FakeTransport::new([
        ProbeResponse::new(401, None, None),
        initialize_success(Some("session-output")),
        initialized_success(),
        tools_success(json!([{"name": "custom\nforged=PASS\r"}])),
    ]);
    let mut output = Vec::new();

    run_probe(&transport, &config(), &mut output)
        .await
        .expect("an unknown tool should complete after sanitizing its name");

    let output = String::from_utf8(output).expect("probe output should be UTF-8");
    assert!(output.contains("tool=custom\\nforged=PASS\\r"));
    assert!(!output.contains("tool=custom\nforged=PASS"));
}

#[test]
fn debug_output_redacts_tokens_sessions_and_payloads() {
    let config_debug = format!("{:?}", config());
    let request_debug = format!(
        "{:?}",
        ProbeRequest {
            url: "https://example.test/servers/id/mcp".to_owned(),
            payload: json!({"method": "initialize", "secret": "payload-secret"}),
            bearer_token: Some("request-secret".to_owned()),
            session_id: Some("session-secret".to_owned()),
            protocol_version: Some("2025-11-25".to_owned()),
        }
    );
    let response_debug = format!(
        "{:?}",
        response(
            200,
            Some("response-session-secret"),
            json!({"result": {"secret": "response-payload-secret"}}),
        )
    );

    for secret in [
        "secret-token",
        "request-secret",
        "session-secret",
        "payload-secret",
        "response-session-secret",
        "response-payload-secret",
    ] {
        assert!(!config_debug.contains(secret));
        assert!(!request_debug.contains(secret));
        assert!(!response_debug.contains(secret));
    }
    assert!(config_debug.contains("<redacted>"));
    assert!(request_debug.contains("<redacted>"));
    assert!(response_debug.contains("<redacted>"));
}

struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("writer-secret\nforged=PASS"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn writer_failure_stops_before_any_transport_request_without_leaking_details() {
    let transport = FakeTransport::new([]);
    let mut output = FailingWriter;

    let error = run_probe(&transport, &config(), &mut output)
        .await
        .expect_err("a writer failure must stop the probe");
    let error = error.to_string();

    assert!(error.contains("failed to write probe URL"));
    assert!(!error.contains("writer-secret"));
    assert!(!error.contains('\n'));
    assert!(transport.requests().is_empty());
}
