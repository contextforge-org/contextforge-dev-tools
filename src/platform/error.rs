//! Application failure and exit-code mapping.

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::process::ExitStatus;

/// A harness error or a direct child-process failure.
#[derive(Debug)]
pub enum PlatformError {
    /// A child process exited unsuccessfully.
    ChildExit {
        /// Program that was executed.
        program: OsString,
        /// Native child exit status.
        status: ExitStatus,
    },
    /// A native harness operation failed.
    Native(anyhow::Error),
}

impl PlatformError {
    pub(crate) fn child_exit(program: OsString, status: ExitStatus) -> Self {
        Self::ChildExit { program, status }
    }

    /// Returns the process exit code represented by this failure.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::ChildExit { status, .. } => child_exit_code(status),
            Self::Native(_) => 1,
        }
    }
}

impl From<anyhow::Error> for PlatformError {
    fn from(error: anyhow::Error) -> Self {
        Self::Native(error)
    }
}

impl fmt::Display for PlatformError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChildExit { program, status } => {
                write!(formatter, "program {program:?} exited with {status}")
            }
            Self::Native(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl Error for PlatformError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ChildExit { .. } => None,
            Self::Native(error) => Some(error.as_ref()),
        }
    }
}

fn child_exit_code(status: &ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }

    signal_exit_code(status).unwrap_or(1)
}

#[cfg(unix)]
fn signal_exit_code(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;

    status.signal().map(|signal| 128_i32.saturating_add(signal))
}

#[cfg(not(unix))]
fn signal_exit_code(_status: &ExitStatus) -> Option<i32> {
    None
}
