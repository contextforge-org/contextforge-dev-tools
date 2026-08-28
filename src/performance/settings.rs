//! Load settings precedence and validation.

use std::ffi::OsStr;
use std::num::NonZeroUsize;

use crate::infrastructure::config::{AppConfig, SourcedValue, ValueOrigin};
use anyhow::{Context, Result, bail};

const SMOKE_USERS: &str = "1";
const SMOKE_SPAWN_RATE: &str = "1";
const SMOKE_RUN_TIME: &str = "10s";
const RUN_TIME_ERROR: &str = "LOCUST_RUN_TIME must be a positive Locust duration using h, m, and s at most once in that order";

/// User-selected settings before configuration precedence is applied.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LoadRequest {
    /// Whether dotenv/default values should use smoke-test replacements.
    pub(crate) smoke: bool,
    /// Explicit concurrent-user override.
    pub(crate) users: Option<usize>,
    /// Explicit users-per-second override.
    pub(crate) spawn_rate: Option<f64>,
    /// Explicit engine duration override.
    pub(crate) run_time: Option<String>,
}

/// Validated load settings after applying CLI, process, dotenv, and default precedence.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LoadSettings {
    users: NonZeroUsize,
    spawn_rate: f64,
    run_time: String,
}

impl LoadSettings {
    /// Resolves settings using request > process > dotenv/default precedence.
    ///
    /// Smoke mode replaces only dotenv and built-in values. Explicit process
    /// values remain authoritative, including invalid empty values, which are
    /// reported rather than silently replaced.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero or malformed user count, a non-finite or
    /// non-positive spawn rate, or an invalid Locust run-time expression.
    pub(crate) fn resolve(config: &AppConfig, request: &LoadRequest) -> Result<Self> {
        let users = match request.users {
            Some(users) => users,
            None => parse_users(selected_value(
                config.locust_users(),
                request.smoke,
                SMOKE_USERS,
            ))?,
        };
        let users = NonZeroUsize::new(users)
            .context("LOCUST_USERS must be an integer greater than zero")?;

        let spawn_rate = match request.spawn_rate {
            Some(spawn_rate) if spawn_rate.is_finite() && spawn_rate > 0.0 => spawn_rate,
            Some(_) => bail!("LOCUST_SPAWN_RATE must be a finite number greater than zero"),
            None => parse_spawn_rate(selected_value(
                config.locust_spawn_rate(),
                request.smoke,
                SMOKE_SPAWN_RATE,
            ))?,
        };

        let run_time = request.run_time.as_deref().map_or_else(
            || {
                selected_value(config.locust_run_time(), request.smoke, SMOKE_RUN_TIME)
                    .to_str()
                    .map(str::to_owned)
                    .context("LOCUST_RUN_TIME must be valid UTF-8")
            },
            |run_time| Ok(run_time.to_owned()),
        )?;
        validate_locust_run_time(&run_time)?;

        Ok(Self {
            users,
            spawn_rate,
            run_time,
        })
    }

    /// Returns the concurrent user count.
    #[must_use]
    pub(crate) fn users(&self) -> NonZeroUsize {
        self.users
    }

    /// Returns the users spawned per second.
    #[must_use]
    pub(crate) fn spawn_rate(&self) -> f64 {
        self.spawn_rate
    }

    /// Returns the validated duration expression.
    #[must_use]
    pub(crate) fn run_time(&self) -> &str {
        &self.run_time
    }
}

fn selected_value<'a>(
    configured: &'a SourcedValue,
    smoke: bool,
    smoke_default: &'static str,
) -> &'a OsStr {
    if smoke && configured.origin != ValueOrigin::Process {
        OsStr::new(smoke_default)
    } else {
        &configured.value
    }
}

fn parse_users(value: &OsStr) -> Result<usize> {
    let value = value.to_str().context("LOCUST_USERS must be valid UTF-8")?;
    let users = value
        .parse::<usize>()
        .context("LOCUST_USERS must be an integer greater than zero")?;
    if users == 0 {
        bail!("LOCUST_USERS must be an integer greater than zero");
    }
    Ok(users)
}

fn parse_spawn_rate(value: &OsStr) -> Result<f64> {
    let value = value
        .to_str()
        .context("LOCUST_SPAWN_RATE must be valid UTF-8")?;
    let spawn_rate = value
        .parse::<f64>()
        .context("LOCUST_SPAWN_RATE must be a finite number greater than zero")?;
    if !spawn_rate.is_finite() || spawn_rate <= 0.0 {
        bail!("LOCUST_SPAWN_RATE must be a finite number greater than zero");
    }
    Ok(spawn_rate)
}

fn validate_locust_run_time(value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    let mut position = 0;
    if bytes.is_empty() {
        bail!(RUN_TIME_ERROR);
    }
    let mut previous_unit = None;
    while position < bytes.len() {
        let number_start = position;
        while position < bytes.len() && bytes[position].is_ascii_digit() {
            position += 1;
        }
        if number_start == position
            || value[number_start..position]
                .parse::<u64>()
                .ok()
                .is_none_or(|amount| amount == 0)
        {
            bail!(RUN_TIME_ERROR);
        }
        let unit = match bytes.get(position) {
            Some(b'h') => 0,
            Some(b'm') => 1,
            Some(b's') => 2,
            _ => bail!(RUN_TIME_ERROR),
        };
        if previous_unit.is_some_and(|previous| unit <= previous) {
            bail!(RUN_TIME_ERROR);
        }
        previous_unit = Some(unit);
        position += 1;
    }
    Ok(())
}
