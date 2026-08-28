//! Operating-system-backed execution of resolved CLI actions.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, anyhow};
use cf_integration_compliance::conformance::{
    ComparisonFixtureTrust, ComparisonReport, ConformanceFixtureMetadata, ConformanceResults,
    ConformanceRunMetadata, ConformanceServerEra, ConformanceTarget,
    compare_result_sets_with_fixture_trust, expected_server_scenarios, is_trusted_official_fixture,
    load_server_results, official_server_command, validate_server_scenario_set,
    write_comparison_report,
};
use cf_integration_compliance::conformance_fixture::{
    ConformanceFixtureClient, OFFICIAL_CONFORMANCE_BACKEND_URL, OFFICIAL_CONFORMANCE_PROXY_SERVICE,
    OFFICIAL_CONFORMANCE_REPOSITORY, OFFICIAL_CONFORMANCE_REVISION, OFFICIAL_CONFORMANCE_SERVER_ID,
    OFFICIAL_CONFORMANCE_SERVICE,
};
use cf_integration_load::{LoadSettings, LocustCommand, audit_locust_reports};
use cf_integration_mcp::GatewayTopology;
use cf_integration_mcp::auth_proxy::AuthProxy;
use cf_integration_mcp::gateway::GatewayClient;
use cf_integration_mcp::http_transport::ReqwestProbeTransport;
use cf_integration_mcp::mcp::ACCEPT as MCP_ACCEPT;
use cf_integration_mcp::probe::{ProbeConfig, run_probe};
use cf_integration_platform::checkout::{CheckoutManager, CheckoutRequest};
use cf_integration_platform::compose::{ComposeProject, validate_integration_contract};
use cf_integration_platform::config::AppConfig;
use cf_integration_platform::process::{CommandSpec, LoggingProcessRunner, ProcessRunner};
use cf_integration_platform::stack::{
    BuildInputs, BuildMode, CleanupKind, FreshnessSnapshot, ServiceSnapshot, StackCommandPlan,
    StackFreshness, resolve_build,
};
use cf_integration_platform::{PlatformError, StackMode};
use serde::Deserialize;

use crate::OutputStyle;
use crate::app::{
    Action, ConformanceAction, DebugAction, LiveLane, ResolvedLoadArgs, StackAction,
    selected_topologies, topology_selection,
};
use crate::cli::{LiveGroup, ProtocolVersion, TokenKind as CliTokenKind, TopologySelection};
use crate::error::AppFailure;

type AppResult<T> = std::result::Result<T, AppFailure>;

const STACK_READY_TIMEOUT: Duration = Duration::from_secs(90);
const STACK_READY_POLL_INTERVAL: Duration = Duration::from_millis(250);
const STACK_READY_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MANAGED_TOKEN_DESCRIPTION: &str = "Ephemeral cf-integration dataplane credential";
const CONFORMANCE_TOKEN_DESCRIPTION: &str = "Ephemeral cf-integration conformance credential";

mod compliance;
mod inspect;
mod live;
mod reports;
mod sources;
mod stack;
mod workloads;

use inspect::*;
use reports::*;

/// Runtime services backed by one loaded configuration and process runner.
pub struct RuntimeExecutor<R> {
    config: AppConfig,
    runner: R,
}

struct ManagedBearerToken {
    value: String,
    catalog_id: Option<String>,
    catalog_admin_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenCreateResponse {
    token: TokenRecord,
    access_token: String,
}

#[derive(Deserialize)]
struct TokenRecord {
    id: String,
}

#[derive(Deserialize)]
struct AuthenticationResponse {
    access_token: String,
}

impl<R> RuntimeExecutor<R> {
    /// Creates an executor without starting any process.
    #[must_use]
    pub fn new(config: AppConfig, runner: R) -> Self {
        Self { config, runner }
    }

