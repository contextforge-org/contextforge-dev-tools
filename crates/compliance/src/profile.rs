//! Coherent official conformance runner, fixture, and protocol pins.

/// Published official CLI package used as the conformance client.
pub const OFFICIAL_CONFORMANCE_PACKAGE: &str = "@modelcontextprotocol/conformance@0.2.0-alpha.11";
/// Official repository containing the matching TypeScript fixture server.
pub const OFFICIAL_CONFORMANCE_REPOSITORY: &str =
    "https://github.com/modelcontextprotocol/conformance";
/// Exact source revision behind the published CLI and TypeScript fixture.
pub const OFFICIAL_CONFORMANCE_REVISION: &str = "c321dd32035556e6769d3724a8ee97d87c3faaac";
/// Default draft protocol revision exercised by official conformance commands.
pub const DEFAULT_MCP_SPEC_VERSION: &str = "2026-07-28";
/// Previous stable revision supported by the pinned official conformance package.
pub const STABLE_MCP_SPEC_VERSION: &str = "2025-11-25";
/// Oldest revision supported by the pinned official conformance package.
pub const LEGACY_MCP_SPEC_VERSION: &str = "2025-06-18";
