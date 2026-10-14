//! VPN Engine - main entry point for mobile/desktop clients.
//!
//! Architecture:
//! 1. DNS queries (UDP:53) -> intercepted via Fake DNS
//! 2. Other IPv4 UDP -> UdpRelay (flow table + tunnel socket to VPS)
//! 3. TCP SYN -> smoltcp socket (listen on unique ephemeral port)
//! 4. TCP Established -> spawn relay task to xr-server or direct
//! 5. TCP data: smoltcp <-> channels <-> relay task

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};

use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::State as TcpState;
use tokio::sync::mpsc;
use tokio::time::Duration;

use xr_proto::config::{decode_key, RoutingConfig, ServerEntry};
use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};
use xr_proto::protocol::Codec;
use xr_proto::routing::{Action, Router};
use xr_proto::server_pool::{PoolProfile, PoolServer, ServerPool};

use crate::dns::FakeDns;
use crate::ip_stack::{IpStack, PacketQueue};
use crate::session::{relay_session_with_domain, ProtectSocketFn, SessionContext, SystemResolverFn, TcpSessionKey};
use crate::state::{StateHandle, VpnState};
use crate::stats::Stats;

/// Буферы smoltcp на одну TCP-сессию по умолчанию: заводятся на каждый SYN и
/// живут, пока живёт сессия. На них держится бюджет памяти движка под NE
/// (LLD-39): расширение NEPacketTunnelProvider не переживает 50 МБ на процесс,
/// а 128 КиБ на сессию это главная статья расхода. Конкретные значения живут
/// в `tuning::TuningProfile`, NE-профиль ужимает их до 16 КБ. Меняя размер,
/// смотри `tests/ios_memory_budget.rs`.
pub const SESSION_RX_BUF: usize = crate::tuning::DEFAULT_SESSION_RX_BUF;
pub const SESSION_TX_BUF: usize = crate::tuning::DEFAULT_SESSION_TX_BUF;

/// Ephemeral port counter for unique listen endpoints.
/// Each smoltcp socket listens on a unique port so multiple SYNs
/// to the same dst port (e.g. 443) don't conflict.
static EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(10000);

/// Текст ошибки прогрева, он же уходит на экран приложения. Пул из нескольких
/// серверов присылает ошибку, которая уже перечисляет их все (XR-131): префикс
/// про один сервер сузил бы её до последнего резерва, хотя при блокировке
/// оператором ложатся все разом, primary в том числе.
fn warmup_error_text(pool_size: usize, e: &io::Error) -> String {
    if pool_size > 1 {
        format!("Нет связи, {}", e)
    } else {
        format!("Сервер недоступен: {}", e)
    }
}

fn next_ephemeral_port() -> u16 {
    let port = EPHEMERAL_PORT.fetch_add(1, Ordering::Relaxed);
    if port >= 60000 {
        EPHEMERAL_PORT.store(10000, Ordering::Relaxed);
    }
    port
}

pub struct VpnConfig {
    /// Legacy одиночный сервер; используется, когда `servers` пуст.
    pub server_address: String,
    pub server_port: u16,
    /// Пул серверов профиля (LLD-10): primary/backup по `priority`.
    /// Ключ/salt/modifier общие на профиль; per-endpoint override на
    /// Android не поддерживается by design (§2.6), поля записей игнорируются.
    pub servers: Vec<ServerEntry>,
    pub obfuscation_key: String,
    pub modifier: String,
    pub salt: u32,
    pub padding_min: u8,
    pub padding_max: u8,
    pub routing: RoutingConfig,
    pub geoip_path: Option<String>,
    pub on_server_down: String,
    /// DNS resolvers (host:port or IP) used for direct-connection name
    /// resolution via protected sockets. Typically supplied by the host
    /// platform (e.g. Android ConnectivityManager). Empty means use only
    /// the public fallback list.
    pub dns_resolvers: Vec<String>,
    /// Hub configuration for centralized presets.
    pub hub_url: Option<String>,
    pub hub_preset: Option<String>,
    pub hub_cache_dir: Option<String>,
    /// Seconds between background preset refresh attempts (default 300).
    /// Same semantic as `xr-client` and hot-swaps the active Router when
    /// the preset version changes on the hub. Ignored if `hub_url`/
    /// `hub_preset`/`hub_cache_dir` are not all set.
    pub hub_refresh_interval_secs: Option<u64>,
    /// Публичный ключ проверки подписи пресета (XR-207), base64. Ключ задан:
    /// пресет без подписи или с неверной подписью не применяется, движок
    /// живёт на прежних правилах. Ключа нет: пресет хаба доверяется целиком.
    pub hub_trusted_public_key: Option<String>,
    /// Optional host-OS resolver for direct-mode fake-IP resolution.
    /// On Android it should call `Network.getAllByName()` on the underlying
    /// non-VPN network. When set, it's tried before the UDP:53 fallback.
    pub system_resolver: Option<SystemResolverFn>,
    /// Number of parallel mux tunnels (0 -> pool default).
    pub mux_pool_size: usize,
    /// Порт UDP relay на VPS. По умолчанию 9999 (`DEFAULT_RELAY_PORT`),
    /// нестандартный задаётся конфигом: без этого UDP молчал бы при живом TCP.
    pub udp_relay_port: u16,
    /// Рычаги тюнинга памяти (LLD-39). `Default` повторяет прежние литералы,
    /// профиль `ne` ужимает буферы и очереди под Network Extension.
    pub tuning: crate::tuning::TuningProfile,
    /// Резать QUIC (UDP/443) на входе в туннель. На mac-пути это срез H3:
    /// браузер откатывается на TCP, а чувствительные веб-интерфейсы не уходят
    /// с IP сервера мимо резидента. Android по умолчанию не режет.
    pub block_quic: bool,
}

pub struct VpnEngine {
    config: VpnConfig,
    state: StateHandle,
    stats: Stats,
    shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
    /// Live server pool, kept after `start()` so `on_network_changed` can
    /// recycle it when the underlying network switches. `None` while not
    /// running.
    server_pool: Option<Arc<ServerPool>>,
    /// Network-generation signal. Bumped on every underlying-network switch;
    /// the event loop drops all active sessions when it changes so they
    /// re-establish on the new uplink. `None` while not running.
    netgen_tx: Option<tokio::sync::watch::Sender<u64>>,
    /// Контекст живых сессий: через него `apply_user_rules` подменяет
    /// активный роутер. `None`, пока движок не запущен.
    ctx: Option<Arc<SessionContext>>,
    /// Локальные правила пользователя в общем владении с фоновым watch'ем
    /// пресета, он же точка сериализации пересборок (см. `swap_router`).
    /// Отдельная копия нужна потому, что merged-роутер собирается из двух
    /// половин: правки пользователя и правил хаба. Читай watch эту половину
    /// из своей копии, снятой на старте, первая же новая версия пресета
    /// откатила бы применённое на лету правило.
    live_overrides: Option<Arc<std::sync::Mutex<RoutingConfig>>>,
}

/// Собрать роутер из локальных правил и (если он есть) пресета хаба. Одна
/// точка сборки на все три повода: старт движка, новая версия с хаба и
/// правка пользователя на живом туннеле.
fn build_router(
    overrides: &RoutingConfig,
    preset: Option<&RoutingConfig>,
    geoip_path: Option<&str>,
) -> Router {
    match preset {
        Some(preset) => Router::from_merged(overrides, preset, geoip_path),
        None => Router::new(overrides, geoip_path),
    }
}

/// Подменить активный роутер живого движка пересобранным. Оба повода
/// пересборки (новая версия пресета с хаба и правка пользователя) ходят
/// сюда, и обоим локальную половину даёт один и тот же `overrides`: разъедься
/// они, публикация с хаба вернула бы правила, какими они были на старте.
///
/// Лок `overrides` держится на весь цикл «записать новую половину, собрать,
/// подменить». Двум поводам иначе нечем разойтись: пересекись они, порядок
/// записи в `ctx.router` решал бы не порядок событий, а то, чей снимок
/// локальной половины оказался старше, и публикация пресета молча перекрыла
/// бы только что применённое правило пользователя. Новую локальную половину
/// пишет тот же вызов (`new_overrides`), отдельного захвата под неё нет.
fn swap_router(
    ctx: &SessionContext,
    overrides: &std::sync::Mutex<RoutingConfig>,
    new_overrides: Option<RoutingConfig>,
    preset: Option<&RoutingConfig>,
    geoip_path: Option<&str>,
    reason: &str,
) -> bool {
    // Отравленный лок восстанавливаем: паника в чужом месте не должна
    // застревать правила маршрутизации до перезапуска движка.
    let mut overrides = match overrides.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!("local overrides lock poisoned, recovering");
            poisoned.into_inner()
        }
    };
    if let Some(fresh) = new_overrides {
        *overrides = fresh;
    }
    let new_router = build_router(&overrides, preset, geoip_path);
    match ctx.router.write() {
        Ok(mut guard) => {
            *guard = Arc::new(new_router);
            tracing::info!("routing rules active without restart: {}", reason);
            true
        }
        Err(e) => {
            tracing::error!("failed to acquire router write lock: {}", e);
            false
        }
    }
}

impl VpnEngine {
    pub fn new(config: VpnConfig) -> Self {
        Self {
            config,
            state: StateHandle::new(),
            stats: Stats::new(),
            shutdown_tx: None,
            server_pool: None,
            netgen_tx: None,
            ctx: None,
            live_overrides: None,
        }
    }
    pub fn state(&self) -> &StateHandle { &self.state }
    pub fn stats(&self) -> &Stats { &self.stats }

