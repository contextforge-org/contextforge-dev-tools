use std::fs;
use std::path::Path;

use axum::http::{HeaderMap, HeaderValue};
use cf_integration_mcp::backend_identity::{
    BACKEND_HEADER, BackendIdentity, CONTROLPLANE_BACKEND, CONTROLPLANE_FALLBACK_BACKEND,
    DATAPLANE_BACKEND, sanitized_backend_value,
};

#[test]
fn backend_identity_requires_one_exact_visible_marker() {
    let mut headers = HeaderMap::new();
    assert_eq!(
        BackendIdentity::from_headers(&headers),
        BackendIdentity::Missing
    );

    headers.insert(BACKEND_HEADER, HeaderValue::from_static(DATAPLANE_BACKEND));
    assert_eq!(
        BackendIdentity::from_headers(&headers),
        BackendIdentity::Dataplane
    );

    headers.insert(
        BACKEND_HEADER,
        HeaderValue::from_static(CONTROLPLANE_FALLBACK_BACKEND),
    );
    assert_eq!(
        BackendIdentity::from_headers(&headers),
        BackendIdentity::ControlplaneFallback
    );

    headers.insert(
        BACKEND_HEADER,
        HeaderValue::from_static(CONTROLPLANE_BACKEND),
    );
    assert_eq!(
        BackendIdentity::from_headers(&headers),
        BackendIdentity::Controlplane
    );

    headers.insert(BACKEND_HEADER, HeaderValue::from_static("forged"));
    assert_eq!(
        BackendIdentity::from_headers(&headers),
        BackendIdentity::Invalid
    );

    headers.append(BACKEND_HEADER, HeaderValue::from_static(DATAPLANE_BACKEND));
    assert_eq!(
        BackendIdentity::from_headers(&headers),
        BackendIdentity::Multiple
    );
}

#[test]
fn backend_identity_errors_and_capture_never_echo_an_untrusted_marker() {
    let forged = "private-upstream-detail";
    let value = HeaderValue::from_str(forged).expect("test header should be valid");

    assert_eq!(sanitized_backend_value(&value), "<invalid>");
    for identity in [
        BackendIdentity::Missing,
        BackendIdentity::ControlplaneFallback,
        BackendIdentity::Controlplane,
        BackendIdentity::Invalid,
        BackendIdentity::Multiple,
    ] {
        let error = identity
            .dataplane_error()
            .expect("non-dataplane identity must fail closed");
        assert!(!error.contains(forged));
        assert!(error.contains("backend marker"));
    }
    assert_eq!(BackendIdentity::Dataplane.dataplane_error(), None);
}

#[test]
fn dataplane_nginx_replaces_upstream_markers_at_every_public_backend_boundary() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("MCP crate should be nested under crates");
    let nginx = fs::read_to_string(root.join("docker/nginx.cf-dataplane.conf"))
        .expect("dataplane nginx configuration should be readable");

    assert!(nginx.contains("proxy_hide_header X-CF-Integration-Backend;"));
    assert!(nginx.contains("add_header X-CF-Integration-Backend dataplane always;"));
    assert!(!nginx.contains("controlplane_mcp_fallback"));
    assert!(!nginx.contains("proxy_intercept_errors on;"));
    assert!(nginx.contains("add_header X-CF-Integration-Backend controlplane always;"));

    let primary = nginx
        .split("location ~ ^/servers/([^/]+)/mcp/?$ {")
        .nth(1)
        .and_then(|tail| tail.split("location @controlplane_mcp_fallback").next())
        .expect("primary dataplane location should exist");
    assert_eq!(
        primary
            .matches("X-CF-Integration-Backend dataplane always;")
            .count(),
        1,
        "the primary marker must not be duplicated during fallback"
    );
}
