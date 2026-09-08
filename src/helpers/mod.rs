//! Private container operations performed by the same integration executable.

use std::ffi::OsString;

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use url::Url;

mod auth;
mod config;
#[cfg(test)]
mod tests;

const KEY_PATH: &str = "/keys/jwt.key";
const JWKS_ADDRESS: &str = "127.0.0.1:4446";

#[derive(Parser)]
struct HelperArgs {
    #[command(subcommand)]
    command: HelperCommand,
}

#[derive(Subcommand)]
enum HelperCommand {
    Auth,
    Health,
    Token {
        tenant_id: String,
        user_id: String,
    },
    Fixture(ConfigArgs),
    Client {
        #[command(flatten)]
        config: ConfigArgs,
        tool_names_json: String,
    },
}

#[derive(Args)]
struct ConfigArgs {
    server_id: String,
    backend_url: Url,
    protocol_version: String,
}

pub(crate) async fn run(arguments: &[OsString]) -> Result<()> {
    let (args, tools) = match HelperArgs::try_parse_from(arguments)?.command {
        HelperCommand::Auth => {
            let router = auth::router(std::path::Path::new(KEY_PATH))?;
            let listener = tokio::net::TcpListener::bind(JWKS_ADDRESS).await?;
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown_signal())
                .await?;
            return Ok(());
        }
        HelperCommand::Health => {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(1))
                .build()?
                .get(format!("http://{JWKS_ADDRESS}/.well-known/jwks.json"))
                .send()
                .await?
                .error_for_status()?;
            return Ok(());
        }
        HelperCommand::Token { tenant_id, user_id } => {
            ensure!(
                !tenant_id.is_empty() && !user_id.is_empty(),
                "tenant-id and user-id must not be empty"
            );
            println!(
                "{}",
                auth::issue_token(std::path::Path::new(KEY_PATH), &tenant_id, &user_id)?
            );
            return Ok(());
        }
        HelperCommand::Fixture(args) => (args, None),
        HelperCommand::Client {
            config,
            tool_names_json,
        } => {
            let tools: Vec<String> = serde_json::from_str(&tool_names_json)
                .context("tool-names-json must be a JSON string array")?;
            ensure!(
                tools.iter().all(|name| !name.is_empty()),
                "tool names must not be empty"
            );
            (config, Some(tools))
        }
    };
    ensure!(
        !args.server_id.is_empty() && !args.protocol_version.is_empty(),
        "virtual-host-id and protocol-version must not be empty"
    );
    ensure!(
        matches!(args.backend_url.scheme(), "http" | "https"),
        "backend-url must use HTTP(S)"
    );
    let token =
        std::env::var("MCP_CONFORMANCE_TOKEN").context("MCP_CONFORMANCE_TOKEN is required")?;
    let subject = auth::token_subject(&token)?;
    let catalog = match tools {
        Some(tools) => config::Catalog::for_client(tools),
        None => config::fixture_catalog(args.backend_url.clone(), &args.protocol_version).await?,
    };
    let body = catalog.config(
        &args.server_id,
        args.backend_url.as_str(),
        &args.protocol_version,
    );
    let redis_url =
        std::env::var("CF_CONFIG_REDIS_URL").unwrap_or_else(|_| "redis://redis:6379".to_owned());
    config::publish(&redis_url, &subject, &body).await?;
    println!("{}", serde_json::to_string(&catalog.tool_names())?);
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate.recv() => {},
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
