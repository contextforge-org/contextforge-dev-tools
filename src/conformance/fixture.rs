//! Provisioning for the pinned official MCP conformance fixture.

use std::fmt;
use std::time::Duration;

use anyhow::{Result, anyhow};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use url::Url;

pub(crate) use crate::conformance::profile::{
    OFFICIAL_CONFORMANCE_REPOSITORY, OFFICIAL_CONFORMANCE_REVISION,
};
/// Docker Compose service name for the official conformance server.
pub(crate) const OFFICIAL_CONFORMANCE_SERVICE: &str = "mcp_conformance_server";
/// Docker Compose service name for the fixture's backend-only Host proxy.
pub(crate) const OFFICIAL_CONFORMANCE_PROXY_SERVICE: &str = "mcp_conformance_proxy";
/// Backend URL reachable from the control-plane and dataplane containers.
pub(crate) const OFFICIAL_CONFORMANCE_BACKEND_URL: &str = "http://mcp_conformance_proxy/mcp";
/// Reserved gateway name used by the fixture.
///
/// `_` intentionally produces an empty gateway slug when paired with the
/// conformance-only underscore separator. This preserves the upstream
/// `test_*` tool and prompt names exactly.
pub(crate) const OFFICIAL_CONFORMANCE_GATEWAY_NAME: &str = "_";
/// Deterministic virtual-server ID used by the fixture.
pub(crate) const OFFICIAL_CONFORMANCE_SERVER_ID: &str = "3f33286667d34b65a31c3bafd30e4c21";

const SERVER_NAME: &str = "Official MCP Conformance Server";
const SERVER_DESCRIPTION: &str = "Virtual server for the pinned official MCP conformance fixture.";
const GATEWAY_DESCRIPTION: &str = "Official MCP conformance fixture";
const GATEWAY_TRANSPORT: &str = "STREAMABLEHTTP";
const REQUIRED_TOOL: &str = "test_simple_text";
const REQUIRED_RESOURCE: &str = "test://static-text";
const REQUIRED_PROMPT: &str = "test_simple_prompt";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_MAX_ATTEMPTS: usize = 40;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_RECONCILIATION_ATTEMPTS: usize = 5;
const DEFAULT_RECONCILIATION_INTERVAL: Duration = Duration::from_millis(100);
const REQUIRED_RECONCILIATION_QUIET_ATTEMPTS: usize = 2;
const REDACTED: &str = "<redacted>";

/// IDs created for one official conformance fixture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProvisionedConformanceFixture {
    /// ID of the newly created backing gateway.
    pub(crate) gateway_id: String,
    /// ID of the deterministic virtual server.
    pub(crate) server_id: String,
}

/// Builder for [`ConformanceFixtureClient`].
#[must_use = "a conformance fixture client builder does nothing until build() is called"]
#[derive(Clone)]
pub(crate) struct ConformanceFixtureClientBuilder {
    base_url: String,
    admin_token: String,
    poll_interval: Duration,
    max_attempts: usize,
    request_timeout: Duration,
    reconciliation_attempts: usize,
    reconciliation_interval: Duration,
}

impl fmt::Debug for ConformanceFixtureClientBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConformanceFixtureClientBuilder")
            .field("base_url", &redact(&self.base_url, &self.admin_token))
            .field("admin_token", &REDACTED)
            .field("poll_interval", &self.poll_interval)
            .field("max_attempts", &self.max_attempts)
            .field("request_timeout", &self.request_timeout)
            .field("reconciliation_attempts", &self.reconciliation_attempts)
            .field("reconciliation_interval", &self.reconciliation_interval)
            .finish()
    }
}

impl ConformanceFixtureClientBuilder {
    /// Sets the delay between unsuccessful catalog polling attempts.
    #[cfg(test)]
    pub(crate) fn poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// Sets the maximum number of catalog polling attempts.
    #[cfg(test)]
    pub(crate) fn max_attempts(mut self, max_attempts: usize) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    /// Sets the total timeout for each admin HTTP request. Defaults to 30 seconds.
    #[cfg(test)]
    pub(crate) fn request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Sets the bounded number of gateway reconciliation polls. Defaults to five.
    #[cfg(test)]
    pub(crate) fn reconciliation_attempts(mut self, reconciliation_attempts: usize) -> Self {
        self.reconciliation_attempts = reconciliation_attempts;
        self
    }

