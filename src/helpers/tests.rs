use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use url::Url;

use super::{auth, config};
use crate::mcp::protocol::{LEGACY_PROTOCOL_VERSION, PROTOCOL_VERSION};

async fn serve(router: Router) -> (Url, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture");
    let url = Url::parse(&format!(
        "http://{}/mcp",
        listener.local_addr().expect("fixture address")
    ))
    .expect("fixture URL");
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve fixture");
    });
    (url, task)
}

#[derive(Clone)]
struct Fixture {
    version: &'static str,
    calls: Arc<Mutex<Vec<(Value, HeaderMap)>>>,
    invalid_tools: Option<Value>,
}

impl Fixture {
    fn router(&self) -> Router {
        Router::new()
            .route("/mcp", post(fixture_rpc).delete(fixture_delete))
            .with_state(self.clone())
    }
}

async fn fixture_rpc(
    State(fixture): State<Fixture>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    fixture
        .calls
        .lock()
        .expect("calls lock")
        .push((body.clone(), headers));
    let result = match body["method"].as_str().expect("method") {
        "initialize" | "server/discover" => json!({"protocolVersion": fixture.version}),
        "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
        "tools/list" => fixture.invalid_tools.unwrap_or_else(|| {
            if body["params"].get("cursor").is_some() {
                json!({"tools": [{"name": "diagnostic", "inputSchema": {
                    "type": "object", "properties": {"value": {"type": "string", "x-mcp-header": "Value"}}
                }}], "nextCursor": null})
            } else {
                json!({"tools": [{"name": "first", "inputSchema": {}}], "nextCursor": ""})
            }
        }),
        "resources/list" => json!({"resources": [{"uri": "test://resource"}]}),
        "resources/templates/list" => json!({"resourceTemplates": [{"uriTemplate": "test://resource/{id}"}]}),
        "prompts/list" => json!({"prompts": [{"name": "prompt"}]}),
        method => panic!("unexpected fixture method {method}"),
    };
    let message = json!({"jsonrpc": "2.0", "id": body["id"], "result": result});
    let sse = fixture.version == LEGACY_PROTOCOL_VERSION || body["params"].get("cursor").is_some();
    let mut response = if sse {
        (
            [("content-type", "text/event-stream")],
            format!("event: message\r\ndata:\r\n\r\ndata: {message}\r\n\r\n"),
        )
            .into_response()
    } else {
        Json(message).into_response()
    };
    if fixture.version == LEGACY_PROTOCOL_VERSION {
        response.headers_mut().insert(
            "mcp-session-id",
            "legacy-session".parse().expect("session header"),
        );
    }
    response
}

async fn fixture_delete(State(fixture): State<Fixture>, headers: HeaderMap) -> StatusCode {
    fixture
        .calls
        .lock()
        .expect("calls lock")
        .push((json!({"method": "DELETE"}), headers));
    StatusCode::NO_CONTENT
}

#[tokio::test]
async fn catalog_discovers_pages_and_preserves_protocol_headers_and_schemas() {
    for version in [PROTOCOL_VERSION, LEGACY_PROTOCOL_VERSION] {
        let fixture = Fixture {
            version,
            calls: Arc::default(),
            invalid_tools: None,
        };
        let (url, task) = serve(fixture.router()).await;
        let catalog = config::fixture_catalog(url, version)
            .await
            .expect("fixture catalog");
        task.abort();
        assert_eq!(catalog.tool_names(), ["first", "diagnostic"]);
        let config = catalog.config("server", "http://fixture/mcp", version);
        let host = &config["virtual_hosts"]["server"];
        assert_eq!(
            host["backends"]["conformance-backend"]["tool_schemas"]["diagnostic"]["properties"]["value"]
                ["x-mcp-header"],
            "Value"
        );
        assert_eq!(
            host["resources"]["test://resource"]["upstream_name"],
            "test://resource"
        );
        assert!(
            host["resource_templates"]
                .get("test://resource/{id}")
                .is_some()
        );
        assert!(host["prompts"].get("prompt").is_some());
        let calls = fixture.calls.lock().expect("calls lock");
        assert_eq!(
            calls
                .iter()
                .filter(|(body, _)| body["method"] == "tools/list")
                .count(),
            2
        );
        for (body, headers) in calls.iter() {
            if version == PROTOCOL_VERSION {
                assert_eq!(
                    body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
                    version
                );
                assert_eq!(
                    headers["mcp-method"],
                    body["method"].as_str().expect("method")
                );
            } else {
                assert!(body["params"].get("_meta").is_none());
                assert!(!headers.contains_key("mcp-method"));
                if body["method"] == "initialize" {
                    assert!(!headers.contains_key("mcp-protocol-version"));
                    assert_eq!(body["params"]["protocolVersion"], version);
                } else {
                    assert_eq!(headers["mcp-session-id"], "legacy-session");
                    assert_eq!(headers["mcp-protocol-version"], version);
                }
            }
        }
        if version == LEGACY_PROTOCOL_VERSION {
            assert_eq!(calls[0].0["method"], "initialize");
            assert_eq!(calls[1].0["method"], "notifications/initialized");
            assert!(calls[1].0.get("id").is_none());
            assert_eq!(calls.last().expect("cleanup").0["method"], "DELETE");
        } else {
            assert_eq!(calls[0].0["method"], "server/discover");
        }
    }
}

