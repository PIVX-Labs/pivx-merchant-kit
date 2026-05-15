use pivx_merchant_kit::cli;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let argv: Vec<String> = std::env::args().collect();
    let (cmd, config_path) = cli::parse(&argv);
    cli::dispatch(cmd, config_path).await?;
    Ok(())
}