    /// Sets the delay between gateway reconciliation polls. Defaults to 100ms.
    #[cfg(test)]
    pub(crate) fn reconciliation_interval(mut self, reconciliation_interval: Duration) -> Self {
        self.reconciliation_interval = reconciliation_interval;
        self
    }

    /// Validates configuration and builds the client.
    ///
    /// # Errors
    ///
    /// Returns an error when the base URL or bearer token is invalid, or when
    /// `max_attempts` or `request_timeout` is zero, or fewer than two gateway
    /// reconciliation attempts are configured.
    pub(crate) fn build(self) -> Result<ConformanceFixtureClient> {
        if self.max_attempts == 0 {
            return Err(anyhow!("max_attempts must be greater than zero"));
        }
        if self.request_timeout.is_zero() {
            return Err(anyhow!("request_timeout must be greater than zero"));
        }
        if self.reconciliation_attempts < REQUIRED_RECONCILIATION_QUIET_ATTEMPTS {
            return Err(anyhow!(
                "reconciliation_attempts must allow at least two quiet polls"
            ));
        }
        let base_url = validate_base_url(&self.base_url)?;
        if !is_valid_bearer_token(&self.admin_token) {
            return Err(anyhow!("admin token must be a valid RFC 6750 bearer token"));
        }
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(self.request_timeout)
            .build()
            .map_err(|_| anyhow!("failed to build conformance fixture HTTP client"))?;
        Ok(ConformanceFixtureClient {
            base_url,
            admin_token: self.admin_token,
            poll_interval: self.poll_interval,
            max_attempts: self.max_attempts,
            request_timeout: self.request_timeout,
            reconciliation_attempts: self.reconciliation_attempts,
            reconciliation_interval: self.reconciliation_interval,
            http,
        })
    }
}

/// Authenticated client for provisioning the official conformance fixture.
#[derive(Clone)]
pub(crate) struct ConformanceFixtureClient {
    base_url: Url,
    admin_token: String,
    poll_interval: Duration,
    max_attempts: usize,
    request_timeout: Duration,
    reconciliation_attempts: usize,
    reconciliation_interval: Duration,
    http: reqwest::Client,
}

impl fmt::Debug for ConformanceFixtureClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConformanceFixtureClient")
            .field(
                "base_url",
                &redact(self.base_url.as_str(), &self.admin_token),
            )
            .field("admin_token", &REDACTED)
            .field("poll_interval", &self.poll_interval)
            .field("max_attempts", &self.max_attempts)
            .field("request_timeout", &self.request_timeout)
            .field("reconciliation_attempts", &self.reconciliation_attempts)
            .field("reconciliation_interval", &self.reconciliation_interval)
            .finish_non_exhaustive()
    }
}

impl ConformanceFixtureClient {
    /// Starts a fixture client builder.
    pub(crate) fn builder(
        base_url: impl AsRef<str>,
        admin_token: impl Into<String>,
    ) -> ConformanceFixtureClientBuilder {
        ConformanceFixtureClientBuilder {
            base_url: base_url.as_ref().to_owned(),
            admin_token: admin_token.into(),
            poll_interval: DEFAULT_POLL_INTERVAL,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            reconciliation_attempts: DEFAULT_RECONCILIATION_ATTEMPTS,
            reconciliation_interval: DEFAULT_RECONCILIATION_INTERVAL,
        }
    }

