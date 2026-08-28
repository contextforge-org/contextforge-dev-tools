//! Reusable live MCP gateway client.

use std::collections::BTreeMap;
use std::fmt;

use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
#[cfg(test)]
use serde_json::Map;
use serde_json::Value;
use thiserror::Error;
use url::Url;

use crate::mcp::GatewayTopology;

use crate::mcp::backend_identity::{BACKEND_HEADER, BackendIdentity, sanitized_backend_value};
use crate::mcp::protocol::{
    ACCEPT as MCP_ACCEPT, PROTOCOL_VERSION, is_stateless_protocol, parse_mcp_body, routing_name,
};
#[cfg(test)]
use crate::mcp::protocol::{initialize_with_id_and_version, jsonrpc_with_id};

/// Default MCP protocol version used in request bodies and HTTP headers.
pub(crate) const DEFAULT_PROTOCOL_VERSION: &str = PROTOCOL_VERSION;
/// MCP protocol-version HTTP header.
pub(crate) const MCP_PROTOCOL_VERSION: &str = "mcp-protocol-version";
/// MCP streamable-HTTP session header.
pub(crate) const MCP_SESSION_ID: &str = "mcp-session-id";

const JSON_CONTENT_TYPE: &str = "application/json";
const SSE_ACCEPT: &str = "text/event-stream";
const REDACTED: &str = "<redacted>";
/// Maximum response body buffered by the MCP client.
pub(crate) const MAX_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;

#[cfg(test)]
#[path = "gateway_probe_tests.rs"]
mod probe_tests;

/// Controls one request header without changing the client's stored defaults.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) enum HeaderOverride {
    /// Use the client protocol version or response-derived session ID.
    #[default]
    Automatic,
    /// Omit the header for a negative compliance case.
    Omit,
    /// Send this exact value for an invalid-version or invalid-session case.
    Value(String),
}

impl fmt::Debug for HeaderOverride {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Automatic => formatter.write_str("Automatic"),
            Self::Omit => formatter.write_str("Omit"),
            Self::Value(_) => formatter.debug_tuple("Value").field(&REDACTED).finish(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Payload {
    #[cfg(test)]
    Initialize {
        id: Value,
    },
    Json(Value),
    #[cfg(test)]
    Raw(Vec<u8>),
    #[cfg(test)]
    None,
}

#[derive(Clone, PartialEq)]
enum ResponseExpectation {
    #[cfg(test)]
    JsonRpc {
        id: Value,
    },
    #[cfg(test)]
    NotificationAccepted,
    Unchecked,
}

impl fmt::Debug for ResponseExpectation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(test)]
            Self::JsonRpc { .. } => formatter.write_str("JsonRpc { id: <redacted> }"),
            #[cfg(test)]
            Self::NotificationAccepted => formatter.write_str("NotificationAccepted"),
            Self::Unchecked => formatter.write_str("Unchecked"),
        }
    }
}

/// One protected HTTP exchange to issue against the public MCP endpoint.
#[derive(Clone, PartialEq)]
pub(crate) struct GatewayRequest {
    method: Method,
    payload: Payload,
    authorization: HeaderOverride,
    protocol_version: HeaderOverride,
    session: HeaderOverride,
    expectation: ResponseExpectation,
}

impl fmt::Debug for GatewayRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let payload = match &self.payload {
            #[cfg(test)]
            Payload::Initialize { .. } => "initialize",
            Payload::Json(_) => "json:<redacted>",
            #[cfg(test)]
            Payload::Raw(_) => "raw:<redacted>",
            #[cfg(test)]
            Payload::None => "none",
        };
        formatter
            .debug_struct("GatewayRequest")
            .field("method", &self.method)
            .field("payload", &payload)
            .field("authorization", &self.authorization)
            .field("protocol_version", &self.protocol_version)
            .field("session", &self.session)
            .field("expectation", &self.expectation)
            .finish()
    }
}

