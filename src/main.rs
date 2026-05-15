use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "pivx-merchant-kit starting");
    tracing::warn!("daemon is a scaffold — wiring lands in later stages");

    Ok(())
}