    /// Creates a gateway and deterministic virtual server for the fixture.
    ///
    /// # Errors
    ///
    /// Returns an error when an admin request fails or the required conformance
    /// catalog identities do not appear within the configured attempts. A
    /// fixture created before the failure is cleaned up automatically.
    pub(crate) async fn provision(
        &self,
        backend_url: &str,
    ) -> Result<ProvisionedConformanceFixture> {
        if backend_url != OFFICIAL_CONFORMANCE_BACKEND_URL {
            return Err(anyhow!(
                "official conformance backend URL does not match the pinned fixture"
            ));
        }
        self.delete_owned_server(OFFICIAL_CONFORMANCE_SERVER_ID)
            .await?;
        self.delete_reserved_gateways_once().await?;

        let gateway_result: Result<GatewayRecord> = self
            .post_json(
                "/gateways",
                &json!({
                    "name": OFFICIAL_CONFORMANCE_GATEWAY_NAME,
                    "url": backend_url,
                    "transport": GATEWAY_TRANSPORT,
                    "authType": "authheaders",
                    "authHeaders": [{"key": "Host", "value": "localhost:3000"}],
                    "description": GATEWAY_DESCRIPTION,
                }),
            )
            .await;
        let gateway_result = gateway_result.and_then(|gateway| {
            if gateway.is_owned() {
                Ok(gateway)
            } else {
                Err(anyhow!(
                    "created gateway is not owned by the conformance fixture"
                ))
            }
        });
        let gateway = match gateway_result {
            Ok(gateway) => gateway,
            Err(primary) => {
                return match self.reconcile_reserved_gateways().await {
                    Ok(()) => Err(anyhow!(safe_error(&primary, &self.admin_token))),
                    Err(reconciliation) => Err(anyhow!(
                        "{}; gateway-create reconciliation failed: {}",
                        safe_error(&primary, &self.admin_token),
                        safe_error(&reconciliation, &self.admin_token)
                    )),
                };
            }
        };
        let fixture = ProvisionedConformanceFixture {
            gateway_id: gateway.id,
            server_id: OFFICIAL_CONFORMANCE_SERVER_ID.to_owned(),
        };
        match self.finish_provision(&fixture).await {
            Ok(()) => Ok(fixture),
            Err(primary) => match self.cleanup(Some(&fixture)).await {
                Ok(()) => Err(anyhow!(safe_error(&primary, &self.admin_token))),
                Err(cleanup) => Err(anyhow!(
                    "{}; cleanup failed: {}",
                    safe_error(&primary, &self.admin_token),
                    safe_error(&cleanup, &self.admin_token)
                )),
            },
        }
    }

