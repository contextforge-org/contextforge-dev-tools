//! Shared support for the `cf-integration` executable.

pub mod app;
pub mod cli;
pub mod compliance;
pub mod error;
pub mod load;
pub mod mcp;
mod output;
pub mod platform;
pub mod runtime;

pub use output::OutputStyle;
