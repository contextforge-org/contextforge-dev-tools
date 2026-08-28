//! Reqwest implementation of the public MCP probe transport.

use std::fmt;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use reqwest::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use url::Url;

use crate::mcp::backend_identity::{BackendIdentity, is_dataplane_endpoint};
use crate::mcp::probe::{ProbeRequest, ProbeResponse, ProbeTransport};
use crate::mcp::protocol::{
    ACCEPT as MCP_ACCEPT, is_stateless_protocol, parse_mcp_body, routing_name,
};

const REDACTED: &str = "<redacted>";
/// Maximum response buffered by the small public-route probe.
pub const MAX_MCP_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// HTTPS-validating, redirect-free MCP probe transport.
#[derive(Clone)]
pub struct ReqwestProbeTransport {
    client: Client,
}

impl ReqwestProbeTransport {
    /// Builds a transport with redirects and environment proxies disabled.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be configured.
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .context("failed to configure MCP probe HTTP client")?;
        Ok(Self { client })
    }
}

impl fmt::Debug for ReqwestProbeTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReqwestProbeTransport")
            .field("client", &REDACTED)
            .finish()
    }
}

#[async_trait]
impl ProbeTransport for ReqwestProbeTransport {
    async fn post(&self, request: ProbeRequest) -> Result<ProbeResponse> {
        let endpoint = validated_endpoint(&request.url)?;
        let require_dataplane_backend = is_dataplane_endpoint(&endpoint);
        let payload = serde_json::to_vec(&request.payload)
            .context("failed to encode MCP probe JSON-RPC request")?;
        let mut builder = self
            .client
            .post(endpoint)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .header(ACCEPT, HeaderValue::from_static(MCP_ACCEPT))
            .body(payload);

        if let Some(protocol_version) = request.protocol_version.as_deref() {
            let protocol_version = safe_header(protocol_version, "MCP-Protocol-Version")?;
            builder = builder.header("MCP-Protocol-Version", protocol_version);
        }
        if request
            .protocol_version
            .as_deref()
            .is_some_and(is_stateless_protocol)
            && let Some(method) = request
                .payload
                .get("method")
                .and_then(serde_json::Value::as_str)
        {
            builder = builder.header("MCP-Method", safe_header(method, "MCP-Method")?);
            if let Some(name) = routing_name(method, request.payload.get("params")) {
                builder = builder.header("MCP-Name", safe_header(name, "MCP-Name")?);
            }
        }
        if let Some(token) = request.bearer_token.as_deref() {
            let mut authorization = safe_header(&format!("Bearer {token}"), "Authorization")?;
            authorization.set_sensitive(true);
            builder = builder.header(AUTHORIZATION, authorization);
        }
        if let Some(session_id) = request.session_id.as_deref() {
            let mut session = safe_header(session_id, "MCP-Session-Id")?;
            session.set_sensitive(true);
            builder = builder.header("MCP-Session-Id", session);
        }

        let mut response = builder
            .send()
            .await
            .map_err(|_| anyhow!("MCP probe HTTP request failed"))?;
        let status = response.status();
        let backend_identity = BackendIdentity::from_headers(response.headers());
        if require_dataplane_backend && let Some(message) = backend_identity.dataplane_error() {
            bail!(message);
        }
        let session_id = response
            .headers()
            .get("MCP-Session-Id")
            .map(|value| {
                value
                    .to_str()
                    .map(str::to_owned)
                    .map_err(|_| anyhow!("MCP response has an invalid session header"))
            })
            .transpose()?;

        if !status.is_success() {
            return Ok(ProbeResponse::new(status.as_u16(), session_id, None)
                .with_backend_identity(backend_identity));
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .map(|value| {
                value
                    .to_str()
                    .map(str::to_owned)
                    .map_err(|_| anyhow!("MCP response has an invalid content type"))
            })
            .transpose()?;
        let body = bounded_body(&mut response).await?;
        if body.is_empty() {
            return Ok(ProbeResponse::new(status.as_u16(), session_id, None)
                .with_backend_identity(backend_identity));
        }
        let content_type = content_type
            .as_deref()
            .filter(|value| is_mcp_content_type(value))
            .ok_or_else(|| anyhow!("unsupported MCP response content type"))?;
        let body = std::str::from_utf8(&body)
            .map_err(|_| anyhow!("MCP response body is not valid UTF-8"))?;
        let message = parse_mcp_body(body, content_type)
            .map_err(|_| anyhow!("failed to parse MCP response body"))?;

        Ok(ProbeResponse::new(status.as_u16(), session_id, message)
            .with_backend_identity(backend_identity))
    }
}

fn validated_endpoint(raw: &str) -> Result<Url> {
    let endpoint = Url::parse(raw).map_err(|_| anyhow!("invalid MCP probe URL"))?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || endpoint.host().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.fragment().is_some()
    {
        bail!("invalid MCP probe URL");
    }
    Ok(endpoint)
}

fn safe_header(value: &str, name: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value).map_err(|_| anyhow!("invalid {name} header value"))
}

async fn bounded_body(response: &mut reqwest::Response) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("failed to read MCP response body"))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_MCP_RESPONSE_BYTES {
            bail!("MCP response body exceeds safety limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn is_mcp_content_type(value: &str) -> bool {
    let media_type = value
        .split_once(';')
        .map_or(value, |(media_type, _)| media_type)
        .trim();
    media_type.eq_ignore_ascii_case("application/json")
        || media_type.eq_ignore_ascii_case("text/event-stream")
}
