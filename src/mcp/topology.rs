/// Public MCP route shape and backend identity expectation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayTopology {
    /// `/mcp` routes directly to the control plane.
    Direct,
    /// `/servers/{virtual_host_id}/mcp` routes through the dataplane.
    Dataplane,
}

impl GatewayTopology {
    /// Returns whether responses require trusted dataplane backend identity.
    #[must_use]
    pub const fn requires_dataplane(self) -> bool {
        matches!(self, Self::Dataplane)
    }

    /// Returns the stable user-facing stack label.
    #[must_use]
    pub const fn report_label(self) -> &'static str {
        match self {
            Self::Direct => "controlplane",
            Self::Dataplane => "dataplane",
        }
    }
}
