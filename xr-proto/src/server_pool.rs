//! Pool of *servers* on the client side (LLD-10).
//!
//! `MuxPool` (LLD-09) держит N параллельных TCP-туннелей к **одному** серверу;
//! `ServerPool` это тонкий слой выбора над несколькими `MuxPool` (по одному на
//! VPS): все стримы идут в пул активного сервера, при его падении активным
//! становится следующий по приоритету здоровый (failover), после
//! восстановления primary трафик возвращается на него с анти-флаппинг
//! задержкой (failback c hold-down).
//!
//! ```text
//! open_stream(target) -> [ServerPool] -> MuxPool активного -> MuxStream
//!                              │
//!                              └─ health_loop: проба primary + failback
//! ```
//!
//! Wire-протокол и логика слотов не трогаются, весь failover-механизм
//! сводится к выбору индекса активного `MuxPool`.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::mux::MuxStream;
use crate::mux_pool::MuxPool;
use crate::protocol::TargetAddr;

/// Callback для событий пула (failover/failback/recycle): хост может
/// дублировать их в свой пользовательский журнал (на Android это engine-лог).
pub type PoolEventFn = Arc<dyn Fn(&str) + Send + Sync>;

/// Верхняя граница одной health-пробы. Без неё проба мёртвого сервера,
/// молча съедающего SYN, висела бы до таймаута ОС и копилась от тика к тику.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Энергетический профиль пула (LLD-10 §2.7). Роутер может позволить себе
/// тёплые резервы и частые пробы; телефону каждое лишнее пробуждение радио
/// стоит батареи, поэтому там пробер живёт только в деградированном состоянии.
#[derive(Debug, Clone)]
pub struct PoolProfile {
    /// Держать mux-соединения ко всем серверам (мгновенный failover, но
    /// постоянные ESTAB на каждом VPS). Холодный профиль поднимает mux к
    /// backup только в момент failover (+1 RTT на handshake).
    pub warm_backups: bool,
    /// Пробить серверы и в здоровом состоянии (active == primary). При false
    /// пробер молчит, пока активен primary: пассивный breaker сам поймает его
    /// падение по реальному трафику, а failback без деградации не нужен.
    pub probe_when_healthy: bool,
    pub probe_interval: Duration,
    /// Сколько primary должен быть непрерывно живым, прежде чем активный
    /// трафик вернётся на него. Гасит флаппинг на нестабильной связи.
    pub failback_hold: Duration,
}

impl PoolProfile {
    /// Роутер: тёплые резервы, проба каждые 15с, failback после минуты up.
    pub fn router() -> Self {
        Self {
            warm_backups: true,
            probe_when_healthy: true,
            probe_interval: Duration::from_secs(15),
            failback_hold: Duration::from_secs(60),
        }
    }

    /// Мобильный клиент: холодный backup, пробы только в деградации и реже.
    /// В здоровом простое пул не добавляет ни одного пробуждения радио.
    pub fn mobile() -> Self {
        Self {
            warm_backups: false,
            probe_when_healthy: false,
            probe_interval: Duration::from_secs(60),
            failback_hold: Duration::from_secs(60),
        }
    }
}

/// Классификация сбоя сервера. Пока единственный вариант; настоящие классы
/// (`ServerUnreachable`/`HandshakeReset`/...) появятся в LLD-11.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownClass {
    Unknown,
}

/// Здоровье сервера глазами пула.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    Up,
    Down { since: Instant, class: DownClass },
}

/// Сервер на входе в пул: лейблы для логов + готовый `MuxPool`.
/// Кодек (в т.ч. per-server override ключа) собирает вызывающий.
pub struct PoolServer {
    /// Человекочитаемое имя (из конфига/инвайта); если пустое, берётся адрес.
    pub name: String,
    /// "ip:port" для логов и статусной строки.
    pub addr: String,
    pub pool: Arc<MuxPool>,
}

struct SlotState {
    health: HealthState,
    /// Начало непрерывного подтверждённого up, от него отсчитывается
    /// hold-down для failback. Сбрасывается любым сбоем (пробы или
    /// реального стрима).
    up_since: Option<Instant>,
}

