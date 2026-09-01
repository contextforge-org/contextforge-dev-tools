/// Deployment topology managed by the integration harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StackMode {
    /// Python control plane only.
    Controlplane,
    /// Python control plane routed through the Rust dataplane.
    Dataplane,
}

impl StackMode {
    /// Semantic topology name shown to users.
    #[must_use]
    pub(crate) const fn topology_label(self) -> &'static str {
        match self {
            Self::Controlplane => "built-in dataplane",
            Self::Dataplane => "external dataplane",
        }
    }

    /// Canonical physical topology value accepted by stack commands.
    #[must_use]
    pub(crate) const fn cli_value(self) -> &'static str {
        match self {
            Self::Controlplane => "controlplane",
            Self::Dataplane => "dataplane",
        }
    }
}
