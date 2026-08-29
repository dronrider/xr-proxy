mod fallback;
mod handler;
mod mux_handler;
mod udp_relay;

use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::Duration;
use xr_proto::accept::accept_loop;
use xr_proto::config::{decode_key, load_server_config};
use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};
use xr_proto::protocol::Codec;

#[derive(Parser)]
#[command(name = "xr-server", about = "XR Proxy Server — lightweight obfuscated proxy server")]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value = "/etc/xr-proxy/server.toml")]
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
    let udp_obfuscator = obfuscator.clone();
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
    // Кап стримов внутри mux-сессий (XR-199): семафор коннектов их не видит.
    let stream_limits = mux_handler::StreamLimits::new(
        config.limits.max_streams as usize,
        config.limits.max_streams_per_mux as usize,
    );
    // Кап называется в логе на старте: иначе о нём узнают только по отказу, а
    // молчание неотличимо от сервера, где лимита нет вовсе.
    tracing::info!(
        "Limits: {} connections, {} streams total, {} streams per mux session",
        max_conns,
        config.limits.max_streams,
        config.limits.max_streams_per_mux
    );

    // Start UDP relay if configured
    if let Some(udp_config) = config.udp_relay {
        if udp_config.enabled {
            let udp_obfs = udp_obfuscator;
            tokio::spawn(async move {
                if let Err(e) = udp_relay::run_udp_relay_server(
                    udp_config.listen_port,
                    udp_obfs,
                    udp_config.flow_timeout_sec,
                    udp_config.max_flows,
                    udp_config.incoming_port_min,
                    udp_config.incoming_port_max,
                ).await {
                    tracing::error!("UDP relay server failed: {}", e);
                }
            });
        }
    }

    // Accept loop
    let listener = &listener;
    let outcome = accept_loop(
        "server",
        move || async move {
            tokio::select! {
                result = listener.accept() => result.map(Some),
                _ = shutdown_signal() => {
                    tracing::info!("Shutdown signal received");
                    Ok(None)
                }
            }
        },
        |stream, addr| {
            let codec = codec.clone();
            let fallback = fallback_response.clone();
            let sem = semaphore.clone();
            let limits = stream_limits.clone();

            tokio::spawn(async move {
                handler::serve_connection(stream, addr, codec, timeout, fallback, limits, &sem)
                    .await;
            });
            std::future::ready(())
        },
    )
    .await;

    tracing::info!("XR Proxy Server stopped");
    outcome?;
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
