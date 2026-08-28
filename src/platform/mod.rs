//! Source, process, configuration, Compose, and stack primitives.

pub mod checkout;
pub mod compose;
pub mod config;
pub mod error;
mod mode;
pub mod process;
pub mod stack;

pub use error::PlatformError;
pub use mode::StackMode;
