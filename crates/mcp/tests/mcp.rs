use cf_integration_mcp::mcp::{
    ACCEPT, PROTOCOL_VERSION, STATELESS_PROTOCOL_VERSION, initialize, initialize_with_id,
    initialize_with_id_and_version, is_stateless_protocol, jsonrpc, jsonrpc_with_id,
    parse_mcp_body, routing_name, stateless_jsonrpc_with_id, tool_call_args,
};
use serde_json::{Value, json};
use uuid::Uuid;

#[test]
fn protocol_constants_match_the_streamable_http_contract() {
    assert_eq!(PROTOCOL_VERSION, "2025-11-25");
    assert_eq!(STATELESS_PROTOCOL_VERSION, "2026-07-28");
    assert_eq!(ACCEPT, "application/json, text/event-stream");
}

#[test]
fn stateless_requests_carry_complete_metadata_and_preserve_existing_params() {
    let request = stateless_jsonrpc_with_id(
        "tools/call",
        Some(json!({
            "name": "echo",
            "arguments": {"message": "hello"},
            "_meta": {"extension.example/trace": "trace-1"}
        })),
        json!(9),
        STATELESS_PROTOCOL_VERSION,
    );

    assert_eq!(request["method"], "tools/call");
    assert_eq!(request["params"]["name"], "echo");
    assert_eq!(
        request["params"]["_meta"],
        json!({
            "extension.example/trace": "trace-1",
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {
                "name": "cf-integration",
                "version": "1.0"
            },
            "io.modelcontextprotocol/clientCapabilities": {}
        })
    );
    assert_eq!(
        routing_name("tools/call", request.get("params")),
        Some("echo")
    );
    assert!(is_stateless_protocol("2026-07-28"));
    assert!(is_stateless_protocol("2027-01-01"));
    assert!(!is_stateless_protocol("2025-11-25"));
}

#[test]
fn deterministic_jsonrpc_omits_absent_params() {
    assert_eq!(
        jsonrpc_with_id("ping", None, json!("request-1")),
        json!({
            "jsonrpc": "2.0",
            "id": "request-1",
            "method": "ping"
        })
    );
}

#[test]
fn deterministic_jsonrpc_preserves_explicit_id_and_params() {
    assert_eq!(
        jsonrpc_with_id("tools/list", Some(json!({})), json!(7)),
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/list",
            "params": {}
        })
    );
}

#[test]
fn production_jsonrpc_uses_a_v4_uuid_string_id() {
    let request = jsonrpc("ping", None);

    let id = request["id"].as_str().expect("request ID must be a string");
    let id = Uuid::parse_str(id).expect("request ID must be a UUID");
    assert_eq!(id.get_version_num(), 4);
    assert_eq!(request["jsonrpc"], "2.0");
    assert_eq!(request["method"], "ping");
    assert!(request.get("params").is_none());
}

#[test]
fn deterministic_initialize_has_exact_client_payload() {
    assert_eq!(
        initialize_with_id(json!("init-1")),
        json!({
            "jsonrpc": "2.0",
            "id": "init-1",
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {
                    "name": "cf-integration",
                    "version": "1.0"
                }
            }
        })
    );
}

#[test]
fn deterministic_initialize_accepts_an_explicit_protocol_version() {
    assert_eq!(
        initialize_with_id_and_version(json!(17), "2025-06-18"),
        json!({
            "jsonrpc": "2.0",
            "id": 17,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {
                    "name": "cf-integration",
                    "version": "1.0"
                }
            }
        })
    );
}

#[test]
fn production_initialize_uses_a_v4_uuid_string_id() {
    let request = initialize();

    let id = request["id"].as_str().expect("request ID must be a string");
    let id = Uuid::parse_str(id).expect("request ID must be a UUID");
    assert_eq!(id.get_version_num(), 4);
    assert_eq!(request["method"], "initialize");
}

#[test]
fn empty_json_body_has_no_message() {
    assert_eq!(
        parse_mcp_body("", "application/json").expect("empty body must parse"),
        None
    );
}

#[test]
fn json_body_returns_the_parsed_message() {
    let body = r#"{"jsonrpc":"2.0","id":"1","result":{"tools":[]}}"#;

    assert_eq!(
        parse_mcp_body(body, "application/json; charset=utf-8").expect("JSON body must parse"),
        Some(json!({
            "jsonrpc": "2.0",
            "id": "1",
            "result": {"tools": []}
        }))
    );
}

#[test]
fn jsonrpc_error_body_is_preserved_as_a_message() {
    let body = r#"{"jsonrpc":"2.0","id":"1","error":{"code":-32600,"message":"bad request"}}"#;

    assert_eq!(
        parse_mcp_body(body, "application/json").expect("JSON-RPC error must parse"),
        Some(json!({
            "jsonrpc": "2.0",
            "id": "1",
            "error": {"code": -32600, "message": "bad request"}
        }))
    );
}

