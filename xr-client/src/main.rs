mod proxy;
mod redirect;
mod udp_relay;

use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use xr_proto::config::{decode_key, load_client_config};
use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};
use xr_proto::protocol::Codec;
use xr_proto::routing;

const CRASH_LOG: &str = "/etc/xr-proxy/crash.log";

/// Append a line to the persistent crash log file.
fn log_to_file(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(CRASH_LOG)
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{}] {}", ts, msg);
    }
}

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
async fn main() {
    // Install panic hook — write to file so we don't lose crash info
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("PANIC: {}", info);
        eprintln!("{}", msg);
        log_to_file(&msg);
    }));

    if let Err(e) = run().await {
        let msg = format!("FATAL: {}", e);
        eprintln!("{}", msg);
        log_to_file(&msg);
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
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
    log_to_file("xr-client starting");

    // Build obfuscator
    let key = decode_key(&config.obfuscation.key)?;
    let strategy = ModifierStrategy::from_str(&config.obfuscation.modifier)
        .ok_or("unknown modifier strategy")?;
    let obfuscator = Obfuscator::new(key, config.obfuscation.salt as u32, strategy);
    let udp_obfuscator = obfuscator.clone(); // for UDP relay
    let codec = Codec::new(
        obfuscator,
        config.obfuscation.padding_min,
        config.obfuscation.padding_max,
    );

    // Build router, optionally merging with hub preset.
    let geoip_path = config.geoip.as_ref().map(|g| g.database.as_str());
    let hub_config = config.hub.as_ref();
    let router = if let Some(hub) = hub_config {
        let cache_dir = std::path::Path::new("/var/lib/xr-proxy/presets");
        let mut cache = xr_core::presets::PresetCache::new(cache_dir, &hub.url, &hub.preset);
        cache.load_from_disk();
        // Forced fetch at startup with short timeout.
        let _ = cache.fetch_if_stale(std::time::Duration::from_secs(2)).await;
        if let Some(preset_rules) = cache.routing_config() {
            tracing::info!("preset '{}' loaded, merging with local overrides", hub.preset);
            routing::Router::from_merged(&config.routing, preset_rules, geoip_path)
        } else {
            tracing::warn!(
                "preset '{}' unavailable, running with local overrides only",
                hub.preset
            );
            routing::Router::new(&config.routing, geoip_path)
        }
    } else {
        routing::Router::new(&config.routing, geoip_path)
    };

    // Resolve server address
    let server_addr: SocketAddr = format!("{}:{}", config.server.address, config.server.port)
        .parse()
        .map_err(|e| format!("invalid server address: {}", e))?;

    let on_server_down = routing::Action::from_str(&config.client.on_server_down);

    // Build mux pool: persistent multiplexed connection to server.
    let mux_pool = {
        let addr = server_addr;
        xr_proto::mux_pool::MuxPool::new(
            Arc::new(move || {
                Box::pin(async move {
                    xr_proto::tunnel::connect_to_server(&addr).await
                })
            }),
            codec.clone(),
        )
    };

    let state = Arc::new(proxy::ProxyState {
        router: std::sync::RwLock::new(Arc::new(router)),
        on_server_down,
        listen_port: config.client.listen_port,
        mux_pool,
    });

    // Setup firewall redirect
    let fw_backend = if config.client.auto_redirect {
        match redirect::detect_backend() {
            Some(backend) => {
                redirect::setup_redirect(
                    backend,
                    config.client.listen_port,
                    &config.server.address,
                    &config.client.bypass_ips,
                )?;
                Some(backend)
            }
            None => {
                tracing::error!(
                    "No firewall backend found! Checked: {:?} and {:?}. \
                     Traffic will NOT be redirected to the proxy. \
                     Install nftables or iptables, or set auto_redirect = false and configure manually.",
                    ["/usr/sbin/nft", "/sbin/nft"],
                    ["/usr/sbin/iptables", "/sbin/iptables"],
                );
                None
            }
        }
    } else {
        None
    };

    // Run TCP proxy
    let proxy_handle = tokio::spawn(proxy::run_proxy(config.client.listen_port, state.clone()));

    // Background preset refresh task — hot-swaps the active Router when
    // the hub publishes a new preset version. Без этого таска изменения
    // в xr-hub применялись бы только при рестарте xr-client, а обойти
    // десяток роутеров вручную оператор не готов.
    //
    // Новые TCP-сессии после swap'а видят обновлённые правила; уже
    // активные продолжают со своим выбранным Action — это честная
    // семантика "изменение применяется к новым соединениям".
    if let Some(hub) = config.hub.as_ref() {
        let hub_url = hub.url.clone();
        let preset_name = hub.preset.clone();
        let interval_secs = hub.refresh_interval_secs;
        let local_overrides = config.routing.clone();
        let geoip_path_owned = config.geoip.as_ref().map(|g| g.database.clone());
        let state = state.clone();
        tokio::spawn(async move {
            let cache_dir = std::path::Path::new("/var/lib/xr-proxy/presets");
            let mut cache = xr_core::presets::PresetCache::new(cache_dir, &hub_url, &preset_name);
            cache.load_from_disk();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
                let changed = cache
                    .fetch_if_stale(std::time::Duration::from_secs(5))
                    .await;
                if !changed {
                    continue;
                }
                let Some(preset_rules) = cache.routing_config() else {
                    continue;
                };
                let new_router = routing::Router::from_merged(
                    &local_overrides,
                    preset_rules,
                    geoip_path_owned.as_deref(),
                );
                match state.router.write() {
                    Ok(mut guard) => {
                        *guard = Arc::new(new_router);
                        tracing::info!(
                            "preset '{}' hot-swapped: new rules active without restart",
                            preset_name
                        );
                    }
                    Err(e) => {
                        tracing::error!("failed to acquire router write lock: {}", e);
                    }
                }
            }
        });
    }

    // Run UDP relay if configured
    let server_address = config.server.address.clone();
    let udp_handle = if let Some(udp_config) = config.udp_relay {
        if udp_config.enabled {
            tracing::info!("Starting UDP relay (port {})", udp_config.listen_port);
            Some(tokio::spawn(async move {
                if let Err(e) = udp_relay::run_udp_relay(&udp_config, udp_obfuscator, &server_address).await {
                    tracing::error!("UDP relay failed: {}", e);
                }
            }))
        } else {
            None
        }
    } else {
        None
    };

    // Wait for shutdown signal
    tokio::select! {
        result = proxy_handle => {
            if let Err(e) = result {
                let msg = format!("Proxy task failed: {}", e);
                tracing::error!("{}", msg);
                log_to_file(&msg);
            }
        }
        _ = async {
            if let Some(h) = udp_handle { h.await.ok(); }
            else { std::future::pending::<()>().await; }
        } => {
            tracing::warn!("UDP relay exited");
        }
        _ = shutdown_signal() => {
            tracing::info!("Shutdown signal received");
            log_to_file("shutdown signal received");
        }
    }

    // Cleanup firewall rules
    if let Some(backend) = fw_backend {
        if let Err(e) = redirect::cleanup_redirect(backend) {
            tracing::warn!("Failed to cleanup firewall rules: {}", e);
        }
    }

    tracing::info!("XR Proxy Client stopped");
    log_to_file("xr-client stopped");
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
