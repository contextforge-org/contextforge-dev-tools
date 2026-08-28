//! Application failure and exit-code mapping.

use std::error::Error;
use std::fmt;

use crate::infrastructure::InfrastructureError;

/// An application operation or infrastructure operation failure.
#[derive(Debug)]
pub enum AppFailure {
    /// A reusable infrastructure operation failed.
    Infrastructure(InfrastructureError),
    /// An application orchestration operation failed.
    Native(anyhow::Error),
}

impl AppFailure {
    /// Returns the process exit code represented by this failure.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Infrastructure(error) => error.exit_code(),
            Self::Native(_) => 1,
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
        }
    }
}

impl Error for AppFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Infrastructure(error) => Some(error),
            Self::Native(error) => Some(error.as_ref()),
        }
    }
}