impl GatewayRequest {
    /// Builds an MCP initialize request.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn initialize(id: Value) -> Self {
        let mut request = Self::post(
            Payload::Initialize { id: id.clone() },
            ResponseExpectation::JsonRpc { id },
        );
        // The negotiated version header is required on subsequent HTTP
        // requests, not on the initialize request that establishes it.
        request.protocol_version = HeaderOverride::Omit;
        request
    }

    /// Builds the required `notifications/initialized` notification.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn initialized() -> Self {
        Self::notification("notifications/initialized", None)
    }

    /// Builds a generic JSON-RPC request whose response must match `id`.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn request(method: &str, params: Option<Value>, id: Value) -> Self {
        Self::post(
            Payload::Json(jsonrpc_with_id(method, params, id.clone())),
            ResponseExpectation::JsonRpc { id },
        )
    }

    /// Builds a generic JSON-RPC notification.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn notification(method: &str, params: Option<Value>) -> Self {
        Self::post(
            Payload::Json(notification_message(method, params)),
            ResponseExpectation::NotificationAccepted,
        )
    }

    /// Builds an unchecked streamable-HTTP GET request.
    ///
    /// The response body is intentionally not consumed because a successful
    /// Streamable HTTP GET can remain open indefinitely. The returned exchange
    /// contains the status and headers with an empty body.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn get() -> Self {
        Self {
            method: Method::GET,
            payload: Payload::None,
            authorization: HeaderOverride::Automatic,
            protocol_version: HeaderOverride::Automatic,
            session: HeaderOverride::Automatic,
            expectation: ResponseExpectation::Unchecked,
        }
    }

    /// Builds an unchecked streamable-HTTP DELETE request.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn delete() -> Self {
        Self {
            method: Method::DELETE,
            payload: Payload::None,
            authorization: HeaderOverride::Automatic,
            protocol_version: HeaderOverride::Automatic,
            session: HeaderOverride::Automatic,
            expectation: ResponseExpectation::Unchecked,
        }
    }

    /// Builds an unchecked JSON POST with an arbitrary, potentially malformed body.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn raw_post(body: impl AsRef<[u8]>) -> Self {
        Self::post(
            Payload::Raw(body.as_ref().to_vec()),
            ResponseExpectation::Unchecked,
        )
    }

    /// Builds an unchecked JSON POST for workflow-level validation.
    #[must_use]
    pub(crate) fn probe(payload: Value) -> Self {
        Self::post(Payload::Json(payload), ResponseExpectation::Unchecked)
    }

    /// Overrides or omits the configured authorization header.
    #[must_use]
    pub(crate) fn authorization(mut self, authorization: HeaderOverride) -> Self {
        self.authorization = authorization;
        self
    }

    /// Overrides or omits the configured protocol-version header.
    #[must_use]
    pub(crate) fn protocol_version(mut self, protocol_version: HeaderOverride) -> Self {
        self.protocol_version = protocol_version;
        self
    }

    /// Overrides or omits the response-derived session header.
    #[must_use]
    pub(crate) fn session(mut self, session: HeaderOverride) -> Self {
        self.session = session;
        self
    }

    fn post(payload: Payload, expectation: ResponseExpectation) -> Self {
        Self {
            method: Method::POST,
            payload,
            authorization: HeaderOverride::Automatic,
            protocol_version: HeaderOverride::Automatic,
            session: HeaderOverride::Automatic,
            expectation,
        }
    }
}

/// Builds a JSON-RPC 2.0 notification without an `id` member.
#[must_use]
#[cfg(test)]
pub(crate) fn notification_message(method: &str, params: Option<Value>) -> Value {
    let mut payload = Map::new();
    payload.insert("jsonrpc".to_owned(), Value::String("2.0".to_owned()));
    payload.insert("method".to_owned(), Value::String(method.to_owned()));
    if let Some(params) = params {
        payload.insert("params".to_owned(), params);
    }
    Value::Object(payload)
}

/// Safe diagnostic snapshot of an outbound HTTP request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RequestCapture {
    mode: GatewayTopology,
    method: String,
    url: String,
    headers: BTreeMap<String, String>,
    body: Option<String>,
}

impl RequestCapture {
    /// Stack mode used for this request.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn mode(&self) -> GatewayTopology {
        self.mode
    }

    /// Sanitized headers; authentication, cookies, and sessions are redacted.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn headers(&self) -> &BTreeMap<String, String> {
        &self.headers
    }

    /// Sanitized request body.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn body(&self) -> Option<&str> {
        self.body.as_deref()
    }
}

