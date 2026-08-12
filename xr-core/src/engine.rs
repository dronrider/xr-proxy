//! VPN Engine — main entry point for mobile/desktop clients.
//!
//! Architecture:
//! 1. DNS queries (UDP:53) → intercepted via Fake DNS
//! 2. TCP SYN → smoltcp socket (listen on unique ephemeral port)
//! 3. TCP Established → spawn relay task to xr-server or direct
//! 4. TCP data: smoltcp ↔ channels ↔ relay task

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
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

/// Буферы smoltcp на одну TCP-сессию: заводятся на каждый SYN и живут, пока
/// живёт сессия. Вынесены в константы, потому что на них держится бюджет
/// памяти движка под iOS (LLD-39): расширение NEPacketTunnelProvider не
/// переживает 50 МБ на процесс, а 128 КиБ на сессию это главная статья
/// расхода. Меняя размер, смотри `tests/ios_memory_budget.rs`.
pub const SESSION_RX_BUF: usize = 65535;
pub const SESSION_TX_BUF: usize = 65535;

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
    /// Same semantic as `xr-client` — hot-swaps active Router when preset
    /// version changes on the hub. Ignored if `hub_url`/`hub_preset`/
    /// `hub_cache_dir` are not all set.
    pub hub_refresh_interval_secs: Option<u64>,
    /// Optional host-OS resolver for direct-mode fake-IP resolution.
    /// On Android it should call `Network.getAllByName()` on the underlying
    /// non-VPN network. When set, it's tried before the UDP:53 fallback.
    pub system_resolver: Option<SystemResolverFn>,
    /// Number of parallel mux tunnels (0 → pool default).
    pub mux_pool_size: usize,
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
        let codec = Codec::new(obfuscator, self.config.padding_min, self.config.padding_max);

        // Build router, optionally merging with hub preset.
        let router = if let (Some(hub_url), Some(preset_name), Some(cache_dir)) = (
            &self.config.hub_url,
            &self.config.hub_preset,
            &self.config.hub_cache_dir,
        ) {
            let mut cache = crate::presets::PresetCache::new(
                std::path::Path::new(cache_dir),
                hub_url,
                preset_name,
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
            let mux_pool = xr_proto::mux_pool::MuxPool::new(
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

        let ctx = Arc::new(SessionContext {
            router: std::sync::RwLock::new(Arc::new(router)),
            codec,
            fake_dns: fake_dns.clone(),
            stats: self.stats.clone(),
            on_server_down, protect_socket,
            server_pool: server_pool.clone(),
            dns_resolvers,
            system_resolver: self.config.system_resolver.clone(),
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
            let local_overrides = live_overrides.clone();
            let geoip_path_owned = self.config.geoip_path.clone();
            let ctx_bg = ctx.clone();
            let mut shutdown_rx_bg = shutdown_rx.clone();
            tokio::spawn(async move {
                let mut cache = crate::presets::PresetCache::new(
                    std::path::Path::new(&cache_dir),
                    &hub_url,
                    &preset_name,
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
            if let Err(e) = run_event_loop(queue, ctx, fake_dns, shutdown_rx, netgen_rx).await {
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

// ── Event loop ──────────────────────────────────────────────────────

async fn run_event_loop(
    queue: PacketQueue,
    ctx: Arc<SessionContext>,
    fake_dns: Arc<FakeDns>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    mut netgen_rx: tokio::sync::watch::Receiver<u64>,
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

        // ── 1. Intercept DNS ────────────────────────────────────────
        let mut tcp_packets = Vec::new();
        while let Some(packet) = queue.pop_inbound_public() {
            if let Some(dns_response) = try_handle_dns(&packet, &fake_dns) {
                ctx.stats.add_dns_query();
                queue.push_outbound_public(dns_response);
            } else {
                tcp_packets.push(packet);
            }
        }

        // ── 2. Process TCP packets: rewrite dst port for SYNs ───────
        for mut pkt in tcp_packets {
            if pkt.len() < 20 { queue.push_inbound(pkt); continue; }
            if pkt[0] >> 4 != 4 { queue.push_inbound(pkt); continue; }
            let ihl = (pkt[0] & 0x0F) as usize * 4;
            let protocol = pkt[9];

            // Only rewrite TCP packets.
            if protocol == 6 && pkt.len() >= ihl + 14 {
                let src_port = u16::from_be_bytes([pkt[ihl], pkt[ihl+1]]);
                let dst_port = u16::from_be_bytes([pkt[ihl+2], pkt[ihl+3]]);
                let flags = pkt[ihl + 13];
                let is_syn = flags & 0x02 != 0 && flags & 0x10 == 0;

                let src_ip = Ipv4Addr::new(pkt[12], pkt[13], pkt[14], pkt[15]);
                let dst_ip = Ipv4Addr::new(pkt[16], pkt[17], pkt[18], pkt[19]);
                let orig_key = TcpSessionKey {
                    src_addr: SocketAddr::new(IpAddr::V4(src_ip), src_port),
                    dst_addr: SocketAddr::new(IpAddr::V4(dst_ip), dst_port),
                };

                if is_syn && !sessions.contains_key(&orig_key) {
                    // New connection: assign ephemeral port, create listen socket.
                    let eph_port = next_ephemeral_port();
                    let handle = stack.add_tcp_socket(SESSION_RX_BUF, SESSION_TX_BUF);
                    let socket = stack.tcp_socket_mut(handle);

                    if socket.listen(eph_port).is_ok() {
                        // Cache domain NOW — FakeDns TTL might expire before Established.
                        let domain = if let IpAddr::V4(v4) = dst_ip.into() {
                            fake_dns.lookup(v4)
                        } else { None };

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

                        // Rewrite dst → smoltcp's IP:eph_port.
                        pkt[ihl+2] = (eph_port >> 8) as u8;
                        pkt[ihl+3] = eph_port as u8;
                        let smol_ip = crate::ip_stack::SMOL_IP;
                        let src_ip_inb = Ipv4Addr::new(pkt[12], pkt[13], pkt[14], pkt[15]);
                        pkt[16] = smol_ip.octets()[0];
                        pkt[17] = smol_ip.octets()[1];
                        pkt[18] = smol_ip.octets()[2];
                        pkt[19] = smol_ip.octets()[3];
                        pkt[10] = 0; pkt[11] = 0;
                        let ip_cksum = ipv4_checksum(&pkt[..ihl]);
                        pkt[10..12].copy_from_slice(&ip_cksum.to_be_bytes());
                        let smol_addr = Ipv4Addr::new(smol_ip.octets()[0], smol_ip.octets()[1], smol_ip.octets()[2], smol_ip.octets()[3]);
                        tcp_checksum_update(&mut pkt, ihl, &src_ip_inb, &smol_addr);
                    } else {
                        stack.remove_socket(handle);
                    }
                } else if let Some(session) = sessions.get(&orig_key) {
                    // Existing connection (ACK, data, FIN, etc.):
                    // Rewrite dst → smoltcp's IP:eph_port.
                    let ep = session.eph_port;
                    pkt[ihl+2] = (ep >> 8) as u8;
                    pkt[ihl+3] = ep as u8;
                    let src_ip_inb = Ipv4Addr::new(pkt[12], pkt[13], pkt[14], pkt[15]);
                    let smol_ip = crate::ip_stack::SMOL_IP;
                    pkt[16] = smol_ip.octets()[0];
                    pkt[17] = smol_ip.octets()[1];
                    pkt[18] = smol_ip.octets()[2];
                    pkt[19] = smol_ip.octets()[3];
                    pkt[10] = 0; pkt[11] = 0;
                    let ip_cksum = ipv4_checksum(&pkt[..ihl]);
                    pkt[10..12].copy_from_slice(&ip_cksum.to_be_bytes());
                    let smol_addr = Ipv4Addr::new(smol_ip.octets()[0], smol_ip.octets()[1], smol_ip.octets()[2], smol_ip.octets()[3]);
                    tcp_checksum_update(&mut pkt, ihl, &src_ip_inb, &smol_addr);
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
                let (to_relay_tx, to_relay_rx) = mpsc::channel(512);
                let (from_relay_tx, from_relay_rx) = mpsc::channel(512);
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
                // Detect dead relay: if receiver dropped, relay task died.
                // Abort socket so the app gets RST and retries with fresh DNS.
                if relay.to_relay.is_closed() {
                    socket.abort();
                    stale_keys.push(*key);
                    continue;
                }

                // smoltcp → relay (upload).
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

                // relay → smoltcp (download).
                match drain_download_to_tx(
                    &mut *socket,
                    &mut relay.from_relay,
                    &mut relay.pending_download,
                    &ctx.stats,
                ) {
                    DownloadDrain::Idle => {}
                    DownloadDrain::Disconnected => {
                        // Relay sender dropped — relay task finished.
                        socket.abort();
                        stale_keys.push(*key);
                    }
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

        // ── 7. Rewrite outbound packets: restore original dst ───────
        // smoltcp sends packets with src=SMOL_IP:eph_port.
        // We need to rewrite them to src=original_dst_ip:original_dst_port
        // so the TUN client sees the response from the expected address.
        while let Some(mut pkt) = queue.pop_smol_outbound() {
            if pkt.len() >= 40 && pkt[0] >> 4 == 4 && pkt[9] == 6 {
                let ihl = (pkt[0] & 0x0F) as usize * 4;
                if pkt.len() >= ihl + 4 {
                    let src_port = u16::from_be_bytes([pkt[ihl], pkt[ihl+1]]);
                    if let Some(orig_key) = port_to_key.get(&src_port) {
                        if let IpAddr::V4(orig_dst_ip) = orig_key.dst_addr.ip() {
                            pkt[12] = orig_dst_ip.octets()[0];
                            pkt[13] = orig_dst_ip.octets()[1];
                            pkt[14] = orig_dst_ip.octets()[2];
                            pkt[15] = orig_dst_ip.octets()[3];
                            let orig_port = orig_key.dst_addr.port();
                            pkt[ihl] = (orig_port >> 8) as u8;
                            pkt[ihl+1] = orig_port as u8;
                            pkt[10] = 0; pkt[11] = 0;
                            let ip_cksum = ipv4_checksum(&pkt[..ihl]);
                            pkt[10..12].copy_from_slice(&ip_cksum.to_be_bytes());
                            let dst_ip = Ipv4Addr::new(pkt[16], pkt[17], pkt[18], pkt[19]);
                            tcp_checksum_update(&mut pkt, ihl, &orig_dst_ip, &dst_ip);
                        }
                    }
                }
            }
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
}

impl TxSink for smoltcp::socket::tcp::Socket<'_> {
    fn can_send(&self) -> bool {
        smoltcp::socket::tcp::Socket::can_send(self)
    }
    fn enqueue(&mut self, data: &[u8]) -> usize {
        // send_slice errors only if the socket can't send at all; treat that
        // as "0 accepted" — `can_send()` gates the call anyway.
        self.send_slice(data).unwrap_or(0)
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
    let (src_ip, dst_ip, protocol, ihl) = parse_ipv4_header(packet)?;
    if protocol != 17 { return None; }
    let udp = &packet[ihl..];
    let (src_port, dst_port, data_offset) = parse_udp_header(udp)?;
    if dst_port != 53 { return None; }
    let (dns_response, _) = fake_dns.handle_query(&udp[data_offset..])?;
    Some(build_udp_response(dst_ip, src_ip, dst_port, src_port, &dns_response))
}

pub fn parse_ipv4_header(p: &[u8]) -> Option<(Ipv4Addr, Ipv4Addr, u8, usize)> {
    if p.len() < 20 || p[0] >> 4 != 4 { return None; }
    let ihl = (p[0] & 0x0F) as usize * 4;
    if ihl < 20 || p.len() < ihl { return None; }
    Some((Ipv4Addr::new(p[12],p[13],p[14],p[15]), Ipv4Addr::new(p[16],p[17],p[18],p[19]), p[9], ihl))
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
            system_resolver: None,
            mux_pool_size: 1,
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

        let mut sink = MockSink { buf: Vec::new(), capacity: 2048, max_accept: 700 };
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
        let mut sink = MockSink { buf: Vec::new(), capacity: 4096, max_accept: 4096 };
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
        let mut sink = MockSink { buf: Vec::new(), capacity: 400, max_accept: 400 };
        let mut pending = Vec::new();

        assert!(matches!(
            drain_download_to_tx(&mut sink, &mut rx, &mut pending, &stats),
            DownloadDrain::Idle
        ));
        assert_eq!(sink.buf.len(), 400, "should fill exactly the available room");
        assert_eq!(pending.len(), 600, "remainder must be preserved, not dropped");

        // Drain the sink (app reads) and run again — the rest must flush.
        sink.buf.clear();
        let _ = drain_download_to_tx(&mut sink, &mut rx, &mut pending, &stats);
        assert_eq!(sink.buf.len(), 400);
        assert_eq!(pending.len(), 200);
    }
}
