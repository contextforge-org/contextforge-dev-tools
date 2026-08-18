//! Short-lived loopback proxy that injects authentication for external tools.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::http::header::{
    AUTHORIZATION, CONNECTION, CONTENT_LENGTH, HOST, HeaderName, HeaderValue, ORIGIN,
};
use axum::http::{HeaderMap, Method, Response, StatusCode};
use reqwest::Client;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use url::Url;
use uuid::Uuid;

use crate::backend_identity::{BackendIdentity, is_dataplane_endpoint};

const REDACTED: &str = "<redacted>";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const LOOPBACK_BIND_ADDRESS: &str = "127.0.0.1:0";

/// Maximum request size buffered before a request is sent to the upstream.
pub const MAX_REQUEST_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Failures that can occur while starting or stopping an [`AuthProxy`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuthProxyError {
    /// The upstream is not an absolute HTTP(S) URL without credentials or a fragment.
    #[error("upstream must be an absolute HTTP or HTTPS URL without credentials or a fragment")]
    InvalidUpstream,
    /// The bearer token cannot safely be encoded as an HTTP header.
    #[error("bearer token cannot be encoded as an HTTP Authorization header")]
    InvalidBearerToken,
    /// The loopback listener could not be created.
    #[error("failed to bind loopback authentication proxy")]
    Bind(#[source] std::io::Error),
    /// The HTTP client could not be configured.
    #[error("failed to configure authentication proxy HTTP client")]
    ClientConfiguration,
    /// The generated loopback endpoint could not be represented as a URL.
    #[error("failed to configure authentication proxy endpoint")]
    EndpointConfiguration,
    /// The proxy server failed while running.
    #[error("authentication proxy server failed")]
    Server,
    /// The proxy task stopped unexpectedly.
    #[error("authentication proxy task stopped unexpectedly")]
    Task,
}

struct ProxyState {
    upstream: Url,
    authorization: HeaderValue,
    proxy_path: String,
    loopback_authority: String,
    require_dataplane_backend: bool,
    protocol_version: Option<String>,
    client: Client,
}

/// Running authentication-injection proxy bound to a random IPv4 loopback port.
///
/// The generated endpoint contains a random path and is the only accepted path.
/// Call [`AuthProxy::shutdown`] to stop accepting connections and wait for active
/// connections to finish.
pub struct AuthProxy {
    endpoint: Url,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), AuthProxyError>>>,
}

impl AuthProxy {
    /// Starts a proxy for one fixed upstream and bearer token.
    ///
    /// The listener is always bound to `127.0.0.1` on an operating-system
    /// selected port. Redirect following and environment HTTP proxies are
    /// disabled, and HTTPS uses normal certificate validation.
    ///
    /// # Errors
    ///
    /// Returns an error if the upstream or token is invalid, the HTTP client
    /// cannot be configured, or the loopback listener cannot be bound.
    pub async fn start(
        upstream: Url,
        bearer_token: impl AsRef<str>,
    ) -> Result<Self, AuthProxyError> {
        Self::start_with_protocol_version(upstream, bearer_token, None).await
    }

    /// Starts a proxy that also rewrites MCP initialize requests to one version.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::start`].
    pub async fn start_with_protocol_version(
        upstream: Url,
        bearer_token: impl AsRef<str>,
        protocol_version: Option<&str>,
    ) -> Result<Self, AuthProxyError> {
        validate_upstream(&upstream)?;
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", bearer_token.as_ref()))
            .map_err(|_| AuthProxyError::InvalidBearerToken)?;
        authorization.set_sensitive(true);

        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|_| AuthProxyError::ClientConfiguration)?;
        let listener = TcpListener::bind(LOOPBACK_BIND_ADDRESS)
            .await
            .map_err(AuthProxyError::Bind)?;
        let address = listener.local_addr().map_err(AuthProxyError::Bind)?;
        let loopback_authority = address.to_string();
        let proxy_path = format!("/mcp-auth/{}", Uuid::new_v4().simple());
        let endpoint = Url::parse(&format!("http://{loopback_authority}{proxy_path}"))
            .map_err(|_| AuthProxyError::EndpointConfiguration)?;

        let state = Arc::new(ProxyState {
            require_dataplane_backend: is_dataplane_endpoint(&upstream),
            upstream,
            authorization,
            proxy_path,
            loopback_authority,
            client,
            protocol_version: protocol_version.map(str::to_owned),
        });
        let application = Router::new().fallback(forward).with_state(state);
        let (shutdown, shutdown_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, application)
                .with_graceful_shutdown(async {
                    let _ = shutdown_receiver.await;
                })
                .await
                .map_err(|_| AuthProxyError::Server)
        });

        Ok(Self {
            endpoint,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    /// Returns the unguessable loopback URL external tools should target.
    #[must_use]
    pub fn url(&self) -> &Url {
        &self.endpoint
    }

    /// Gracefully stops the proxy and waits for its server task.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP server failed or its task did not complete
    /// normally.
    pub async fn shutdown(mut self) -> Result<(), AuthProxyError> {
        self.signal_shutdown();
        let Some(task) = self.task.take() else {
            return Err(AuthProxyError::Task);
        };
        task.await.map_err(|_| AuthProxyError::Task)?
    }

    fn signal_shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

impl fmt::Debug for AuthProxy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthProxy")
            .field("endpoint", &REDACTED)
            .field("running", &self.task.is_some())
            .finish()
    }
}

