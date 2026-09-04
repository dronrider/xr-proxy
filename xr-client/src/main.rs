mod dns;
mod proxy;
mod redirect;
mod udp_relay;

use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use xr_proto::config::{
    decode_key, load_client_config, validate_client_config, ObfuscationConfig, ServerEntry,
};
use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};
use xr_proto::protocol::{Codec, TargetAddr};
use xr_proto::routing;
use xr_proto::server_pool::{PoolProfile, PoolServer, ServerPool};

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
#[command(name = "xr-client", about = "XR Proxy Client, transparent proxy for OpenWRT")]
struct Cli {
    /// Path to config file
    #[arg(short, long, global = true, default_value = "/etc/xr-proxy/config.toml")]
    config: PathBuf,

    /// Override log level
    #[arg(short, long)]
    log_level: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Проверить конфиг и выйти (XR-227): парсинг, ключи, salt, адреса.
    /// Листенеры и файрвол не трогаются, годный конфиг отвечает ok.
    Validate,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Install a panic hook that writes to a file, so crash info is not lost.
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("PANIC: {}", info);
        eprintln!("{}", msg);
        log_to_file(&msg);
    }));

    let cli = Cli::parse();
    if let Some(Commands::Validate) = cli.command {
        validate_or_exit(&cli.config);
    }

    if let Err(e) = run(cli).await {
        let msg = format!("FATAL: {}", e);
        eprintln!("{}", msg);
        log_to_file(&msg);
        std::process::exit(1);
    }
}