#[test]
fn malformed_json_body_returns_a_parse_error() {
    let result = parse_mcp_body("{not-json", "application/json");

    assert!(result.is_err());
}

#[test]
fn sse_returns_last_valid_blank_delimited_event() {
    let body = concat!(
        "event: message\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":\"first\",\"result\":1}\n",
        "\n",
        "data: not-json\n",
        "\n",
        ": keep-alive\n",
        "ignored: {\"id\":\"ignored\"}\n",
        "data:{\"jsonrpc\":\"2.0\",\"id\":\"last\",\"result\":2}\n",
        "\n",
        "data: also-not-json\n",
        "\n",
    );

    assert_eq!(
        parse_mcp_body(body, "text/event-stream; charset=utf-8").expect("SSE body must parse"),
        Some(json!({
            "jsonrpc": "2.0",
            "id": "last",
            "result": 2
        }))
    );
}

#[test]
fn sse_without_a_valid_data_message_has_no_message() {
    let body = "event: message\ndata: not-json\n\ndata: \n\n";

    assert_eq!(
        parse_mcp_body(body, "text/event-stream").expect("SSE body must parse"),
        None
    );
}

#[test]
fn sse_joins_multiline_data_fields_and_ignores_other_fields() {
    let body = concat!(
        "data: {\n",
        ": comment inside the event\n",
        "event: message\n",
        "data:   \"jsonrpc\": \"2.0\",\n",
        "data:   \"id\": \"multiline\",\n",
        "retry: 1000\n",
        "data:   \"result\": {\"ok\": true}\n",
        "data: }\n",
        "\n",
    );

    assert_eq!(
        parse_mcp_body(body, "text/event-stream").expect("SSE body must parse"),
        Some(json!({
            "jsonrpc": "2.0",
            "id": "multiline",
            "result": {"ok": true}
        }))
    );
}

#[test]
fn sse_handles_crlf_and_flushes_the_last_event_at_eof() {
    let body = concat!(
        "data: {\r\n",
        "data:   \"jsonrpc\": \"2.0\",\r\n",
        "data:   \"id\": \"eof\",\r\n",
        "data:   \"result\": 7\r\n",
        "data: }",
    );

    assert_eq!(
        parse_mcp_body(body, "text/event-stream").expect("SSE body must parse"),
        Some(json!({
            "jsonrpc": "2.0",
            "id": "eof",
            "result": 7
        }))
    );
}

#[test]
fn sse_ignores_a_malformed_event_before_a_valid_event() {
    let body = concat!(
        "data: {not-json\n",
        "data: still-not-json}\n",
        "\n",
        "data: {\n",
        "data:   \"jsonrpc\": \"2.0\",\n",
        "data:   \"id\": \"valid\",\n",
        "data:   \"result\": true\n",
        "data: }\n",
        "\n",
    );

    assert_eq!(
        parse_mcp_body(body, "text/event-stream").expect("SSE body must parse"),
        Some(json!({
            "jsonrpc": "2.0",
            "id": "valid",
            "result": true
        }))
    );
}

#[test]
fn sse_media_type_matching_is_case_insensitive() {
    let body = "data: {\"jsonrpc\":\"2.0\",\"id\":\"mixed-case\",\"result\":1}\n\n";

    assert_eq!(
        parse_mcp_body(body, " Text/Event-Stream; charset=utf-8 ")
            .expect("mixed-case SSE media type must parse"),
        Some(json!({
            "jsonrpc": "2.0",
            "id": "mixed-case",
            "result": 1
        }))
    );
}

#[test]
fn sse_media_type_matching_rejects_supersets() {
    let body = "data: {\"jsonrpc\":\"2.0\",\"id\":\"not-sse\",\"result\":1}\n\n";

    assert!(parse_mcp_body(body, "application/x-text/event-stream").is_err());
}

#[test]
fn exact_echo_names_select_the_known_echo_arguments() {
    for name in ["echo", "fast_time_echo", "fast-time-echo"] {
        assert_eq!(
            tool_call_args(name),
            Some(json!({"message": "cf-integration"}))
        );
    }
}

#[test]
fn echo_suffix_does_not_select_an_untrusted_tool() {
    assert_eq!(tool_call_args("delete_everything_echo"), None::<Value>);
}

#[test]
fn exact_system_time_names_select_utc_arguments() {
    for name in [
        "get_system_time",
        "get-system-time",
        "fast_time_get_system_time",
        "fast-time-get_system_time",
        "fast-time-get-system-time",
    ] {
        assert_eq!(tool_call_args(name), Some(json!({"timezone": "UTC"})));
    }
}

#[test]
fn system_time_suffix_does_not_select_an_untrusted_tool() {
    assert_eq!(tool_call_args("clock.get_system_time"), None::<Value>);
}

#[test]
fn unknown_tool_has_no_known_arguments() {
    assert_eq!(tool_call_args("namespace.counter"), None::<Value>);
}
