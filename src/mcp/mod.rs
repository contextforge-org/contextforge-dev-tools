//! MCP transport, gateway, authentication proxy, and probe primitives.

pub mod auth_proxy;
pub mod backend_identity;
pub mod gateway;
pub mod probe;
pub mod protocol;
mod topology;

pub use topology::GatewayTopology;
