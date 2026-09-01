//! MCP transport, gateway, authentication proxy, and probe primitives.

pub(crate) mod auth_proxy;
pub(crate) mod backend_identity;
pub(crate) mod gateway;
pub(crate) mod probe;
pub(crate) mod protocol;
mod topology;

pub(crate) use topology::GatewayTopology;
