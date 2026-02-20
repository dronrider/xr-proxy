mod fallback;
mod handler;

use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::Duration;
use xr_proto::config::{decode_key, load_server_config};
use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};
use xr_proto::protocol::Codec;

#[derive(Parser)]
#[command(name = "xr-server", about = "XR Proxy Server — lightweight obfuscated proxy server")]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value = "/etc/xr-proxy/configs/server.toml")]
    config: PathBuf,

    /// Override log level
    #[arg(short, long)]
    log_level: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Load config
    let config = load_server_config(&cli.config)?;

    // Setup logging
    let log_level = cli.log_level.as_deref().unwrap_or(&config.logging.level);
    let filter = tracing_subscriber::EnvFilter::try_new(log_level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    tracing::info!("XR Proxy Server starting");

    // Build obfuscator
    let key = decode_key(&config.obfuscation.key)?;
    let strategy = ModifierStrategy::from_str(&config.obfuscation.modifier)
        .ok_or("unknown modifier strategy")?;
    let obfuscator = Obfuscator::new(key, config.obfuscation.salt as u32, strategy);
    // Server doesn't need padding — it uses whatever the client sends
    let codec = Codec::new(obfuscator, 16, 128);

    // Build fallback response
    let fallback_response = if config.fallback.enabled {
        Some(fallback::build_fallback_response(
            config.fallback.response_file.as_deref(),
        ))
    } else {
        None
    };

    let timeout = Duration::from_secs(config.limits.connection_timeout_sec);
    let max_conns = config.limits.max_connections as usize;

    // Bind listener
    let bind_addr = format!("{}:{}", config.server.listen, config.server.port);
    let listener = TcpListener::bind(&bind_addr).await?;
    tracing::info!("Server listening on {}", bind_addr);

    // Connection limiter
    let semaphore = Arc::new(Semaphore::new(max_conns));

    // Accept loop
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, addr) = result?;
                let codec = codec.clone();
                let fallback = fallback_response.clone();
                let sem = semaphore.clone();

                tokio::spawn(async move {
                    let _permit = match sem.try_acquire() {
                        Ok(p) => p,
                        Err(_) => {
                            tracing::warn!("Connection limit reached, rejecting {}", addr);
                            return;
                        }
                    };

                    if let Err(e) = handler::handle_client(stream, addr, codec, timeout, fallback).await {
                        tracing::debug!("Client {} error: {}", addr, e);
                    }
                });
            }
            _ = shutdown_signal() => {
                tracing::info!("Shutdown signal received");
                break;
            }
        }
    }

    tracing::info!("XR Proxy Server stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to setup SIGTERM handler");

    tokio::select! {
        _ = ctrl_c => {},
        _ = sigterm.recv() => {},
    }
}