    pub fn start(&mut self, queue: PacketQueue, protect_socket: ProtectSocketFn) -> io::Result<()> {
        if self.shutdown_tx.is_some() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "already running"));
        }
        self.state.set(VpnState::Connecting);

        let key = decode_key(&self.config.obfuscation_key)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let strategy = ModifierStrategy::from_str(&self.config.modifier)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unknown modifier"))?;
        let obfuscator = Obfuscator::new(key, self.config.salt, strategy);
        // Копия уезжает в UDP-реле: провод общий с туннелем, мутаторы те же.
        let udp_obfuscator = obfuscator.clone();
        let codec = Codec::new(obfuscator, self.config.padding_min, self.config.padding_max);

        // Build router, optionally merging with hub preset.
        let router = if let (Some(hub_url), Some(preset_name), Some(cache_dir)) = (
            &self.config.hub_url,
            &self.config.hub_preset,
            &self.config.hub_cache_dir,
        ) {
            crate::presets::warn_if_unverified(self.config.hub_trusted_public_key.as_deref());
            let mut cache = crate::presets::PresetCache::new(
                std::path::Path::new(cache_dir),
                hub_url,
                preset_name,
                self.config.hub_trusted_public_key.as_deref(),
            );
            cache.load_from_disk();
            // Try short fetch (2s timeout) at startup.
            let rt = tokio::runtime::Handle::try_current();
            if let Ok(handle) = rt {
                let _ = handle.block_on(cache.fetch_if_stale(Duration::from_secs(2)));
            }
            if cache.routing_config().is_some() {
                tracing::info!("merging preset '{}' with local overrides", preset_name);
            } else {
                tracing::warn!(
                    "preset '{}' unavailable, running with local overrides only",
                    preset_name
                );
            }
            build_router(
                &self.config.routing,
                cache.routing_config(),
                self.config.geoip_path.as_deref(),
            )
        } else {
            build_router(&self.config.routing, None, self.config.geoip_path.as_deref())
        };
        let on_server_down = Action::on_server_down_from_str(&self.config.on_server_down);
        let fake_dns = Arc::new(FakeDns::with_stats(self.stats.clone()));

        // Пул серверов профиля (LLD-10): `servers` по приоритету, при пустом
        // списке legacy-одиночный адрес как пул из одного слота.
        let mut entries = self.config.servers.clone();
        entries.sort_by_key(|e| e.priority);
        if entries.is_empty() {
            entries.push(ServerEntry {
                name: String::new(),
                address: self.config.server_address.clone(),
                port: self.config.server_port,
                priority: 0,
                key: None,
                salt: None,
                modifier: None,
            });
        }

        // Build per-server mux pools with the protected socket factory.
        let mut pool_servers = Vec::with_capacity(entries.len());
        for entry in &entries {
            let addr: SocketAddr = format!("{}:{}", entry.address, entry.port)
                .parse()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{}", e)))?;
            let protect = protect_socket.clone();
            let mux_pool = xr_proto::mux_pool::MuxPool::new_windowed(
                Arc::new(move || {
                    let protect = protect.clone();
                    let addr = addr;
                    Box::pin(async move {
                        let stream = crate::session::connect_protected_pub(addr, &protect).await?;
                        // Nagle off на mux-сокете: Connect нового стрима не должен
                        // коалесцироваться за upload-Data (симметрично роутеру,
                        // см. tunnel::connect_to_server).
                        let _ = stream.set_nodelay(true);
                        Ok(stream)
                    })
                }),
                codec.clone(),
                self.config.mux_pool_size,
                self.config.tuning.recv_window,
            );
            pool_servers.push(PoolServer {
                name: entry.display_name().to_string(),
                addr: addr.to_string(),
                pool: mux_pool,
            });
        }
        // Failover/failback дублируем в пользовательский журнал движка,
        // на эти записи опирается индикация LLD-10 §2.6.
        let stats_events = self.stats.clone();
        let server_pool = ServerPool::new(
            pool_servers,
            PoolProfile::mobile(),
            Some(Arc::new(move |msg: &str| stats_events.add_log(msg))),
        );

        // Parse DNS resolvers from config strings.
        // Accept both "1.2.3.4" (assume :53) and "1.2.3.4:53". Bad entries
        // are logged and skipped — they shouldn't kill startup.
        let dns_resolvers: Vec<SocketAddr> = self.config.dns_resolvers.iter()
            .filter_map(|s| {
                let s = s.trim();
                if s.is_empty() { return None; }
                if let Ok(addr) = s.parse::<SocketAddr>() {
                    return Some(addr);
                }
                if let Ok(ip) = s.parse::<IpAddr>() {
                    return Some(SocketAddr::new(ip, 53));
                }
                tracing::warn!("ignoring malformed DNS resolver entry: {}", s);
                None
            })
            .collect();

        // Keep a handle to the pool so a later network switch can recycle it
        // without tearing down the whole engine.
        self.server_pool = Some(server_pool.clone());

        let live_overrides = Arc::new(std::sync::Mutex::new(self.config.routing.clone()));
        self.live_overrides = Some(live_overrides.clone());

        let protect_for_udp = protect_socket.clone();
        let ctx = Arc::new(SessionContext {
            router: std::sync::RwLock::new(Arc::new(router)),
            codec,
            fake_dns: fake_dns.clone(),
            stats: self.stats.clone(),
            on_server_down, protect_socket,
            server_pool: server_pool.clone(),
            dns_resolvers,
            system_resolver: self.config.system_resolver.clone(),
            tuning: self.config.tuning.clone(),
            block_quic: self.config.block_quic,
        });
        self.ctx = Some(ctx.clone());

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);
        // Network-change generation: `on_network_changed` bumps it, the event
        // loop drops live sessions when it observes the change.
        let (netgen_tx, netgen_rx) = tokio::sync::watch::channel(0u64);
        self.netgen_tx = Some(netgen_tx);
        let state = self.state.clone();
        let stats = self.stats.clone();

        // UDP relay (XR-301): собственный сокет к relay-порту VPS, мимо mux.
        // Хост берётся у активного сервера пула и по его failover уезжает
        // вместе с TCP. Порт один на профиль, по умолчанию `DEFAULT_RELAY_PORT`.
        // Без реле TCP продолжает работать, UDP тогда уходит в smoltcp и
        // тонет там; старт из-за UDP не блокируем.
        let udp_relay_handle = {
            let relay_endpoints: Vec<Option<SocketAddr>> = entries
                .iter()
                .map(|e| {
                    let parsed = format!("{}:{}", e.address, self.config.udp_relay_port).parse();
                    if let Err(err) = &parsed {
                        tracing::warn!(
                            "UDP relay endpoint for {} unusable: {}",
                            e.display_name(),
                            err
                        );
                    }
                    parsed.ok()
                })
                .collect();
            match bind_relay_socket(&protect_for_udp) {
                Ok(socket) => {
                    tracing::info!(
                        "UDP relay tunnel on port {}",
                        self.config.udp_relay_port
                    );
                    Some(UdpRelayHandle {
                        relay: Arc::new(std::sync::Mutex::new(crate::udp_relay::UdpRelay::new(
                            udp_obfuscator,
                            self.config.tuning.relay_max_flows,
                            self.config.tuning.relay_flow_timeout,
                        ))),
                        socket: Arc::new(socket),
                        server_pool: server_pool.clone(),
                        relay_endpoints,
                    })
                }
                Err(e) => {
                    tracing::warn!("UDP relay disabled: {}", e);
                    None
                }
            }
        };

        // Health-пробер пула (LLD-10 §2.7, экономный профиль): в здоровом
        // состоянии молчит и не будит радио; при активном резерве пробит
        // primary раз в минуту ради failback'а с hold-down.
        {
            let pool_hl = server_pool.clone();
            let mut shutdown_hl = shutdown_rx.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = pool_hl.health_loop() => {},
                    _ = shutdown_hl.changed() => {},
                }
            });
        }

        // Background preset watch: hot-swaps `ctx.router` when the hub
        // publishes a new version. Поведение то же, что в `xr-client` (см.
        // LLD-01 §5.4): оператор правит пресет один раз, а каждый VpnEngine
        // (телефон, ноутбук) подхватывает правила без рестарта. Live-сессии
        // продолжают со своим Action, новые подключения видят обновления.
        // Цикл общий с xr-client (LLD-37): пока хаб отвечает, правило
        // доезжает за секунды висящим запросом, иначе прежний опрос.
        if let (Some(hub_url), Some(preset_name), Some(cache_dir)) = (
            self.config.hub_url.clone(),
            self.config.hub_preset.clone(),
            self.config.hub_cache_dir.clone(),
        ) {
            let interval_secs = self.config.hub_refresh_interval_secs.unwrap_or(300);
            let trusted_key = self.config.hub_trusted_public_key.clone();
            let local_overrides = live_overrides.clone();
            let geoip_path_owned = self.config.geoip_path.clone();
            let ctx_bg = ctx.clone();
            let mut shutdown_rx_bg = shutdown_rx.clone();
            tokio::spawn(async move {
                let mut cache = crate::presets::PresetCache::new(
                    std::path::Path::new(&cache_dir),
                    &hub_url,
                    &preset_name,
                    trusted_key.as_deref(),
                );
                cache.load_from_disk();
                crate::presets::watch_loop(
                    cache,
                    Duration::from_secs(interval_secs),
                    async move {
                        let _ = shutdown_rx_bg.changed().await;
                    },
                    |preset_rules| {
                        swap_router(
                            &ctx_bg,
                            &local_overrides,
                            None,
                            Some(preset_rules),
                            geoip_path_owned.as_deref(),
                            &format!("пресет '{}' обновился", preset_name),
                        );
                    },
                )
                .await;
            });
        }

        if let Some(udp_handle) = udp_relay_handle.clone() {
            spawn_udp_companions(udp_handle, queue.clone(), shutdown_rx.clone());
        }

        tokio::spawn(async move {
            stats.mark_started();

            // Health check: establish the mux connection (TCP + MuxInit
            // handshake) before reporting Connected. This verifies end-to-end
            // protocol reachability AND pre-warms the pool — the first relay
            // stream will open instantly without a separate TCP connect.
            let mut health_shutdown = shutdown_rx.clone();
            let health_result = tokio::select! {
                r = ctx.server_pool.warmup() => Some(r),
                _ = health_shutdown.changed() => None,
            };
            match health_result {
                None => {
                    // Shutdown requested during health check.
                    state.set(VpnState::Disconnected);
                    return;
                }
                Some(Err(e)) => {
                    let msg = warmup_error_text(ctx.server_pool.size(), &e);
                    stats.add_error(&msg);
                    state.set(VpnState::Error(msg));
                    return;
                }
                Some(Ok(())) => {
                    // Mux connection established — server is reachable.
                }
            }

            state.set(VpnState::Connected);
            if let Err(e) = run_event_loop(
                queue,
                ctx,
                fake_dns,
                shutdown_rx,
                netgen_rx,
                udp_relay_handle,
            ).await {
                state.set(VpnState::Error(e.to_string()));
            } else {
                state.set(VpnState::Disconnected);
            }
        });
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            self.state.set(VpnState::Disconnecting);
            let _ = tx.send(true);
        }
        // Drop the post-start handles so a stray `on_network_changed` after
        // stop is a no-op and the pool can be released.
        self.server_pool = None;
        self.netgen_tx = None;
        self.ctx = None;
        self.live_overrides = None;
    }
    pub fn is_running(&self) -> bool { self.shutdown_tx.is_some() }

    /// Применить новый список правил пользователя к живому туннелю (XR-180).
    ///
    /// Раньше правки экрана правил ждали переподключения: движок собирал
    /// merged-роутер только на старте и на новой версии пресета. Путь тот же,
    /// что у пресета: пересобрать роутер из двух половин и подменить его в
    /// `SessionContext`. Новый список и пересборка едут одним вызовом
    /// `swap_router`, то есть под общим локом: параллельная публикация с хаба
    /// либо ждёт своей очереди, либо уже видит новые правила. Пресет берётся
    /// из дискового кэша, в сеть эта ручка не ходит. Живые сессии продолжают
    /// со своим `Action`, новые решения принимаются по свежим правилам.
    ///
    /// Возвращает `false`, когда движок не запущен: правила тогда просто
    /// лежат в конфиге и уедут в роутер на ближайшем старте.
    pub fn apply_user_rules(&mut self, routing: RoutingConfig) -> bool {
        self.config.routing = routing.clone();
        let Some(overrides) = self.live_overrides.clone() else {
            return false;
        };
        let Some(ctx) = self.ctx.clone() else { return false };

        let cache = match (
            &self.config.hub_url,
            &self.config.hub_preset,
            &self.config.hub_cache_dir,
        ) {
            (Some(hub_url), Some(preset_name), Some(cache_dir)) => {
                let mut cache = crate::presets::PresetCache::new(
                    std::path::Path::new(cache_dir),
                    hub_url,
                    preset_name,
                    self.config.hub_trusted_public_key.as_deref(),
                );
                cache.load_from_disk();
                Some(cache)
            }
            _ => None,
        };
        let preset = cache.as_ref().and_then(|c| c.routing_config());
        swap_router(
            &ctx,
            &overrides,
            Some(routing),
            preset,
            self.config.geoip_path.as_deref(),
            "правила пользователя изменены",
        )
    }

    /// Активный роутер запущенного движка. Нужен проверке применения правил
    /// на лету, снаружи движка решения по нему никто не принимает.
    pub fn active_router(&self) -> Option<Arc<Router>> {
        self.ctx
            .as_ref()
            .and_then(|ctx| ctx.router.read().ok().map(|guard| guard.clone()))
    }

    /// Активный сервер пула: `(имя, работаем_через_резерв)`. Питает строку
    /// статуса «через X (резерв)» на главном экране (LLD-10 §2.6).
    /// `None`, пока движок не запущен.
    pub fn active_server_info(&self) -> Option<(String, bool)> {
        self.server_pool
            .as_ref()
            .map(|p| (p.active_name(), p.is_backup_active()))
    }

    /// React to an underlying-network switch (Android LTE↔Wi-Fi).
    ///
    /// Eagerly recycles the mux pool — its TCP sockets are bound to the now
    /// dead interface — and bumps the network generation so the event loop
    /// drops every active session, forcing each to re-establish on the new
    /// uplink. This is the in-place equivalent of the manual off/on the user
    /// previously had to do; without it, recovery waits on the slow
    /// consecutive-timeout detector and in-flight Direct sessions stall until
    /// the 1h relay timeout. No-op when the engine isn't running.
    ///
    /// Must be called from within the engine's tokio runtime context (the JNI
    /// layer enters it) because it spawns the recycle/warmup task.
    pub fn on_network_changed(&self) {
        // Signal the event loop to tear down live sessions.
        if let Some(tx) = &self.netgen_tx {
            tx.send_modify(|g| *g = g.wrapping_add(1));
        }
        // Recycle off-task: drop stale multiplexers + clear the fail-open
        // breakers (all pool servers) and reset stickiness back to primary,
        // then best-effort pre-warm so the first proxied stream after the
        // switch opens without paying a reconnect. A warmup failure is fine,
        // slots reconnect lazily on the next `open_stream`.
        if let Some(pool) = &self.server_pool {
            let pool = pool.clone();
            tokio::spawn(async move {
                pool.recycle().await;
                let _ = pool.warmup().await;
            });
        }
    }

    /// Хост сообщил, что физического аплинка не осталось (XR-296). Пул
    /// серверов замирает на прежнем активном: без сети молчат все серверы
    /// разом, и перебор резервов в это окно только рвёт сессии. Живые сессии
    /// не трогаются, они и так упираются в мёртвый интерфейс и переживут
    /// окно либо умрут своими таймаутами. No-op, пока движок не запущен;
    /// признак до старта держит JNI-слой и досылает его после `start`.
    pub fn on_uplink_lost(&self) {
        if let Some(pool) = &self.server_pool {
            pool.uplink_lost();
        }
    }

    /// Тот же аплинк вернулся после пропажи (XR-296). Живые сессии сидят на
    /// мёртвом интерфейсе и сбрасываются поколением сети, как при смене
    /// сети, а пул пересобирает mux без сброса здоровья и без возврата на
    /// primary, после чего греется через прежний активный. Настоящая смена
    /// сети идёт через `on_network_changed`. Без взведённой заморозки это
    /// no-op: рвать сессии нечем, окна не было. Вызывать изнутри runtime
    /// движка, метод поднимает задачу пересборки.
    pub fn on_uplink_restored(&self) {
        let Some(pool) = &self.server_pool else { return };
        if !pool.is_uplink_down() {
            return;
        }
        if let Some(tx) = &self.netgen_tx {
            tx.send_modify(|g| *g = g.wrapping_add(1));
        }
        let pool = pool.clone();
        tokio::spawn(async move {
            pool.uplink_restored().await;
            let _ = pool.warmup().await;
        });
    }
}

// ── Session ─────────────────────────────────────────────────────────

struct ActiveSession {
    smol_handle: SocketHandle,
    /// The real destination (from the SYN packet).
    real_dst: SocketAddr,
    /// Domain from Fake DNS (cached at SYN time, not at relay time).
    domain: Option<String>,
    /// Ephemeral port assigned to this session for smoltcp.
    eph_port: u16,
    /// Set when TCP reaches Established.
    relay: Option<RelayChannels>,
}