/// Complete safe diagnostic record for one live gateway exchange.
#[derive(Clone, PartialEq)]
pub(crate) struct Exchange {
    mode: GatewayTopology,
    request: RequestCapture,
    status: u16,
    headers: BTreeMap<String, String>,
    body: String,
    message: Option<Value>,
    session_id: Option<String>,
}

impl fmt::Debug for Exchange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Exchange")
            .field("mode", &self.mode)
            .field("request", &self.request)
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body", &self.body)
            .field("message", &self.message.as_ref().map(|_| "<parsed>"))
            .field("session_id", &self.session_id.as_ref().map(|_| REDACTED))
            .finish()
    }
}

impl Exchange {
    /// Stack mode used for this exchange.
    #[must_use]
    pub(crate) fn mode(&self) -> GatewayTopology {
        self.mode
    }

    /// Safe outbound request diagnostic.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn request(&self) -> &RequestCapture {
        &self.request
    }

    /// HTTP status code.
    #[must_use]
    pub(crate) fn status(&self) -> u16 {
        self.status
    }

    /// Sanitized response headers.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn headers(&self) -> &BTreeMap<String, String> {
        &self.headers
    }

    /// Sanitized response body. This is empty for GET requests because an SSE
    /// stream can remain open indefinitely and is not consumed by this client.
    #[must_use]
    pub(crate) fn body(&self) -> &str {
        &self.body
    }

    /// Parsed JSON-RPC response, when the body uses JSON or SSE.
    #[must_use]
    pub(crate) fn message(&self) -> Option<&Value> {
        self.message.as_ref()
    }

    /// Session returned by this response header, if any.
    #[must_use]
    pub(crate) fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

/// Builder for [`GatewayClient`].
#[must_use = "a gateway client builder does nothing until build() is called"]
#[derive(Clone)]
pub(crate) struct GatewayClientBuilder {
    mode: GatewayTopology,
    base_url: String,
    server_id: String,
    bearer_token: String,
    protocol_version: String,
}

impl fmt::Debug for GatewayClientBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let base_url = redact_and_sanitize(&self.base_url, &self.bearer_token);
        let server_id = redact_and_sanitize(&self.server_id, &self.bearer_token);
        let protocol_version = redact_and_sanitize(&self.protocol_version, &self.bearer_token);
        formatter
            .debug_struct("GatewayClientBuilder")
            .field("mode", &self.mode)
            .field("base_url", &base_url)
            .field("server_id", &server_id)
            .field("bearer_token", &REDACTED)
            .field("protocol_version", &protocol_version)
            .finish()
    }
}

impl GatewayClientBuilder {
    /// Selects a non-default MCP protocol version.
    pub(crate) fn protocol_version(mut self, protocol_version: impl Into<String>) -> Self {
        self.protocol_version = protocol_version.into();
        self
    }

    /// Validates the endpoint and headers and creates the client.
    ///
    /// # Errors
    ///
    /// Returns a mode-aware configuration error for an invalid base URL,
    /// endpoint, bearer token, or protocol-version header.
    pub(crate) fn build(self) -> Result<GatewayClient, GatewayError> {
        if self.bearer_token.is_empty() {
            return Err(GatewayError::configuration(
                self.mode,
                "bearer token must not be empty",
            ));
        }
        validate_header_value(
            self.mode,
            "Authorization",
            &format!("Bearer {}", self.bearer_token),
        )?;
        if self.protocol_version.trim().is_empty() {
            return Err(GatewayError::configuration(
                self.mode,
                "protocol version must not be empty",
            ));
        }
        validate_header_value(self.mode, MCP_PROTOCOL_VERSION, &self.protocol_version)?;
        let endpoint = gateway_endpoint(self.mode, &self.base_url, &self.server_id)?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|error| {
                GatewayError::configuration(
                    self.mode,
                    redact_and_sanitize(&error.to_string(), &self.bearer_token),
                )
            })?;

        Ok(GatewayClient {
            mode: self.mode,
            endpoint,
            bearer_token: self.bearer_token,
            protocol_version: self.protocol_version,
            session_id: None,
            http,
        })
    }
}

