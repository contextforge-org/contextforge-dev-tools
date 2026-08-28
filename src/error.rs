//! Application failure and exit-code mapping.

use std::error::Error;
use std::fmt;

use crate::platform::PlatformError;

/// An application operation or platform operation failure.
#[derive(Debug)]
pub enum AppFailure {
    /// A reusable platform operation failed.
    Platform(PlatformError),
    /// An application orchestration operation failed.
    Native(anyhow::Error),
}

impl AppFailure {
    /// Returns the process exit code represented by this failure.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Platform(error) => error.exit_code(),
            Self::Native(_) => 1,
        }
    }
}

impl From<anyhow::Error> for AppFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::Native(error)
    }
}

impl From<PlatformError> for AppFailure {
    fn from(error: PlatformError) -> Self {
        Self::Platform(error)
    }
}

impl fmt::Display for AppFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Platform(error) => write!(formatter, "{error}"),
            Self::Native(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl Error for AppFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Platform(error) => Some(error),
            Self::Native(error) => Some(error.as_ref()),
        }
    }
}
