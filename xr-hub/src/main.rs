mod api;
mod config;
mod embed;
mod signing;
mod state;
mod storage;

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "xr-hub", about = "Control-plane for xr-proxy: presets & invites")]
struct Cli {
    /// Path to config file.
    #[arg(long, short, default_value = "/etc/xr-hub/config.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Load config.
    let config_path = Path::new(&cli.config);
    if !config_path.exists() {
        eprintln!("Config file not found: {}", cli.config);
        eprintln!();
        eprintln!("Minimal example:");
        eprintln!();
        eprintln!("[server]");
        eprintln!("bind = \"0.0.0.0:8080\"");
        eprintln!("data_dir = \"/var/lib/xr-hub\"");
        eprintln!();
        eprintln!("[admin]");
        eprintln!("token = \"<your-secret-token>\"");
        std::process::exit(2);
    }

    let config_str = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let hub_config: config::HubConfig =
        toml::from_str(&config_str).context("parsing config")?;

    // Logging.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let bind_addr: SocketAddr = hub_config
        .server
        .bind
        .parse()
        .context("invalid bind address")?;
    let tls_config = hub_config.tls.clone();

    // Hydrate state from disk.
    let app_state = state::hydrate(hub_config)?;
    let app = api::router(app_state);

    match tls_config {
        Some(tls) => {
            tracing::info!("starting xr-hub on {} (TLS)", bind_addr);
            let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
                &tls.cert,
                &tls.key,
            )
            .await
            .context("loading TLS certificates")?;

            axum_server::bind_rustls(bind_addr, rustls_config)
                .serve(app.into_make_service())
                .await
                .context("running TLS server")?;
        }
        None => {
            tracing::warn!("TLS not configured — starting in plain HTTP mode (dev only!)");
            tracing::info!("starting xr-hub on {} (HTTP)", bind_addr);
            let listener = tokio::net::TcpListener::bind(bind_addr)
                .await
                .context("binding listener")?;
            axum::serve(listener, app)
                .await
                .context("running server")?;
        }
    }

    Ok(())
}