/// Stateful client for the protected public MCP gateway route.
#[derive(Clone)]
pub(crate) struct GatewayClient {
    mode: GatewayTopology,
    endpoint: Url,
    bearer_token: String,
    protocol_version: String,
    session_id: Option<String>,
    http: reqwest::Client,
}

impl fmt::Debug for GatewayClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let endpoint = redact_and_sanitize(self.endpoint.as_str(), &self.bearer_token);
        let protocol_version = redact_and_sanitize(&self.protocol_version, &self.bearer_token);
        formatter
            .debug_struct("GatewayClient")
            .field("mode", &self.mode)
            .field("endpoint", &endpoint)
            .field("bearer_token", &REDACTED)
            .field("protocol_version", &protocol_version)
            .field("session_id", &self.session_id.as_ref().map(|_| REDACTED))
            .finish_non_exhaustive()
    }
}

impl GatewayClient {
    /// Creates a gateway client using [`DEFAULT_PROTOCOL_VERSION`].
    ///
    /// # Errors
    ///
    /// Returns a mode-aware configuration error for invalid inputs.
    pub(crate) fn new(
        mode: GatewayTopology,
        base_url: &str,
        server_id: &str,
        bearer_token: &str,
    ) -> Result<Self, GatewayError> {
        Self::builder(mode, base_url, server_id, bearer_token).build()
    }

    /// Starts a configurable gateway client builder.
    pub(crate) fn builder(
        mode: GatewayTopology,
        base_url: &str,
        server_id: &str,
        bearer_token: &str,
    ) -> GatewayClientBuilder {
        GatewayClientBuilder {
            mode,
            base_url: base_url.to_owned(),
            server_id: server_id.to_owned(),
            bearer_token: bearer_token.to_owned(),
            protocol_version: DEFAULT_PROTOCOL_VERSION.to_owned(),
        }
    }

    /// Fixed percent-encoded public MCP endpoint.
    #[must_use]
    pub(crate) fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Most recent non-empty session ID received in a successful response.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Sends and validates one protected gateway request.
    ///
    /// JSON-RPC request builders require a successful HTTP response and validate
    /// the version, ID, and result/error shape. Notification builders require an
    /// empty `202 Accepted` response. GET, DELETE, raw, and explicitly unchecked
    /// requests return any HTTP status for scenario-level assertions.
    ///
    /// # Errors
    ///
    /// Returns a mode-aware error with a safe request or full exchange capture
    /// for header, transport, body parsing, status, or JSON-RPC failures.
    pub(crate) async fn send(&mut self, request: GatewayRequest) -> Result<Exchange, GatewayError> {
        let body = materialize_payload(self.mode, &request.payload, &self.protocol_version)?;
        let outbound = self.build_request(&request, body.as_deref())?;
        let outbound_session = outbound
            .headers()
            .get(MCP_SESSION_ID)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let request_capture = capture_request(self.mode, &outbound, &self.bearer_token);
        let mut response = self.http.execute(outbound).await.map_err(|error| {
            GatewayError::request(
                self.mode,
                redact_and_sanitize(&error.to_string(), &self.bearer_token),
                request_capture.clone(),
            )
        })?;

        let status = response.status();
        let raw_headers = response.headers().clone();
        let session_result = response_session(&raw_headers);
        let response_session = session_result
            .as_ref()
            .ok()
            .and_then(|value| value.as_deref());
        let response_secrets = [
            self.bearer_token.as_str(),
            outbound_session.as_deref().unwrap_or(""),
            response_session.unwrap_or(""),
        ];
        if self.mode.requires_dataplane()
            && let Some(message) = BackendIdentity::from_headers(&raw_headers).dataplane_error()
        {
            let exchange = Exchange {
                mode: self.mode,
                request: request_capture,
                status: status.as_u16(),
                headers: capture_headers(&raw_headers, &response_secrets),
                body: "<response body rejected before reading>".to_owned(),
                message: None,
                session_id: None,
            };
            return Err(GatewayError::with_exchange(self.mode, message, exchange));
        }
        if request.method == Method::GET {
            let session_id = session_result.as_ref().ok().cloned().flatten();
            let exchange = Exchange {
                mode: self.mode,
                request: request_capture,
                status: status.as_u16(),
                headers: capture_headers(&raw_headers, &response_secrets),
                body: String::new(),
                message: None,
                session_id,
            };
            if let Err(message) = session_result {
                return Err(GatewayError::with_exchange(self.mode, message, exchange));
            }
            self.validate_exchange(&request.expectation, status, Ok(None), &exchange)?;
            if status.is_success()
                && let Some(session_id) = exchange.session_id.as_ref()
            {
                self.session_id = Some(session_id.clone());
            }
            return Ok(exchange);
        }
        let raw_body = match bounded_response_body(&mut response).await {
            Ok(body) => body,
            Err(error) => {
                let exchange = Exchange {
                    mode: self.mode,
                    request: request_capture,
                    status: status.as_u16(),
                    headers: capture_headers(&raw_headers, &response_secrets),
                    body: "<response body unavailable>".to_owned(),
                    message: None,
                    session_id: None,
                };
                return Err(GatewayError::with_exchange(
                    self.mode,
                    redact_and_sanitize(&error.to_string(), &self.bearer_token),
                    exchange,
                ));
            }
        };
        let session_id = session_result.as_ref().ok().cloned().flatten();
        let parsed = parse_response_body(&raw_body, &raw_headers);
        let message = parsed.as_ref().ok().cloned().flatten();
        let exchange = Exchange {
            mode: self.mode,
            request: request_capture,
            status: status.as_u16(),
            headers: capture_headers(&raw_headers, &response_secrets),
            body: redact_and_sanitize_secrets(
                &String::from_utf8_lossy(&raw_body),
                &response_secrets,
            ),
            message,
            session_id,
        };

        if let Err(message) = session_result {
            return Err(GatewayError::with_exchange(self.mode, message, exchange));
        }

        self.validate_exchange(&request.expectation, status, parsed, &exchange)?;
        if status.is_success()
            && let Some(session_id) = exchange.session_id.as_ref()
        {
            self.session_id = Some(session_id.clone());
        }
        Ok(exchange)
    }