struct ServerSlot {
    name: String,
    addr: String,
    pool: Arc<MuxPool>,
    state: Mutex<SlotState>,
}

impl ServerSlot {
    fn label(&self) -> &str {
        if self.name.is_empty() { &self.addr } else { &self.name }
    }

    fn mark_up(&self) {
        let mut st = self.state.lock().unwrap();
        st.health = HealthState::Up;
        if st.up_since.is_none() {
            st.up_since = Some(Instant::now());
        }
    }

    fn mark_down(&self) {
        let mut st = self.state.lock().unwrap();
        if !matches!(st.health, HealthState::Down { .. }) {
            st.health = HealthState::Down {
                since: Instant::now(),
                class: DownClass::Unknown,
            };
        }
        st.up_since = None;
    }

    fn up_for(&self) -> Option<Duration> {
        self.state.lock().unwrap().up_since.map(|t| t.elapsed())
    }

    fn reset(&self) {
        let mut st = self.state.lock().unwrap();
        st.health = HealthState::Up;
        st.up_since = None;
    }
}

/// Клиентский пул серверов: primary/backup по приоритету, sticky-to-primary.
pub struct ServerPool {
    /// Отсортированы по приоритету, индекс 0 это primary.
    slots: Vec<ServerSlot>,
    active: AtomicUsize,
    profile: PoolProfile,
    on_event: Option<PoolEventFn>,
}

impl ServerPool {
    /// `servers` уже отсортированы вызывающим по приоритету (primary первым).
    ///
    /// # Panics
    /// Пустой список это ошибка программирования вызывающего: конфиг и профиль
    /// валидируются раньше (>=1 сервер обязателен, как `source_ips` у relay).
    pub fn new(
        servers: Vec<PoolServer>,
        profile: PoolProfile,
        on_event: Option<PoolEventFn>,
    ) -> Arc<Self> {
        assert!(!servers.is_empty(), "server pool requires at least one server");
        let slots = servers
            .into_iter()
            .map(|s| ServerSlot {
                name: s.name,
                addr: s.addr,
                pool: s.pool,
                state: Mutex::new(SlotState { health: HealthState::Up, up_since: None }),
            })
            .collect();
        Arc::new(Self {
            slots,
            active: AtomicUsize::new(0),
            profile,
            on_event,
        })
    }

    pub fn size(&self) -> usize {
        self.slots.len()
    }

    pub fn active_index(&self) -> usize {
        self.active.load(Ordering::Relaxed).min(self.slots.len() - 1)
    }

    /// Имя активного сервера (для статусной строки «через X (резерв)»).
    pub fn active_name(&self) -> String {
        self.slots[self.active_index()].label().to_string()
    }

    /// "name (ip:port)" активного, идёт в дебаг-строки вместо прежнего
    /// одиночного `server_addr`.
    pub fn active_label(&self) -> String {
        let slot = &self.slots[self.active_index()];
        if slot.name.is_empty() {
            slot.addr.clone()
        } else {
            format!("{} ({})", slot.name, slot.addr)
        }
    }

    /// Активен не-primary, то есть клиент работает через резерв.
    pub fn is_backup_active(&self) -> bool {
        self.active_index() != 0
    }

    /// Хук для мониторинга/панели здоровья (LLD-11).
    pub fn server_health(&self, idx: usize) -> Option<HealthState> {
        self.slots.get(idx).map(|s| s.state.lock().unwrap().health)
    }

    fn emit(&self, msg: &str) {
        tracing::info!("{}", msg);
        if let Some(cb) = &self.on_event {
            cb(msg);
        }
    }

    /// Атомарно переключает активный сервер. CAS защищает от дублей при
    /// конкурентных failover'ах: лог и событие получает только победитель.
    fn switch_active(&self, from: usize, to: usize, reason: &str) -> bool {
        if from == to {
            return false;
        }
        let switched = self
            .active
            .compare_exchange(from, to, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok();
        if switched {
            self.emit(&format!(
                "server {}: {} -> {}",
                reason,
                self.slots[from].label(),
                self.slots[to].label(),
            ));
        }
        switched
    }

    /// Порядок обхода: активный первым, дальше все остальные по приоритету.
    /// Падение активного backup'а таким образом сразу пробует primary. Это
    /// failover на единственного здорового, hold-down здесь не применяется
    /// (он гасит только добровольный возврат при живом backup'е).
    fn walk_order(&self, start: usize) -> impl Iterator<Item = usize> + '_ {
        let n = self.slots.len();
        std::iter::once(start).chain((0..n).filter(move |i| *i != start))
    }