#[tokio::test]
async fn invalid_catalogs_fail_closed_and_release_legacy_sessions() {
    for page in [
        Value::Null,
        json!({}),
        json!({"tools": []}),
        json!({"tools": [{"name": "bad", "inputSchema": []}]}),
        json!({"tools": [{"name": "tool", "inputSchema": {}}], "nextCursor": []}),
        json!({"tools": [{"name": "tool", "inputSchema": {}}], "nextCursor": "repeated"}),
    ] {
        let fixture = Fixture {
            version: LEGACY_PROTOCOL_VERSION,
            calls: Arc::default(),
            invalid_tools: Some(page),
        };
        let (url, task) = serve(fixture.router()).await;
        assert!(
            config::fixture_catalog(url, LEGACY_PROTOCOL_VERSION)
                .await
                .is_err()
        );
        task.abort();
        assert_eq!(
            fixture
                .calls
                .lock()
                .expect("calls lock")
                .last()
                .expect("cleanup")
                .0["method"],
            "DELETE"
        );
    }
}

#[test]
fn client_config_uses_named_messagepack_maps_and_compact_user_key() {
    let catalog = config::Catalog::for_client(vec!["metadata_probe".into(), "add_numbers".into()]);
    let body = catalog.config("scenario", "http://fixture/mcp", PROTOCOL_VERSION);
    let packed = rmp_serde::to_vec_named(&body).expect("encode config");
    let decoded: Value = rmp_serde::from_slice(&packed).expect("decode config");
    let host = &decoded["virtual_hosts"]["scenario"];
    for name in catalog.tool_names() {
        assert_eq!(
            host["tools"][name],
            json!({"backend_name": "conformance-backend", "upstream_name": name})
        );
        assert_eq!(
            host["backends"]["conformance-backend"]["tool_schemas"][name],
            json!({})
        );
    }
    for field in ["resources", "resource_templates", "prompts"] {
        assert_eq!(host[field], json!({}));
    }
    assert_eq!(
        rmp_serde::to_vec(&("UserConfig", "subject")).expect("encode key"),
        b"\x92\xaaUserConfig\xa7subject"
    );
}

#[tokio::test]
async fn auth_reuses_private_key_and_serves_only_public_jwks() {
    let directory = tempfile::tempdir().expect("key directory");
    let key = directory.path().join("jwt.key");
    let http = reqwest::Client::new();
    let mut original = Value::Null;
    for _ in 0..2 {
        let router = auth::router(&key).expect("auth router");
        let (base, task) = serve(router).await;
        let url = base.join("/.well-known/jwks.json").expect("JWKS URL");
        let jwks: Value = http
            .get(url.clone())
            .send()
            .await
            .expect("JWKS response")
            .json()
            .await
            .expect("JWKS JSON");
        if !original.is_null() {
            assert_eq!(original, jwks);
        }
        original = jwks.clone();
        let jwk = &jwks["keys"][0];
        for field in ["d", "p", "q", "dp", "dq", "qi"] {
            assert!(jwk.get(field).is_none());
        }
        let token = auth::issue_token(&key, "tenant", "subject").expect("sign JWT");
        let jwk: jsonwebtoken::jwk::Jwk = serde_json::from_value(jwk.clone()).expect("public JWK");
        let decoded = jsonwebtoken::decode::<Value>(
            &token,
            &DecodingKey::from_jwk(&jwk).expect("JWK decoding key"),
            &Validation::new(Algorithm::RS256),
        )
        .expect("verify signature");
        assert_eq!(decoded.claims["sub"], "subject");
        assert_eq!(decoded.claims["tenant_id"], "tenant");
        assert_eq!(
            auth::token_subject(&token).expect("token subject"),
            "subject"
        );
        assert_eq!(
            decoded.header.kid.as_deref(),
            Some("cf-integration-standalone")
        );
        assert_eq!(
            http.head(url.clone())
                .send()
                .await
                .expect("HEAD")
                .bytes()
                .await
                .expect("HEAD body")
                .len(),
            0
        );
        assert_eq!(http.post(url).send().await.expect("POST").status(), 405);
        assert_eq!(
            http.get(base.join("/jwt.key").expect("key URL"))
                .send()
                .await
                .expect("private path")
                .status(),
            404
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&key)
                    .expect("key metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        task.abort();
    }
}

#[test]
fn invalid_existing_key_is_preserved_and_rejected() {
    let directory = tempfile::tempdir().expect("key directory");
    let key = directory.path().join("jwt.key");
    std::fs::write(&key, "invalid-key").expect("invalid key fixture");
    assert!(auth::router(&key).is_err());
    assert_eq!(
        std::fs::read_to_string(key).expect("preserved key"),
        "invalid-key"
    );
}

#[test]
fn token_subject_rejects_missing_or_invalid_claims() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    for claims in [json!({}), json!({"sub": ""}), json!({"sub": 123})] {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256"}"#);
        let token = format!(
            "{header}.{}.signature",
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        assert!(auth::token_subject(&token).is_err());
    }
    assert!(auth::token_subject("not-a-token").is_err());
}