    /// Returns the loaded application configuration.
    #[must_use]
    pub fn config(&self) -> &AppConfig {
        &self.config
    }
}

impl<R: ProcessRunner> RuntimeExecutor<R> {
    /// Executes one fully resolved operation.
    pub async fn execute(&mut self, action: Action) -> AppResult<()> {
        match action {
            Action::Stack(action) => self.execute_stack(action).await,
            Action::Probe {
                topology,
                protocol_version,
            } => self.run_probe(topology, &protocol_version).await,
            Action::Load(args) => self.run_load(args).await,
            Action::Live {
                lane,
                group,
                protocol_version,
            } => self.run_live(lane, group, &protocol_version).await,
            Action::Conformance(action) => self.execute_conformance(action).await,
            Action::Debug(DebugAction::Token { kind, server_id }) => {
                self.print_token(kind, server_id).await
            }
            Action::Debug(DebugAction::Inspect {
                topology,
                protocol_version,
                method,
                server_id,
            }) => {
                self.inspect(topology, &protocol_version, &method, server_id.as_deref())
                    .await
            }
        }
    }
}

impl<R: ProcessRunner> RuntimeExecutor<R> {
    async fn print_token(&self, kind: CliTokenKind, server_id: Option<String>) -> AppResult<()> {
        let token = match kind {
            CliTokenKind::Scoped => {
                let server_id = server_id.unwrap_or_else(|| self.default_server_id().to_owned());
                self.issue_dataplane_token(&server_id).await?.value
            }
            CliTokenKind::Admin => self.admin_session_token().await?,
        };
        println!("{token}");
        Ok(())
    }

    fn default_server_id(&self) -> &str {
        self.environment_text("MCP_SERVER_ID")
            .filter(|value| !value.is_empty())
            .or_else(|| {
                self.environment_text("MCP_VIRTUAL_SERVER_ID")
                    .filter(|value| !value.is_empty())
            })
            .or_else(|| self.config.fast_time_server_id().value.to_str())
            .unwrap_or("9779b6698cbd4b4995ee04a4fab38737")
    }

    fn base_url(&self) -> AppResult<&str> {
        required_text(&self.config.base_url().value, "MCP_CLI_BASE_URL")
    }

    async fn managed_bearer_token(
        &self,
        mode: StackMode,
        server_id: &str,
    ) -> AppResult<ManagedBearerToken> {
        if let Some(token) = self
            .environment_text("MCPGATEWAY_BEARER_TOKEN")
            .filter(|token| !token.is_empty())
        {
            return Ok(ManagedBearerToken {
                value: token.to_owned(),
                catalog_id: None,
                catalog_admin_token: None,
            });
        }
        if mode == StackMode::Controlplane {
            return Ok(ManagedBearerToken {
                value: self.admin_session_token().await?,
                catalog_id: None,
                catalog_admin_token: None,
            });
        }

        self.issue_dataplane_token(server_id).await
    }

    async fn admin_session_token(&self) -> AppResult<String> {
        let endpoint = url::Url::parse(self.base_url()?)
            .context("MCP_CLI_BASE_URL is not a valid URL")
            .and_then(|base| {
                base.join("/v1/auth/email/login")
                    .context("failed to construct control-plane login URL")
            })
            .map_err(AppFailure::from)?;
        let email = required_text(
            &self.config.platform_admin_email().value,
            "PLATFORM_ADMIN_EMAIL",
        )?;
        let password = required_text(
            &self.config.platform_admin_password().value,
            "PLATFORM_ADMIN_PASSWORD",
        )?;
        let response = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build control-plane login client")
            .map_err(AppFailure::from)?
            .post(endpoint)
            .json(&serde_json::json!({"email": email, "password": password}))
            .send()
            .await
            .context("control-plane login failed before receiving a response")
            .map_err(AppFailure::from)?;
        if !response.status().is_success() {
            return Err(AppFailure::from(anyhow!(
                "control-plane login returned HTTP {}",
                response.status().as_u16()
            )));
        }
        let authenticated: AuthenticationResponse = response
            .json()
            .await
            .context("control-plane login returned an invalid authentication response")
            .map_err(AppFailure::from)?;
        if authenticated.access_token.is_empty() {
            return Err(AppFailure::from(anyhow!(
                "control-plane login returned an empty access token"
            )));
        }
        Ok(authenticated.access_token)
    }

    async fn issue_dataplane_token(&self, server_id: &str) -> AppResult<ManagedBearerToken> {
        self.issue_catalog_token(Some(server_id), MANAGED_TOKEN_DESCRIPTION)
            .await
    }

    async fn issue_conformance_token(&self) -> AppResult<ManagedBearerToken> {
        self.issue_catalog_token(None, CONFORMANCE_TOKEN_DESCRIPTION)
            .await
    }

