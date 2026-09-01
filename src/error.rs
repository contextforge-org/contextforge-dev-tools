//! Application failure and exit-code mapping.

use std::error::Error;
use std::fmt;

use crate::infrastructure::InfrastructureError;

/// An application operation or infrastructure operation failure.
#[derive(Debug)]
pub(crate) enum AppFailure {
    /// A reusable infrastructure operation failed.
    Infrastructure(InfrastructureError),
    /// An application orchestration operation failed.
    Native(anyhow::Error),
    /// The workflow already rendered a complete user-facing diagnostic.
    Reported,
}

impl AppFailure {
    /// Creates a failure whose diagnostic has already been rendered.
    #[must_use]
    pub(crate) const fn reported() -> Self {
        Self::Reported
    }

    /// Whether the caller must suppress duplicate terminal rendering.
    #[must_use]
    pub(crate) const fn is_reported(&self) -> bool {
        matches!(self, Self::Reported)
    }

    /// Returns the process exit code represented by this failure.
    #[must_use]
    pub(crate) fn exit_code(&self) -> i32 {
        match self {
            Self::Infrastructure(error) => error.exit_code(),
            Self::Native(_) | Self::Reported => 1,
        }
    }
}

impl From<anyhow::Error> for AppFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::Native(error)
    }
}

impl From<InfrastructureError> for AppFailure {
    fn from(error: InfrastructureError) -> Self {
        Self::Infrastructure(error)
    }
}

impl fmt::Display for AppFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Infrastructure(error) => write!(formatter, "{error}"),
            Self::Native(error) => write!(formatter, "{error:#}"),
            Self::Reported => formatter.write_str("failure already reported"),
        }
    }
}

impl Error for AppFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Infrastructure(error) => Some(error),
            Self::Native(error) => Some(error.as_ref()),
            Self::Reported => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_failure_preserves_failure_exit_without_duplicate_rendering() {
        let failure = AppFailure::reported();

        assert!(failure.is_reported());
        assert_eq!(failure.exit_code(), 1);
    }
}
