//! Control-plane authentication and managed credential lifecycle.

use std::time::Duration;

use anyhow::{Context, anyhow};
use reqwest::{Client, StatusCode};
use serde::Deserialize;

use crate::infrastructure::config::AppConfig;

use super::{AppFailure, AppResult, required_text};

const MANAGED_TOKEN_DESCRIPTION: &str = "Ephemeral cf-integration dataplane credential";
pub(super) const CONFORMANCE_TOKEN_DESCRIPTION: &str =
    "Ephemeral cf-integration conformance credential";

pub(super) struct ManagedBearerToken {
    pub(super) value: String,
    pub(super) catalog_id: Option<String>,
    pub(super) catalog_admin_token: Option<String>,
}

impl ManagedBearerToken {
    pub(super) fn unmanaged(value: String) -> Self {
        Self {
            value,
            catalog_id: None,
            catalog_admin_token: None,
        }
    }
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

/// Owns all control-plane HTTP behavior and credential cleanup.
pub(super) struct ControlPlaneClient {
    base_url: url::Url,
    admin_email: String,
    admin_password: String,
    http: Client,
}

impl ControlPlaneClient {
    pub(super) fn new(config: &AppConfig) -> AppResult<Self> {
        let base_url =
            url::Url::parse(required_text(&config.base_url().value, "MCP_CLI_BASE_URL")?)
                .context("MCP_CLI_BASE_URL is not a valid URL")
                .map_err(AppFailure::from)?;
        let admin_email =
            required_text(&config.platform_admin_email().value, "PLATFORM_ADMIN_EMAIL")?.to_owned();
        let admin_password = required_text(
            &config.platform_admin_password().value,
            "PLATFORM_ADMIN_PASSWORD",
        )?
        .to_owned();
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build control-plane client")
            .map_err(AppFailure::from)?;
        Ok(Self {
            base_url,
            admin_email,
            admin_password,
            http,
        })
    }

    pub(super) async fn admin_session_token(&self) -> AppResult<String> {
        let endpoint = self.endpoint("/v1/auth/email/login", "login")?;
        let response = self
            .http
            .post(endpoint)
            .json(&serde_json::json!({
                "email": self.admin_email,
                "password": self.admin_password,
            }))
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

    pub(super) async fn issue_dataplane_token(
        &self,
        server_id: &str,
    ) -> AppResult<ManagedBearerToken> {
        self.issue_catalog_token(Some(server_id), MANAGED_TOKEN_DESCRIPTION)
            .await
    }

    pub(super) async fn issue_conformance_token(&self) -> AppResult<ManagedBearerToken> {
        self.issue_catalog_token(None, CONFORMANCE_TOKEN_DESCRIPTION)
            .await
    }

    async fn issue_catalog_token(
        &self,
        server_id: Option<&str>,
        description: &str,
    ) -> AppResult<ManagedBearerToken> {
        let endpoint = self.endpoint("/v1/tokens", "token catalog")?;
        let admin_token = self.admin_session_token().await?;
        let mut payload = serde_json::json!({
            "name": format!("cf-integration-{}", uuid::Uuid::new_v4()),
            "description": description,
            "expires_in_days": 1,
            "user_email": self.admin_email,
        });
        if let Some(server_id) = server_id {
            payload["scope"] = serde_json::json!({
                "server_id": server_id,
                "permissions": ["servers.read", "servers.use", "tools.read", "tools.call"],
            });
        }
        let response = self
            .http
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

    pub(super) async fn revoke(&self, token: &ManagedBearerToken) -> AppResult<()> {
        let Some(id) = token.catalog_id.as_deref() else {
            return Ok(());
        };
        let admin_token = token.catalog_admin_token.as_deref().ok_or_else(|| {
            AppFailure::from(anyhow!(
                "managed token is missing its control-plane cleanup credential"
            ))
        })?;
        let endpoint = self.endpoint(&format!("/v1/tokens/{id}"), "token revocation")?;
        let response = self
            .http
            .delete(endpoint)
            .bearer_auth(admin_token)
            .send()
            .await
            .context("token revocation failed before receiving a response")
            .map_err(AppFailure::from)?;
        if response.status().is_success() || response.status() == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(AppFailure::from(anyhow!(
                "token catalog returned HTTP {} while revoking the dataplane credential",
                response.status().as_u16()
            )))
        }
    }

    fn endpoint(&self, path: &str, label: &str) -> AppResult<url::Url> {
        self.base_url
            .join(path)
            .with_context(|| format!("failed to construct control-plane {label} URL"))
            .map_err(AppFailure::from)
    }
}