    /// Deletes a fixture's server before its gateway.
    ///
    /// Passing `None` deletes only the deterministic server ID, which is useful
    /// before a gateway has been created.
    ///
    /// # Errors
    ///
    /// Returns an error for transport failures or delete responses other than a
    /// successful status or `404 Not Found`.
    pub(crate) async fn cleanup(
        &self,
        fixture: Option<&ProvisionedConformanceFixture>,
    ) -> Result<()> {
        let server_id = fixture.map_or(OFFICIAL_CONFORMANCE_SERVER_ID, |value| {
            value.server_id.as_str()
        });
        let server_result = self.delete_owned_server(server_id).await;
        let Some(fixture) = fixture else {
            return server_result;
        };
        let gateway_result = self.delete_owned_gateway(&fixture.gateway_id).await;
        match (server_result, gateway_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(server), Err(gateway)) => Err(anyhow!(
                "server cleanup failed: {}; gateway cleanup failed: {}",
                safe_error(&server, &self.admin_token),
                safe_error(&gateway, &self.admin_token)
            )),
        }
    }

    async fn finish_provision(&self, fixture: &ProvisionedConformanceFixture) -> Result<()> {
        let refresh_path = format!(
            "/gateways/{}/tools/refresh?include_resources=true&include_prompts=true",
            fixture.gateway_id
        );
        self.post_success(&refresh_path, None).await?;
        let catalogs = self.poll_catalogs(&fixture.gateway_id).await?;
        self.post_success(
            "/servers",
            Some(&json!({
                "server": {
                    "id": fixture.server_id,
                    "name": SERVER_NAME,
                    "description": SERVER_DESCRIPTION,
                    "associated_tools": catalogs.tool_ids,
                    "associated_resources": catalogs.resource_ids,
                    "associated_prompts": catalogs.prompt_ids,
                }
            })),
        )
        .await
    }

    async fn delete_reserved_gateways_once(&self) -> Result<bool> {
        let gateways: Vec<GatewayRecord> = self.get_json("/gateways").await?;
        let mut failures = Vec::new();
        let reserved: Vec<_> = gateways
            .iter()
            .filter(|gateway| gateway.name == OFFICIAL_CONFORMANCE_GATEWAY_NAME)
            .collect();
        if reserved.iter().any(|gateway| !gateway.is_owned()) {
            return Err(anyhow!(
                "reserved-name gateway collision is not owned by the conformance fixture"
            ));
        }
        for gateway in &reserved {
            if let Err(error) = self.delete("gateways", &gateway.id).await {
                failures.push(safe_error(&error, &self.admin_token));
            }
        }
        if failures.is_empty() {
            Ok(!reserved.is_empty())
        } else {
            Err(anyhow!(failures.join("; ")))
        }
    }

    async fn delete_owned_server(&self, id: &str) -> Result<()> {
        let servers: Vec<ServerRecord> = self.get_json("/servers").await?;
        if id != OFFICIAL_CONFORMANCE_SERVER_ID {
            return Err(anyhow!(redact(
                &format!("server {id} is not owned by the conformance fixture"),
                &self.admin_token
            )));
        }
        let matching: Vec<_> = servers.iter().filter(|server| server.id == id).collect();
        if matching.is_empty() {
            return Ok(());
        }
        if matching.len() != 1 || !matching[0].is_owned() {
            return Err(anyhow!(redact(
                &format!("server {id} is not owned by the conformance fixture"),
                &self.admin_token
            )));
        }
        self.delete("servers", id).await
    }

    async fn delete_owned_gateway(&self, id: &str) -> Result<()> {
        let gateways: Vec<GatewayRecord> = self.get_json("/gateways").await?;
        let matching: Vec<_> = gateways.iter().filter(|gateway| gateway.id == id).collect();
        if matching.is_empty() {
            return Ok(());
        }
        if matching.len() != 1 || !matching[0].is_owned() {
            return Err(anyhow!(redact(
                &format!("gateway {id} is not owned by the conformance fixture"),
                &self.admin_token
            )));
        }
        self.delete("gateways", id).await
    }

    async fn reconcile_reserved_gateways(&self) -> Result<()> {
        let mut quiet_attempts = 0;
        for attempt in 0..self.reconciliation_attempts {
            if self.delete_reserved_gateways_once().await? {
                quiet_attempts = 0;
            } else {
                quiet_attempts += 1;
                if quiet_attempts == REQUIRED_RECONCILIATION_QUIET_ATTEMPTS {
                    return Ok(());
                }
            }
            if attempt + 1 < self.reconciliation_attempts {
                tokio::time::sleep(self.reconciliation_interval).await;
            }
        }
        Err(anyhow!(
            "gateway reconciliation did not reach two consecutive quiet polls after {} attempts",
            self.reconciliation_attempts
        ))
    }

    async fn poll_catalogs(&self, gateway_id: &str) -> Result<CatalogIds> {
        let mut last_missing = Vec::new();
        for attempt in 0..self.max_attempts {
            let tools: Vec<CatalogRecord> = self.get_json("/tools").await?;
            let resources: Vec<CatalogRecord> = self.get_json("/resources").await?;
            let prompts: Vec<CatalogRecord> = self.get_json("/prompts").await?;
            let filtered = CatalogIds::from_records(gateway_id, tools, resources, prompts);
            last_missing = filtered.missing_identities();
            if last_missing.is_empty() {
                return Ok(filtered);
            }
            if attempt + 1 < self.max_attempts {
                tokio::time::sleep(self.poll_interval).await;
            }
        }
        Err(anyhow!(redact(
            &format!(
                "conformance catalogs for gateway {gateway_id} remained incomplete; missing: {}",
                last_missing.join(", ")
            ),
            &self.admin_token
        )))
    }

    async fn delete(&self, collection: &str, id: &str) -> Result<()> {
        let path = format!("/{collection}/{id}");
        let response = self.request(Method::DELETE, &path, None).await?;
        if response.status().is_success() || response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(anyhow!(redact(
            &format!(
                "DELETE {path} returned status {}",
                response.status().as_u16()
            ),
            &self.admin_token
        )))
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self.request(Method::GET, path, None).await?;
        self.parse_json(path, response).await
    }

    async fn post_json<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T> {
        let response = self.request(Method::POST, path, Some(body)).await?;
        self.parse_json(path, response).await
    }

    async fn post_success(&self, path: &str, body: Option<&Value>) -> Result<()> {
        let response = self.request(Method::POST, path, body).await?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(anyhow!(redact(
            &format!("POST {path} returned status {}", response.status().as_u16()),
            &self.admin_token
        )))
    }

    async fn parse_json<T: DeserializeOwned>(
        &self,
        path: &str,
        response: reqwest::Response,
    ) -> Result<T> {
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!(redact(
                &format!("request to {path} returned status {}", status.as_u16()),
                &self.admin_token
            )));
        }
        response.json().await.map_err(|error| {
            let message = if error.is_timeout() {
                format!(
                    "response body from {path} timed out after {:?}",
                    self.request_timeout
                )
            } else {
                format!("request to {path} returned invalid JSON")
            };
            anyhow!(redact(&message, &self.admin_token))
        })
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<reqwest::Response> {
        let url = self
            .base_url
            .join(path)
            .map_err(|_| anyhow!("failed to construct admin API URL"))?;
        let mut request = self
            .http
            .request(method.clone(), url)
            .bearer_auth(&self.admin_token);
        if let Some(body) = body {
            request = request.json(body);
        }
        request.send().await.map_err(|error| {
            let message = if error.is_timeout() {
                format!(
                    "{} {path} timed out after {:?}",
                    method.as_str(),
                    self.request_timeout
                )
            } else {
                format!(
                    "{} {path} failed before receiving a response",
                    method.as_str()
                )
            };
            anyhow!(redact(&message, &self.admin_token))
        })
    }
}