    async fn issue_catalog_token(
        &self,
        server_id: Option<&str>,
        description: &str,
    ) -> AppResult<ManagedBearerToken> {
        let endpoint = url::Url::parse(self.base_url()?)
            .context("MCP_CLI_BASE_URL is not a valid URL")
            .and_then(|base| {
                base.join("/v1/tokens")
                    .context("failed to construct token catalog URL")
            })
            .map_err(AppFailure::from)?;
        let admin_token = self.admin_session_token().await?;
        let user_email = required_text(
            &self.config.platform_admin_email().value,
            "PLATFORM_ADMIN_EMAIL",
        )?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build token catalog client")
            .map_err(AppFailure::from)?;
        let mut payload = serde_json::json!({
            "name": format!("cf-integration-{}", uuid::Uuid::new_v4()),
            "description": description,
            "expires_in_days": 1,
            "user_email": user_email,
        });
        if let Some(server_id) = server_id {
            payload["scope"] = serde_json::json!({
                "server_id": server_id,
                "permissions": ["servers.read", "servers.use", "tools.read", "tools.call"],
            });
        }
        let response = http
            .post(endpoint)
            .bearer_auth(&admin_token)
            .json(&payload)
            .send()
            .await
            .context("token catalog request failed before receiving a response")
            .map_err(AppFailure::from)?;
        if !response.status().is_success() {
            return Err(AppFailure::from(anyhow!(
                "token catalog returned HTTP {} while issuing a managed credential",
                response.status().as_u16()
            )));
        }
        let issued: TokenCreateResponse = response
            .json()
            .await
            .context("token catalog returned an invalid credential response")
            .map_err(AppFailure::from)?;
        if issued.token.id.is_empty() || issued.access_token.is_empty() {
            return Err(AppFailure::from(anyhow!(
                "token catalog returned an incomplete credential response"
            )));
        }
        Ok(ManagedBearerToken {
            value: issued.access_token,
            catalog_id: Some(issued.token.id),
            catalog_admin_token: Some(admin_token),
        })
    }

    async fn revoke_managed_token(&self, token: &ManagedBearerToken) -> AppResult<()> {
        let Some(id) = token.catalog_id.as_deref() else {
            return Ok(());
        };
        let admin_token = token.catalog_admin_token.as_deref().ok_or_else(|| {
            AppFailure::from(anyhow!(
                "managed token is missing its control-plane cleanup credential"
            ))
        })?;
        let endpoint = url::Url::parse(self.base_url()?)
            .context("MCP_CLI_BASE_URL is not a valid URL")
            .and_then(|base| {
                base.join(&format!("/v1/tokens/{id}"))
                    .context("failed to construct token revocation URL")
            })
            .map_err(AppFailure::from)?;
        let response = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build token catalog client")
            .map_err(AppFailure::from)?
            .delete(endpoint)
            .bearer_auth(admin_token)
            .send()
            .await
            .context("token revocation failed before receiving a response")
            .map_err(AppFailure::from)?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(AppFailure::from(anyhow!(
                "token catalog returned HTTP {} while revoking the dataplane credential",
                response.status().as_u16()
            )))
        }
    }
}

fn required_text<'a>(value: &'a OsStr, name: &str) -> AppResult<&'a str> {
    value
        .to_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppFailure::from(anyhow!("{name} must be nonempty UTF-8")))
}

fn finish_with_cleanup(primary: Option<AppFailure>, cleanup: AppResult<()>) -> AppResult<()> {
    match (primary, cleanup) {
        (None, Ok(())) => Ok(()),
        (Some(primary), Ok(())) => Err(primary),
        (None, Err(cleanup)) => Err(cleanup),
        (Some(primary), Err(cleanup)) => Err(AppFailure::from(anyhow!(
            "{primary}; additionally cleanup failed: {cleanup}"
        ))),
    }
}

