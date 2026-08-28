//! Standalone entrypoint for the `cf-integration` executable.

#[cfg(test)]
extern crate self as cf_integration;

use std::process::ExitCode;

use clap::Parser;

mod app;
mod cli;
mod compliance;
mod error;
mod load;
mod mcp;
mod output;
mod platform;
mod runtime;

use app::resolve_action;
use cli::Cli;
use error::AppFailure;
pub(crate) use output::OutputStyle;
use platform::config::{AppConfig, ConfigBootstrap, ConfigRequirements, Environment};
use platform::process::SystemProcessRunner;
use runtime::RuntimeDispatcher;

/// Runs the CLI using the current process arguments and environment.
pub async fn run() -> ExitCode {
    let cli = Cli::parse();
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
    let requirements = if action.requires_runtime_assets() {
        ConfigRequirements::RUNTIME
    } else {
        ConfigRequirements::READ_ONLY
    };
    let config = match AppConfig::load(bootstrap, requirements) {
        Ok(config) => config,
        Err(error) => return report_failure(AppFailure::from(error)),
    };
    let runtime = RuntimeDispatcher::new(config, SystemProcessRunner);
    match runtime.execute(action).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", OutputStyle::stderr().failure(&error.to_string()));
            exit_code(error.exit_code())
        }
    }
}

fn report_failure(error: AppFailure) -> ExitCode {
    eprintln!("{}", OutputStyle::stderr().failure(&error.to_string()));
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
#[path = "compliance/conformance_fixture_integration_tests.rs"]
mod compliance_conformance_fixture_tests;
#[cfg(test)]
#[path = "compliance/conformance_integration_tests.rs"]
mod compliance_conformance_tests;
#[cfg(test)]
#[path = "load/locust_integration_tests.rs"]
mod load_locust_tests;
#[cfg(test)]
#[path = "load/python_adapter_tests.rs"]
mod load_python_adapter_tests;
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
#[path = "platform/checkout_integration_tests.rs"]
mod platform_checkout_tests;
#[cfg(test)]
#[path = "platform/compose_integration_tests.rs"]
mod platform_compose_tests;
#[cfg(test)]
#[path = "platform/config_integration_tests.rs"]
mod platform_config_tests;
#[cfg(test)]
#[path = "platform/process_integration_tests.rs"]
mod platform_process_tests;
#[cfg(test)]
#[path = "platform/stack_integration_tests.rs"]
mod platform_stack_tests;
