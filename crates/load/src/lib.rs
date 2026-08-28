//! Locust load-testing primitives.

mod locust;
mod settings;

pub use locust::{LocustCommand, audit_reports as audit_locust_reports};
pub use settings::{LoadRequest, LoadSettings};