#[derive(Deserialize)]
struct GatewayRecord {
    id: String,
    name: String,
    url: String,
    transport: String,
    #[serde(default)]
    description: Option<String>,
}

impl GatewayRecord {
    fn is_owned(&self) -> bool {
        self.name == OFFICIAL_CONFORMANCE_GATEWAY_NAME
            && self.url == OFFICIAL_CONFORMANCE_BACKEND_URL
            && self.transport == GATEWAY_TRANSPORT
            && self.description.as_deref() == Some(GATEWAY_DESCRIPTION)
    }
}

#[derive(Deserialize)]
struct ServerRecord {
    id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
}

impl ServerRecord {
    fn is_owned(&self) -> bool {
        self.name == SERVER_NAME && self.description.as_deref() == Some(SERVER_DESCRIPTION)
    }
}

#[derive(Deserialize)]
struct CatalogRecord {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    uri: String,
    #[serde(default, rename = "gatewayId")]
    gateway_id: Option<String>,
}

struct CatalogIds {
    tool_ids: Vec<String>,
    resource_ids: Vec<String>,
    prompt_ids: Vec<String>,
    has_required_tool: bool,
    has_required_resource: bool,
    has_required_prompt: bool,
}

impl CatalogIds {
    fn from_records(
        gateway_id: &str,
        tools: Vec<CatalogRecord>,
        resources: Vec<CatalogRecord>,
        prompts: Vec<CatalogRecord>,
    ) -> Self {
        let tools: Vec<_> = tools
            .into_iter()
            .filter(|record| record.gateway_id.as_deref() == Some(gateway_id))
            .collect();
        let resources: Vec<_> = resources
            .into_iter()
            .filter(|record| record.gateway_id.as_deref() == Some(gateway_id))
            .collect();
        let prompts: Vec<_> = prompts
            .into_iter()
            .filter(|record| record.gateway_id.as_deref() == Some(gateway_id))
            .collect();
        Self {
            has_required_tool: tools.iter().any(|record| record.name == REQUIRED_TOOL),
            has_required_resource: resources
                .iter()
                .any(|record| record.uri == REQUIRED_RESOURCE),
            has_required_prompt: prompts.iter().any(|record| record.name == REQUIRED_PROMPT),
            tool_ids: tools.into_iter().map(|record| record.id).collect(),
            resource_ids: resources.into_iter().map(|record| record.id).collect(),
            prompt_ids: prompts.into_iter().map(|record| record.id).collect(),
        }
    }

    fn missing_identities(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.tool_ids.is_empty() || !self.has_required_tool {
            missing.push(REQUIRED_TOOL);
        }
        if self.resource_ids.is_empty() || !self.has_required_resource {
            missing.push(REQUIRED_RESOURCE);
        }
        if self.prompt_ids.is_empty() || !self.has_required_prompt {
            missing.push(REQUIRED_PROMPT);
        }
        missing
    }
}

fn validate_base_url(base_url: &str) -> Result<Url> {
    let url = Url::parse(base_url).map_err(|_| anyhow!("base URL is invalid"))?;
    if !matches!(url.scheme(), "http" | "https") || !url.has_host() || url.cannot_be_a_base() {
        return Err(anyhow!(
            "base URL must be an absolute hierarchical HTTP URL"
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(anyhow!("base URL must not contain credentials"));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(anyhow!(
            "base URL must contain only an HTTP origin with a root path"
        ));
    }
    Ok(url)
}

fn is_valid_bearer_token(token: &str) -> bool {
    let unpadded_len = token.find('=').unwrap_or(token.len());
    let (unpadded, padding) = token.split_at(unpadded_len);
    !unpadded.is_empty()
        && unpadded.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
        })
        && padding.bytes().all(|byte| byte == b'=')
}

fn safe_error(error: &anyhow::Error, token: &str) -> String {
    redact(&format!("{error:#}"), token)
}

fn redact(value: &str, token: &str) -> String {
    if token.is_empty() {
        value.to_owned()
    } else {
        value.replace(token, REDACTED)
    }
}
