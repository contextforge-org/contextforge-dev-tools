//! Locust load-testing primitives.

mod locust;
mod settings;

pub(crate) use locust::{LocustCommand, audit_reports as audit_locust_reports};
pub(crate) use settings::{LoadRequest, LoadSettings};
