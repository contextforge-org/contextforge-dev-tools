//! Standalone entrypoint for the `cf-integration` executable.

#[cfg(test)]
extern crate self as cf_integration;

use std::{io::Write, process::ExitCode};

use clap::Parser;

mod app;
mod cli;
mod conformance;
mod error;
mod infrastructure;
mod mcp;
mod output;
mod performance;
mod runtime;

use app::resolve_action;
use cli::Cli;
use error::AppFailure;
use infrastructure::config::{AppConfig, ConfigBootstrap, Environment};
use infrastructure::process::SystemProcessRunner;
pub(crate) use output::{Activity, OutputStyle, TestStatus};
use runtime::RuntimeDispatcher;

/// Runs the CLI using the current process arguments and environment.
pub async fn run() -> ExitCode {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    if conformance::client::is_internal_client_invocation(&arguments) {
        return match conformance::client::run_internal_client(&arguments).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!(
                    "{}: {}",
                    conformance::client::CLIENT_DRIVER_FAILURE_PREFIX,
                    OutputStyle::stderr().failure(&format!("{error:#}"))
                );
                ExitCode::FAILURE
            }
        };
    }
    let cli = Cli::parse_from(arguments);
    let environment: Environment = std::env::vars_os().collect();
    let cwd = match std::env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!(
                "{}",
                OutputStyle::stderr()
                    .failure(&format!("failed to determine current directory: {error}"))
            );
            return ExitCode::FAILURE;
        }
    };
    let bootstrap = match ConfigBootstrap::load(&environment, &cwd) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("{}", OutputStyle::stderr().failure(&format!("{error:#}")));
            return ExitCode::FAILURE;
        }
    };
    for warning in bootstrap.warnings() {
        eprintln!(
            "{}",
            OutputStyle::stderr().warning(&format!("warning: {warning}"))
        );
    }

    let effective_environment = bootstrap
        .environment()
        .iter()
        .map(|(key, value)| (key.clone(), value.value.clone()))
        .collect::<Environment>();
    let action = match resolve_action(cli, &effective_environment) {
        Ok(action) => action,
        Err(error) => return report_failure(AppFailure::from(error)),
    };
    let requirements = action.config_requirements();
    eprintln!("{}", OutputStyle::stderr().info(&action.startup_summary()));
    let activity = action
        .uses_global_activity()
        .then(|| Activity::spinner(action.description()));
    let config = match AppConfig::load(bootstrap, requirements) {
        Ok(config) => config,
        Err(error) => {
            if let Some(activity) = activity {
                activity.finish(false);
            }
            return report_failure(AppFailure::from(error));
        }
    };
    let runtime = RuntimeDispatcher::new(config, SystemProcessRunner);
    let result = runtime.execute(action).await;
    if let Some(activity) = activity {
        activity.finish(result.is_ok());
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => report_failure(error),
    }
}

fn report_failure(error: AppFailure) -> ExitCode {
    // Keep completed result output ahead of wrapper diagnostics such as Make's
    // nonzero-exit message when stdout and stderr are captured separately.
    let _ = std::io::stdout().flush();
    if !error.is_reported() {
        eprintln!("{}", OutputStyle::stderr().failure(&error.to_string()));
    }
    exit_code(error.exit_code())
}

fn exit_code(code: i32) -> ExitCode {
    u8::try_from(code)
        .map(ExitCode::from)
        .unwrap_or(ExitCode::FAILURE)
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod app_tests;
#[cfg(test)]
#[path = "cli_public_tests.rs"]
mod cli_public_tests;
#[cfg(test)]
#[path = "conformance/fixture_tests.rs"]
mod conformance_fixture_tests;
#[cfg(test)]
#[path = "conformance/results_tests.rs"]
mod conformance_tests;
#[cfg(test)]
#[path = "infrastructure/checkout_integration_tests.rs"]
mod infrastructure_checkout_tests;
#[cfg(test)]
#[path = "infrastructure/compose_integration_tests.rs"]
mod infrastructure_compose_tests;
#[cfg(test)]
#[path = "infrastructure/config_integration_tests.rs"]
mod infrastructure_config_tests;
#[cfg(test)]
#[path = "infrastructure/process_integration_tests.rs"]
mod infrastructure_process_tests;
#[cfg(test)]
#[path = "infrastructure/stack_integration_tests.rs"]
mod infrastructure_stack_tests;
#[cfg(test)]
#[path = "mcp/auth_proxy_integration_tests.rs"]
mod mcp_auth_proxy_tests;
#[cfg(test)]
#[path = "mcp/backend_identity_integration_tests.rs"]
mod mcp_backend_identity_tests;
#[cfg(test)]
#[path = "mcp/gateway_integration_tests.rs"]
mod mcp_gateway_tests;
#[cfg(test)]
#[path = "mcp/protocol_integration_tests.rs"]
mod mcp_protocol_tests;
#[cfg(test)]
#[path = "performance/locust_integration_tests.rs"]
mod performance_locust_tests;
#[cfg(test)]
#[path = "performance/python_adapter_tests.rs"]
mod performance_python_adapter_tests;
