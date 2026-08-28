use std::process::ExitCode;

use cf_integration::OutputStyle;
use cf_integration::app::resolve_action;
use cf_integration::cli::Cli;
use cf_integration::error::AppFailure;
use cf_integration::platform::config::{AppConfig, Environment};
use cf_integration::platform::process::SystemProcessRunner;
use cf_integration::runtime::RuntimeExecutor;
use clap::Parser;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let environment: Environment = std::env::vars_os().collect();
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!(
                "{}",
                OutputStyle::stderr().failure(&format!(
                    "failed to locate cf-integration executable: {error}"
                ))
            );
            return ExitCode::FAILURE;
        }
    };
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
    let loaded = match AppConfig::load(&environment, &executable, &cwd) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("{}", OutputStyle::stderr().failure(&format!("{error:#}")));
            return ExitCode::FAILURE;
        }
    };
    for warning in &loaded.warnings {
        eprintln!(
            "{}",
            OutputStyle::stderr().warning(&format!("warning: {warning}"))
        );
    }

    let effective_environment = loaded
        .config
        .environment()
        .iter()
        .map(|(key, value)| (key.clone(), value.value.clone()))
        .collect::<Environment>();
    let mut runtime = RuntimeExecutor::new(loaded.config, SystemProcessRunner);
    let result = match resolve_action(cli, &effective_environment) {
        Ok(action) => runtime.execute(action).await,
        Err(error) => Err(AppFailure::from(error)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", OutputStyle::stderr().failure(&error.to_string()));
            exit_code(error.exit_code())
        }
    }
}

fn exit_code(code: i32) -> ExitCode {
    u8::try_from(code)
        .map(ExitCode::from)
        .unwrap_or(ExitCode::FAILURE)
}
