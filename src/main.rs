use std::process::ExitCode;

use cf_integration::OutputStyle;
use cf_integration::app::resolve_action;
use cf_integration::cli::Cli;
use cf_integration::error::AppFailure;
use cf_integration::platform::config::{
    AppConfig, ConfigBootstrap, ConfigRequirements, Environment,
};
use cf_integration::platform::process::SystemProcessRunner;
use cf_integration::runtime::RuntimeExecutor;
use clap::Parser;

#[tokio::main]
async fn main() -> ExitCode {
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
    let mut runtime = RuntimeExecutor::new(config, SystemProcessRunner);
    let result = runtime.execute(action).await;
    match result {
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