    fn build_request(
        &self,
        request: &GatewayRequest,
        body: Option<&[u8]>,
    ) -> Result<reqwest::Request, GatewayError> {
        let mut builder = self
            .http
            .request(request.method.clone(), self.endpoint.clone())
            .header(
                ACCEPT,
                if request.method == Method::GET {
                    SSE_ACCEPT
                } else {
                    MCP_ACCEPT
                },
            );
        let automatic_authorization = format!("Bearer {}", self.bearer_token);
        builder = apply_header(
            self.mode,
            builder,
            "Authorization",
            &request.authorization,
            Some(&automatic_authorization),
        )?;
        if request.method == Method::POST {
            builder = builder.header(CONTENT_TYPE, JSON_CONTENT_TYPE);
        }
        builder = apply_header(
            self.mode,
            builder,
            MCP_PROTOCOL_VERSION,
            &request.protocol_version,
            Some(&self.protocol_version),
        )?;
        if request.method == Method::POST {
            let protocol_version = match &request.protocol_version {
                HeaderOverride::Automatic => Some(self.protocol_version.as_str()),
                HeaderOverride::Omit => None,
                HeaderOverride::Value(value) => Some(value.as_str()),
            };
            if protocol_version.is_some_and(is_stateless_protocol)
                && let Payload::Json(payload) = &request.payload
                && let Some(method) = payload.get("method").and_then(Value::as_str)
            {
                builder = apply_literal_header(self.mode, builder, "mcp-method", method)?;
                if let Some(name) = routing_name(method, payload.get("params")) {
                    builder = apply_literal_header(self.mode, builder, "mcp-name", name)?;
                }
            }
        }
        builder = apply_header(
            self.mode,
            builder,
            MCP_SESSION_ID,
            &request.session,
            self.session_id.as_deref(),
        )?;
        if let Some(body) = body {
            builder = builder.body(body.to_vec());
        }
        builder.build().map_err(|error| {
            GatewayError::configuration(
                self.mode,
                redact_and_sanitize(&error.to_string(), &self.bearer_token),
            )
        })
    }

