//! Register the shared Fast Time workload using control-plane login, independent
//! of its JWT signing algorithm. Never mint or print an administrative token.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use url::Url;

pub(super) async fn register(health_url: Url, backend_url: Url, server_id: &str) -> Result<()> {
    let base = Url::parse(
        &std::env::var("GATEWAY_URL").unwrap_or_else(|_| "http://gateway:4444".to_owned()),
    )?;
    let email = std::env::var("PLATFORM_ADMIN_EMAIL")?;
    let password = std::env::var("PLATFORM_ADMIN_PASSWORD")?;
    register_with_credentials(base, health_url, backend_url, server_id, &email, &password).await
}

async fn register_with_credentials(
    base: Url,
    health_url: Url,
    backend_url: Url,
    server_id: &str,
    email: &str,
    password: &str,
) -> Result<()> {
    let http = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()?;
    let mut healthy = false;
    for _ in 0..30 {
        if http
            .get(health_url.clone())
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            healthy = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    ensure!(
        healthy,
        "Fast Time health check failed; registration stopped"
    );
    let login: Value = http
        .post(base.join("/v1/auth/email/login")?)
        .json(&json!({"email":email,"password":password}))
        .send()
        .await?
        .error_for_status()
        .context("control-plane login failed")?
        .json()
        .await?;
    let token = login["access_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .context("control-plane login returned no access token")?;
    let request = |method: Method, path: &str| -> Result<reqwest::RequestBuilder> {
        Ok(http.request(method, base.join(path)?).bearer_auth(token))
    };
    let gateways: Vec<Value> = request(Method::GET, "/gateways")?
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let gateway = if let Some(gateway) = gateways
        .iter()
        .find(|gateway| gateway["name"] == "fast_time")
    {
        ensure!(
            gateway["url"].as_str() == Some(backend_url.as_str()),
            "existing fast_time gateway uses a different backend"
        );
        gateway.clone()
    } else {
        request(Method::POST, "/gateways")?
            .json(&json!({
                "name":"fast_time", "url":backend_url.as_str(), "transport":"STREAMABLEHTTP",
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?
    };
    let gateway_id = gateway["id"]
        .as_str()
        .context("gateway registration returned no ID")?;
    request(
        Method::POST,
        &format!(
            "/gateways/{gateway_id}/tools/refresh?include_resources=true&include_prompts=true"
        ),
    )?
    .send()
    .await?
    .error_for_status()?;
    let mut server = json!({"id":server_id,"name":"Fast Time Server",
        "description":"Virtual server exposing Fast Time MCP tools, resources, and prompts"});
    for catalog in ["tools", "resources", "prompts"] {
        let items: Vec<Value> = request(Method::GET, &format!("/{catalog}"))?
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let ids: Vec<Value> = items
            .iter()
            .filter(|item| {
                item.get("gatewayId")
                    .or_else(|| item.get("gateway_id"))
                    .and_then(Value::as_str)
                    == Some(gateway_id)
            })
            .filter_map(|item| item.get("id").cloned())
            .collect();
        if catalog == "tools" {
            ensure!(
                !ids.is_empty(),
                "Fast Time registered without tools; setup stopped"
            );
        }
        server[format!("associated_{catalog}")] = json!(ids);
    }
    let existing = request(Method::GET, &format!("/servers/{server_id}"))?
        .send()
        .await?;
    if existing.status() == StatusCode::NOT_FOUND {
        request(Method::POST, "/servers")?
            .json(&json!({"server":server}))
            .send()
            .await?
            .error_for_status()?;
    } else {
        existing.error_for_status()?;
        request(Method::PUT, &format!("/servers/{server_id}"))?
            .json(&server)
            .send()
            .await?
            .error_for_status()?;
    }
    println!("Fast Time virtual server registered: {server_id}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router, extract::State, http::HeaderMap, response::IntoResponse, routing::get,
    };
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn registration_logs_in_filters_catalogs_and_creates_server() {
        let saved = Arc::new(Mutex::new(Value::Null));
        let app = Router::new()
            .route("/health", get(|| async { StatusCode::OK }))
            .route(
                "/v1/auth/email/login",
                axum::routing::post(|Json(body): Json<Value>| async move {
                    assert_eq!(
                        body,
                        json!({"email":"admin@example.com","password":"test-password"})
                    );
                    Json(json!({"access_token":"issued-by-control-plane"}))
                }),
            )
            .route(
                "/gateways",
                get(|headers: HeaderMap| async move {
                    assert_eq!(headers["authorization"], "Bearer issued-by-control-plane");
                    Json(json!([]))
                })
                .post(|Json(body): Json<Value>| async move {
                    assert_eq!(body["transport"], "STREAMABLEHTTP");
                    Json(json!({"id":"gateway"}))
                }),
            )
            .route(
                "/gateways/gateway/tools/refresh",
                axum::routing::post(|| async { StatusCode::OK }),
            )
            .route(
                "/tools",
                get(|| async {
                    Json(json!([
                        {"id":"ours","gatewayId":"gateway"}, {"id":"foreign","gatewayId":"other"}
                    ]))
                }),
            )
            .route("/resources", get(|| async { Json(json!([])) }))
            .route("/prompts", get(|| async { Json(json!([])) }))
            .route("/servers/server", get(|| async { StatusCode::NOT_FOUND }))
            .route(
                "/servers",
                axum::routing::post(
                    |State(saved): State<Arc<Mutex<Value>>>, Json(body): Json<Value>| async move {
                        *saved.lock().expect("saved server") = body;
                        StatusCode::CREATED.into_response()
                    },
                ),
            )
            .with_state(saved.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = Url::parse(&format!(
            "http://{}",
            listener.local_addr().expect("address")
        ))
        .expect("base URL");
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        register_with_credentials(
            base.clone(),
            base.join("/health").expect("health"),
            base.join("/mcp").expect("backend"),
            "server",
            "admin@example.com",
            "test-password",
        )
        .await
        .expect("register");
        assert_eq!(
            saved.lock().expect("saved server")["server"]["associated_tools"],
            json!(["ours"])
        );
        task.abort();
    }
}