impl Drop for AuthProxy {
    fn drop(&mut self) {
        self.signal_shutdown();
    }
}

async fn forward(State(state): State<Arc<ProxyState>>, request: Request) -> Response<Body> {
    if !matches!(
        *request.method(),
        Method::GET | Method::POST | Method::DELETE
    ) {
        return method_not_allowed();
    }
    if request.uri().path() != state.proxy_path || request.uri().query().is_some() {
        return empty_response(StatusCode::NOT_FOUND);
    }
    let (parts, body) = request.into_parts();
    let body = match to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => return empty_response(StatusCode::PAYLOAD_TOO_LARGE),
    };
    let body = if let Some(protocol_version) = state.protocol_version.as_deref() {
        rewrite_initialize_protocol_version(body, protocol_version)
    } else {
        body
    };
    let mut headers = end_to_end_headers(parts.headers, true);
    if headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|authority| authority == state.loopback_authority)
    {
        // A normal client addresses the loopback shim. Removing that Host lets
        // reqwest synthesize the fixed upstream authority. Deliberately
        // mutated Host values remain untouched for the rebinding scenario.
        headers.remove(HOST);
    }
    rewrite_loopback_origin(&mut headers, &state.loopback_authority, &state.upstream);
    headers.insert(AUTHORIZATION, state.authorization.clone());

    let upstream_response = match state
        .client
        .request(parts.method, state.upstream.clone())
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return empty_response(StatusCode::BAD_GATEWAY),
    };

    if state.require_dataplane_backend
        && BackendIdentity::from_headers(upstream_response.headers())
            .dataplane_error()
            .is_some()
    {
        return empty_response(StatusCode::BAD_GATEWAY);
    }

    let status = upstream_response.status();
    let headers = end_to_end_headers(upstream_response.headers().clone(), false);
    let body = Body::from_stream(upstream_response.bytes_stream());
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

fn rewrite_loopback_origin(headers: &mut HeaderMap, loopback_authority: &str, upstream: &Url) {
    let Some(origin) = headers.get(ORIGIN).and_then(|value| value.to_str().ok()) else {
        return;
    };
    if origin != format!("http://{loopback_authority}") {
        return;
    }
    if let Ok(value) = HeaderValue::from_str(&upstream.origin().ascii_serialization()) {
        headers.insert(ORIGIN, value);
    }
}

fn rewrite_initialize_protocol_version(body: Bytes, protocol_version: &str) -> Bytes {
    let Ok(mut request) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    if request.get("method").and_then(Value::as_str) != Some("initialize") {
        return body;
    }
    let Some(params) = request.get_mut("params").and_then(Value::as_object_mut) else {
        return body;
    };
    params.insert(
        "protocolVersion".to_owned(),
        Value::String(protocol_version.to_owned()),
    );
    serde_json::to_vec(&request).map_or(body, Bytes::from)
}

fn validate_upstream(upstream: &Url) -> Result<(), AuthProxyError> {
    let valid_scheme = matches!(upstream.scheme(), "http" | "https");
    let has_authority = upstream.host().is_some() && !upstream.cannot_be_a_base();
    let has_credentials = !upstream.username().is_empty() || upstream.password().is_some();
    if !valid_scheme || !has_authority || has_credentials || upstream.fragment().is_some() {
        return Err(AuthProxyError::InvalidUpstream);
    }
    Ok(())
}

fn end_to_end_headers(mut headers: HeaderMap, is_request: bool) -> HeaderMap {
    let connection_headers = connection_named_headers(&headers);
    for header in connection_headers {
        headers.remove(header);
    }
    for header in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(header);
    }
    if is_request {
        headers.remove(AUTHORIZATION);
        headers.remove(CONTENT_LENGTH);
    }

    // Origin and deliberately mutated Host values remain end-to-end. The
    // official dns-rebinding-protection scenario mutates both; rejecting
    // either here would create a false pass without exercising the gateway.
    // Loopback-only binding plus the random 128-bit path protects the
    // short-lived shim while the fixed upstream prevents open-proxy behavior.
    headers
}

fn connection_named_headers(headers: &HeaderMap) -> Vec<HeaderName> {
    headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect()
}

fn method_not_allowed() -> Response<Body> {
    let mut response = empty_response(StatusCode::METHOD_NOT_ALLOWED);
    response
        .headers_mut()
        .insert("allow", HeaderValue::from_static("GET, POST, DELETE"));
    response
}

fn empty_response(status: StatusCode) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
}
