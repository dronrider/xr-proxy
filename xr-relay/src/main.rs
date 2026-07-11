//! xr-relay binary: blind transit for shares behind NAT (LLD-23).

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use tokio::net::TcpListener;
use xr_relay::config::RelayConfig;
use xr_relay::{serve, spawn_counter_logger, RelayState};

#[derive(Parser)]
#[command(name = "xr-relay", about = "XR share relay: blind transit for NAT'd agents")]
struct Cli {
    /// Path to the relay config file.
    #[arg(short, long, default_value = "/etc/xr-proxy/relay.toml")]
    config: PathBuf,
    /// Override the log level from config.
    #[arg(short, long)]
    log_level: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = RelayConfig::load(&cli.config)?;

    let log_level = cli.log_level.as_deref().unwrap_or(&config.log_level);
    let filter = tracing_subscriber::EnvFilter::try_new(log_level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    let codec = config.codec()?;
    let hub_key = config.hub_key()?;
    let state = RelayState::new(
        hub_key,
        config.max_streams,
        config.max_registrations_per_ip,
        Duration::from_secs(config.splice_lifetime_secs),
    );

    let bind = format!("{}:{}", config.listen, config.port);
    let listener = TcpListener::bind(&bind).await?;
    tracing::info!("xr-relay listening on {bind}");

    spawn_counter_logger(state.clone(), Duration::from_secs(config.counter_log_secs));

    let shutdown = tokio::signal::ctrl_c();
    tokio::select! {
        _ = serve(listener, codec, state, config.max_connections) => {}
        _ = shutdown => tracing::info!("xr-relay shutting down"),
    }
    Ok(())
}