    /// Открыть логический стрим через активный сервер, при его отказе через
    /// следующий по приоритету здоровый (тот становится активным).
    /// `Err` значит, что исчерпан весь пул; вызывающий уводит соединение
    /// в Direct.
    pub async fn open_stream(&self, target: &TargetAddr) -> io::Result<MuxStream> {
        let start = self.active_index();
        let mut last_err: Option<io::Error> = None;

        for idx in self.walk_order(start) {
            match self.slots[idx].pool.open_stream(target).await {
                Ok(stream) => {
                    self.slots[idx].mark_up();
                    if idx != start {
                        self.switch_active(start, idx, "failover");
                    }
                    return Ok(stream);
                }
                Err(e) => {
                    self.slots[idx].mark_down();
                    tracing::debug!(
                        "server {} unavailable ({}), trying next",
                        self.slots[idx].label(),
                        e
                    );
                    last_err = Some(e);
                }
            }
        }

        Err(last_err
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::Other, "no servers in pool")))
    }

    /// Прогрев при старте / после смены сети. Тёплый профиль поднимает mux ко
    /// всем серверам параллельно, холодный только к активному (при его отказе
    /// к следующему по приоритету). `Ok` значит, что хотя бы один сервер
    /// отвечает.
    pub async fn warmup(&self) -> io::Result<()> {
        if self.profile.warm_backups {
            let mut handles = Vec::with_capacity(self.slots.len());
            for (idx, slot) in self.slots.iter().enumerate() {
                let pool = slot.pool.clone();
                handles.push(tokio::spawn(async move { (idx, pool.warmup().await) }));
            }
            let mut best: Option<usize> = None;
            let mut last_err: Option<io::Error> = None;
            for h in handles {
                let Ok((idx, res)) = h.await else { continue };
                match res {
                    Ok(()) => {
                        self.slots[idx].mark_up();
                        best = Some(best.map_or(idx, |b| b.min(idx)));
                    }
                    Err(e) => {
                        self.slots[idx].mark_down();
                        last_err = Some(e);
                    }
                }
            }
            match best {
                Some(idx) => {
                    // На старте/после re-bind липкость не действует: активным
                    // сразу становится самый приоритетный из живых.
                    let cur = self.active.load(Ordering::Relaxed);
                    self.switch_active(cur, idx, "warmup");
                    Ok(())
                }
                None => Err(last_err.unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::Other, "no servers in pool")
                })),
            }
        } else {
            let start = self.active_index();
            let mut last_err: Option<io::Error> = None;
            for idx in self.walk_order(start) {
                match self.slots[idx].pool.warmup().await {
                    Ok(()) => {
                        self.slots[idx].mark_up();
                        if idx != start {
                            self.switch_active(start, idx, "failover");
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        self.slots[idx].mark_down();
                        last_err = Some(e);
                    }
                }
            }
            Err(last_err
                .unwrap_or_else(|| io::Error::new(io::ErrorKind::Other, "no servers in pool")))
        }
    }

    /// Сетевой re-bind (C4, Android LTE<->Wi-Fi): пересоздать все mux-пулы и
    /// вернуть активного на primary. Знание о здоровье, накопленное на
    /// прежней сети, устарело вместе с ней.
    pub async fn recycle(&self) {
        for slot in &self.slots {
            slot.pool.recycle().await;
            slot.reset();
        }
        let cur = self.active.swap(0, Ordering::Relaxed);
        if cur != 0 {
            self.emit(&format!(
                "server pool recycled, active reset to {}",
                self.slots[0].label()
            ));
        }
    }

    /// Фоновый пробер: держит здоровье серверов свежим и возвращает трафик на
    /// primary после его восстановления (failback c hold-down §2.5). Работает
    /// бесконечно, вызывающий оборачивает его в `select!` со своим shutdown.
    pub async fn health_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(self.profile.probe_interval).await;
            self.health_tick().await;
        }
    }

    async fn health_tick(&self) {
        let active = self.active_index();

        // Экономный профиль: пока активен primary, не пробим вообще.
        // Падение primary поймает пассивный breaker по реальному трафику,
        // а failback без деградации не нужен (§2.7).
        if !self.profile.probe_when_healthy && active == 0 {
            return;
        }

        // Кого пробить: кандидатов на failback (выше по приоритету активного)
        // всегда, а в тёплом профиле все серверы целиком, что заодно держит
        // mux к каждому из них живым (мгновенный failover без cold-start).
        let upto = if self.profile.probe_when_healthy {
            self.slots.len()
        } else {
            active
        };

        for idx in 0..upto {
            let slot = &self.slots[idx];
            // Тёплый профиль прогревает все N слотов mux-пула, холодный
            // ограничивается одним соединением-пробой.
            let probe = async {
                if self.profile.warm_backups {
                    slot.pool.warmup().await
                } else {
                    slot.pool.probe().await
                }
            };
            match tokio::time::timeout(PROBE_TIMEOUT, probe).await {
                Ok(Ok(())) => slot.mark_up(),
                Ok(Err(_)) | Err(_) => slot.mark_down(),
            }
        }

        // Failback: самый приоритетный сервер, непрерывно живой не меньше
        // hold-down, забирает активность. Мигание primary в течение окна
        // сбрасывает up_since (mark_down выше), и таймер начинается заново.
        let active = self.active_index();
        for idx in 0..active {
            if let Some(up) = self.slots[idx].up_for() {
                if up >= self.profile.failback_hold {
                    self.switch_active(active, idx, "failback");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::{mux_handshake_server, Multiplexer};
    use crate::mux_pool::ConnectFn;
    use crate::obfuscation::{ModifierStrategy, Obfuscator};
    use crate::protocol::{Codec, Command};
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, AtomicU32};
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};

    fn test_codec() -> Codec {
        let key = b"test-key-32-bytes-long-enough!!!".to_vec();
        let obfs = Obfuscator::new(key, 0xDEADBEEF, ModifierStrategy::PositionalXorRotate);
        Codec::new(obfs, 0, 0)
    }

    /// Минимальный in-process mux-сервер: MuxInit-handshake + мгновенный
    /// ConnectAck на каждый Connect. Ровно то, что нужно, чтобы
    /// `MuxPool::open_stream` завершился успехом без настоящего xr-server.
    async fn spawn_test_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let codec = test_codec();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let mut filled = 0;
                    let init = loop {
                        let Ok(n) = sock.read(&mut buf[filled..]).await else { return };
                        if n == 0 {
                            return;
                        }
                        filled += n;
                        match codec.decode_frame(&buf[..filled]) {
                            Ok(Some((frame, _))) => break frame,
                            Ok(None) => continue,
                            Err(_) => return,
                        }
                    };
                    if !mux_handshake_server(&mut sock, &codec, &init)
                        .await
                        .unwrap_or(false)
                    {
                        return;
                    }
                    let mux = Multiplexer::new_server(sock, codec.clone());
                    let Some(mut rx) = mux.take_new_stream_rx().await else { return };
                    while let Some(ns) = rx.recv().await {
                        let _ = mux
                            .send_frame(ns.stream_id, Command::ConnectAck, vec![0])
                            .await;
                        let _ = mux.register_stream(ns.stream_id).await;
                    }
                });
            }
        });
        addr
    }

    fn connect_to(addr: SocketAddr, counter: Arc<AtomicU32>) -> ConnectFn {
        Arc::new(move || {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::Relaxed);
                TcpStream::connect(addr).await
            })
        })
    }

    fn failing_connect(counter: Arc<AtomicU32>) -> ConnectFn {
        Arc::new(move || {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::Relaxed);
                Err(io::Error::new(io::ErrorKind::ConnectionRefused, "test"))
            })
        })
    }

    /// connect_fn с рубильником: пока `dead`, соединение отказывает. Так
    /// имитируется падение и восстановление primary для failover/failback.
    fn switchable_connect(addr: SocketAddr, dead: Arc<AtomicBool>) -> ConnectFn {
        Arc::new(move || {
            let dead = dead.clone();
            Box::pin(async move {
                if dead.load(Ordering::Relaxed) {
                    return Err(io::Error::new(io::ErrorKind::ConnectionRefused, "down"));
                }
                TcpStream::connect(addr).await
            })
        })
    }

    fn slot(name: &str, connect: ConnectFn) -> PoolServer {
        PoolServer {
            name: name.to_string(),
            addr: "127.0.0.1:0".to_string(),
            pool: MuxPool::new(connect, test_codec(), 1),
        }
    }

    fn target() -> TargetAddr {
        TargetAddr::Domain("test.example".to_string(), 443)
    }

    #[tokio::test]
    async fn test_failover_to_backup_when_primary_down() {
        let backup_addr = spawn_test_server().await;
        let primary_calls = Arc::new(AtomicU32::new(0));
        let backup_calls = Arc::new(AtomicU32::new(0));

        let pool = ServerPool::new(
            vec![
                slot("primary", failing_connect(primary_calls.clone())),
                slot("backup", connect_to(backup_addr, backup_calls.clone())),
            ],
            PoolProfile::mobile(),
            None,
        );

        let stream = pool.open_stream(&target()).await.expect("backup must serve");
        assert!(stream.is_alive());
        assert_eq!(pool.active_index(), 1, "active must move to the backup");
        assert!(pool.is_backup_active());
        assert_eq!(pool.active_name(), "backup");
        assert!(primary_calls.load(Ordering::Relaxed) >= 1);

        // Следующий стрим идёт сразу в backup, primary не трогается
        // (его breaker взведён, а активный уже сместился).
        let primary_before = primary_calls.load(Ordering::Relaxed);
        let _ = pool.open_stream(&target()).await.expect("still via backup");
        assert_eq!(primary_calls.load(Ordering::Relaxed), primary_before);
    }

    #[tokio::test]
    async fn test_all_down_returns_err() {
        let pool = ServerPool::new(
            vec![
                slot("a", failing_connect(Arc::new(AtomicU32::new(0)))),
                slot("b", failing_connect(Arc::new(AtomicU32::new(0)))),
            ],
            PoolProfile::mobile(),
            None,
        );
        let err = pool.open_stream(&target()).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
        // Клиент на этом Err уводит соединение в Direct (on_server_down).
    }

    #[tokio::test]
    async fn test_failback_after_holddown() {
        let server_addr = spawn_test_server().await;
        let primary_dead = Arc::new(AtomicBool::new(true));

        let profile = PoolProfile {
            warm_backups: false,
            probe_when_healthy: false,
            probe_interval: Duration::from_millis(10),
            failback_hold: Duration::from_millis(50),
        };
        let pool = ServerPool::new(
            vec![
                slot("primary", switchable_connect(server_addr, primary_dead.clone())),
                slot("backup", connect_to(server_addr, Arc::new(AtomicU32::new(0)))),
            ],
            profile,
            None,
        );

        // Primary мёртв, уезжаем на backup.
        let _ = pool.open_stream(&target()).await.expect("backup serves");
        assert_eq!(pool.active_index(), 1);

        // Primary ожил: первая проба запускает hold-down, но переключения
        // ещё нет.
        primary_dead.store(false, Ordering::Relaxed);
        pool.health_tick().await;
        assert_eq!(pool.active_index(), 1, "hold-down must delay failback");

        // Мигнул вниз, и таймер сбрасывается.
        primary_dead.store(true, Ordering::Relaxed);
        pool.health_tick().await;
        primary_dead.store(false, Ordering::Relaxed);
        pool.health_tick().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        pool.health_tick().await;
        assert_eq!(
            pool.active_index(),
            1,
            "a primary flap must restart the hold-down timer"
        );

        // Непрерывный up дольше hold-down даёт возврат на primary.
        tokio::time::sleep(Duration::from_millis(60)).await;
        pool.health_tick().await;
        assert_eq!(pool.active_index(), 0, "failback must return to primary");
        assert!(!pool.is_backup_active());
    }

    #[tokio::test]
    async fn test_cold_profile_no_warmup_no_probe() {
        let server_addr = spawn_test_server().await;
        let primary_calls = Arc::new(AtomicU32::new(0));
        let backup_calls = Arc::new(AtomicU32::new(0));

        let pool = ServerPool::new(
            vec![
                slot("primary", connect_to(server_addr, primary_calls.clone())),
                slot("backup", connect_to(server_addr, backup_calls.clone())),
            ],
            PoolProfile::mobile(),
            None,
        );

        // Холодный warmup трогает только активного.
        pool.warmup().await.expect("primary is up");
        assert_eq!(primary_calls.load(Ordering::Relaxed), 1);
        assert_eq!(backup_calls.load(Ordering::Relaxed), 0, "backup stays cold");

        // Здоровое состояние (active == primary): тик пробера это no-op,
        // ни одного лишнего соединения (радио телефона не трогаем).
        pool.health_tick().await;
        pool.health_tick().await;
        assert_eq!(primary_calls.load(Ordering::Relaxed), 1);
        assert_eq!(backup_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_warm_profile_warms_backups() {
        let server_addr = spawn_test_server().await;
        let primary_calls = Arc::new(AtomicU32::new(0));
        let backup_calls = Arc::new(AtomicU32::new(0));

        let pool = ServerPool::new(
            vec![
                slot("primary", connect_to(server_addr, primary_calls.clone())),
                slot("backup", connect_to(server_addr, backup_calls.clone())),
            ],
            PoolProfile::router(),
            None,
        );

        pool.warmup().await.expect("both up");
        assert!(primary_calls.load(Ordering::Relaxed) >= 1);
        assert!(
            backup_calls.load(Ordering::Relaxed) >= 1,
            "warm profile must pre-establish the backup too"
        );
        assert_eq!(pool.active_index(), 0);
    }

    #[tokio::test]
    async fn test_recycle_resets_active_to_primary() {
        let backup_addr = spawn_test_server().await;
        let pool = ServerPool::new(
            vec![
                slot("primary", failing_connect(Arc::new(AtomicU32::new(0)))),
                slot("backup", connect_to(backup_addr, Arc::new(AtomicU32::new(0)))),
            ],
            PoolProfile::mobile(),
            None,
        );

        let _ = pool.open_stream(&target()).await.expect("backup serves");
        assert_eq!(pool.active_index(), 1);

        pool.recycle().await;
        assert_eq!(
            pool.active_index(),
            0,
            "network re-bind must reset stickiness back to primary"
        );
    }

    #[tokio::test]
    async fn test_single_server_pool_behaves_plain() {
        let addr = spawn_test_server().await;
        let pool = ServerPool::new(
            vec![slot("only", connect_to(addr, Arc::new(AtomicU32::new(0))))],
            PoolProfile::mobile(),
            None,
        );
        let stream = pool.open_stream(&target()).await.expect("single server serves");
        assert!(stream.is_alive());
        assert_eq!(pool.active_index(), 0);
        assert!(!pool.is_backup_active());
    }

    #[tokio::test]
    async fn test_failover_emits_event() {
        let backup_addr = spawn_test_server().await;
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let events_cb = events.clone();
        let pool = ServerPool::new(
            vec![
                slot("aeza", failing_connect(Arc::new(AtomicU32::new(0)))),
                slot("timeweb", connect_to(backup_addr, Arc::new(AtomicU32::new(0)))),
            ],
            PoolProfile::mobile(),
            Some(Arc::new(move |msg: &str| {
                events_cb.lock().unwrap().push(msg.to_string());
            })),
        );

        let _ = pool.open_stream(&target()).await.expect("backup serves");
        let log = events.lock().unwrap();
        assert!(
            log.iter().any(|m| m.contains("failover") && m.contains("timeweb")),
            "failover must be reported to the host log, got: {:?}",
            *log
        );
    }
}