async fn wait_for_http_endpoint(
    endpoint: &url::Url,
    mode: StackMode,
    timeout: Duration,
) -> AppResult<()> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .context("failed to build the public MCP readiness client")
        .map_err(AppFailure::from)?;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_failure = "no HTTP response".to_owned();

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(AppFailure::from(anyhow!(
                "{} public MCP endpoint {} was not ready within {:.3}s; last result: {last_failure}",
                stack_mode_label(mode),
                endpoint,
                timeout.as_secs_f64()
            )));
        }
        let request_timeout = deadline
            .saturating_duration_since(now)
            .min(STACK_READY_REQUEST_TIMEOUT);
        let request = client
            .get(endpoint.clone())
            .header(reqwest::header::ACCEPT, MCP_ACCEPT);
        match tokio::time::timeout(request_timeout, request.send()).await {
            Ok(Ok(response)) if is_expected_readiness_status(response.status()) => return Ok(()),
            Ok(Ok(response)) => {
                last_failure = format!("HTTP {}", response.status().as_u16());
            }
            Ok(Err(error)) => {
                last_failure = format!("request error: {error}");
            }
            Err(_) => {
                last_failure = format!(
                    "request timed out after {:.3}s",
                    request_timeout.as_secs_f64()
                );
            }
        }
        let now = tokio::time::Instant::now();
        tokio::time::sleep(
            deadline
                .saturating_duration_since(now)
                .min(STACK_READY_POLL_INTERVAL),
        )
        .await;
    }
}

const fn is_expected_readiness_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 401 | 403 | 405)
}

const fn stack_mode_label(mode: StackMode) -> &'static str {
    match mode {
        StackMode::Controlplane => "controlplane",
        StackMode::Dataplane => "dataplane",
    }
}

