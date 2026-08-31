//! Coherent official conformance runner, fixture, and protocol pins.

/// Published official CLI package used as the conformance client.
pub(crate) const OFFICIAL_CONFORMANCE_PACKAGE: &str =
    "@modelcontextprotocol/conformance@0.2.0-alpha.11";
/// Official repository containing the matching TypeScript fixture server.
pub(crate) const OFFICIAL_CONFORMANCE_REPOSITORY: &str =
    "https://github.com/modelcontextprotocol/conformance";
/// Exact source revision behind the published CLI and TypeScript fixture.
pub(crate) const OFFICIAL_CONFORMANCE_REVISION: &str = "c321dd32035556e6769d3724a8ee97d87c3faaac";
/// Default draft protocol revision exercised by official conformance commands.
pub(crate) const DEFAULT_MCP_SPEC_VERSION: &str = "2026-07-28";
/// Previous stable revision supported by the pinned official conformance package.
pub(crate) const STABLE_MCP_SPEC_VERSION: &str = "2025-11-25";
/// Oldest revision supported by the pinned official conformance package.
pub(crate) const LEGACY_MCP_SPEC_VERSION: &str = "2025-06-18";

/// Initialization-based revisions implemented by the pinned official client.
pub(crate) const LEGACY_CLIENT_PROTOCOL_VERSIONS: &[&str] =
    &[LEGACY_MCP_SPEC_VERSION, STABLE_MCP_SPEC_VERSION];
/// Per-request revisions implemented by the pinned official client.
pub(crate) const MODERN_CLIENT_PROTOCOL_VERSIONS: &[&str] = &[DEFAULT_MCP_SPEC_VERSION];
/// Complete protocol set implemented by the pinned official client.
pub(crate) const DUAL_CLIENT_PROTOCOL_VERSIONS: &[&str] = &[
    LEGACY_MCP_SPEC_VERSION,
    STABLE_MCP_SPEC_VERSION,
    DEFAULT_MCP_SPEC_VERSION,
];

/// Stateful protocol revisions accepted by the pinned fixture in legacy mode.
pub(crate) const LEGACY_SERVER_PROTOCOL_VERSIONS: &[&str] = &[
    "2024-11-05",
    "2025-03-26",
    LEGACY_MCP_SPEC_VERSION,
    STABLE_MCP_SPEC_VERSION,
];
/// Per-request protocol revisions accepted by the pinned fixture in modern mode.
pub(crate) const MODERN_SERVER_PROTOCOL_VERSIONS: &[&str] = &[DEFAULT_MCP_SPEC_VERSION];
/// Complete protocol set accepted by the pinned fixture in dual mode.
pub(crate) const DUAL_SERVER_PROTOCOL_VERSIONS: &[&str] = &[
    "2024-11-05",
    "2025-03-26",
    LEGACY_MCP_SPEC_VERSION,
    STABLE_MCP_SPEC_VERSION,
    DEFAULT_MCP_SPEC_VERSION,
];