struct RelayChannels {
    to_relay: mpsc::Sender<Vec<u8>>,
    from_relay: mpsc::Receiver<Vec<u8>>,
    /// Bytes pulled from `from_relay` but not yet fully accepted by smoltcp's
    /// TX buffer. `send_slice` may accept only a prefix of a chunk when the TX
    /// buffer is nearly full; the remainder lives here and is retried on the
    /// next loop tick. Без этого хвост чанка молча терялся, и Direct-скачивание
    /// рвалось на плавающем смещении (баг C2, mail.ru).
    pending_download: Vec<u8>,
}

/// UDP relay (XR-301): свой сокет к relay-порту VPS, не mux. Таблица флоу
/// общая для event loop (upstream из TUN) и таска приёма (downstream в TUN).
/// `relay_endpoints` идёт вровень со слотами пула: индекс активного сервера
/// выбирает хост, на который уезжает UDP.
#[derive(Clone)]
struct UdpRelayHandle {
    relay: Arc<std::sync::Mutex<crate::udp_relay::UdpRelay>>,
    socket: Arc<tokio::net::UdpSocket>,
    server_pool: Arc<ServerPool>,
    relay_endpoints: Vec<Option<SocketAddr>>,
}

impl UdpRelayHandle {
    /// Relay-порт активного сервера. Порт у всех серверов один, хост берётся
    /// у активного, чтобы UDP не остался на мёртвом primary после failover.
    fn vps_addr(&self) -> Option<SocketAddr> {
        pick_relay_endpoint(&self.relay_endpoints, self.server_pool.active_index())
    }
}

/// Адрес relay-порта слота `active`. `None` у слота значит, что хост не
/// разобрался в IP при старте, и UDP для него молчит.
fn pick_relay_endpoint(endpoints: &[Option<SocketAddr>], active: usize) -> Option<SocketAddr> {
    endpoints.get(active).copied().flatten()
}

/// UDP-сокет к VPS: bind на любой порт, `protect` от петли через VPN, nonblocking.
fn bind_relay_socket(protect_socket: &ProtectSocketFn) -> io::Result<tokio::net::UdpSocket> {
    let sock = std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))?;
    sock.set_nonblocking(true)?;
    if !protect_socket(sock.as_raw_fd()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "protect_socket failed for UDP relay",
        ));
    }
    tokio::net::UdpSocket::from_std(sock)
}

/// Задачи-компаньоны UDP-реле: приём ответов, keepalive и чистка протухших.
/// Без них upstream уезжает, а downstream не возвращается.
fn spawn_udp_companions(
    handle: UdpRelayHandle,
    queue: PacketQueue,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    // Downstream: ответ из туннеля собирается в IP-пакет и уходит в TUN.
    {
        let handle = handle.clone();
        let queue = queue.clone();
        let mut shutdown_down = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; crate::udp_relay::UDP_RECV_BUF];
            loop {
                let n = tokio::select! {
                    r = handle.socket.recv(&mut buf) => match r {
                        Ok(n) => n,
                        Err(e) => {
                            tracing::debug!("UDP relay recv error: {}", e);
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        }
                    },
                    _ = shutdown_down.changed() => return,
                };
                let now = std::time::Instant::now();
                let packet = {
                    let mut relay = handle.relay.lock().unwrap();
                    relay.wire_to_tun(&buf[..n], now)
                };
                if let Some(pkt) = packet {
                    queue.push_outbound_public(pkt);
                }
            }
        });
    }

    // Keepalive + чистка. Keepalive держит NAT-дыру, пока есть хоть один флоу;
    // простой флоу уезжает по таймауту и возвращает порт в пул.
    {
        let handle = handle.clone();
        let mut shutdown_ka = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut keepalive = tokio::time::interval(crate::udp_relay::DEFAULT_KEEPALIVE);
            let mut cleanup = tokio::time::interval(Duration::from_secs(30));
            // Первый тик interval срабатывает сразу, это дешёвый пустой проход.
            keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            cleanup.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = keepalive.tick() => {
                        let (wire, empty) = {
                            let relay = handle.relay.lock().unwrap();
                            (relay.keepalive(), relay.is_empty())
                        };
                        if !empty {
                            if let Some(vps_addr) = handle.vps_addr() {
                                let _ = handle.socket.send_to(&wire, vps_addr).await;
                            }
                        }
                    }
                    _ = cleanup.tick() => {
                        let mut relay = handle.relay.lock().unwrap();
                        relay.retire_expired(std::time::Instant::now());
                    }
                    _ = shutdown_ka.changed() => return,
                }
            }
        });
    }
}

// ── Event loop ──────────────────────────────────────────────────────

async fn run_event_loop(
    queue: PacketQueue,
    ctx: Arc<SessionContext>,
    fake_dns: Arc<FakeDns>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    mut netgen_rx: tokio::sync::watch::Receiver<u64>,
    udp_relay: Option<UdpRelayHandle>,
) -> io::Result<()> {
    let mut stack = IpStack::new(queue.clone());
    let waker = queue.notifier();

    // Last observed network generation. When `on_network_changed` bumps it we
    // drop every active session so it re-establishes on the new uplink.
    let mut last_netgen = *netgen_rx.borrow();

    // Key: (src_addr, dst_addr) from original SYN.
    // We rewrite the SYN dst_port → ephemeral port so smoltcp can handle
    // multiple connections to the same dst port (443).
    let mut sessions: HashMap<TcpSessionKey, ActiveSession> = HashMap::new();

    // Map: ephemeral_port → original TcpSessionKey.
    // Used to find which session a smoltcp socket belongs to.
    let mut port_to_key: HashMap<u16, TcpSessionKey> = HashMap::new();

    let mut stale_keys: Vec<TcpSessionKey> = Vec::new();

    loop {
        if *shutdown_rx.borrow() { return Ok(()); }

        // ── 0. Underlying network switched? ─────────────────────────
        // On LTE↔Wi-Fi the smoltcp sessions are backed by relay sockets bound
        // to the now-dead interface; they'd otherwise hang until the 1h relay
        // timeout. Abort every session here so the app reconnects fresh on the
        // new uplink — the in-place equivalent of a manual off/on. The mux pool
        // is recycled separately in `on_network_changed`.
        {
            let cur_netgen = *netgen_rx.borrow_and_update();
            if cur_netgen != last_netgen {
                last_netgen = cur_netgen;
                // UDP-флоу старого аплинка не живут: порты возвращаются в пул.
                if let Some(udp) = &udp_relay {
                    udp.relay.lock().unwrap().clear();
                }
                if !sessions.is_empty() {
                    tracing::info!(
                        "underlying network changed (gen {}), dropping {} active session(s)",
                        cur_netgen, sessions.len()
                    );
                    for session in sessions.values_mut() {
                        stack.tcp_socket_mut(session.smol_handle).abort();
                        // Drop the relay channels → the relay task observes the
                        // closed channel, exits, and releases its protected
                        // socket on the old network.
                        session.relay = None;
                    }
                    // Flush the RSTs to the app so it reconnects promptly.
                    for _ in 0..16 { if !stack.poll() { break; } }
                    for (_key, session) in sessions.drain() {
                        stack.remove_socket(session.smol_handle);
                        ctx.stats.connection_closed();
                    }
                    port_to_key.clear();
                }
            }
        }

        // -- 1. Intercept DNS, then UDP relay --
        // DNS отвечает FakeDns, остальной UDP любого семейства уезжает своим
        // relay-портом мимо smoltcp. TCP и прочее уходят в smoltcp как раньше.
        let mut tcp_packets = Vec::new();
        while let Some(packet) = queue.pop_inbound_public() {
            // Срез QUIC (mac-путь, XR-300): UDP/443 не уезжает в туннель, иначе
            // H3 к веб-интерфейсам уходит с IP сервера мимо резидента. Браузер
            // по таймауту откатывается на TCP, тот идёт через туннель как обычно.
            if ctx.block_quic && is_udp_dst_port(&packet, 443) {
                ctx.stats.add_quic_drop();
                continue;
            }
            if let Some(dns_response) = try_handle_dns(&packet, &fake_dns) {
                ctx.stats.add_dns_query();
                queue.push_outbound_public(dns_response);
                continue;
            }
            if let Some(udp) = &udp_relay {
                if is_ip_udp(&packet) {
                    let wire = {
                        let mut relay = udp.relay.lock().unwrap();
                        relay.tun_to_wire(&packet, std::time::Instant::now())
                    };
                    if let Some(wire) = wire {
                        // try_send: очередь сокета полная, пакет теряем как UDP
                        // и теряет его на проводе; event loop не ждёт сеть.
                        if let Some(vps_addr) = udp.vps_addr() {
                            if let Err(e) = udp.socket.try_send_to(&wire, vps_addr) {
                                if e.kind() != io::ErrorKind::WouldBlock {
                                    tracing::debug!("UDP relay send failed: {}", e);
                                }
                            }
                        }
                    }
                    // Исключённый порт и переполненный пул не возвращаются
                    // в smoltcp: там UDP не обслуживается.
                    continue;
                }
            }
            tcp_packets.push(packet);
        }

        // -- 2. Process TCP packets: rewrite dst port for SYNs --
        for mut pkt in tcp_packets {
            let Some((src_ip, dst_ip, protocol, transport_off)) = parse_ip_header(&pkt) else {
                queue.push_inbound(pkt);
                continue;
            };

            // Only rewrite TCP packets.
            if protocol == 6 && pkt.len() >= transport_off + 14 {
                let src_port = u16::from_be_bytes([pkt[transport_off], pkt[transport_off + 1]]);
                let dst_port = u16::from_be_bytes([pkt[transport_off + 2], pkt[transport_off + 3]]);
                let flags = pkt[transport_off + 13];
                let is_syn = flags & 0x02 != 0 && flags & 0x10 == 0;

                let orig_key = TcpSessionKey {
                    src_addr: SocketAddr::new(src_ip, src_port),
                    dst_addr: SocketAddr::new(dst_ip, dst_port),
                };

                if is_syn && !sessions.contains_key(&orig_key) {
                    // Кап одновременных сессий (LLD-39, п. 2): новый SYN выше
                    // потолка роняется, клиент повторит сам, всплеск не съедает
                    // память. 0 на дефолтном профиле означает без капа, как было.
                    let max_sessions = ctx.tuning.max_sessions;
                    if max_sessions > 0 && sessions.len() >= max_sessions {
                        ctx.stats.add_warn(&format!(
                            "session cap {} reached, dropping {}",
                            max_sessions, orig_key.dst_addr
                        ));
                        continue;
                    }
                    // New connection: assign ephemeral port, create listen socket.
                    let eph_port = next_ephemeral_port();
                    let handle = stack.add_tcp_socket(
                        ctx.tuning.session_rx_buf,
                        ctx.tuning.session_tx_buf,
                    );
                    let socket = stack.tcp_socket_mut(handle);

                    if socket.listen(eph_port).is_ok() {
                        // Cache domain NOW: FakeDns TTL might expire before Established.
                        let domain = fake_dns.lookup_ip(dst_ip);

                        sessions.insert(orig_key, ActiveSession {
                            smol_handle: handle,
                            real_dst: orig_key.dst_addr,
                            domain,
                            eph_port,
                            relay: None,
                        });
                        port_to_key.insert(eph_port, orig_key);
                        ctx.stats.connection_opened();
                        ctx.stats.add_tcp_syn();

                        // Rewrite dst -> smoltcp's IP:eph_port.
                        rewrite_dst_to_smol(&mut pkt, transport_off, eph_port);
                    } else {
                        stack.remove_socket(handle);
                    }
                } else if let Some(session) = sessions.get(&orig_key) {
                    // Existing connection (ACK, data, FIN, etc.):
                    // Rewrite dst -> smoltcp's IP:eph_port.
                    let ep = session.eph_port;
                    rewrite_dst_to_smol(&mut pkt, transport_off, ep);
                }
            }

            queue.push_inbound(pkt);
        }

        // ── 3. Poll smoltcp ─────────────────────────────────────────
        for _ in 0..16 {
            if !stack.poll() { break; }
        }

        // ── 4. Check sessions: spawn relay, transfer data ───────────
        stale_keys.clear();

        for (key, session) in sessions.iter_mut() {
            let socket = stack.tcp_socket_mut(session.smol_handle);
            let tcp_state = socket.state();

            // Spawn relay when handshake completes.
            if tcp_state == TcpState::Established && session.relay.is_none() {
                let (to_relay_tx, to_relay_rx) = mpsc::channel(ctx.tuning.session_channel);
                let (from_relay_tx, from_relay_rx) = mpsc::channel(ctx.tuning.session_channel);
                session.relay = Some(RelayChannels {
                    to_relay: to_relay_tx,
                    from_relay: from_relay_rx,
                    pending_download: Vec::new(),
                });

                let ctx_clone = ctx.clone();
                let real_dst = session.real_dst;
                let cached_domain = session.domain.clone();
                let key_clone = *key;
                // Метка цели для записи в журнал: домен вместе с реальным адресом,
                // а без восстановленного домена (IP-литерал или просроченный fake
                // DNS-кэш) только адрес.
                let target_label = match cached_domain.as_deref() {
                    Some(d) => format!("{} ({})", d, real_dst),
                    None => real_dst.to_string(),
                };
                let relay_waker = waker.clone();
                tokio::spawn(async move {
                    let relay_key = TcpSessionKey {
                        src_addr: key_clone.src_addr,
                        dst_addr: real_dst,
                    };
                    match relay_session_with_domain(ctx_clone.clone(), relay_key, cached_domain, to_relay_rx, from_relay_tx, relay_waker).await {
                        Ok(()) => {}
                        Err(e) => {
                            let msg = format!("сбой соединения {}: {}", target_label, e);
                            let err_text = e.to_string();

                            // Специальный случай: DoT-блок. Он работает по контракту
                            // Android (kind=ConnectionRefused заставляет Private DNS
                            // упасть в UDP fallback), случается на КАЖДОМ DNS-запросе
                            // и не является ни warning'ом, ни error'ом. Пишем только
                            // в tracing для отладки; в recent_errors не пускаем.
                            if err_text.contains("DoT blocked") {
                                tracing::debug!("{}", msg);
                                return;
                            }

                            // Классификация по io::ErrorKind:
                            //   InvalidInput → policy drop (fake IP, private IP) → WARN.
                            //   ConnectionReset / ConnectionAborted → удалённый сервер
                            //     закрыл соединение (RST) или ядро оборвало при смене
                            //     сети. Не наш косяк — это нормальное сетевое событие,
                            //     понижаем до WARN, чтобы реальные ошибки не тонули в
                            //     повторяющемся шуме (напр. Kaspersky-телеметрия в
                            //     цикле получает RST от своих же серверов).
                            //     Исключение: "mux open fail" всегда ERROR — тут
                            //     недоступен наш прокси-сервер, пользователю это важно.
                            //   Всё остальное (TimedOut на connect, Other, ...) → ERROR.
                            let is_mux_fail = err_text.contains("mux open fail");
                            match e.kind() {
                                std::io::ErrorKind::InvalidInput => {
                                    ctx_clone.stats.add_warn(&msg);
                                }
                                std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                                    if !is_mux_fail =>
                                {
                                    ctx_clone.stats.add_warn(&msg);
                                }
                                _ => {
                                    ctx_clone.stats.add_error(&msg);
                                }
                            }
                        }
                    }
                });
            }

            // Transfer data.
            if let Some(ref mut relay) = session.relay {
                // smoltcp -> relay (upload). Пропускаем, когда релей уже
                // кончился (его приёмник закрыт): заливать вверх больше некому,
                // но хвост скачивания ниже надо ещё доставить.
                if !relay.to_relay.is_closed() {
                    // Никогда не делаем recv_slice, если канал к relay полон:
                    // consume'нутые из smoltcp байты тогда было бы некуда деть и
                    // их пришлось бы выбросить (потеря данных при больших upload'ах).
                    while socket.can_recv() {
                        if relay.to_relay.capacity() == 0 { break; }
                        let mut buf = vec![0u8; 32768];
                        match socket.recv_slice(&mut buf) {
                            Ok(n) if n > 0 => {
                                buf.truncate(n);
                                ctx.stats.add_smol_recv(n as u64);
                                if relay.to_relay.try_send(buf).is_err() { break; }
                            }
                            _ => break,
                        }
                    }
                }

                // relay -> smoltcp (download). На завершении релея хвост
                // доливается и сокет закрывается через FIN, а не обрывается
                // RST-ом: иначе последние байты ответа теряются (обрыв картинок).
                if finish_or_pump_download(
                    &mut *socket,
                    &mut relay.from_relay,
                    &mut relay.pending_download,
                    &ctx.stats,
                ) {
                    // Сокет закрывается штатно: перестаём его обслуживать, а
                    // саму запись снимет проверка Closed/TimeWait ниже, когда
                    // smoltcp дольёт буфер и завершит закрытие.
                    session.relay = None;
                }
            }

            // Detect closed.
            if tcp_state == TcpState::Closed || tcp_state == TcpState::TimeWait {
                stale_keys.push(*key);
            }
        }

        // ── 5. Cleanup ──────────────────────────────────────────────
        for key in &stale_keys {
            if let Some(session) = sessions.remove(key) {
                port_to_key.remove(&session.eph_port);
                stack.remove_socket(session.smol_handle);
                ctx.stats.connection_closed();
            }
        }

        // ── 6. Poll again (process data written by relay → smoltcp) ─
        for _ in 0..16 {
            if !stack.poll() { break; }
        }

        // -- 7. Rewrite outbound packets: restore original dst --
        // smoltcp sends packets with src=SMOL_IP:eph_port (or SMOL_IPV6).
        // We need to rewrite them to src=original_dst_ip:original_dst_port
        // so the TUN client sees the response from the expected address.
        while let Some(mut pkt) = queue.pop_smol_outbound() {
            restore_outbound_src(&mut pkt, &port_to_key);
            queue.push_outbound_public(pkt);
        }

        // ── 8. Debug ────────────────────────────────────────────────
        {
            let (mut established, mut syn_recv, mut listen, mut other) = (0u32, 0u32, 0u32, 0u32);
            for s in sessions.values() {
                match stack.tcp_socket(s.smol_handle).state() {
                    TcpState::Established => established += 1,
                    TcpState::SynReceived => syn_recv += 1,
                    TcpState::Listen => listen += 1,
                    _ => other += 1,
                }
            }
            if !sessions.is_empty() {
                ctx.stats.set_debug(format!(
                    "dev rx:{} tx:{} | L:{} SR:{} E:{} o:{} | s:{}",
                    stack.device.rx_count, stack.device.tx_count,
                    listen, syn_recv, established, other, sessions.len()
                ));
            }
        }

        // ── 9. Wait for new data or smoltcp timer ────────────────
        // If there's still pending data, loop immediately without waiting.
        if queue.has_inbound() || queue.has_outbound() {
            tokio::task::yield_now().await;
            continue;
        }

        // Sleep until woken by: TUN packet, relay data, smoltcp timer, or shutdown.
        let poll_delay = stack.poll_delay()
            .unwrap_or(Duration::from_millis(50))
            .min(Duration::from_millis(50));
        tokio::select! {
            _ = waker.notified() => {}
            _ = tokio::time::sleep(poll_delay) => {}
            _ = shutdown_rx.changed() => {}
            _ = netgen_rx.changed() => {}
        }
    }
}