    fn validate_exchange(
        &self,
        expectation: &ResponseExpectation,
        status: StatusCode,
        parsed: Result<Option<Value>, String>,
        exchange: &Exchange,
    ) -> Result<(), GatewayError> {
        match expectation {
            #[cfg(test)]
            ResponseExpectation::JsonRpc { id } => {
                if status != StatusCode::OK {
                    return Err(GatewayError::with_exchange(
                        self.mode,
                        format!(
                            "JSON-RPC response expected HTTP 200, got status {}",
                            status.as_u16()
                        ),
                        exchange.clone(),
                    ));
                }
                let message = parsed
                    .map_err(|message| {
                        GatewayError::with_exchange(self.mode, message, exchange.clone())
                    })?
                    .ok_or_else(|| {
                        GatewayError::with_exchange(
                            self.mode,
                            "response did not contain a JSON or SSE message",
                            exchange.clone(),
                        )
                    })?;
                validate_jsonrpc_response(&message, id).map_err(|message| {
                    GatewayError::with_exchange(self.mode, message, exchange.clone())
                })?;
            }
            #[cfg(test)]
            ResponseExpectation::NotificationAccepted => {
                if status != StatusCode::ACCEPTED {
                    return Err(GatewayError::with_exchange(
                        self.mode,
                        format!(
                            "notification expected status 202, got status {}",
                            status.as_u16()
                        ),
                        exchange.clone(),
                    ));
                }
                if !exchange.body.is_empty() {
                    return Err(GatewayError::with_exchange(
                        self.mode,
                        "notification response body must be empty",
                        exchange.clone(),
                    ));
                }
            }
            ResponseExpectation::Unchecked => {
                if status.is_success()
                    && let Err(message) = parsed
                {
                    return Err(GatewayError::with_exchange(
                        self.mode,
                        message,
                        exchange.clone(),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn apply_literal_header(
    mode: GatewayTopology,
    builder: reqwest::RequestBuilder,
    name: &'static str,
    value: &str,
) -> Result<reqwest::RequestBuilder, GatewayError> {
    validate_header_value(mode, name, value)?;
    Ok(builder.header(name, value))
}

async fn bounded_response_body(response: &mut reqwest::Response) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "failed to read response body".to_owned())?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BODY_BYTES {
            return Err("response body exceeds safety limit".to_owned());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Mode-aware gateway client failure.
#[derive(Debug, Error)]
pub(crate) enum GatewayError {
    /// Invalid local client configuration.
    #[error("gateway {mode:?}: configuration error: {message}")]
    Configuration {
        /// Stack mode for the failed operation.
        mode: GatewayTopology,
        /// Safe failure detail.
        message: String,
    },
    /// Request construction or transport failure before a response was captured.
    #[error("gateway {mode:?}: request failed: {message}; request={request:?}")]
    Request {
        /// Stack mode for the failed operation.
        mode: GatewayTopology,
        /// Safe failure detail.
        message: String,
        /// Safe outbound request capture.
        request: Box<RequestCapture>,
    },
    /// Status, body, or protocol failure with a complete exchange capture.
    #[error("gateway {mode:?}: {message}; exchange={exchange:?}")]
    Exchange {
        /// Stack mode for the failed operation.
        mode: GatewayTopology,
        /// Safe failure detail.
        message: String,
        /// Complete safe exchange capture.
        exchange: Box<Exchange>,
    },
}

impl GatewayError {
    /// Stack mode in which the failure occurred.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn mode(&self) -> GatewayTopology {
        match self {
            Self::Configuration { mode, .. }
            | Self::Request { mode, .. }
            | Self::Exchange { mode, .. } => *mode,
        }
    }

    /// Full exchange for response-time failures.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn exchange(&self) -> Option<&Exchange> {
        match self {
            Self::Exchange { exchange, .. } => Some(exchange),
            Self::Configuration { .. } | Self::Request { .. } => None,
        }
    }

    fn configuration(mode: GatewayTopology, message: impl Into<String>) -> Self {
        Self::Configuration {
            mode,
            message: message.into(),
        }
    }

    fn request(mode: GatewayTopology, message: impl Into<String>, request: RequestCapture) -> Self {
        Self::Request {
            mode,
            message: message.into(),
            request: Box::new(request),
        }
    }

    fn with_exchange(
        mode: GatewayTopology,
        message: impl Into<String>,
        exchange: Exchange,
    ) -> Self {
        Self::Exchange {
            mode,
            message: message.into(),
            exchange: Box::new(exchange),
        }
    }
}

fn gateway_endpoint(
    mode: GatewayTopology,
    base_url: &str,
    server_id: &str,
) -> Result<Url, GatewayError> {
    if server_id.is_empty() {
        return Err(GatewayError::configuration(
            mode,
            "server ID must not be empty",
        ));
    }
    let mut endpoint = Url::parse(base_url)
        .map_err(|_| GatewayError::configuration(mode, "base URL is invalid"))?;
    if endpoint.cannot_be_a_base() || !endpoint.has_host() {
        return Err(GatewayError::configuration(
            mode,
            "base URL must be an absolute hierarchical HTTP URL",
        ));
    }
    if !matches!(endpoint.scheme(), "http" | "https") {
        return Err(GatewayError::configuration(
            mode,
            "base URL scheme must be http or https",
        ));
    }
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        return Err(GatewayError::configuration(
            mode,
            "base URL must not contain credentials",
        ));
    }
    endpoint.set_query(None);
    endpoint.set_fragment(None);
    let mut segments = endpoint
        .path_segments_mut()
        .map_err(|()| GatewayError::configuration(mode, "base URL cannot contain path segments"))?;
    segments.clear();
    if mode.requires_dataplane() {
        segments.push("servers");
        segments.push(server_id);
    }
    segments.push("mcp");
    drop(segments);
    Ok(endpoint)
}

fn materialize_payload(
    mode: GatewayTopology,
    payload: &Payload,
    _protocol_version: &str,
) -> Result<Option<Vec<u8>>, GatewayError> {
    let value = match payload {
        #[cfg(test)]
        Payload::Initialize { id } => Some(initialize_with_id_and_version(
            id.clone(),
            _protocol_version,
        )),
        Payload::Json(value) => Some(value.clone()),
        #[cfg(test)]
        Payload::Raw(body) => return Ok(Some(body.clone())),
        #[cfg(test)]
        Payload::None => None,
    };
    value
        .map(|value| {
            serde_json::to_vec(&value)
                .map_err(|_| GatewayError::configuration(mode, "failed to serialize JSON request"))
        })
        .transpose()
}

fn apply_header(
    mode: GatewayTopology,
    mut builder: reqwest::RequestBuilder,
    name: &'static str,
    header_override: &HeaderOverride,
    automatic: Option<&str>,
) -> Result<reqwest::RequestBuilder, GatewayError> {
    let value = match header_override {
        HeaderOverride::Automatic => automatic,
        HeaderOverride::Omit => None,
        HeaderOverride::Value(value) => Some(value.as_str()),
    };
    if let Some(value) = value {
        validate_header_value(mode, name, value)?;
        builder = builder.header(name, value);
    }
    Ok(builder)
}

fn validate_header_value(
    mode: GatewayTopology,
    name: &str,
    value: &str,
) -> Result<(), GatewayError> {
    HeaderValue::from_str(value)
        .map(|_| ())
        .map_err(|_| GatewayError::configuration(mode, format!("{name} header value is invalid")))
}

fn capture_request(
    mode: GatewayTopology,
    request: &reqwest::Request,
    token: &str,
) -> RequestCapture {
    let session = request
        .headers()
        .get(MCP_SESSION_ID)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let secrets = [token, session];
    RequestCapture {
        mode,
        method: request.method().to_string(),
        url: redact_and_sanitize_secrets(request.url().as_str(), &secrets),
        headers: capture_headers(request.headers(), &secrets),
        body: request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .map(|body| redact_and_sanitize_secrets(&String::from_utf8_lossy(body), &secrets)),
    }
}

fn capture_headers(headers: &HeaderMap, secrets: &[&str]) -> BTreeMap<String, String> {
    let mut captured = BTreeMap::new();
    for (name, value) in headers {
        let name_text = name.as_str();
        let value = if name.as_str().eq_ignore_ascii_case(BACKEND_HEADER) {
            sanitized_backend_value(value).to_owned()
        } else if is_sensitive_header(name) {
            REDACTED.to_owned()
        } else {
            value.to_str().map_or_else(
                |_| "<non-visible-ascii>".to_owned(),
                |value| redact_and_sanitize_secrets(value, secrets),
            )
        };
        captured
            .entry(name_text.to_owned())
            .and_modify(|existing: &mut String| {
                existing.push_str(", ");
                existing.push_str(&value);
            })
            .or_insert(value);
    }
    captured
}

fn is_sensitive_header(name: &HeaderName) -> bool {
    name == AUTHORIZATION
        || name.as_str().eq_ignore_ascii_case("proxy-authorization")
        || name.as_str().eq_ignore_ascii_case("cookie")
        || name.as_str().eq_ignore_ascii_case("set-cookie")
        || name.as_str().eq_ignore_ascii_case(MCP_SESSION_ID)
}

fn response_session(headers: &HeaderMap) -> Result<Option<String>, String> {
    let mut values = headers.get_all(MCP_SESSION_ID).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err("response contains multiple MCP session headers".to_owned());
    }
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return Err("response MCP session header must not be empty".to_owned());
    }
    if !bytes.iter().all(|byte| (0x21..=0x7e).contains(byte)) {
        return Err(
            "response MCP session header must contain only visible ASCII (0x21-0x7E)".to_owned(),
        );
    }
    let value = std::str::from_utf8(bytes)
        .map_err(|_| "response MCP session header is not visible ASCII".to_owned())?;
    Ok(Some(value.to_owned()))
}

fn parse_response_body(body: &[u8], headers: &HeaderMap) -> Result<Option<Value>, String> {
    if body.is_empty() {
        return Ok(None);
    }
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let media_type = content_type
        .split_once(';')
        .map_or(content_type, |(media_type, _)| media_type)
        .trim();
    if !media_type.eq_ignore_ascii_case(JSON_CONTENT_TYPE)
        && !media_type.eq_ignore_ascii_case(SSE_ACCEPT)
    {
        return Ok(None);
    }
    let body =
        std::str::from_utf8(body).map_err(|_| "response body is not valid UTF-8".to_owned())?;
    parse_mcp_body(body, content_type)
        .map_err(|_| "response body is not valid JSON or SSE".to_owned())
}

#[cfg(test)]
fn validate_jsonrpc_response(message: &Value, expected_id: &Value) -> Result<(), String> {
    let object = message
        .as_object()
        .ok_or_else(|| "JSON-RPC response must be an object".to_owned())?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err("invalid JSON-RPC version".to_owned());
    }
    if object.get("id") != Some(expected_id) {
        return Err("JSON-RPC response id does not match request id".to_owned());
    }
    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");
    if has_result == has_error {
        return Err("JSON-RPC response must contain exactly one of result or error".to_owned());
    }
    if let Some(error) = object.get("error") {
        let error = error
            .as_object()
            .ok_or_else(|| "JSON-RPC error object must be an object".to_owned())?;
        if error.get("code").and_then(Value::as_i64).is_none()
            || error.get("message").and_then(Value::as_str).is_none()
        {
            return Err(
                "JSON-RPC error object must contain an integer code and string message".to_owned(),
            );
        }
    }
    Ok(())
}

fn redact_and_sanitize(value: &str, token: &str) -> String {
    redact_and_sanitize_secrets(value, &[token])
}

fn redact_and_sanitize_secrets(value: &str, secrets: &[&str]) -> String {
    let mut redacted = value.to_owned();
    for secret in secrets.iter().copied().filter(|secret| !secret.is_empty()) {
        redacted = redacted.replace(secret, REDACTED);
    }
    let mut sanitized = String::with_capacity(redacted.len());
    for character in redacted.chars() {
        match character {
            '\n' => sanitized.push_str("\\n"),
            '\r' => sanitized.push_str("\\r"),
            '\t' => sanitized.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write;

                let _ = write!(sanitized, "\\u{{{:04x}}}", character as u32);
            }
            character => sanitized.push(character),
        }
    }
    sanitized
}
