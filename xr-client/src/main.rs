mod dns;
mod proxy;
mod redirect;
mod udp_relay;
mod wan;

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
    // Пул по умолчанию ходит основным маршрутом; профили выхода XR-310
    // добавляют свои пулы на других каналах с раздельным здоровьем.
    let server_pool = build_server_pool(
        &server_entries,
        None,
        &config.obfuscation,
        &codec,
        config.client.mux_pool_size,
    )?;

    // Фоновый пробер: держит mux ко всем серверам тёплым и возвращает трафик
    // на primary после восстановления (failback с hold-down).
    tokio::spawn(server_pool.clone().health_loop());

    // Политики устройств (XR-310) уже прошли validate, здесь они собираются
    // повторно тем же разбором и превращаются в пулы по ключу канала и
    // подмножества серверов. Профиль без wan с полным пулом ложится на пул
    // по умолчанию, отдельного пула для него не строится.
    let policy = xr_proto::device::DevicePolicySet::compile(&config)?;
    let mut pools: std::collections::HashMap<String, Arc<ServerPool>> =
        std::collections::HashMap::new();
    pools.insert(
        xr_proto::device::pool_key(None, &server_entries),
        server_pool.clone(),
    );
    for spec in policy.pool_specs(&config)? {
        if pools.contains_key(&spec.key) {
            continue;
        }
        let pool = build_server_pool(
            &spec.servers,
            spec.interface.as_deref(),
            &config.obfuscation,
            &codec,
            config.client.mux_pool_size,
        )?;
        tracing::info!(
            "exit pool '{}' on {} ({} server(s))",
            spec.key,
            spec.interface.as_deref().unwrap_or("main"),
            spec.servers.len()
        );
        tokio::spawn(pool.clone().health_loop());
        pools.insert(spec.key, pool);
    }
    let wan_pool = if config.client.wan_pool.is_empty() {
        None
    } else {
        let mut specs = policy.pool_specs(&config)?;
        let default_key = xr_proto::device::pool_key(None, &server_entries);
        let mut balanced = std::collections::HashMap::new();
        let mut entries_by_key = vec![(default_key, server_entries.clone())];
        for spec in specs.drain(..).filter(|spec| spec.interface.is_none()) {
            if !entries_by_key.iter().any(|(key, _)| key == &spec.key) {
                entries_by_key.push((spec.key, spec.servers));
            }
        }
        for (key, entries) in entries_by_key {
            let mut member_pools = Vec::new();
            for member in &config.client.wan_pool {
                let pool = build_server_pool(
                    &entries, Some(&member.interface), &config.obfuscation,
                    &codec, config.client.mux_pool_size,
                )?;
                tokio::spawn(pool.clone().health_loop());
                member_pools.push(pool);
                tracing::info!("WAN pool '{}' via {} weight {}", key, member.interface, member.weight);
            }
            balanced.insert(key, member_pools);
        }
        Some(Arc::new(wan::WanPool::new(config.client.wan_pool.clone(), balanced)))
    };

    if let Some(udp) = config.udp_relay.as_ref().filter(|u| u.enabled) {
        for warning in policy.udp_relay_overlap_warnings(&udp.source_ips) {
            tracing::warn!("{warning}");
        }
    }

    let state = Arc::new(proxy::ProxyState {
        router: std::sync::RwLock::new(Arc::new(router)),
        on_server_down,
        listen_port: config.client.listen_port,
        server_pool,
        policy,
        pools,
        wan_pool,
        wan_pool_bypass_ips: config.client.wan_pool_bypass_ips.clone(),
        default_pool_key: xr_proto::device::pool_key(None, &server_entries),
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
        // Пулы DoT независимы от политик устройств. Гонка до завершения TLS
        // не даёт мёртвому каналу съесть общий срок DNS-запроса.
        let mut dns_pools = vec![state.server_pool.clone()];
        let mut extra_keys: Vec<&String> = state.pools.keys().collect();
        extra_keys.sort();
        for key in extra_keys {
            let pool = state.pools.get(key).expect("ключ только что был");
            if !dns_pools.iter().any(|p| Arc::ptr_eq(p, pool)) {
                dns_pools.push(pool.clone());
            }
        }
        if let Some(wan_pool) = &state.wan_pool {
            if let Some(pools) = wan_pool.pools.get(&state.default_pool_key) {
                dns_pools.extend(pools.iter().cloned());
            }
        }
        match dns_upstreams(&dns_config) {
            Ok((upstreams, tls_connector, server_name)) => {
                tokio::spawn(async move {
                    let connect = move || {
                        let dns_pools = dns_pools.clone();
                        let upstreams = upstreams.clone();
                        let tls_connector = tls_connector.clone();
                        let server_name = server_name.clone();
                        async move {
                            open_dot(dns_pools, tls_connector, upstreams, server_name).await
                        }
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

/// Собрать пул серверов (LLD-10) на заданном канале. `interface` это имя
/// интерфейса канала для SO_BINDTODEVICE (XR-310): туннели, переподключения
/// и пробы здоровья пула ходят только через него. `None` отпускает соединения
/// на основной маршрут ОС, как было до XR-310.
fn build_server_pool(
    entries: &[ServerEntry],
    interface: Option<&str>,
    obfuscation: &ObfuscationConfig,
    shared: &Codec,
    mux_pool_size: usize,
) -> Result<Arc<ServerPool>, Box<dyn std::error::Error>> {
    let mut pool_servers = Vec::with_capacity(entries.len());
    for entry in entries {
        let addr: SocketAddr = format!("{}:{}", entry.address, entry.port)
            .parse()
            .map_err(|e| format!("invalid server address {}: {}", entry.address, e))?;
        let entry_codec = codec_for_entry(entry, obfuscation, shared)?;
        let iface = interface.map(str::to_string);
        let mux_pool = xr_proto::mux_pool::MuxPool::new(
            Arc::new(move || {
                let iface = iface.clone();
                Box::pin(async move { wan::tunnel_connect(iface.as_deref(), addr).await })
            }),
            entry_codec,
            mux_pool_size,
        );
        pool_servers.push(PoolServer {
            name: entry.display_name().to_string(),
            addr: addr.to_string(),
            pool: mux_pool,
        });
    }
    Ok(ServerPool::new(pool_servers, PoolProfile::router(), None))
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

/// Первый готовый DoT через любой настроенный туннель. Последовательный
/// обход пулов тратил весь DNS timeout на blackhole первого WAN, хотя
/// второй WAN отвечал. На пул приходится одна попытка; апстримы внутри
/// него сохраняют порядок. Общий срок ограничивает DNS-форвардер.
async fn open_dot(
    pools: Vec<Arc<ServerPool>>,
    tls: tokio_rustls::TlsConnector,
    upstreams: Vec<SocketAddr>,
    server_name: rustls::pki_types::ServerName<'static>,
) -> std::io::Result<tokio_rustls::client::TlsStream<xr_proto::mux::MuxStreamIo>> {
    let mut attempts = tokio::task::JoinSet::new();
    for (index, pool) in pools.into_iter().enumerate() {
        let tls = tls.clone();
        let upstreams = upstreams.clone();
        let server_name = server_name.clone();
        attempts.spawn(async move {
            let mut last = None;
            for addr in upstreams {
                let result = async {
                    let stream = pool.open_stream(&TargetAddr::Ip(addr)).await?;
                    tls.connect(server_name.clone(), stream.into_io()).await
                }
                .await;
                match result {
                    Ok(stream) => return Ok((index, stream)),
                    Err(e) => last = Some(e),
                }
            }
            Err(last.unwrap_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "no dns upstreams configured")
            }))
        });
    }
    let mut last = None;
    while let Some(result) = attempts.join_next().await {
        match result {
            Ok(Ok((index, stream))) => {
                tracing::info!("dns: DoT connected through tunnel pool {}", index);
                // Drop JoinSet отменяет проигравшие попытки, в том числе TLS.
                return Ok(stream);
            }
            Ok(Err(e)) => last = Some(e),
            Err(e) => last = Some(std::io::Error::other(e)),
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no dns tunnel pools configured",
        )
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
    fn dns_test_codec() -> Codec {
        Codec::new(
            Obfuscator::new(vec![42; 32], 12, ModifierStrategy::PositionalXorRotate),
            0,
            0,
        )
    }

    fn dns_test_tls() -> (tokio_rustls::TlsConnector, tokio_rustls::TlsAcceptor) {
        // Одноразовый ключ и самоподписанный localhost-сертификат только
        // для стенда. В боевом dns_upstreams остаются корни webpki.
        let cert = rustls::pki_types::CertificateDer::from(
            include_bytes!("../tests/fixtures/dns-localhost.der").to_vec(),
        );
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(
            include_bytes!("../tests/fixtures/dns-localhost-key.der").to_vec(),
        );
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.clone()).unwrap();
        let client = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key.into())
            .unwrap();
        (
            tokio_rustls::TlsConnector::from(Arc::new(client)),
            tokio_rustls::TlsAcceptor::from(Arc::new(server)),
        )
    }

    fn dns_test_pool(connect: xr_proto::mux_pool::ConnectFn) -> Arc<ServerPool> {
        ServerPool::new(
            vec![PoolServer {
                name: "dns-test".into(),
                addr: "127.0.0.1:1".into(),
                pool: xr_proto::mux_pool::MuxPool::new(connect, dns_test_codec(), 1),
            }],
            PoolProfile::mobile(),
            None,
        )
    }

    async fn dns_test_serving_pool(
        acceptor: tokio_rustls::TlsAcceptor,
        stall_tls: bool,
    ) -> Arc<ServerPool> {
        use tokio::io::AsyncReadExt;
        use xr_proto::mux::{mux_handshake_server, Multiplexer};
        use xr_proto::protocol::Command;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let codec = dns_test_codec();
            let mut bytes = Vec::new();
            let init = loop {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).await.unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some((frame, _)) = codec.decode_frame(&bytes).unwrap() {
                    break frame;
                }
            };
            let caps = mux_handshake_server(&mut stream, &codec, &init)
                .await
                .unwrap()
                .unwrap();
            let mux = Multiplexer::new_server(stream, codec, caps);
            let mut incoming = mux.take_new_stream_rx().await.unwrap();
            while let Some(ns) = incoming.recv().await {
                let stream = mux.register_stream(ns.stream_id).await;
                mux.send_frame(ns.stream_id, Command::ConnectAck, vec![0])
                    .await
                    .unwrap();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if stall_tls {
                        let _stream = stream;
                        std::future::pending::<()>().await;
                    } else if let Ok(mut tls) = acceptor.accept(stream.into_io()).await {
                        // Настоящие данные поверх TLS подтверждают не только
                        // создание mux-стрима, но и готовность DoT-транспорта.
                        use tokio::io::AsyncWriteExt;
                        loop {
                            let mut len = [0; 2];
                            if tls.read_exact(&mut len).await.is_err() { break; }
                            let mut msg = vec![0; u16::from_be_bytes(len) as usize];
                            if tls.read_exact(&mut msg).await.is_err() { break; }
                            if tls.write_all(&len).await.is_err() { break; }
                            if tls.write_all(&msg).await.is_err() { break; }
                        }
                    }
                });
            }
        });
        dns_test_pool(Arc::new(move || {
            Box::pin(tokio::net::TcpStream::connect(addr))
        }))
    }

    struct DnsActiveConnect(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for DnsActiveConnect {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    // Последовательный open_dot проводит весь срок на первом пуле.
    // Живой пул стоит после пяти мёртвых, чтобы очередь с лимитом 4 тоже
    // не могла скрыть регрессию. Проверяется TLS, а не один ConnectAck.
    #[tokio::test]
    async fn dns_live_wan_wins_over_first_blackhole_pool() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (tls, acceptor) = dns_test_tls();
        let active = Arc::new(AtomicUsize::new(0));
        let mut pools: Vec<_> = (0..5).map(|_| {
            let active = active.clone();
            dns_test_pool(Arc::new(move || {
                let active = active.clone();
                Box::pin(async move {
                    active.fetch_add(1, Ordering::SeqCst);
                    let _guard = DnsActiveConnect(active);
                    std::future::pending().await
                })
            }))
        }).collect();
        pools.push(dns_test_serving_pool(acceptor, false).await);
        let stream = tokio::time::timeout(std::time::Duration::from_secs(5), open_dot(
            pools, tls, vec!["9.9.9.9:853".parse().unwrap()],
            "localhost".try_into().unwrap(),
        )).await.expect("рабочий WAN не должен ждать blackhole другого канала")
            .expect("стенд должен завершить настоящий TLS через mux");
        drop(stream);
        tokio::task::yield_now().await;
        assert_eq!(active.load(Ordering::SeqCst), 0,
            "готовый DoT должен отменить все проигравшие подключения");
    }

    #[tokio::test]
    async fn dns_live_wan_wins_over_first_stalled_tls() {
        let (tls, acceptor) = dns_test_tls();
        let stalled = dns_test_serving_pool(acceptor.clone(), true).await;
        let alive = dns_test_serving_pool(acceptor, false).await;
        let stream = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            open_dot(
                vec![stalled, alive],
                tls,
                vec!["9.9.9.9:853".parse().unwrap()],
                "localhost".try_into().unwrap(),
            ),
        )
        .await
        .expect("готовый mux с зависшим TLS не должен блокировать другой WAN")
        .expect("живой WAN должен завершить TLS");
        drop(stream);
    }

    #[tokio::test(start_paused = true)]
    async fn dns_all_failed_pools_return_error_without_direct_fallback() {
        let (tls, _) = dns_test_tls();
        let failed = dns_test_pool(Arc::new(|| {
            Box::pin(async {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NetworkUnreachable,
                    "WAN down",
                ))
            })
        }));
        let result = open_dot(
            vec![failed],
            tls,
            vec!["9.9.9.9:853".parse().unwrap()],
            "localhost".try_into().unwrap(),
        )
        .await;
        assert!(result.is_err(), "в обход туннеля DNS уходить не должен");
    }

    #[tokio::test(start_paused = true)]
    async fn dns_blackhole_deadline_cancels_connector() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Active(Arc<AtomicUsize>);
        impl Drop for Active {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let active = Arc::new(AtomicUsize::new(0));
        let connect_count = active.clone();
        let stalled = dns_test_pool(Arc::new(move || {
            let active = connect_count.clone();
            Box::pin(async move {
                active.fetch_add(1, Ordering::SeqCst);
                let _guard = Active(active);
                std::future::pending().await
            })
        }));
        let (tls, _) = dns_test_tls();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            open_dot(
                vec![stalled],
                tls,
                vec!["9.9.9.9:853".parse().unwrap()],
                "localhost".try_into().unwrap(),
            ),
        )
        .await;
        assert!(result.is_err(), "blackhole должен закончиться DNS deadline");
        tokio::task::yield_now().await;
        assert_eq!(
            active.load(Ordering::SeqCst),
            0,
            "после deadline не должно остаться фонового подключения"
        );
    }
    #[tokio::test]
    async fn dns_racing_two_healthy_pools_keeps_winner_stream_alive() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (tls, acceptor) = dns_test_tls();
        let first = dns_test_serving_pool(acceptor.clone(), false).await;
        let second = dns_test_serving_pool(acceptor, false).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for round in 0..16u8 {
                let mut stream = open_dot(vec![first.clone(), second.clone()], tls.clone(),
                    vec!["9.9.9.9:853".parse().unwrap()], "localhost".try_into().unwrap())
                    .await.unwrap();
                for n in 0..8u8 {
                    let request = [0, 4, round, n, 42, 7];
                    stream.write_all(&request).await.unwrap();
                    let mut reply = [0; 6];
                    stream.read_exact(&mut reply).await
                        .expect("отмена второго рабочего пула закрыла победивший DoT");
                    assert_eq!(reply, request);
                }
            }
        }).await.expect("гонка двух рабочих пулов не должна вызвать DNS churn");
    }

}