// ── Download draining (relay → smoltcp) ─────────────────────────────

/// Abstraction over the smoltcp TX side so the drain logic can be unit-tested
/// without a full TCP handshake. The real impl forwards to smoltcp; tests use
/// a sink with a bounded accept size to exercise partial writes.
trait TxSink {
    fn can_send(&self) -> bool;
    /// Enqueue as many leading bytes as fit. Returns the count accepted
    /// (`0..=data.len()`). A short return means the TX buffer is (nearly) full.
    fn enqueue(&mut self, data: &[u8]) -> usize;
    /// Half-close the send side: no more bytes will be written, and the FIN
    /// follows whatever is still buffered. Unlike an abort, the already
    /// enqueued tail is delivered, not discarded.
    fn close(&mut self);
}

impl TxSink for smoltcp::socket::tcp::Socket<'_> {
    fn can_send(&self) -> bool {
        smoltcp::socket::tcp::Socket::can_send(self)
    }
    fn enqueue(&mut self, data: &[u8]) -> usize {
        // send_slice errors only if the socket can't send at all; treat that
        // as "0 accepted" since `can_send()` gates the call anyway.
        self.send_slice(data).unwrap_or(0)
    }
    fn close(&mut self) {
        smoltcp::socket::tcp::Socket::close(self);
    }
}

/// Обслужить сторону скачивания (таргет -> приложение) одной сессии. Пока релей
/// жив, просто перекладывает байты в буфер smoltcp. Когда релей кончился и весь
/// произведённый им хвост уже лёг в буфер, закрывает сокет через FIN и
/// возвращает `true`: сессия дальше не обслуживается, а сокет доливает буфер и
/// шлёт FIN сам.
///
/// **Почему FIN, а не abort.** Раньше на завершении релея сокет обрывался
/// `abort()`-ом (RST). Байты, уже принятые из релея в буфер smoltcp, но ещё не
/// ушедшие приложению, RST выбрасывал, и ответ обрывался на хвосте. Это било по
/// картинкам: CDN закрывает соединение сразу за последними байтами, и мы RST-ом
/// сносили их раньше доставки. `close()` доливает буфер и шлёт FIN.
fn finish_or_pump_download<S: TxSink>(
    sink: &mut S,
    from_relay: &mut mpsc::Receiver<Vec<u8>>,
    pending: &mut Vec<u8>,
    stats: &Stats,
) -> bool {
    match drain_download_to_tx(sink, from_relay, pending, stats) {
        DownloadDrain::Idle => false,
        DownloadDrain::Disconnected => {
            sink.close();
            true
        }
    }
}

enum DownloadDrain {
    /// No more to do this tick (channel empty or TX buffer full).
    Idle,
    /// Relay sender dropped — the relay task has finished.
    Disconnected,
}

/// Move download bytes (target → app) from the relay channel into the smoltcp
/// TX buffer, honoring partial `send_slice` accepts.
///
/// **Why this exists.** `send_slice` may accept only a prefix of the offered
/// slice when the TX buffer is nearly full. The old loop did
/// `let _ = socket.send_slice(&data)` and dropped the unaccepted tail, which
/// truncated fast Direct downloads at a timing-dependent offset (C2: mail.ru
/// attachments rwали ~на 3 МБ из 5, плавало). Here the leftover is stashed in
/// `pending` and retried next tick — no byte is ever dropped.
fn drain_download_to_tx(
    sink: &mut impl TxSink,
    from_relay: &mut mpsc::Receiver<Vec<u8>>,
    pending: &mut Vec<u8>,
    stats: &Stats,
) -> DownloadDrain {
    loop {
        // Flush leftover from a previous partial accept before pulling more.
        if !pending.is_empty() {
            if !sink.can_send() {
                return DownloadDrain::Idle;
            }
            let sent = sink.enqueue(pending);
            stats.add_smol_send(sent as u64);
            if sent < pending.len() {
                pending.drain(..sent);
                return DownloadDrain::Idle; // TX buffer full — retry next tick.
            }
            pending.clear();
        }

        if !sink.can_send() {
            return DownloadDrain::Idle;
        }
        match from_relay.try_recv() {
            Ok(data) => {
                let sent = sink.enqueue(&data);
                stats.add_smol_send(sent as u64);
                if sent < data.len() {
                    *pending = data[sent..].to_vec();
                }
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return DownloadDrain::Idle,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                return DownloadDrain::Disconnected;
            }
        }
    }
}

// ── Packet helpers ──────────────────────────────────────────────────

fn try_handle_dns(packet: &[u8], fake_dns: &FakeDns) -> Option<Vec<u8>> {
    let (src_ip, dst_ip, protocol, transport_off) = parse_ip_header(packet)?;
    if protocol != 17 { return None; }
    let udp = &packet[transport_off..];
    let (src_port, dst_port, data_offset) = parse_udp_header(udp)?;
    if dst_port != 53 { return None; }
    let (dns_response, _) = fake_dns.handle_query(&udp[data_offset..])?;
    Some(build_udp_response_ip(
        dst_ip,
        src_ip,
        dst_port,
        src_port,
        &dns_response,
    ))
}

/// UDP любого семейства. DNS отсекается раньше, его берёт FakeDns; такой
/// пакет уходит в UDP-реле.
fn is_ip_udp(packet: &[u8]) -> bool {
    let Some((_, _, protocol, _)) = parse_ip_header(packet) else {
        return false;
    };
    protocol == 17
}

/// UDP-пакет с портом назначения `port`. Семейство любое, расширения IPv6
/// разбирает `parse_ip_header`.
fn is_udp_dst_port(packet: &[u8], port: u16) -> bool {
    let Some((_, _, protocol, transport_off)) = parse_ip_header(packet) else {
        return false;
    };
    if protocol != 17 {
        return false;
    }
    let Some((_, dst_port, _)) = parse_udp_header(&packet[transport_off..]) else {
        return false;
    };
    dst_port == port
}

/// Ответ DNS или UDP-реле обратно в TUN: сборка семейства по адресам.
pub(crate) fn build_udp_response_ip(
    src_ip: IpAddr,
    dst_ip: IpAddr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    match (src_ip, dst_ip) {
        (IpAddr::V4(s), IpAddr::V4(d)) => build_udp_response(s, d, src_port, dst_port, payload),
        (IpAddr::V6(s), IpAddr::V6(d)) => build_udp_response_v6(s, d, src_port, dst_port, payload),
        _ => Vec::new(),
    }
}

pub fn parse_ipv4_header(p: &[u8]) -> Option<(Ipv4Addr, Ipv4Addr, u8, usize)> {
    if p.len() < 20 || p[0] >> 4 != 4 { return None; }
    let ihl = (p[0] & 0x0F) as usize * 4;
    if ihl < 20 || p.len() < ihl { return None; }
    Some((Ipv4Addr::new(p[12],p[13],p[14],p[15]), Ipv4Addr::new(p[16],p[17],p[18],p[19]), p[9], ihl))
}

/// Разбор IP-пакета любого семейства: адреса, протокол транспорта и смещение
/// его заголовка. У IPv6 смещение уходит за цепочку расширений.
pub fn parse_ip_header(p: &[u8]) -> Option<(IpAddr, IpAddr, u8, usize)> {
    if p.is_empty() {
        return None;
    }
    match p[0] >> 4 {
        4 => {
            let (src, dst, proto, ihl) = parse_ipv4_header(p)?;
            Some((IpAddr::V4(src), IpAddr::V4(dst), proto, ihl))
        }
        6 => parse_ipv6_header(p),
        _ => None,
    }
}