/// `xr-client validate` (XR-227): сухой прогон конфига, без листенеров
/// и файрвола. Годный конфиг отвечает ok и нулём, битый уходит в stderr
/// с названной причиной, чтобы init и deploy видели отказ до рестартов.
fn validate_or_exit(path: &Path) -> ! {
    let verdict = load_client_config(path)
        .map_err(|e| format!("{}: {e}", path.display()))
        .and_then(|config| validate_client_config(&config));
    match verdict {
        Ok(()) => {
            println!("ok");
            std::process::exit(0);
        }
        Err(reason) => {
            eprintln!("config invalid: {reason}");
            std::process::exit(1);
        }
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    // Load config
    let config = load_client_config(&cli.config)?;
    validate_client_config(&config)?;

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

    // Пул серверов (LLD-10): [[servers]] по приоритету либо legacy [server]
    // как пул из одного. Пустой пул это ошибка старта.
    let server_entries = config.server_entries()?;

    // Build router, optionally merging with hub preset.
    let geoip_path = config.geoip.as_ref().map(|g| g.database.as_str());
    let hub_config = config.hub.as_ref();
    let router = if let Some(hub) = hub_config {
        xr_core::presets::warn_if_unverified(hub.trusted_public_key.as_deref());
        let cache_dir = std::path::Path::new("/var/lib/xr-proxy/presets");
        let mut cache = xr_core::presets::PresetCache::new(
            cache_dir,
            &hub.url,
            &hub.preset,
            hub.trusted_public_key.as_deref(),
        );
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

    let on_server_down = routing::Action::on_server_down_from_str(&config.client.on_server_down);

    // Build the server pool: per-server MuxPool (N parallel mux tunnels each),
    // primary/backup by priority, failover/failback inside the pool (LLD-10).
    let mut pool_servers = Vec::with_capacity(server_entries.len());
    for entry in &server_entries {
        let addr: SocketAddr = format!("{}:{}", entry.address, entry.port)
            .parse()
            .map_err(|e| format!("invalid server address {}: {}", entry.address, e))?;
        let entry_codec = codec_for_entry(entry, &config.obfuscation, &codec)?;
        let mux_pool = xr_proto::mux_pool::MuxPool::new(
            Arc::new(move || {
                Box::pin(async move {
                    xr_proto::tunnel::connect_to_server(&addr).await
                })
            }),
            entry_codec,
            config.client.mux_pool_size,
        );
        pool_servers.push(PoolServer {
            name: entry.display_name().to_string(),
            addr: addr.to_string(),
            pool: mux_pool,
        });
    }
    let server_pool = ServerPool::new(pool_servers, PoolProfile::router(), None);

    // Фоновый пробер: держит mux ко всем серверам тёплым и возвращает трафик
    // на primary после восстановления (failback с hold-down).
    tokio::spawn(server_pool.clone().health_loop());

    let state = Arc::new(proxy::ProxyState {
        router: std::sync::RwLock::new(Arc::new(router)),
        on_server_down,
        listen_port: config.client.listen_port,
        server_pool,
    });

    // Setup firewall redirect
    let server_endpoints: Vec<redirect::ServerEndpoint> = server_entries
        .iter()
        .map(|e| redirect::ServerEndpoint {
            address: e.address.clone(),
            port: e.port,
        })
        .collect();
    let fw_backend = if config.client.auto_redirect {
        match redirect::detect_backend() {
            Some(backend) => {
                redirect::setup_redirect(
                    backend,
                    config.client.listen_port,
                    &server_endpoints,
                    &config.client.bypass_ips,
                    &config.client.bypass_rules,
                    config.client.block_quic,
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

    // Локальный DNS-форвардер (XR-285): dnsmasq спрашивает его на петле, а он
    // уносит запрос в туннель и говорит с публичным резолвером по DoT. Отказ
    // тут не роняет прокси, но и не откатывает резолв на провайдерский: он
    // ложится строкой в журнал, иначе снаружи молчание не отличить от работы.
    if config.dns.enabled {
        let dns_config = config.dns.clone();
        let pool = state.server_pool.clone();
        match dns_upstreams(&dns_config) {
            Ok((upstreams, tls_connector, server_name)) => {
                tokio::spawn(async move {
                    let connect = move || {
                        let pool = pool.clone();
                        let upstreams = upstreams.clone();
                        let tls_connector = tls_connector.clone();
                        let server_name = server_name.clone();
                        async move { open_dot(pool, tls_connector, upstreams, server_name).await }
                    };
                    if let Err(e) = dns::run_forwarder(&dns_config, connect).await {
                        tracing::error!(
                            "DNS forwarder is down ({e}); dnsmasq keeps asking it and gets nothing, \
                             LAN resolution will not fall back to the provider"
                        );
                        log_to_file(&format!("dns forwarder failed: {e}"));
                    }
                });
            }
            Err(e) => {
                tracing::error!("DNS forwarder not started: {e}");
                log_to_file(&format!("dns forwarder not started: {e}"));
            }
        }
    } else {
        tracing::warn!("DNS forwarder disabled by config: router resolution goes out in the clear");
    }

    // Background preset watch: hot-swaps the active Router when the hub
    // publishes a new preset version. Без этого таска изменения в xr-hub
    // применялись бы только при рестарте xr-client, а обойти десяток
    // роутеров вручную оператор не готов. Сам цикл общий с движком Android
    // (LLD-37): висящий запрос на хабе привозит правило за секунды, а при
    // недоступности хаба остаётся прежний опрос с backoff.
    //
    // Новые TCP-сессии после swap'а видят обновлённые правила, уже
    // активные продолжают со своим выбранным Action. Это честная семантика
    // "изменение применяется к новым соединениям".
    if let Some(hub) = config.hub.as_ref() {
        let hub_url = hub.url.clone();
        let preset_name = hub.preset.clone();
        let interval_secs = hub.refresh_interval_secs;
        let trusted_key = hub.trusted_public_key.clone();
        let local_overrides = config.routing.clone();
        let geoip_path_owned = config.geoip.as_ref().map(|g| g.database.clone());
        let state = state.clone();
        tokio::spawn(async move {
            let cache_dir = std::path::Path::new("/var/lib/xr-proxy/presets");
            let mut cache = xr_core::presets::PresetCache::new(
                cache_dir,
                &hub_url,
                &preset_name,
                trusted_key.as_deref(),
            );
            cache.load_from_disk();
            xr_core::presets::watch_loop(
                cache,
                std::time::Duration::from_secs(interval_secs),
                std::future::pending(),
                |preset_rules| {
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
                },
            )
            .await;
        });
    }

    // Run UDP relay if configured. Relay ходит только через primary: его
    // failover не входит в LLD-10 (у relay свой канал и своя семантика).
    let server_address = server_entries[0].address.clone();
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
    let mut fatal: Option<String> = None;
    tokio::select! {
        result = proxy_handle => {
            if let Some(msg) = proxy_failure(result) {
                tracing::error!("{}", msg);
                fatal = Some(msg);
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
    match fatal {
        // Правила файрвола к этому моменту уже сняты, поэтому падение
        // объявляем в самом конце, а не выходом из середины.
        Some(msg) => Err(msg.into()),
        None => Ok(()),
    }
}

/// Кодек для сервера пула: общий `[obfuscation]`, либо собранный заново, если
/// у записи есть override (`key`/`salt`/`modifier`). Это кейс «у резерва
/// другой провайдер и другой ключ» (LLD-10 §2.1).
fn codec_for_entry(
    entry: &ServerEntry,
    obfuscation: &ObfuscationConfig,
    shared: &Codec,
) -> Result<Codec, Box<dyn std::error::Error>> {
    if entry.key.is_none() && entry.salt.is_none() && entry.modifier.is_none() {
        return Ok(shared.clone());
    }
    let key = decode_key(entry.key.as_deref().unwrap_or(&obfuscation.key))?;
    let modifier = entry.modifier.as_deref().unwrap_or(&obfuscation.modifier);
    let strategy = ModifierStrategy::from_str(modifier)
        .ok_or_else(|| format!("unknown modifier strategy for server {}", entry.display_name()))?;
    let salt = entry.salt.unwrap_or(obfuscation.salt);
    let obfuscator = Obfuscator::new(key, salt as u32, strategy);
    Ok(Codec::new(obfuscator, obfuscation.padding_min, obfuscation.padding_max))
}

/// Разобранная секция `[dns]`: адреса апстримов, TLS-коннектор с корнями
/// webpki и имя, которое обязано стоять в сертификате. Ошибка тут значит, что
/// форвардер поднимать не из чего.
#[allow(clippy::type_complexity)]
fn dns_upstreams(
    cfg: &xr_proto::config::DnsClientConfig,
) -> Result<
    (
        Vec<SocketAddr>,
        tokio_rustls::TlsConnector,
        rustls::pki_types::ServerName<'static>,
    ),
    String,
> {
    let mut upstreams = Vec::with_capacity(cfg.upstreams.len());
    for up in &cfg.upstreams {
        upstreams.push(
            up.parse::<SocketAddr>()
                .map_err(|e| format!("dns upstream '{up}': {e}"))?,
        );
    }
    if upstreams.is_empty() {
        return Err("dns upstreams is empty".to_string());
    }
    let server_name = rustls::pki_types::ServerName::try_from(cfg.tls_name.clone())
        .map_err(|e| format!("dns tls_name '{}': {e}", cfg.tls_name))?;
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("rustls versions: {e}"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok((upstreams, tokio_rustls::TlsConnector::from(Arc::new(tls)), server_name))
}

/// Соединение до DoT-резолвера поверх туннельного стрима. Апстримы обходятся
/// по порядку: первый ответивший и берётся, а падение всех это причина, с
/// которой форвардер жалуется в журнал.
async fn open_dot(
    pool: Arc<ServerPool>,
    tls: tokio_rustls::TlsConnector,
    upstreams: Vec<SocketAddr>,
    server_name: rustls::pki_types::ServerName<'static>,
) -> std::io::Result<tokio_rustls::client::TlsStream<xr_proto::mux::MuxStreamIo>> {
    let mut last: Option<std::io::Error> = None;
    for addr in upstreams {
        let stream = match pool.open_stream(&TargetAddr::Ip(addr)).await {
            Ok(s) => s,
            Err(e) => {
                last = Some(e);
                continue;
            }
        };
        match tls.connect(server_name.clone(), stream.into_io()).await {
            Ok(tls_stream) => return Ok(tls_stream),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no dns upstreams configured")
    }))
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

/// Итог задачи прокси в сообщение об отказе. Мёртвый листенер и упавшая задача
/// одинаково значат, что прокси больше нет: оба случая должны довести процесс
/// до ненулевого кода выхода, иначе для procd это неотличимо от штатной
/// остановки и в crash.log ничего не попадёт.
fn proxy_failure<E: std::fmt::Display>(
    result: Result<std::io::Result<()>, E>,
) -> Option<String> {
    match result {
        Err(e) => Some(format!("Proxy task failed: {}", e)),
        Ok(Err(e)) => Some(format!("Proxy listener is dead: {}", e)),
        Ok(Ok(())) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ради этого случая замечание и заводилось: `run_proxy` возвращает
    /// `io::Result`, и раньше его ошибка терялась, проверялся только JoinError.
    #[test]
    fn dead_listener_is_a_failure() {
        let dead: Result<std::io::Result<()>, String> =
            Ok(Err(std::io::Error::from_raw_os_error(libc::EMFILE)));
        let msg = proxy_failure(dead).expect("dead listener must be reported");
        assert!(msg.contains("listener is dead"), "{msg}");
        assert!(msg.contains("Too many open files"), "{msg}");
    }

    #[test]
    fn panicked_task_is_a_failure() {
        let panicked: Result<std::io::Result<()>, String> = Err("task panicked".into());
        let msg = proxy_failure(panicked).expect("panicked task must be reported");
        assert!(msg.contains("task failed"), "{msg}");
    }

    #[test]
    fn clean_stop_is_not_a_failure() {
        let clean: Result<std::io::Result<()>, String> = Ok(Ok(()));
        assert_eq!(proxy_failure(clean), None);
    }
}