const fn gateway_topology(mode: StackMode) -> GatewayTopology {
    match mode {
        StackMode::Controlplane => GatewayTopology::Direct,
        StackMode::Dataplane => GatewayTopology::Dataplane,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::Body;
    use axum::extract::{Request, State};
    use axum::http::{HeaderMap, Method, Response, StatusCode};
    use axum::routing::any;
    use cf_integration_platform::config::Environment;
    use cf_integration_platform::process::SystemProcessRunner;
    use serde_json::{Value, json};
    use tokio::net::TcpListener;

    use super::*;

    type CapturedRequest = (Method, String, HeaderMap, Value);

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<CapturedRequest>>>);

    async fn token_catalog(State(capture): State<Capture>, request: Request) -> Response<Body> {
        let (parts, body) = request.into_parts();
        let body = axum::body::to_bytes(body, 64 * 1024)
            .await
            .expect("token request body should fit");
        let body = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).expect("token request body should be JSON")
        };
        capture
            .0
            .lock()
            .expect("token capture lock should not be poisoned")
            .push((
                parts.method.clone(),
                parts.uri.path().to_owned(),
                parts.headers,
                body,
            ));

        if parts.method == Method::POST && parts.uri.path() == "/v1/auth/email/login" {
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"access_token": "admin-session-token"}).to_string(),
                ))
                .expect("login response should build")
        } else if parts.method == Method::POST && parts.uri.path() == "/v1/tokens" {
            Response::builder()
                .status(StatusCode::CREATED)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "token": {"id": "catalog-token-id"},
                        "access_token": "issued-dataplane-token"
                    })
                    .to_string(),
                ))
                .expect("token response should build")
        } else if parts.method == Method::DELETE
            && parts.uri.path() == "/v1/tokens/catalog-token-id"
        {
            Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::empty())
                .expect("revocation response should build")
        } else {
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .expect("not-found response should build")
        }
    }

    fn app_config(root: &Path, base_url: &str, extra: &[(&str, &str)]) -> AppConfig {
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='test'\nversion='0.0.0'\n",
        )
        .expect("temporary manifest should be written");
        fs::create_dir_all(root.join("docker")).expect("temporary docker directory should exist");
        fs::write(
            root.join("docker/docker-compose.cf-integration.yaml"),
            "services: {}\n",
        )
        .expect("temporary Compose marker should be written");
        let mut environment = Environment::from([
            (OsString::from("MCP_CLI_BASE_URL"), OsString::from(base_url)),
            (
                OsString::from("JWT_SECRET_KEY"),
                OsString::from("test-jwt-secret-that-is-longer-than-32-bytes"),
            ),
            (
                OsString::from("AUTH_ENCRYPTION_SECRET"),
                OsString::from("test-auth-secret-that-is-longer-than-32-bytes"),
            ),
            (
                OsString::from("PLATFORM_ADMIN_EMAIL"),
                OsString::from("operator@example.test"),
            ),
            (
                OsString::from("PLATFORM_ADMIN_PASSWORD"),
                OsString::from("integration-password"),
            ),
        ]);
        environment.extend(
            extra
                .iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value))),
        );
        AppConfig::load(
            &environment,
            &root.join("target/debug/cf-integration"),
            root,
        )
        .expect("test application config should load")
        .config
    }

    #[tokio::test]
    async fn dataplane_tokens_are_issued_by_uuid_aware_catalog_and_revoked() {
        let capture = Capture::default();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("token catalog listener should bind");
        let address = listener
            .local_addr()
            .expect("token catalog listener should have an address");
        let server = tokio::spawn(
            axum::serve(
                listener,
                Router::new()
                    .fallback(any(token_catalog))
                    .with_state(capture.clone()),
            )
            .into_future(),
        );
        let root = tempfile::tempdir().expect("temporary repository should be created");
        let config = app_config(root.path(), &format!("http://{address}"), &[]);
        let runtime = RuntimeExecutor::new(config, SystemProcessRunner);

        let token = runtime
            .managed_bearer_token(StackMode::Dataplane, "server-id")
            .await
            .expect("dataplane token should be issued");
        assert_eq!(token.value, "issued-dataplane-token");
        assert_eq!(token.catalog_id.as_deref(), Some("catalog-token-id"));
        runtime
            .revoke_managed_token(&token)
            .await
            .expect("managed token should be revoked");
        server.abort();

        let requests = capture
            .0
            .lock()
            .expect("token capture lock should not be poisoned");
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].0, Method::POST);
        assert_eq!(requests[0].1, "/v1/auth/email/login");
        assert_eq!(requests[0].3["email"], "operator@example.test");
        assert_eq!(requests[1].0, Method::POST);
        assert_eq!(requests[1].1, "/v1/tokens");
        assert_eq!(requests[1].3["user_email"], "operator@example.test");
        assert_eq!(requests[1].3["expires_in_days"], 1);
        assert_eq!(requests[1].3["scope"]["server_id"], "server-id");
        assert_eq!(
            requests[1].3["scope"]["permissions"],
            json!(["servers.read", "servers.use", "tools.read", "tools.call"])
        );
        assert!(
            requests[1].3["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("cf-integration-"))
        );
        assert!(requests[1].2.contains_key("authorization"));
        assert_eq!(requests[2].0, Method::DELETE);
        assert_eq!(requests[2].1, "/v1/tokens/catalog-token-id");
        assert!(requests[2].2.contains_key("authorization"));
        assert_eq!(
            requests[1].2["authorization"],
            requests[2].2["authorization"]
        );
    }

    #[tokio::test]
    async fn caller_managed_tokens_bypass_catalog_and_are_not_revoked() {
        let root = tempfile::tempdir().expect("temporary repository should be created");
        let config = app_config(
            root.path(),
            "http://127.0.0.1:9",
            &[("MCPGATEWAY_BEARER_TOKEN", "caller-token")],
        );
        let runtime = RuntimeExecutor::new(config, SystemProcessRunner);

        let token = runtime
            .managed_bearer_token(StackMode::Dataplane, "server-id")
            .await
            .expect("caller token should not contact the catalog");
        assert_eq!(token.value, "caller-token");
        assert_eq!(token.catalog_id, None);
        assert_eq!(token.catalog_admin_token, None);
        runtime
            .revoke_managed_token(&token)
            .await
            .expect("caller token cleanup should be a no-op");
    }

    #[tokio::test]
    async fn conformance_tokens_match_the_unscoped_controlplane_lane_contract() {
        let capture = Capture::default();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("token catalog listener should bind");
        let address = listener
            .local_addr()
            .expect("token catalog listener should have an address");
        let server = tokio::spawn(
            axum::serve(
                listener,
                Router::new()
                    .fallback(any(token_catalog))
                    .with_state(capture.clone()),
            )
            .into_future(),
        );
        let root = tempfile::tempdir().expect("temporary repository should be created");
        let config = app_config(root.path(), &format!("http://{address}"), &[]);
        let runtime = RuntimeExecutor::new(config, SystemProcessRunner);

        let token = runtime
            .issue_conformance_token()
            .await
            .expect("conformance token should be issued");
        runtime
            .revoke_managed_token(&token)
            .await
            .expect("conformance token should be revoked");
        server.abort();

        let requests = capture
            .0
            .lock()
            .expect("token capture lock should not be poisoned");
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].1, "/v1/tokens");
        assert_eq!(requests[1].3["description"], CONFORMANCE_TOKEN_DESCRIPTION);
        assert!(requests[1].3.get("scope").is_none());
    }
}
