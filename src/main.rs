use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    cf_integration::run().await
}
