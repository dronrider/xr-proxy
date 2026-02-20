mod proxy;
mod redirect;
mod routing;
mod sni;

use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use xr_proto::config::{decode_key, load_client_config};
use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};
use xr_proto::protocol::Codec;

#[derive(Parser)]
#[command(name = "xr-client", about = "XR Proxy Client — lightweight transparent proxy for OpenWRT")]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value = "/etc/xr-proxy/config.toml")]
    config: PathBuf,

    /// Override log level
    #[arg(short, long)]
    log_level: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Load config
    let config = load_client_config(&cli.config)?;

    // Setup logging
    let log_level = cli.log_level.as_deref()
        .unwrap_or(&config.client.log_level);
    let filter = tracing_subscriber::EnvFilter::try_new(log_level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    tracing::info!("XR Proxy Client starting");

    // Build obfuscator
    let key = decode_key(&config.obfuscation.key)?;
    let strategy = ModifierStrategy::from_str(&config.obfuscation.modifier)
        .ok_or("unknown modifier strategy")?;
    let obfuscator = Obfuscator::new(key, config.obfuscation.salt as u32, strategy);
    let codec = Codec::new(
        obfuscator,
        config.obfuscation.padding_min,
        config.obfuscation.padding_max,
    );

    // Build router
    let geoip_path = config.geoip.as_ref().map(|g| g.database.as_str());
    let router = routing::Router::new(&config.routing, geoip_path);

    // Resolve server address
    let server_addr: SocketAddr = format!("{}:{}", config.server.address, config.server.port)
        .parse()
        .map_err(|e| format!("invalid server address: {}", e))?;

    let on_server_down = routing::Action::from_str(&config.client.on_server_down);

    let state = Arc::new(proxy::ProxyState {
        router,
        codec,
        server_addr,
        on_server_down,
    });

    // Setup firewall redirect
    let fw_backend = if config.client.auto_redirect {
        match redirect::detect_backend() {
            Some(backend) => {
                redirect::setup_redirect(
                    backend,
                    config.client.listen_port,
                    &config.server.address,
                )?;
                Some(backend)
            }
            None => {
                tracing::warn!("No firewall backend (nftables/iptables) found, skipping auto-redirect");
                None
            }
        }
    } else {
        None
    };

    // Run proxy (with graceful shutdown on SIGINT/SIGTERM)
    let proxy_handle = tokio::spawn(proxy::run_proxy(config.client.listen_port, state));

    // Wait for shutdown signal
    tokio::select! {
        result = proxy_handle => {
            if let Err(e) = result {
                tracing::error!("Proxy task failed: {}", e);
            }
        }
        _ = shutdown_signal() => {
            tracing::info!("Shutdown signal received");
        }
    }

    // Cleanup firewall rules
    if let Some(backend) = fw_backend {
        if let Err(e) = redirect::cleanup_redirect(backend) {
            tracing::warn!("Failed to cleanup firewall rules: {}", e);
        }
    }

    tracing::info!("XR Proxy Client stopped");
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