/// Разбор IPv6: адреса, next header транспорта, смещение за расширениями.
///
/// Цепочка расширений обходится до транспортного заголовка: Hop-by-Hop,
/// Routing, Dest Options, Fragment и AH. Фрагменты после первого не содержат
/// транспорта, такой пакет уходит в стек как есть.
fn parse_ipv6_header(p: &[u8]) -> Option<(IpAddr, IpAddr, u8, usize)> {
    if p.len() < 40 || p[0] >> 4 != 6 {
        return None;
    }
    let mut next = p[6];
    let mut off = 40usize;
    loop {
        match next {
            // Hop-by-Hop, Routing, Dest Options.
            0 | 43 | 60 => {
                if off + 2 > p.len() {
                    return None;
                }
                let ext_len = (p[off + 1] as usize + 1) * 8;
                if off + ext_len > p.len() {
                    return None;
                }
                next = p[off];
                off += ext_len;
            }
            // Fragment: транспорт несёт только первый (offset == 0).
            // Не-первый фрагмент уходит в стек как есть, без сессии и NAT.
            44 => {
                if off + 8 > p.len() {
                    return None;
                }
                let frag_off = u16::from_be_bytes([p[off + 2], p[off + 3]]) >> 3;
                if frag_off != 0 {
                    return None;
                }
                next = p[off];
                off += 8;
            }
            // Authentication Header.
            51 => {
                if off + 2 > p.len() {
                    return None;
                }
                let ext_len = (p[off + 1] as usize + 2) * 4;
                if off + ext_len > p.len() {
                    return None;
                }
                next = p[off];
                off += ext_len;
            }
            _ => break,
        }
    }
    let src = Ipv6Addr::from(<[u8; 16]>::try_from(&p[8..24]).ok()?);
    let dst = Ipv6Addr::from(<[u8; 16]>::try_from(&p[24..40]).ok()?);
    Some((IpAddr::V6(src), IpAddr::V6(dst), next, off))
}

/// IPv6 не несёт контрольной суммы заголовка: после подмены адреса смысла
/// пересчитывать нечего, правится только TCP.
fn tcp_checksum_update_ip(pkt: &mut [u8], transport_off: usize, src: &IpAddr, dst: &IpAddr) {
    match (src, dst) {
        (IpAddr::V4(s), IpAddr::V4(d)) => tcp_checksum_update(pkt, transport_off, s, d),
        (IpAddr::V6(s), IpAddr::V6(d)) => {
            let tcp_len = pkt.len() - transport_off;
            pkt[transport_off + 16] = 0;
            pkt[transport_off + 17] = 0;

            let mut sum = 0u32;
            for part in [s.octets(), d.octets()] {
                for pair in part.chunks(2) {
                    sum += u16::from_be_bytes([pair[0], pair[1]]) as u32;
                }
            }
            sum += (tcp_len >> 16) as u32;
            sum += (tcp_len & 0xFFFF) as u32;
            sum += 6u32; // next header TCP

            let tcp = &pkt[transport_off..];
            for i in (0..tcp.len()).step_by(2) {
                let word = if i + 1 < tcp.len() {
                    u16::from_be_bytes([tcp[i], tcp[i + 1]])
                } else {
                    u16::from_be_bytes([tcp[i], 0])
                };
                sum += word as u32;
            }

            while sum >> 16 != 0 {
                sum = (sum & 0xFFFF) + (sum >> 16);
            }
            let cksum = !(sum as u16);
            pkt[transport_off + 16] = (cksum >> 8) as u8;
            pkt[transport_off + 17] = cksum as u8;
        }
        _ => {}
    }
}

/// Возврат исходящего TCP smoltcp к источнику из TUN: src:port становится
/// парой оригинального назначения, checksum пересчитывается под новые адреса.
/// Такой пакет уходит наверх, в TUN.
fn restore_outbound_src(pkt: &mut [u8], port_to_key: &HashMap<u16, TcpSessionKey>) {
    if pkt.len() < 40 {
        return;
    }
    let Some((_, dst_ip, protocol, transport_off)) = parse_ip_header(pkt) else {
        return;
    };
    if protocol != 6 || pkt.len() < transport_off + 4 {
        return;
    }
    let src_port = u16::from_be_bytes([pkt[transport_off], pkt[transport_off + 1]]);
    let Some(orig_key) = port_to_key.get(&src_port) else {
        return;
    };
    let orig_ip = orig_key.dst_addr.ip();
    let orig_port = orig_key.dst_addr.port();
    if pkt[0] >> 4 == 4 {
        if let IpAddr::V4(v4) = orig_ip {
            pkt[12..16].copy_from_slice(&v4.octets());
            pkt[10] = 0;
            pkt[11] = 0;
            let ip_cksum = ipv4_checksum(&pkt[..transport_off]);
            pkt[10..12].copy_from_slice(&ip_cksum.to_be_bytes());
        }
    } else if let IpAddr::V6(v6) = orig_ip {
        pkt[8..24].copy_from_slice(&v6.octets());
    }
    pkt[transport_off] = (orig_port >> 8) as u8;
    pkt[transport_off + 1] = orig_port as u8;
    tcp_checksum_update_ip(pkt, transport_off, &orig_ip, &dst_ip);
}

/// Подмена адреса назначения на адрес стека и порта на ephemeral (входящий SYN
/// и продолжение сессии). IPv4 пересчитывает и заголовок, IPv6 его не имеет.
fn rewrite_dst_to_smol(pkt: &mut [u8], transport_off: usize, eph_port: u16) {
    pkt[transport_off + 2] = (eph_port >> 8) as u8;
    pkt[transport_off + 3] = eph_port as u8;
    let Some((src, _, _, _)) = parse_ip_header(pkt) else {
        return;
    };
    if pkt[0] >> 4 == 4 {
        let smol_ip = crate::ip_stack::SMOL_IP;
        pkt[16] = smol_ip.octets()[0];
        pkt[17] = smol_ip.octets()[1];
        pkt[18] = smol_ip.octets()[2];
        pkt[19] = smol_ip.octets()[3];
        pkt[10] = 0;
        pkt[11] = 0;
        let ip_cksum = ipv4_checksum(&pkt[..transport_off]);
        pkt[10..12].copy_from_slice(&ip_cksum.to_be_bytes());
    } else {
        let smol_ip = crate::ip_stack::SMOL_IPV6;
        pkt[24..40].copy_from_slice(&smol_ip.octets());
    }
    let smol_addr = if pkt[0] >> 4 == 4 {
        IpAddr::V4(Ipv4Addr::new(
            crate::ip_stack::SMOL_IP.octets()[0],
            crate::ip_stack::SMOL_IP.octets()[1],
            crate::ip_stack::SMOL_IP.octets()[2],
            crate::ip_stack::SMOL_IP.octets()[3],
        ))
    } else {
        IpAddr::V6(Ipv6Addr::from(crate::ip_stack::SMOL_IPV6.octets()))
    };
    tcp_checksum_update_ip(pkt, transport_off, &src, &smol_addr);
}

pub fn parse_udp_header(p: &[u8]) -> Option<(u16, u16, usize)> {
    if p.len() < 8 { return None; }
    Some((u16::from_be_bytes([p[0],p[1]]), u16::from_be_bytes([p[2],p[3]]), 8))
}

pub fn parse_tcp_ports(p: &[u8]) -> Option<(u16, u16)> {
    if p.len() < 4 { return None; }
    Some((u16::from_be_bytes([p[0],p[1]]), u16::from_be_bytes([p[2],p[3]])))
}

