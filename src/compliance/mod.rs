//! Official conformance fixture, result, and report primitives.

pub mod conformance;
pub mod conformance_fixture;
pub mod profile;

pub use profile::{
    DEFAULT_MCP_SPEC_VERSION, LEGACY_MCP_SPEC_VERSION, OFFICIAL_CONFORMANCE_PACKAGE,
    OFFICIAL_CONFORMANCE_REPOSITORY, OFFICIAL_CONFORMANCE_REVISION, STABLE_MCP_SPEC_VERSION,
};
