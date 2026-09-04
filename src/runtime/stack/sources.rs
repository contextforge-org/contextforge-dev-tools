//! Source checkout synchronization.

use super::*;

impl<R: ProcessRunner> RuntimeContext<R> {
    pub(super) fn require_mode_sources(&self, mode: StackMode) -> AppResult<()> {
        let controlplane_compose = self.config.controlplane_dir().join("docker-compose.yml");
        if !controlplane_compose.is_file() {
            return Err(AppFailure::from(anyhow!(
                "control-plane checkout is unavailable at {}; run `cf-integration stack up --lane {}` first",
                self.config.controlplane_dir().display(),
                mode.lane_value()
            )));
        }
        if mode == StackMode::Dataplane
            && !self.config.dataplane_ref().value.is_empty()
            && !self.config.dataplane_dir().is_dir()
        {
            return Err(AppFailure::from(anyhow!(
                "dataplane source checkout is unavailable at {}; run `cf-integration stack up --lane external` first",
                self.config.dataplane_dir().display()
            )));
        }
        Ok(())
    }

    pub(super) fn ensure_mode_sources(&self, mode: StackMode) -> AppResult<()> {
        self.ensure_controlplane()?;
        if mode == StackMode::Dataplane {
            self.ensure_dataplane()?;
        }
        Ok(())
    }

    pub(in crate::runtime) fn ensure_controlplane(&self) -> AppResult<()> {
        let request = CheckoutRequest::controlplane(
            self.config.controlplane_dir(),
            self.config.controlplane_repo().value.clone(),
            self.config.controlplane_ref().value.clone(),
        );
        self.ensure_checkout(&request)
    }

    fn ensure_dataplane(&self) -> AppResult<()> {
        let request = CheckoutRequest::dataplane(
            self.config.dataplane_dir(),
            self.config.dataplane_repo().value.clone(),
            self.config.dataplane_ref().value.clone(),
        );
        self.ensure_checkout(&request)
    }

    pub(super) fn ensure_checkout(&self, request: &CheckoutRequest) -> AppResult<()> {
        let manager = CheckoutManager::new(&self.runner);
        Ok(manager
            .ensure(self.config.integration_dir(), request)
            .map(|_| ())?)
    }
}