pub fn build_udp_response(src_ip: Ipv4Addr, dst_ip: Ipv4Addr, src_port: u16, dst_port: u16, payload: &[u8]) -> Vec<u8> {
    let total = 20 + 8 + payload.len();
    let mut p = vec![0u8; total];
    p[0] = 0x45; p[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    p[8] = 64; p[9] = 17;
    p[12..16].copy_from_slice(&src_ip.octets()); p[16..20].copy_from_slice(&dst_ip.octets());
    let ck = ipv4_checksum(&p[..20]); p[10..12].copy_from_slice(&ck.to_be_bytes());
    p[20..22].copy_from_slice(&src_port.to_be_bytes()); p[22..24].copy_from_slice(&dst_port.to_be_bytes());
    p[24..26].copy_from_slice(&((8+payload.len()) as u16).to_be_bytes());
    p[28..].copy_from_slice(payload); p
}

/// UDP-ответ в TUN поверх IPv6. Заголовок 40 байт без своей контрольной суммы,
/// у UDP своя считается по псевдозаголовку IPv6.
pub fn build_udp_response_v6(
    src_ip: Ipv6Addr,
    dst_ip: Ipv6Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let udp_len = 8 + payload.len();
    let mut p = vec![0u8; 40 + udp_len];
    p[0] = 0x60;
    p[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    p[6] = 17; // next header UDP
    p[7] = 64; // hop limit
    p[8..24].copy_from_slice(&src_ip.octets());
    p[24..40].copy_from_slice(&dst_ip.octets());
    p[40..42].copy_from_slice(&src_port.to_be_bytes());
    p[42..44].copy_from_slice(&dst_port.to_be_bytes());
    p[44..46].copy_from_slice(&(udp_len as u16).to_be_bytes());
    p[48..].copy_from_slice(payload);

    // Контрольная сумма UDP по псевдозаголовку IPv6.
    let mut sum = 0u32;
    for part in [src_ip.octets(), dst_ip.octets()] {
        for pair in part.chunks(2) {
            sum += u16::from_be_bytes([pair[0], pair[1]]) as u32;
        }
    }
    sum += (udp_len >> 16) as u32;
    sum += (udp_len & 0xFFFF) as u32;
    sum += 17u32;
    for i in (40..p.len()).step_by(2) {
        let word = if i + 1 < p.len() {
            u16::from_be_bytes([p[i], p[i + 1]])
        } else {
            u16::from_be_bytes([p[i], 0])
        };
        sum += word as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    let cksum = !(sum as u16);
    // Нулевая сумма в IPv6 передаётся как 0xFFFF.
    let cksum = if cksum == 0 { 0xFFFF } else { cksum };
    p[46..48].copy_from_slice(&cksum.to_be_bytes());
    p
}

/// Recalculate TCP checksum after NAT rewrite.
fn tcp_checksum_update(pkt: &mut [u8], ihl: usize, src_ip: &Ipv4Addr, dst_ip: &Ipv4Addr) {
    let tcp_len = pkt.len() - ihl;
    // Clear existing checksum.
    pkt[ihl + 16] = 0;
    pkt[ihl + 17] = 0;

    let mut sum = 0u32;
    // Pseudo-header: src IP, dst IP, zero, protocol(6), TCP length.
    for pair in src_ip.octets().chunks(2) {
        sum += u16::from_be_bytes([pair[0], pair[1]]) as u32;
    }
    for pair in dst_ip.octets().chunks(2) {
        sum += u16::from_be_bytes([pair[0], pair[1]]) as u32;
    }
    sum += 6u32; // protocol TCP
    sum += tcp_len as u32;

    // TCP segment.
    let tcp = &pkt[ihl..];
    for i in (0..tcp.len()).step_by(2) {
        let word = if i + 1 < tcp.len() {
            u16::from_be_bytes([tcp[i], tcp[i + 1]])
        } else {
            u16::from_be_bytes([tcp[i], 0])
        };
        sum += word as u32;
    }

    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    let cksum = !(sum as u16);
    pkt[ihl + 16] = (cksum >> 8) as u8;
    pkt[ihl + 17] = cksum as u8;
}

fn ipv4_checksum(h: &[u8]) -> u16 {
    let mut sum = 0u32;
    for i in (0..h.len()).step_by(2) {
        if i == 10 { continue; }
        sum += if i+1 < h.len() { u16::from_be_bytes([h[i],h[i+1]]) } else { u16::from_be_bytes([h[i],0]) } as u32;
    }
    while sum >> 16 != 0 { sum = (sum & 0xFFFF) + (sum >> 16); }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_config::parse_config;

    // Правила пользователя на живом туннеле (XR-180).

    fn routing_with(rules: Vec<(&str, &str)>) -> RoutingConfig {
        RoutingConfig {
            default_action: "direct".into(),
            rules: rules
                .into_iter()
                .map(|(action, domain)| xr_proto::config::RoutingRule {
                    name: None,
                    action: action.into(),
                    domains: vec![domain.into()],
                    ip_ranges: vec![],
                    geoip: vec![],
                    outbound: None,
                })
                .collect(),
        }
    }

    fn test_config(routing: RoutingConfig) -> VpnConfig {
        VpnConfig {
            server_address: "127.0.0.1".into(),
            // Порт, на котором никто не слушает: прогрев пула упадёт в
            // отдельной задаче, роутер к этому моменту уже собран.
            server_port: 1,
            servers: vec![],
            obfuscation_key: base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                [7u8; 32],
            ),
            modifier: "positional_xor_rotate".into(),
            salt: 0xDEADBEEF,
            padding_min: 16,
            padding_max: 128,
            routing,
            geoip_path: None,
            on_server_down: "direct".into(),
            dns_resolvers: vec![],
            hub_url: None,
            hub_preset: None,
            hub_cache_dir: None,
            hub_refresh_interval_secs: None,
            hub_trusted_public_key: None,
            system_resolver: None,
            mux_pool_size: 1,
            udp_relay_port: crate::udp_relay::DEFAULT_RELAY_PORT,
            tuning: crate::tuning::TuningProfile::default(),
            block_quic: false,
        }
    }

    /// Движок поднимается ровно так же, как его поднимает JNI: собственный
    /// рантайм, старт под его `enter()`. Внутри `#[tokio::test]` старт с
    /// хабом падал бы на блокирующем фетче пресета.
    fn started_engine(config: VpnConfig) -> (tokio::runtime::Runtime, VpnEngine) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let mut engine = VpnEngine::new(config);
        {
            let _guard = rt.enter();
            engine
                .start(PacketQueue::new(), Arc::new(|_fd: i32| true))
                .expect("engine must start");
        }
        (rt, engine)
    }

    fn action_for(engine: &VpnEngine, sni: &str) -> Action {
        engine
            .active_router()
            .expect("running engine must have a router")
            .resolve(Some(sni), IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)))
    }

    /// XR-296: признак «аплинка нет» доезжает до пула, а его снятие рвёт
    /// живые сессии поколением сети и отпускает пул уже после пересборки mux.
    /// Возврат без пропажи ничего не трогает: окна не было, рвать нечего.
    #[test]
    fn uplink_flag_reaches_the_pool_and_lifts_after_rebuild() {
        let (rt, mut engine) = started_engine(test_config(routing_with(vec![])));
        let pool = engine.server_pool.clone().expect("running engine has a pool");
        let netgen = engine.netgen_tx.clone().expect("running engine has a netgen");
        assert!(!pool.is_uplink_down());

        {
            let _guard = rt.enter();
            engine.on_uplink_restored();
        }
        assert_eq!(*netgen.borrow(), 0, "restore without a loss must not drop sessions");

        engine.on_uplink_lost();
        assert!(pool.is_uplink_down(), "the flag must reach the pool");
        {
            let _guard = rt.enter();
            engine.on_uplink_restored();
        }
        assert_eq!(*netgen.borrow(), 1, "restore after a loss re-binds sessions");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pool.is_uplink_down() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!pool.is_uplink_down(), "the freeze must lift once the mux is rebuilt");
        engine.stop();
    }

    /// XR-296: до старта движка признак никуда не идёт и ничего не ломает.
    #[test]
    fn uplink_flag_before_start_is_a_no_op() {
        let engine = VpnEngine::new(test_config(routing_with(vec![])));
        engine.on_uplink_lost();
        engine.on_uplink_restored();
        assert!(engine.server_pool.is_none());
    }

    /// Добавленное правило действует на живом туннеле, без переподключения.
    #[test]
    fn user_rule_applies_to_the_live_tunnel() {
        let (_rt, mut engine) = started_engine(test_config(routing_with(vec![])));
        assert_eq!(action_for(&engine, "example.com"), Action::Direct);

        assert!(engine.apply_user_rules(routing_with(vec![("proxy", "example.com")])));
        assert_eq!(
            action_for(&engine, "example.com"),
            Action::Proxy,
            "правило должно действовать сразу, а не после переподключения"
        );

        // Снятое правило тем же путём возвращает домен на прямой маршрут.
        assert!(engine.apply_user_rules(routing_with(vec![])));
        assert_eq!(action_for(&engine, "example.com"), Action::Direct);
        engine.stop();
    }

    /// Правило пользователя главнее правила пресета и после применения на лету.
    #[test]
    fn live_user_rule_overrides_the_cached_preset() {
        let dir = tempfile::tempdir().unwrap();
        crate::presets::PresetCache::write_to_disk(
            dir.path(),
            &xr_proto::preset::Preset {
                name: "russia".into(),
                version: 1,
                updated_at: "2026-08-11T00:00:00Z".into(),
                description: String::new(),
                rules: routing_with(vec![("proxy", "example.com"), ("proxy", "youtube.com")]),
                signature: None,
            },
        )
        .unwrap();

        let mut config = test_config(routing_with(vec![]));
        config.hub_url = Some("http://127.0.0.1:1".into());
        config.hub_preset = Some("russia".into());
        config.hub_cache_dir = Some(dir.path().to_string_lossy().into_owned());
        let (_rt, mut engine) = started_engine(config);
        assert_eq!(action_for(&engine, "example.com"), Action::Proxy);

        assert!(engine.apply_user_rules(routing_with(vec![("direct", "example.com")])));
        assert_eq!(
            action_for(&engine, "example.com"),
            Action::Direct,
            "своё правило должно перебивать пресет"
        );
        assert_eq!(
            action_for(&engine, "youtube.com"),
            Action::Proxy,
            "остальные правила пресета обязаны остаться"
        );
        engine.stop();
    }

    /// Пресет, обновившийся после правки, не откатывает правила пользователя:
    /// обе пересборки берут локальную половину из общего владения.
    #[test]
    fn preset_update_keeps_the_live_user_rule() {
        let (_rt, mut engine) = started_engine(test_config(routing_with(vec![])));
        assert!(engine.apply_user_rules(routing_with(vec![("direct", "example.com")])));

        // То же, что делает фоновый watch при новой версии с хаба.
        let ctx = engine.ctx.clone().unwrap();
        let overrides = engine.live_overrides.clone().unwrap();
        let preset = routing_with(vec![("proxy", "example.com")]);
        assert!(swap_router(&ctx, &overrides, None, Some(&preset), None, "тест"));

        assert_eq!(
            action_for(&engine, "example.com"),
            Action::Direct,
            "публикация с хаба не должна откатывать правило пользователя"
        );
        engine.stop();
    }

    /// Пересборки не наступают друг другу на пятки: пока лок держит кто-то
    /// один, обе стоят и роутера не касаются, а после освобождения правило
    /// пользователя переживает публикацию пресета при любом их порядке.
    /// Порядок здесь задан локом, а не удачей планировщика: тест держит ту же
    /// точку синхронизации и отпускает её сам.
    #[test]
    fn concurrent_rebuilds_keep_the_user_rule() {
        let (_rt, mut engine) = started_engine(test_config(routing_with(vec![])));
        let ctx = engine.ctx.clone().unwrap();
        let overrides = engine.live_overrides.clone().unwrap();
        let held = overrides.lock().unwrap();

        let watch = {
            let ctx = ctx.clone();
            let overrides = overrides.clone();
            std::thread::spawn(move || {
                let preset = routing_with(vec![("proxy", "example.com")]);
                swap_router(&ctx, &overrides, None, Some(&preset), None, "пресет")
            })
        };
        let user = {
            let ctx = ctx.clone();
            let overrides = overrides.clone();
            std::thread::spawn(move || {
                swap_router(
                    &ctx,
                    &overrides,
                    Some(routing_with(vec![("direct", "example.com")])),
                    None,
                    None,
                    "правила пользователя",
                )
            })
        };

        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(
            action_for(&engine, "example.com"),
            Action::Direct,
            "пересборка не имеет права трогать роутер мимо общего лока"
        );

        drop(held);
        assert!(watch.join().unwrap());
        assert!(user.join().unwrap());
        assert_eq!(
            action_for(&engine, "example.com"),
            Action::Direct,
            "правило пользователя обязано пережить любой порядок пересборок"
        );
        engine.stop();
    }

    /// Пересборка держит лок на весь цикл, а не отпускает его после снимка.
    /// Порядок задан размером пресета: пока watch компилирует свои десятки
    /// тысяч правил, правка пользователя обязана стоять в очереди, поэтому
    /// последним в роутер попадает её результат. Отпусти пересборка лок
    /// раньше сборки, правка успела бы целиком, а watch перекрыл бы её своим
    /// снимком, снятым до правки.
    #[test]
    fn rebuild_holds_the_lock_for_the_whole_cycle() {
        let (_rt, mut engine) = started_engine(test_config(routing_with(vec![])));
        let ctx = engine.ctx.clone().unwrap();
        let overrides = engine.live_overrides.clone().unwrap();

        // Пресет заведомо долгой сборки: на его фоне окно гонки видно.
        let mut heavy: Vec<(String, String)> = (0..150_000)
            .map(|i| ("proxy".to_string(), format!("host{}.example.org", i)))
            .collect();
        heavy.push(("proxy".to_string(), "example.com".to_string()));
        let heavy = RoutingConfig {
            default_action: "direct".into(),
            rules: heavy
                .into_iter()
                .map(|(action, domain)| xr_proto::config::RoutingRule {
                    name: None,
                    action,
                    domains: vec![domain],
                    ip_ranges: vec![],
                    geoip: vec![],
                    outbound: None,
                })
                .collect(),
        };

        let watch = std::thread::spawn(move || {
            swap_router(&ctx, &overrides, None, Some(&heavy), None, "пресет")
        });
        // Дать watch войти в критическую секцию: дальше правка пользователя
        // либо ждёт его целиком, либо (без общего лока) обгоняет и теряется.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(engine.apply_user_rules(routing_with(vec![("direct", "example.com")])));
        assert!(watch.join().unwrap());

        assert_eq!(
            action_for(&engine, "example.com"),
            Action::Direct,
            "правку пользователя перекрыла пересборка со старым снимком правил"
        );
        engine.stop();
    }

    /// Пул из нескольких серверов уже сам говорит, что легли все (XR-131);
    /// экран не должен сводить это к одному серверу.
    #[test]
    fn warmup_error_text_keeps_the_whole_pool() {
        let pool_err = io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "все 2 сервера недоступны: msk (refused), aeza (refused)",
        );
        let msg = warmup_error_text(2, &pool_err);
        assert!(msg.contains("все 2 сервера недоступны"), "{}", msg);
        assert!(!msg.starts_with("Сервер недоступен"), "{}", msg);

        // Один сервер в пуле, текст прежний.
        let one = io::Error::new(io::ErrorKind::ConnectionRefused, "refused");
        assert_eq!(warmup_error_text(1, &one), "Сервер недоступен: refused");
    }

    #[test]
    fn test_parse_ipv4_header() {
        let mut pkt = vec![0u8; 40];
        pkt[0] = 0x45; pkt[9] = 6;
        pkt[12..16].copy_from_slice(&[10, 0, 0, 2]);
        pkt[16..20].copy_from_slice(&[93, 184, 216, 34]);
        let (src, dst, proto, ihl) = parse_ipv4_header(&pkt).unwrap();
        assert_eq!(src, Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(dst, Ipv4Addr::new(93, 184, 216, 34));
        assert_eq!(proto, 6); assert_eq!(ihl, 20);
    }

    // ── drain_download_to_tx ─────────────────────────────────────────

    /// A TX sink with a bounded total capacity and a per-call accept cap, so we
    /// can deterministically force partial `send_slice` accepts and full-buffer
    /// stalls — the conditions that used to drop the tail of Direct downloads.
    struct MockSink {
        buf: Vec<u8>,
        capacity: usize,
        max_accept: usize,
        closed: bool,
    }

    impl MockSink {
        fn new(capacity: usize, max_accept: usize) -> Self {
            Self { buf: Vec::new(), capacity, max_accept, closed: false }
        }
    }

    impl TxSink for MockSink {
        fn can_send(&self) -> bool {
            self.buf.len() < self.capacity
        }
        fn enqueue(&mut self, data: &[u8]) -> usize {
            let room = self.capacity - self.buf.len();
            let n = room.min(self.max_accept).min(data.len());
            self.buf.extend_from_slice(&data[..n]);
            n
        }
        fn close(&mut self) {
            self.closed = true;
        }
    }

    #[tokio::test]
    async fn drain_download_never_drops_bytes_on_partial_send() {
        // Reproduces the C2 bug: a fast download into a TX buffer that the app
        // drains slowly. Every byte the relay produced must reach the app,
        // even though the sink accepts only small partial chunks at a time.
        let stats = Stats::new();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);

        // ~300 KB total, sent as 1000-byte chunks (like an 8 KB Direct read
        // sliced by the channel). Sink capacity and accept cap are far smaller,
        // forcing both partial accepts and buffer-full stalls.
        let total = 300_000usize;
        let chunk = 1000usize;
        let mut expected = Vec::with_capacity(total);
        for i in 0..total {
            expected.push((i % 251) as u8);
        }
        let producer = expected.clone();
        tokio::spawn(async move {
            let mut off = 0;
            while off < producer.len() {
                let end = (off + chunk).min(producer.len());
                tx.send(producer[off..end].to_vec()).await.unwrap();
                off = end;
            }
            // tx dropped here → eventual Disconnected.
        });

        let mut sink = MockSink::new(2048, 700);
        let mut pending = Vec::new();
        let mut received = Vec::with_capacity(total);

        // Bound iterations generously to catch a stuck loop instead of hanging.
        for _ in 0..1_000_000 {
            let outcome = drain_download_to_tx(&mut sink, &mut rx, &mut pending, &stats);
            // Simulate the app reading everything currently in the TX buffer.
            received.extend(sink.buf.drain(..));
            match outcome {
                DownloadDrain::Disconnected if pending.is_empty() => break,
                _ => {
                    // Yield so the producer task can enqueue more.
                    tokio::task::yield_now().await;
                }
            }
        }

        assert_eq!(received.len(), expected.len(), "byte count mismatch — data was dropped");
        assert_eq!(received, expected, "byte stream corrupted/reordered");
    }

    #[tokio::test]
    async fn drain_download_reports_disconnected_when_sender_gone() {
        let stats = Stats::new();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(4);
        drop(tx);
        let mut sink = MockSink::new(4096, 4096);
        let mut pending = Vec::new();
        assert!(matches!(
            drain_download_to_tx(&mut sink, &mut rx, &mut pending, &stats),
            DownloadDrain::Disconnected
        ));
    }

    #[tokio::test]
    async fn drain_download_stops_idle_when_buffer_full() {
        // When the TX buffer is full, the drain must stop and preserve the
        // unsent remainder in `pending` rather than discarding it.
        let stats = Stats::new();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(4);
        tx.send(vec![7u8; 1000]).await.unwrap();
        let mut sink = MockSink::new(400, 400);
        let mut pending = Vec::new();

        assert!(matches!(
            drain_download_to_tx(&mut sink, &mut rx, &mut pending, &stats),
            DownloadDrain::Idle
        ));
        assert_eq!(sink.buf.len(), 400, "should fill exactly the available room");
        assert_eq!(pending.len(), 600, "remainder must be preserved, not dropped");

        // Drain the sink (app reads) and run again, the rest must flush.
        sink.buf.clear();
        let _ = drain_download_to_tx(&mut sink, &mut rx, &mut pending, &stats);
        assert_eq!(sink.buf.len(), 400);
        assert_eq!(pending.len(), 200);
    }

    /// Баг обрыва хвоста: сервер отдаёт ответ и закрывает соединение. Все байты
    /// обязаны доехать до приложения, а сокет обязан закрыться штатно (FIN), а
    /// не оборваться. Раньше на завершении релея сокет обрывался `abort()`-ом
    /// (RST) сразу, и хвост, ещё лежавший в буфере smoltcp, пропадал: картинки
    /// не догружались. `finish_or_pump_download` доливает буфер и зовёт
    /// `close()`.
    #[tokio::test]
    async fn download_tail_is_delivered_and_socket_closes_gracefully() {
        let stats = Stats::new();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);

        let total = 120_000usize;
        let chunk = 1000usize;
        let mut expected = Vec::with_capacity(total);
        for i in 0..total {
            expected.push((i % 251) as u8);
        }
        let producer = expected.clone();
        tokio::spawn(async move {
            let mut off = 0;
            while off < producer.len() {
                let end = (off + chunk).min(producer.len());
                tx.send(producer[off..end].to_vec()).await.unwrap();
                off = end;
            }
            // tx dropped: сервер закрыл соединение сразу за последним байтом.
        });

        let mut sink = MockSink::new(2048, 700);
        let mut pending = Vec::new();
        let mut received = Vec::with_capacity(total);
        let mut finished = false;

        for _ in 0..1_000_000 {
            let done = finish_or_pump_download(&mut sink, &mut rx, &mut pending, &stats);
            received.extend(sink.buf.drain(..));
            if done {
                finished = true;
                break;
            }
            tokio::task::yield_now().await;
        }

        assert!(finished, "сессия так и не закрылась");
        assert!(sink.closed, "сокет закрыт через FIN, а не оборван");
        assert_eq!(received.len(), expected.len(), "хвост ответа потерян");
        assert_eq!(received, expected, "поток побит или переставлен");
    }

    /// UDP уезжает на хост активного сервера пула. Слот без разобранного
    /// адреса не даёт endpoint, соседний продолжает работать.
    #[test]
    fn pick_relay_endpoint_follows_active_slot() {
        let primary = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 9999);
        let backup = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2)), 9999);
        let endpoints = vec![Some(primary), Some(backup), None];

        assert_eq!(pick_relay_endpoint(&endpoints, 0), Some(primary));
        assert_eq!(pick_relay_endpoint(&endpoints, 1), Some(backup));
        assert_eq!(pick_relay_endpoint(&endpoints, 2), None);
        assert_eq!(pick_relay_endpoint(&endpoints, 3), None);
    }

    // -- IPv6 поверх той же обвязки, что и IPv4 (XR-302) --

    const TCP_SYN: u8 = 0x02;
    const TCP_PSH: u8 = 0x08;
    const TCP_ACK: u8 = 0x10;

    fn v6_client() -> Ipv6Addr {
        Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 2)
    }

    fn v6_target() -> Ipv6Addr {
        Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1)
    }

    /// TCP-сегмент поверх IPv6. Контрольные суммы не проставляются: стек
    /// на приёме их игнорирует, как и в боевом пути NAT.
    fn tcp_v6(
        src: Ipv6Addr,
        dst: Ipv6Addr,
        sport: u16,
        dport: u16,
        seq: u32,
        ack: u32,
        flags: u8,
        payload: &[u8],
    ) -> Vec<u8> {
        let tcp_len = 20 + payload.len();
        let mut p = vec![0u8; 40 + tcp_len];
        p[0] = 0x60;
        p[4..6].copy_from_slice(&(tcp_len as u16).to_be_bytes());
        p[6] = 6;
        p[7] = 64;
        p[8..24].copy_from_slice(&src.octets());
        p[24..40].copy_from_slice(&dst.octets());
        p[40..42].copy_from_slice(&sport.to_be_bytes());
        p[42..44].copy_from_slice(&dport.to_be_bytes());
        p[44..48].copy_from_slice(&seq.to_be_bytes());
        p[48..52].copy_from_slice(&ack.to_be_bytes());
        p[52] = 5 << 4;
        p[53] = flags;
        p[54..56].copy_from_slice(&8192u16.to_be_bytes());
        p[60..].copy_from_slice(payload);
        p
    }

    fn tcp_v4(
        src: Ipv4Addr,
        dst: Ipv4Addr,
        sport: u16,
        dport: u16,
        seq: u32,
        ack: u32,
        flags: u8,
        payload: &[u8],
    ) -> Vec<u8> {
        let tcp_len = 20 + payload.len();
        let mut p = vec![0u8; 20 + tcp_len];
        p[0] = 0x45;
        p[2..4].copy_from_slice(&((20 + tcp_len) as u16).to_be_bytes());
        p[8] = 64;
        p[9] = 6;
        p[12..16].copy_from_slice(&src.octets());
        p[16..20].copy_from_slice(&dst.octets());
        p[20..22].copy_from_slice(&sport.to_be_bytes());
        p[22..24].copy_from_slice(&dport.to_be_bytes());
        p[24..28].copy_from_slice(&seq.to_be_bytes());
        p[28..32].copy_from_slice(&ack.to_be_bytes());
        p[32] = 5 << 4;
        p[33] = flags;
        p[34..36].copy_from_slice(&8192u16.to_be_bytes());
        p[40..].copy_from_slice(payload);
        p
    }

    fn udp_v6(src: Ipv6Addr, dst: Ipv6Addr, sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
        build_udp_response_v6(src, dst, sport, dport, payload)
    }

    /// AAAA-запрос в том виде, в каком его собирает клиент.
    fn aaaa_query(domain: &str) -> Vec<u8> {
        let mut buf = vec![0x12, 0x34, 0x01, 0x00];
        buf.extend_from_slice(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        for label in domain.split('.') {
            buf.push(label.len() as u8);
            buf.extend_from_slice(label.as_bytes());
        }
        buf.push(0);
        buf.extend_from_slice(&[0x00, 28, 0x00, 0x01]);
        buf
    }

    /// SYN из TUN на глобальный IPv6 попадает в сессию с адресом стека.
    #[test]
    fn ipv6_syn_from_tun_rewrites_into_v6_session() {
        let mut pkt = tcp_v6(v6_client(), v6_target(), 40000, 443, 1000, 0, TCP_SYN, &[]);
        let (src, dst, proto, off) = parse_ip_header(&pkt).expect("IPv6 SYN обязан разбираться");
        assert_eq!(src, IpAddr::V6(v6_client()));
        assert_eq!(dst, IpAddr::V6(v6_target()));
        assert_eq!(proto, 6);
        assert_eq!(off, 40);

        let eph = 55555u16;
        rewrite_dst_to_smol(&mut pkt, off, eph);
        let (_, dst2, _, off2) = parse_ip_header(&pkt).unwrap();
        assert_eq!(dst2, IpAddr::V6(Ipv6Addr::from(crate::ip_stack::SMOL_IPV6.octets())));
        assert_eq!(
            u16::from_be_bytes([pkt[off2], pkt[off2 + 1]]),
            40000,
            "исходный порт клиента не трогается"
        );
        assert_eq!(
            u16::from_be_bytes([pkt[off2 + 2], pkt[off2 + 3]]),
            eph,
            "порт назначения уходит в ephemeral сессии"
        );

        // Ключ сессии того же семейства, что и адреса из TUN.
        let key = TcpSessionKey {
            src_addr: SocketAddr::new(src, 40000),
            dst_addr: SocketAddr::new(dst, 443),
        };
        assert!(key.src_addr.is_ipv6() && key.dst_addr.is_ipv6());
    }

    /// Разбор IPv6 с цепочкой расширений доходит до TCP.
    #[test]
    fn parse_ipv6_header_walks_extension_chain() {
        // Hop-by-Hop (8 байт) перед TCP.
        let pkt = tcp_v6(v6_client(), v6_target(), 1, 2, 0, 0, TCP_SYN, &[]);
        let mut with_ext = vec![0u8; 48 + 20];
        with_ext[..40].copy_from_slice(&pkt[..40]);
        with_ext[6] = 0; // next = Hop-by-Hop
        with_ext[40] = 6; // ext next = TCP
        with_ext[41] = 0; // ext len (8 bytes)
        with_ext[48..].copy_from_slice(&pkt[40..]);
        let (src, dst, proto, off) = parse_ip_header(&with_ext).unwrap();
        assert_eq!(src, IpAddr::V6(v6_client()));
        assert_eq!(dst, IpAddr::V6(v6_target()));
        assert_eq!(proto, 6);
        assert_eq!(off, 48);
    }

    /// Пакет с заголовком фрагмента IPv6: next, reserved, offset|res|M, id.
    fn ipv6_with_fragment(src: Ipv6Addr, dst: Ipv6Addr, frag_off: u16, more: bool, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; 40 + 8 + payload.len()];
        p[0] = 0x60;
        p[4..6].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        p[6] = 44; // next = Fragment
        p[7] = 64;
        p[8..24].copy_from_slice(&src.octets());
        p[24..40].copy_from_slice(&dst.octets());
        p[40] = 6; // fragment next = TCP (или мусор)
        p[41] = 0;
        let off_flags = (frag_off << 3) | if more { 1 } else { 0 };
        p[42..44].copy_from_slice(&off_flags.to_be_bytes());
        p[44..48].copy_from_slice(&0x11223344u32.to_be_bytes());
        p[48..].copy_from_slice(payload);
        p
    }

    /// Не-первый фрагмент не разбирается в транспорт: NAT и сессии его не трогают.
    #[test]
    fn parse_ipv6_header_rejects_non_first_fragment() {
        let junk = [0u8; 20];
        let pkt = ipv6_with_fragment(v6_client(), v6_target(), 136, true, &junk);
        assert!(
            parse_ip_header(&pkt).is_none(),
            "фрагмент с offset=136 обязан уйти в стек как есть, без транспорта"
        );
    }

    /// Первый фрагмент несёт транспорт и разбирается дальше.
    #[test]
    fn parse_ipv6_header_accepts_first_fragment() {
        let tcp = tcp_v6(v6_client(), v6_target(), 1, 2, 0, 0, TCP_SYN, &[]);
        let pkt = ipv6_with_fragment(v6_client(), v6_target(), 0, true, &tcp[40..]);
        let (_, _, proto, off) = parse_ip_header(&pkt).expect("первый фрагмент обязан разбираться");
        assert_eq!(proto, 6);
        assert_eq!(off, 48);
    }

    /// Контрольная сумма транспорта поверх псевдозаголовка IPv6 (RFC 8200).
    fn ipv6_pseudo_checksum(src: &Ipv6Addr, dst: &Ipv6Addr, next: u8, segment: &[u8]) -> u16 {
        let mut sum = 0u32;
        for part in [src.octets(), dst.octets()] {
            for pair in part.chunks(2) {
                sum += u16::from_be_bytes([pair[0], pair[1]]) as u32;
            }
        }
        let len = segment.len() as u32;
        sum += (len >> 16) as u32;
        sum += (len & 0xFFFF) as u32;
        sum += next as u32;
        for i in (0..segment.len()).step_by(2) {
            let word = if i + 1 < segment.len() {
                u16::from_be_bytes([segment[i], segment[i + 1]])
            } else {
                u16::from_be_bytes([segment[i], 0])
            };
            sum += word as u32;
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        !(sum as u16)
    }

    /// Возврат src:port из smoltcp к назначению TUN и валидная TCP-сумма IPv6.
    #[test]
    fn restore_outbound_src_v6_keeps_tcp_checksum_valid() {
        let client = v6_client();
        let target = v6_target();
        let smol = Ipv6Addr::from(crate::ip_stack::SMOL_IPV6.octets());
        let eph = 55555u16;

        // Пакет, каким его отдаёт smoltcp: src=стек:eph, dst=клиент.
        let mut pkt = tcp_v6(smol, client, eph, 40000, 2000, 1001, TCP_ACK, b"pong");
        let mut map = HashMap::new();
        map.insert(
            eph,
            TcpSessionKey {
                src_addr: SocketAddr::new(IpAddr::V6(client), 40000),
                dst_addr: SocketAddr::new(IpAddr::V6(target), 443),
            },
        );
        restore_outbound_src(&mut pkt, &map);

        let (src, dst, proto, off) = parse_ip_header(&pkt).unwrap();
        assert_eq!(src, IpAddr::V6(target), "источником ответа обязано стать назначение TUN");
        assert_eq!(dst, IpAddr::V6(client));
        assert_eq!(proto, 6);
        assert_eq!(u16::from_be_bytes([pkt[off], pkt[off + 1]]), 443);
        assert_eq!(u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]), 40000);

        let cksum = u16::from_be_bytes([pkt[off + 16], pkt[off + 17]]);
        let mut seg = pkt[off..].to_vec();
        seg[16] = 0;
        seg[17] = 0;
        let expect = ipv6_pseudo_checksum(&target, &client, 6, &seg);
        assert_eq!(cksum, expect, "TCP-сумма после NAT обязана сходиться с RFC 8200");
    }

    /// Контрольная сумма UDP из build_udp_response_v6 сходится с эталоном.
    #[test]
    fn build_udp_response_v6_checksum_matches_rfc() {
        let src: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        let dst: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2);
        let pkt = build_udp_response_v6(src, dst, 53, 53000, b"ping");
        let (s, d, proto, off) = parse_ip_header(&pkt).unwrap();
        assert_eq!(s, IpAddr::V6(src));
        assert_eq!(d, IpAddr::V6(dst));
        assert_eq!(proto, 17);

        let cksum = u16::from_be_bytes([pkt[off + 6], pkt[off + 7]]);
        let mut seg = pkt[off..].to_vec();
        seg[6] = 0;
        seg[7] = 0;
        let expect = ipv6_pseudo_checksum(&src, &dst, 17, &seg);
        let expect = if expect == 0 { 0xFFFF } else { expect };
        assert_eq!(cksum, expect, "UDP-сумма IPv6 обязана сходиться с RFC 8200");
    }

    /// AAAA через FakeDns отвечает в туннель пакетом того же семейства.
    #[test]
    fn aaaa_dns_answer_returns_ipv6_packet_to_tun() {
        let dns = FakeDns::new();
        let query = aaaa_query("example.com");
        let pkt = udp_v6(v6_client(), Ipv6Addr::LOCALHOST, 53000, 53, &query);

        let Some(resp) = try_handle_dns(&pkt, &dns) else {
            panic!("AAAA-запрос обязан получить ответ FakeDns");
        };
        let (src, dst, proto, off) = parse_ip_header(&resp).unwrap();
        assert_eq!(src, IpAddr::V6(Ipv6Addr::LOCALHOST), "ответ идёт с адреса резолвера");
        assert_eq!(dst, IpAddr::V6(v6_client()), "ответ уходит в TUN клиенту");
        assert_eq!(proto, 17);
        assert_eq!(
            u16::from_be_bytes([resp[off], resp[off + 1]]),
            53,
            "источник UDP это порт DNS"
        );
        assert_eq!(
            u16::from_be_bytes([resp[off + 2], resp[off + 3]]),
            53000
        );
        let payload = &resp[off + 8..];
        assert!(payload.len() > 12);
        assert_eq!(payload[2] & 0x80, 0x80, "QR=1");
        assert_eq!(u16::from_be_bytes([payload[6], payload[7]]), 1, "ANCOUNT=1");
    }

    /// A и AAAA живут на одном интерфейсе и не мешают друг другу.
    #[test]
    fn ipv4_and_ipv6_dns_do_not_interfere() {
        let dns = FakeDns::new();
        let q6 = aaaa_query("example.com");
        let pkt6 = udp_v6(v6_client(), Ipv6Addr::LOCALHOST, 53000, 53, &q6);
        let resp6 = try_handle_dns(&pkt6, &dns).unwrap();

        // A-запрос поверх IPv4 тем же доменом получает свой ответ.
        let mut q4 = vec![0xAB, 0xCD, 0x01, 0x00];
        q4.extend_from_slice(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        for label in "example.com".split('.') {
            q4.push(label.len() as u8);
            q4.extend_from_slice(label.as_bytes());
        }
        q4.push(0);
        q4.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]);
        let pkt4 = build_udp_response(
            Ipv4Addr::new(192, 0, 2, 10),
            Ipv4Addr::new(192, 0, 2, 53),
            53000,
            53,
            &q4,
        );
        let resp4 = try_handle_dns(&pkt4, &dns).unwrap();

        let (_, _, _, off6) = parse_ip_header(&resp6).unwrap();
        let (_, _, _, off4) = parse_ip_header(&resp4).unwrap();
        assert!(resp6[0] >> 4 == 6 && resp4[0] >> 4 == 4);
        assert_eq!(u16::from_be_bytes([resp6[off6 + 2], resp6[off6 + 3]]), 53000);
        assert_eq!(u16::from_be_bytes([resp4[off4 + 2], resp4[off4 + 3]]), 53000);

        // Оба ответа несут адрес своего семейства из одного пула домена.
        let (_, ip6) = dns.handle_query(&q6).unwrap();
        let (_, ip4) = dns.handle_query(&q4).unwrap();
        let IpAddr::V6(v6) = ip6 else { panic!("AAAA обязан отдать V6") };
        let IpAddr::V4(v4) = ip4 else { panic!("A обязан отдать V4") };
        assert_eq!(FakeDns::to_fake_v6(v4), v6);
    }

    /// TCP до IPv6-цели проходит PacketQueue туда и обратно.
    #[test]
    fn tcp_v6_round_trip_via_packet_queue() {
        use smoltcp::socket::tcp::State as St;

        let queue = PacketQueue::new();
        let mut stack = IpStack::new(queue.clone());
        let handle = stack.add_tcp_socket(2048, 2048);
        stack.tcp_socket_mut(handle).listen(443).unwrap();

        let eph = 443u16;
        let mut syn = tcp_v6(v6_client(), v6_target(), 40000, 443, 1000, 0, TCP_SYN, &[]);
        let (_, _, _, off) = parse_ip_header(&syn).unwrap();
        rewrite_dst_to_smol(&mut syn, off, eph);
        queue.push_inbound(syn);
        assert!(stack.poll());

        let syn_ack = queue.pop_smol_outbound().expect("smoltcp обязан ответить SYN-ACK");
        let (s, d, proto, off) = parse_ip_header(&syn_ack).expect("SYN-ACK это IPv6");
        assert_eq!(proto, 6);
        assert_eq!(s, IpAddr::V6(Ipv6Addr::from(crate::ip_stack::SMOL_IPV6.octets())));
        assert_eq!(d, IpAddr::V6(v6_client()));
        assert_eq!(syn_ack[off + 13] & 0x12, 0x12, "SYN+ACK");

        let server_seq = u32::from_be_bytes([
            syn_ack[off + 4],
            syn_ack[off + 5],
            syn_ack[off + 6],
            syn_ack[off + 7],
        ]);

        let ack = tcp_v6(
            v6_client(),
            v6_target(),
            40000,
            443,
            1001,
            server_seq.wrapping_add(1),
            TCP_ACK,
            &[],
        );
        let mut ack = ack;
        let (_, _, _, off) = parse_ip_header(&ack).unwrap();
        rewrite_dst_to_smol(&mut ack, off, eph);
        queue.push_inbound(ack);
        assert!(stack.poll());
        assert_eq!(stack.tcp_socket(handle).state(), St::Established);

        // Данные вверх: клиент -> сокет.
        let mut up = tcp_v6(
            v6_client(),
            v6_target(),
            40000,
            443,
            1001,
            server_seq.wrapping_add(1),
            TCP_PSH | TCP_ACK,
            b"ping",
        );
        let (_, _, _, off) = parse_ip_header(&up).unwrap();
        rewrite_dst_to_smol(&mut up, off, eph);
        queue.push_inbound(up);
        assert!(stack.poll());

        let mut got = [0u8; 16];
        let n = stack
            .tcp_socket_mut(handle)
            .recv(|data| {
                let n = data.len().min(got.len());
                got[..n].copy_from_slice(&data[..n]);
                (n, n)
            })
            .expect("данные клиента обязаны дойти до сокета");
        assert_eq!(&got[..n], b"ping");

        // Данные вниз: сокет -> клиент.
        stack.tcp_socket_mut(handle).send_slice(b"pong").unwrap();
        assert!(stack.poll());
        let down = queue
            .pop_smol_outbound()
            .expect("ответ сокета обязан уйти в TUN");
        let (s, d, proto, off) = parse_ip_header(&down).unwrap();
        assert_eq!(proto, 6);
        assert_eq!(s, IpAddr::V6(Ipv6Addr::from(crate::ip_stack::SMOL_IPV6.octets())));
        assert_eq!(d, IpAddr::V6(v6_client()));
        let flags = down[off + 13];
        assert!(flags & TCP_ACK != 0);
        let payload = &down[off + 20..];
        // Сегмент может нести данные сразу или следующим кадром.
        let mut seen = payload.to_vec();
        if !seen.windows(4).any(|w| w == b"pong") {
            if let Some(next) = queue.pop_smol_outbound() {
                let (_, _, _, off) = parse_ip_header(&next).unwrap();
                seen.extend_from_slice(&next[off + 20..]);
            }
        }
        assert!(seen.windows(4).any(|w| w == b"pong"), "ответ сокета потерян");
    }

    /// Один интерфейс обслуживает IPv4 и IPv6 одновременно, сессии не смешиваются.
    #[test]
    fn ipv4_and_ipv6_sessions_coexist_on_one_stack() {
        use smoltcp::socket::tcp::State as St;

        let queue = PacketQueue::new();
        let mut stack = IpStack::new(queue.clone());
        let h6 = stack.add_tcp_socket(1024, 1024);
        let h4 = stack.add_tcp_socket(1024, 1024);
        stack.tcp_socket_mut(h6).listen(443).unwrap();
        stack.tcp_socket_mut(h4).listen(80).unwrap();

        let mut syn6 = tcp_v6(v6_client(), v6_target(), 40001, 443, 10, 0, TCP_SYN, &[]);
        let (_, _, _, off) = parse_ip_header(&syn6).unwrap();
        rewrite_dst_to_smol(&mut syn6, off, 443);
        queue.push_inbound(syn6);

        let mut syn4 = tcp_v4(
            Ipv4Addr::new(192, 0, 2, 10),
            Ipv4Addr::new(192, 0, 2, 20),
            40002,
            80,
            20,
            0,
            TCP_SYN,
            &[],
        );
        let (_, _, _, off) = parse_ip_header(&syn4).unwrap();
        rewrite_dst_to_smol(&mut syn4, off, 80);
        queue.push_inbound(syn4);

        for _ in 0..8 {
            stack.poll();
        }

        let mut saw_v6 = false;
        let mut saw_v4 = false;
        while let Some(pkt) = queue.pop_smol_outbound() {
            match parse_ip_header(&pkt) {
                Some((s, _, _, _)) if s == IpAddr::V6(Ipv6Addr::from(crate::ip_stack::SMOL_IPV6.octets())) => {
                    saw_v6 = true;
                }
                Some((s, _, _, _)) if s == IpAddr::V4(crate::ip_stack::SMOL_IP) => {
                    saw_v4 = true;
                }
                _ => {}
            }
        }
        assert!(saw_v6, "SYN-ACK по IPv6 потерян");
        assert!(saw_v4, "SYN-ACK по IPv4 потерян");
        assert_eq!(stack.tcp_socket(h6).state(), St::SynReceived);
        assert_eq!(stack.tcp_socket(h4).state(), St::SynReceived);
    }

    /// UDP/443 (QUIC) узнаётся на обоих семействах, соседние порты нет.
    #[test]
    fn is_udp_dst_port_flags_quic_on_both_families() {
        let q4 = build_udp_response(
            Ipv4Addr::new(192, 0, 2, 10),
            Ipv4Addr::new(203, 0, 113, 5),
            50000,
            443,
            b"quic",
        );
        assert!(is_udp_dst_port(&q4, 443), "IPv4 UDP/443 обязан считаться QUIC");
        assert!(!is_udp_dst_port(&q4, 853), "чужой порт не QUIC");

        let q6 = build_udp_response_v6(
            Ipv6Addr::new(0xfd00, 0x78, 0, 0, 0, 0, 0, 2),
            Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
            50001,
            443,
            b"quic",
        );
        assert!(is_udp_dst_port(&q6, 443), "IPv6 UDP/443 обязан считаться QUIC");

        // TCP на 443 не путаем с QUIC.
        let syn = tcp_v4(
            Ipv4Addr::new(192, 0, 2, 10),
            Ipv4Addr::new(203, 0, 113, 5),
            50002,
            443,
            1,
            0,
            TCP_SYN,
            &[],
        );
        assert!(!is_udp_dst_port(&syn, 443), "TCP/443 не UDP");
    }

    /// Профиль из JSON: ne_profile берёт набор LLD-39, отдельные ключи уточняют.
    #[test]
    fn parse_config_applies_ne_tuning_profile() {
        let cfg = parse_config(
            r#"{"server_address":"vps.example","server_port":8443,"obfuscation_key":"a2V5",
                "ne_profile":true,"block_quic":true,"max_sessions":64}"#,
        )
        .unwrap();
        assert!(cfg.block_quic);
        assert_eq!(cfg.tuning.session_rx_buf, 16 * 1024);
        assert_eq!(cfg.tuning.max_sessions, 64, "отдельный ключ перекрывает профиль");
        assert_eq!(cfg.tuning.recv_window, 256 * 1024);
    }

    /// Без ключей профиль остаётся прежним: дефолтные литералы.
    #[test]
    fn parse_config_without_tuning_keeps_defaults() {
        let cfg = parse_config(
            r#"{"server_address":"vps.example","server_port":8443,"obfuscation_key":"a2V5"}"#,
        )
        .unwrap();
        assert!(!cfg.block_quic);
        assert_eq!(cfg.tuning.session_rx_buf, SESSION_RX_BUF);
        assert_eq!(cfg.tuning.max_sessions, 0);
    }
}
