/// Deployment topology managed by the integration harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StackMode {
    /// Python control plane only.
    Controlplane,
    /// Python control plane routed through the Rust dataplane.
    Dataplane,
}

impl StackMode {
    /// Semantic lane name shown to users.
    #[must_use]
    pub(crate) const fn lane_label(self) -> &'static str {
        match self {
            Self::Controlplane => "builtin",
            Self::Dataplane => "external",
        }
    }

    /// Canonical semantic lane value accepted by public commands.
    #[must_use]
    pub(crate) const fn lane_value(self) -> &'static str {
        match self {
            Self::Controlplane => "builtin",
            Self::Dataplane => "external",
        }
    }
}
