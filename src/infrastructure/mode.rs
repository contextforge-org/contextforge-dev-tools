/// Deployment topology managed by the integration harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackMode {
    /// Python control plane only.
    Controlplane,
    /// Python control plane routed through the Rust dataplane.
    Dataplane,
}
