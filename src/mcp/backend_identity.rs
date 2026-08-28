//! Trusted response marker for the harness dataplane route.

use reqwest::header::{HeaderMap, HeaderValue};
use url::Url;

/// Response header set by the harness-owned nginx boundary.
pub const BACKEND_HEADER: &str = "x-cf-integration-backend";
/// Marker for a response served by `cf-dataplane`.
pub const DATAPLANE_BACKEND: &str = "dataplane";
/// Marker for a `/servers/.../mcp` response replayed on the control plane.
pub const CONTROLPLANE_FALLBACK_BACKEND: &str = "controlplane-fallback";
/// Marker for a raw control-plane response in the dataplane topology.
pub const CONTROLPLANE_BACKEND: &str = "controlplane";

/// Parsed backend identity without retaining untrusted header contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendIdentity {
    /// The response did not contain the marker.
    Missing,
    /// The response came from the dataplane route.
    Dataplane,
    /// The dataplane route fell back to the control plane.
    ControlplaneFallback,
    /// The raw route used the control plane in a dataplane stack.
    Controlplane,
    /// The response contained an unknown marker value.
    Invalid,
    /// The response contained more than one marker value.
    Multiple,
}

impl BackendIdentity {
    /// Parses exactly one backend marker from response headers.
    #[must_use]
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let mut values = headers.get_all(BACKEND_HEADER).iter();
        let Some(value) = values.next() else {
            return Self::Missing;
        };
        if values.next().is_some() {
            return Self::Multiple;
        }
        Self::from_value(value)
    }

    /// Returns a static dataplane validation failure without reflecting an
    /// untrusted header value.
    #[must_use]
    pub const fn dataplane_error(self) -> Option<&'static str> {
        match self {
            Self::Dataplane => None,
            Self::Missing => Some("dataplane response backend marker is missing"),
            Self::ControlplaneFallback => {
                Some("dataplane response backend marker identifies controlplane fallback")
            }
            Self::Controlplane => Some("dataplane response backend marker identifies controlplane"),
            Self::Invalid => Some("dataplane response backend marker is invalid"),
            Self::Multiple => Some("dataplane response backend marker is duplicated"),
        }
    }

    fn from_value(value: &HeaderValue) -> Self {
        match value.as_bytes() {
            value if value == DATAPLANE_BACKEND.as_bytes() => Self::Dataplane,
            value if value == CONTROLPLANE_FALLBACK_BACKEND.as_bytes() => {
                Self::ControlplaneFallback
            }
            value if value == CONTROLPLANE_BACKEND.as_bytes() => Self::Controlplane,
            _ => Self::Invalid,
        }
    }
}

/// Returns a safe diagnostic representation of one backend marker value.
#[must_use]
pub fn sanitized_backend_value(value: &HeaderValue) -> &'static str {
    match BackendIdentity::from_value(value) {
        BackendIdentity::Dataplane => DATAPLANE_BACKEND,
        BackendIdentity::ControlplaneFallback => CONTROLPLANE_FALLBACK_BACKEND,
        BackendIdentity::Controlplane => CONTROLPLANE_BACKEND,
        BackendIdentity::Missing | BackendIdentity::Invalid | BackendIdentity::Multiple => {
            "<invalid>"
        }
    }
}

/// Returns whether a URL is the fixed public dataplane MCP route.
#[must_use]
pub fn is_dataplane_endpoint(endpoint: &Url) -> bool {
    let Some(mut segments) = endpoint.path_segments() else {
        return false;
    };
    matches!(segments.next(), Some("servers"))
        && segments
            .next()
            .is_some_and(|server_id| !server_id.is_empty())
        && matches!(segments.next(), Some("mcp"))
        && segments.next().is_none()
}
