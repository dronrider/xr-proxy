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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// Сколько ждать стрим от одного сервера, прежде чем перейти на следующий.
///
/// Молчащий (blackhole) primary держит TCP «живым» и не отдаёт RST, поэтому его
/// `MuxPool::open_stream` возвращает ошибку только после обхода всех слотов, где
/// каждый упирается в свой ConnectAck-таймаут (~4×10с). Без этой границы
/// `open_stream` висел бы десятки секунд и уводил соединение в Direct
/// (`on_server_down`), хотя живой резерв принял бы его за один RTT. Граница
/// переводит на следующий сервер за секунды, поэтому трафик уходит на резерв, а
/// не утекает мимо прокси. Значение с запасом над нормальным ConnectAck (<1с),
/// но много меньше полного обхода слотов.
const PER_SERVER_OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Анти-флаппинг failback (XR-082). На прерывисто-достижимом сервере лёгкая
/// проба `probe_fresh` изредка пролезает и набирает hold-down, failback
/// возвращает на него активность, но реальный трафик тут же не идёт и failover
/// уводит обратно. Цикл повторяется каждые ~hold секунд. Если failback
/// откатывается реальным failover'ом в пределах этого окна, считаем его
/// преждевременным (слот «мигнул»).
const FAILBACK_FLAP_WINDOW: Duration = Duration::from_secs(45);
/// Потолок счётчика миганий: `hold * 2^cap` уже перекрывает `FAILBACK_BACKOFF_MAX`,
/// дальше растить незачем (и защита от переполнения сдвига).
const FAILBACK_FLAP_CAP: u32 = 6;
/// Максимальная задержка перед повторным failback на мигающий слот.
const FAILBACK_BACKOFF_MAX: Duration = Duration::from_secs(1800);

/// Минимальное время на сервере после failover (XR-296). Пока свежий
/// резерв активен меньше этого срока, его отказ не двигает активный слот:
/// стрим всё равно уходит следующему живому по обходу, но статус и точка
/// отсчёта failback остаются на месте. Иначе на частично живой связи каждый
/// стрим катал активный между двумя серверами. Первый уход в резерв задержки
/// не получает: у сервера, выбранного при старте, срок не отсчитывается.
/// Меньше `FAILBACK_FLAP_WINDOW`, поэтому штраф за мигание по-прежнему
/// успевает лечь на слот, отказавший вскоре после смены.
///
/// То же окно держит активный после возврата аплинка: ОС объявляет сеть
/// раньше, чем у неё встают маршруты, и первый коннект после `uplink_restored`
/// падает с ENETUNREACH за секунду до того, как заработает второй. На стенде
/// без этого окна возврат сети уводил пул на резерв следующей же строкой
/// журнала, то есть ровно тем катанием, ради которого заводилась заморозка.
const MIN_ACTIVE_DWELL: Duration = Duration::from_secs(30);

/// Энергетический профиль пула (LLD-10 §2.7). Роутер может позволить себе
/// тёплые резервы и частые пробы; телефону каждое лишнее пробуждение радио
/// стоит батареи, поэтому там пробер живёт только в деградированном состоянии.
#[derive(Debug, Clone)]
pub struct PoolProfile {
    /// Держать mux-соединения ко всем серверам (мгновенный failover, но
    /// постоянные ESTAB на каждом VPS). Холодный профиль поднимает mux к
    /// backup только в момент failover (+1 RTT на handshake).
    pub warm_backups: bool,
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
            probe_interval: Duration::from_secs(15),
            failback_hold: Duration::from_secs(60),
        }
    }

    /// Мобильный клиент: холодный backup, пробы только в деградации и реже.
    /// В здоровом простое пул не добавляет ни одного пробуждения радио.
    pub fn mobile() -> Self {
        Self {
            warm_backups: false,
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
    /// Когда слот последний раз стал активным (failover/failback/warmup).
    /// По нему ловим «мигание»: failback тут же откатывается реальным
    /// failover'ом (XR-082).
    became_active_at: Option<Instant>,
    /// Чем слот стал активным в последний раз. Минимальное время на сервере
    /// (`MIN_ACTIVE_DWELL`) отсчитывается только после failover: у failback
    /// своя защита, штраф за мигание XR-082, и она требует немедленного
    /// отката, а не выдержки.
    became_active_by: Option<SwitchReason>,
    /// Сколько раз подряд failback на этот слот оказался преждевременным
    /// (тут же откат). Растит экспоненциальную задержку до следующего
    /// failback, сбрасывается устойчивой работой.
    flap_count: u32,
    /// До этого момента failback на слот подавлен (анти-флаппинг). `None`
    /// значит подавления нет.
    failback_suppressed_until: Option<Instant>,
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

    fn is_down(&self) -> bool {
        matches!(self.state.lock().unwrap().health, HealthState::Down { .. })
    }

    fn up_for(&self) -> Option<Duration> {
        self.state.lock().unwrap().up_since.map(|t| t.elapsed())
    }

    /// Отметить, что слот стал активным (вызывается из switch_active).
    fn note_became_active(&self, reason: SwitchReason) {
        let mut st = self.state.lock().unwrap();
        st.became_active_at = Some(Instant::now());
        st.became_active_by = Some(reason);
    }

    /// Слот стал активным по failover меньше `MIN_ACTIVE_DWELL` назад, и его
    /// отказ ещё не двигает активный (XR-296).
    fn in_failover_dwell(&self) -> bool {
        let st = self.state.lock().unwrap();
        matches!(
            st.became_active_by,
            Some(SwitchReason::Failover | SwitchReason::RelayDegraded)
        ) && st.became_active_at.is_some_and(|t| t.elapsed() < MIN_ACTIVE_DWELL)
    }

    /// Сколько слот пробыл активным с последнего переключения.
    fn active_for(&self) -> Option<Duration> {
        self.state.lock().unwrap().became_active_at.map(|t| t.elapsed())
    }

    /// failback на слот сейчас подавлен анти-флаппингом.
    fn failback_suppressed(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .failback_suppressed_until
            .is_some_and(|t| Instant::now() < t)
    }

    /// Слот мигнул (failback -> немедленный failover): растим счётчик и
    /// подавляем следующий failback на `hold * 2^flap` (с потолком).
    fn penalize_flap(&self, hold: Duration) {
        let mut st = self.state.lock().unwrap();
        st.flap_count = (st.flap_count + 1).min(FAILBACK_FLAP_CAP);
        let backoff = hold
            .saturating_mul(1u32 << st.flap_count)
            .min(FAILBACK_BACKOFF_MAX);
        st.failback_suppressed_until = Some(Instant::now() + backoff);
    }

    /// Слот устойчиво отработал активным: снимаем штраф за мигание.
    fn clear_flap(&self) {
        let mut st = self.state.lock().unwrap();
        st.flap_count = 0;
        st.failback_suppressed_until = None;
    }

    fn reset(&self) {
        let mut st = self.state.lock().unwrap();
        st.health = HealthState::Up;
        st.up_since = None;
        st.became_active_at = None;
        st.became_active_by = None;
        st.flap_count = 0;
        st.failback_suppressed_until = None;
    }
}

/// Почему сменился активный сервер. Причина едет в две ленты сразу, поэтому
/// хранится типом, а не строкой: в tracing уходит привычное разработчику
/// `failover`, в журнал приложения русская формулировка.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SwitchReason {
    /// Активный не отдал стрим (или не ответил в срок), ушли на резерв.
    Failover,
    /// Активный туннелирует, но живой трафик через него сыплется (XR-094).
    RelayDegraded,
    /// Более приоритетный сервер выздоровел и отстоял hold-down.
    Failback,
    /// Первый прогрев: активным становится самый приоритетный из живых.
    Warmup,
}

impl SwitchReason {
    fn dev(self) -> &'static str {
        match self {
            Self::Failover => "failover",
            Self::RelayDegraded => "failover (relay degraded)",
            Self::Failback => "failback",
            Self::Warmup => "warmup",
        }
    }

    fn user(self) -> &'static str {
        match self {
            Self::Failover => "сервер не ответил, перешёл на резерв",
            Self::RelayDegraded => "через сервер не идёт трафик, перешёл на резерв",
            Self::Failback => "основной сервер снова живой, вернулся на него",
            Self::Warmup => "выбран сервер",
        }
    }
}

/// Склонение слова «сервер» при числе: текст про исчерпанный пул читает
/// пользователь в журнале и на экране ошибки, а «все 2 серверов» там режет глаз.
fn servers_word(n: usize) -> &'static str {
    if (11..=14).contains(&(n % 100)) {
        return "серверов";
    }
    match n % 10 {
        1 => "сервер",
        2..=4 => "сервера",
        _ => "серверов",
    }
}

/// Клиентский пул серверов: primary/backup по приоритету, sticky-to-primary.
pub struct ServerPool {
    /// Отсортированы по приоритету, индекс 0 это primary.
    slots: Vec<ServerSlot>,
    active: AtomicUsize,
    profile: PoolProfile,
    on_event: Option<PoolEventFn>,
    /// Момент, с которого ОС хоста считает аплинк пропавшим (XR-296). Пока
    /// `Some`, пул заморожен: `open_stream` не обходит список и не двигает
    /// активный, `health_tick` молчит. Снимает признак хост же (`uplink_restored`)
    /// либо настоящая смена сети (`recycle`). Потолка у заморозки нет: без
    /// аплинка недостижимы все серверы разом, и перебор в это окно даёт только
    /// потраченные секунды и порванные сессии.
    uplink_down_since: Mutex<Option<Instant>>,
    /// Момент снятия заморозки (XR-296). Первые `MIN_ACTIVE_DWELL` после него
    /// отказ активного не двигает слот, стрим уходит соседу по обходу.
    resumed_at: Mutex<Option<Instant>>,
    /// Подмена в окне выдержки уже названа в журнале. Одна строка на окно:
    /// каждый стрим под breaker'ом активного заходит сюда за миллисекунды, и
    /// строка на каждый залила бы журнал. Сбрасывается сменой активного и
    /// снятием заморозки, то есть началом нового окна.
    substitute_noted: AtomicBool,
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
                state: Mutex::new(SlotState {
                    health: HealthState::Up,
                    up_since: None,
                    became_active_at: None,
                    became_active_by: None,
                    flap_count: 0,
                    failback_suppressed_until: None,
                }),
            })
            .collect();
        Arc::new(Self {
            slots,
            active: AtomicUsize::new(0),
            profile,
            on_event,
            uplink_down_since: Mutex::new(None),
            resumed_at: Mutex::new(None),
            substitute_noted: AtomicBool::new(false),
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

    /// Пул заморожен признаком «аплинка нет» от ОС хоста (XR-296).
    pub fn is_uplink_down(&self) -> bool {
        self.uplink_down_since.lock().unwrap().is_some()
    }

    /// ОС хоста сообщила, что физического аплинка не осталось (XR-296).
    /// Пул замирает на прежнем активном: без сети молчат все серверы разом, и
    /// их отказы ничего не говорят о здоровье. Повторный вызов без снятия
    /// признака ничего не меняет. Вход в заморозку виден строкой журнала.
    pub fn uplink_lost(&self) {
        let mut since = self.uplink_down_since.lock().unwrap();
        if since.is_some() {
            return;
        }
        *since = Some(Instant::now());
        drop(since);
        let label = self.slots[self.active_index()].label();
        self.emit(
            &format!("uplink lost, server pool frozen on {}", label),
            &format!("сети нет, жду её возврата на сервере {}, резервы не перебираю", label),
        );
    }

    /// Пропал и вернулся тот же аплинк (XR-296). Mux-соединения сидят на
    /// мёртвом интерфейсе и пересобираются, а знание о серверах остаётся:
    /// сеть та же самая, и устареть оно не успело. Здоровье, штраф за мигание
    /// и активный слот не трогаются, на primary никто не возвращается, для
    /// настоящей смены сети есть `recycle`. Заморозка снимается после
    /// пересборки, а не до: иначе первый же стрим в щели между ними упёрся бы
    /// во взведённый за окно breaker активного и уехал бы на резерв.
    /// Возвращает `false`, когда заморозки не было, тогда и пересборки нет.
    /// Выход виден строкой журнала с длительностью окна.
    pub async fn uplink_restored(&self) -> bool {
        if !self.is_uplink_down() {
            return false;
        }
        for slot in &self.slots {
            slot.pool.recycle().await;
        }
        self.unfreeze()
    }

    /// Аплинк вернулся меньше `MIN_ACTIVE_DWELL` назад, отказ активного ещё
    /// говорит о недоделанной сети, а не о сервере (XR-296).
    fn in_resume_grace(&self) -> bool {
        self.resumed_at
            .lock()
            .unwrap()
            .is_some_and(|t| t.elapsed() < MIN_ACTIVE_DWELL)
    }

    /// Отказ активного `start` сейчас не двигает слот: либо сам `start` стал
    /// активным по failover только что, либо только что вернулся аплинк.
    fn holds_active(&self, start: usize) -> bool {
        self.slots[start].in_failover_dwell() || self.in_resume_grace()
    }

    fn unfreeze(&self) -> bool {
        let Some(since) = self.uplink_down_since.lock().unwrap().take() else {
            return false;
        };
        *self.resumed_at.lock().unwrap() = Some(Instant::now());
        self.substitute_noted.store(false, Ordering::Relaxed);
        let label = self.slots[self.active_index()].label();
        let secs = since.elapsed().as_secs();
        self.emit(
            &format!("uplink restored after {}s, server pool resumes via {}", secs, label),
            &format!("сеть вернулась через {} с, продолжаю через {}", secs, label),
        );
        true
    }

    /// Событие пула уходит в два адреса, и текст у них разный: в tracing по-английски,
    /// его читает разработчик в логах роутера и сервера, а `on_event` ведёт в журнал
    /// приложения, где запись видит пользователь и стиль там русский (XR-089).
    fn emit(&self, dev: &str, user: &str) {
        tracing::info!("{}", dev);
        if let Some(cb) = &self.on_event {
            cb(user);
        }
    }

    /// Атомарно переключает активный сервер. CAS защищает от дублей при
    /// конкурентных failover'ах: лог и событие получает только победитель.
    fn switch_active(&self, from: usize, to: usize, reason: SwitchReason) -> bool {
        if from == to {
            return false;
        }
        let switched = self
            .active
            .compare_exchange(from, to, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok();
        if switched {
            self.slots[to].note_became_active(reason);
            self.substitute_noted.store(false, Ordering::Relaxed);
            let from_label = self.slots[from].label();
            let to_label = self.slots[to].label();
            self.emit(
                &format!("server {}: {} -> {}", reason.dev(), from_label, to_label),
                &format!("{}: {} -> {}", reason.user(), from_label, to_label),
            );
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

    /// Ошибка на исчерпание пула. Прежде наружу уходила ошибка последнего
    /// сервера по порядку обхода, и при поголовной недоступности (оператор
    /// режет все VPS разом) виноватым названным оказывался случайный резерв,
    /// а отказ primary пропадал вместе с диагнозом (XR-131). Теперь ошибка
    /// перечисляет все серверы в порядке приоритета, primary первым, и
    /// сразу говорит, что легли все.
    ///
    /// Отказ одного сервера пересказывать незачем, он возвращается как есть.
    fn pool_exhausted(&self, mut failures: Vec<(usize, io::Error)>) -> io::Error {
        if failures.len() <= 1 {
            return failures.pop().map(|(_, e)| e).unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "no servers in pool")
            });
        }
        failures.sort_by_key(|(idx, _)| *idx);
        // Вид ошибки берём у самого приоритетного: агрегат обязан отвечать за
        // primary, а не за случайный резерв. По виду движок выбирает уровень
        // записи в журнале (классификация в engine.rs), уход в Direct от него
        // не зависит, там решает политика on_server_down.
        let kind = failures[0].1.kind();
        let details = failures
            .iter()
            .map(|(idx, e)| format!("{} ({})", self.slots[*idx].label(), e))
            .collect::<Vec<_>>()
            .join(", ");
        let n = failures.len();
        io::Error::new(
            kind,
            format!("все {} {} недоступны: {}", n, servers_word(n), details),
        )
    }

    /// Открыть логический стрим через активный сервер, при его отказе через
    /// следующий по приоритету здоровый (тот становится активным).
    /// `Err` значит, что исчерпан весь пул; вызывающий уводит соединение
    /// в Direct.
    pub async fn open_stream(&self, target: &TargetAddr) -> io::Result<MuxStream> {
        let start = self.active_index();
        if self.is_uplink_down() {
            return self.open_stream_frozen(start, target).await;
        }
        let mut failures: Vec<(usize, io::Error)> = Vec::new();

        for idx in self.walk_order(start) {
            // Ограничиваем ожидание каждого сервера: молчащий primary иначе
            // держал бы нас десятки секунд (см. PER_SERVER_OPEN_TIMEOUT), и
            // соединение утекло бы в Direct вместо живого резерва.
            let outcome = tokio::time::timeout(
                PER_SERVER_OPEN_TIMEOUT,
                self.slots[idx].pool.open_stream(target),
            )
            .await;
            match outcome {
                Ok(Ok(stream)) => {
                    self.slots[idx].mark_up();
                    if idx != start && self.holds_active(start) {
                        // Активный стал таким по failover только что либо
                        // только что вернулся аплинк. Стрим уходит живому
                        // соседу, а активный остаётся: иначе на частично живой
                        // связи каждый стрим катал бы активный между серверами
                        // (XR-296). Следующие стримы упрутся в breaker `start`
                        // и дойдут сюда за миллисекунды. Статусная строка в
                        // эти секунды показывает активный, а не сосед, поэтому
                        // подмена названа в журнале, один раз на окно.
                        let start_label = self.slots[start].label();
                        let via_label = self.slots[idx].label();
                        if !self.substitute_noted.swap(true, Ordering::Relaxed) {
                            self.emit(
                                &format!(
                                    "server {} failed within the dwell, streams go via {} until it ends",
                                    start_label, via_label
                                ),
                                &format!(
                                    "сервер {} не ответил, стримы идут через {}, активный не меняю до конца выдержки",
                                    start_label, via_label
                                ),
                            );
                        } else {
                            tracing::debug!(
                                "server {} failed within the dwell, serving via {} without switching",
                                start_label, via_label
                            );
                        }
                    } else if idx != start {
                        // Ушли с активного `start` на другой сервер. Если `start`
                        // только что стал активным (недавний failback) и тут же
                        // не смог отдать стрим, это преждевременный failback:
                        // штрафуем его, чтобы health_loop не возвращался на него
                        // сразу (XR-082).
                        if self.slots[start].active_for().is_some_and(|d| d < FAILBACK_FLAP_WINDOW) {
                            self.slots[start].penalize_flap(self.profile.failback_hold);
                        }
                        self.switch_active(start, idx, SwitchReason::Failover);
                    } else if self.slots[start].active_for().is_some_and(|d| d >= FAILBACK_FLAP_WINDOW) {
                        // Активный устойчиво отдаёт реальный трафик дольше окна
                        // мигания: снимаем накопленный штраф, будущий failback
                        // на него снова быстрый.
                        self.slots[start].clear_flap();
                    }
                    return Ok(stream);
                }
                Ok(Err(e)) if e.kind() == io::ErrorKind::InvalidInput => {
                    // Ошибка самого запроса, а не транспорта (некодируемый
                    // адрес, XR-205). Обходить пул бессмысленно, все ответят
                    // тем же, а здоровье серверов от этого страдать не должно:
                    // иначе одно кривое имя из LAN гасит весь пул разом.
                    tracing::debug!("open_stream rejected the target ({}), no failover", e);
                    return Err(e);
                }
                Ok(Err(e)) => {
                    self.slots[idx].mark_down();
                    tracing::debug!(
                        "server {} unavailable ({}), trying next",
                        self.slots[idx].label(),
                        e
                    );
                    failures.push((idx, e));
                }
                Err(_) => {
                    self.slots[idx].mark_down();
                    tracing::debug!(
                        "server {} did not answer in {:?}, trying next",
                        self.slots[idx].label(),
                        PER_SERVER_OPEN_TIMEOUT
                    );
                    // Имя сервера ставит агрегатор, а вот маркер "open timed
                    // out" в тексте оставляем: по нему ищут корень зависаний в
                    // логах роутера (XR-086, tools/loadtest/README.md), туда
                    // текст уезжает целиком через warn клиента.
                    failures.push((
                        idx,
                        io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("open timed out за {:?}", PER_SERVER_OPEN_TIMEOUT),
                        ),
                    ));
                }
            }
        }

        Err(self.pool_exhausted(failures))
    }

    /// `open_stream` под заморозкой (XR-296): одна попытка через прежний
    /// активный, без обхода списка, без смены активного и без отметок
    /// здоровья. Попытка остаётся, а не отказ на месте: признак от ОС может
    /// отстать от вернувшейся связи на мгновение, и тогда стрим всё же
    /// уходит, а устаревший признак вырождается в «без failover», а не в
    /// «без сети». Отказ уходит вызывающему как есть, breaker mux-пула на
    /// время окна взводится и снимается пересборкой при возврате аплинка.
    async fn open_stream_frozen(&self, idx: usize, target: &TargetAddr) -> io::Result<MuxStream> {
        let slot = &self.slots[idx];
        match tokio::time::timeout(PER_SERVER_OPEN_TIMEOUT, slot.pool.open_stream(target)).await {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(e)) => {
                tracing::debug!("uplink is down, {} refused a stream ({}), no failover", slot.label(), e);
                Err(e)
            }
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("open timed out за {:?} (аплинка нет)", PER_SERVER_OPEN_TIMEOUT),
            )),
        }
    }

    /// Прогрев при старте / после смены сети. Тёплый профиль поднимает mux ко
    /// всем серверам параллельно, холодный только к активному (при его отказе
    /// к следующему по приоритету). `Ok` значит, что хотя бы один сервер
    /// отвечает.
    pub async fn warmup(&self) -> io::Result<()> {
        if self.is_uplink_down() {
            // Без аплинка прогрев это та же попытка через прежний активный:
            // обход списка под заморозкой ничего не проверяет (XR-296).
            let slot = &self.slots[self.active_index()];
            return slot.pool.warmup().await;
        }
        if self.profile.warm_backups {
            let mut handles = Vec::with_capacity(self.slots.len());
            for (idx, slot) in self.slots.iter().enumerate() {
                let pool = slot.pool.clone();
                handles.push(tokio::spawn(async move { (idx, pool.warmup().await) }));
            }
            let mut best: Option<usize> = None;
            let mut failures: Vec<(usize, io::Error)> = Vec::new();
            for h in handles {
                let Ok((idx, res)) = h.await else { continue };
                match res {
                    Ok(()) => {
                        self.slots[idx].mark_up();
                        best = Some(best.map_or(idx, |b| b.min(idx)));
                    }
                    Err(e) => {
                        self.slots[idx].mark_down();
                        failures.push((idx, e));
                    }
                }
            }
            match best {
                Some(idx) => {
                    // На старте/после re-bind липкость не действует: активным
                    // сразу становится самый приоритетный из живых.
                    let cur = self.active.load(Ordering::Relaxed);
                    self.switch_active(cur, idx, SwitchReason::Warmup);
                    Ok(())
                }
                None => Err(self.pool_exhausted(failures)),
            }
        } else {
            let start = self.active_index();
            let mut failures: Vec<(usize, io::Error)> = Vec::new();
            for idx in self.walk_order(start) {
                match self.slots[idx].pool.warmup().await {
                    Ok(()) => {
                        self.slots[idx].mark_up();
                        // Свежий после failover или после возврата аплинка
                        // активный остаётся на месте, как в open_stream (XR-296).
                        if idx != start && !self.holds_active(start) {
                            self.switch_active(start, idx, SwitchReason::Failover);
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        self.slots[idx].mark_down();
                        failures.push((idx, e));
                    }
                }
            }
            Err(self.pool_exhausted(failures))
        }
    }

    /// Сетевой re-bind (C4, Android LTE<->Wi-Fi): пересоздать все mux-пулы и
    /// вернуть активного на primary. Знание о здоровье, накопленное на
    /// прежней сети, устарело вместе с ней. Это путь настоящей смены сети;
    /// возврат того же аплинка после пропажи идёт через `uplink_restored`
    /// (XR-296). Заморозку по аплинку смена сети снимает сама: новая сеть это
    /// и есть аплинк, и застрявший признак ей не указ.
    pub async fn recycle(&self) {
        self.unfreeze();
        *self.resumed_at.lock().unwrap() = None;
        for slot in &self.slots {
            slot.pool.recycle().await;
            slot.reset();
        }
        let cur = self.active.swap(0, Ordering::Relaxed);
        if cur != 0 {
            let label = self.slots[0].label();
            self.emit(
                &format!("server pool recycled, active reset to {}", label),
                &format!(
                    "пул серверов пересобран после смены сети, снова работаю через {}",
                    label,
                ),
            );
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
        if self.is_uplink_down() {
            // Без аплинка проба ложится на любой сервер, а relay-счётчики
            // говорят о сети, не о сервере: тик молчит целиком (XR-296).
            return;
        }
        self.relay_degradation_tick();

        let active = self.active_index();

        // Тёплый профиль держит резервы поднятыми: warmup наполняет пустые
        // слоты один раз, дальше mux сам себя keepalive'ит, поэтому в норме это
        // no-op без лишних коннектов. Результат warmup для здоровья НЕ берём: на
        // blackhole'нутом сервере он вернёт Ok по «живым» (но глухим) mux.
        // Реальное здоровье считает только probe_fresh ниже.
        if self.profile.warm_backups {
            for slot in &self.slots {
                let _ = tokio::time::timeout(PROBE_TIMEOUT, slot.pool.warmup()).await;
            }
        }

        // Пробим кандидатов на failback (приоритетнее активного) реальным
        // round-trip'ом: свежий handshake ловит blackhole (mux ещё is_alive, но
        // глотает egress) за таймаут коннекта, а не читает мёртвый primary
        // «живым» минутами. В норме (active == primary) кандидатов нет, поэтому
        // ни одной пробы: пул не шлёт лишних коннектов на VPS. Failover активного
        // ловит ограниченный по времени open_stream по реальному трафику (§2.7).
        for idx in 0..active {
            match tokio::time::timeout(PROBE_TIMEOUT, self.slots[idx].pool.probe_fresh()).await {
                Ok(Ok(())) => self.slots[idx].mark_up(),
                Ok(Err(_)) | Err(_) => self.slots[idx].mark_down(),
            }
        }

        // Failback: самый приоритетный сервер, непрерывно живой не меньше
        // hold-down, забирает активность. Мигание primary в течение окна
        // сбрасывает up_since (mark_down выше), и таймер начинается заново.
        // Слот под анти-флаппинг-штрафом (недавно мигнул) пропускаем, пока штраф
        // не истечёт: иначе на прерывистом сервере проба-пролаз даёт бесконечный
        // цикл failback -> failover каждые ~hold секунд (XR-082).
        let active = self.active_index();
        for idx in 0..active {
            if self.slots[idx].failback_suppressed() {
                continue;
            }
            if let Some(up) = self.slots[idx].up_for() {
                if up >= self.profile.failback_hold {
                    self.switch_active(active, idx, SwitchReason::Failback);
                    break;
                }
            }
        }
    }

    /// Failover по деградации relay (XR-094): у сервера с мёртвым DNS или
    /// egress туннель и keepalive живы, ConnectAck приходит, поэтому ни
    /// breaker, ни dead-link-детект, ни ограниченный по времени open_stream
    /// не срабатывают, но каждый Connect кончается relay error и полезная
    /// работа нулевая. Здесь такой активный сервер помечается Down и
    /// активность уезжает на следующий живой. Возврат идёт обычным failback
    /// с hold-down; если сервер «мигает» (failback -> тут же снова
    /// деградация), включается тот же анти-флаппинг-штраф, что в XR-082.
    fn relay_degradation_tick(&self) {
        let active = self.active_index();
        if self.slots.len() < 2 || !self.slots[active].pool.relay_degraded() {
            return;
        }
        let Some(to) = self.walk_order(active).skip(1).find(|&i| !self.slots[i].is_down())
        else {
            // Уходить некуда (остальные и так Down): продолжаем хромать на
            // деградировавшем, он хотя бы туннелирует IP-таргеты.
            return;
        };
        self.slots[active].mark_down();
        if self.slots[active].active_for().is_some_and(|d| d < FAILBACK_FLAP_WINDOW) {
            self.slots[active].penalize_flap(self.profile.failback_hold);
        }
        // Окно счётчиков чистим: после failback деградацию должен подтвердить
        // свежий трафик, а не хвост старых сбоев.
        self.slots[active].pool.relay_health().reset();
        self.switch_active(active, to, SwitchReason::RelayDegraded);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::{mux_handshake_server, Multiplexer};
    use crate::mux_pool::ConnectFn;
    use crate::obfuscation::{ModifierStrategy, Obfuscator};
    use crate::protocol::{Codec, Command, MAX_DOMAIN_LEN};
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
                    let Ok(Some(caps)) = mux_handshake_server(&mut sock, &codec, &init).await
                    else {
                        return;
                    };
                    let mux = Multiplexer::new_server(sock, codec.clone(), caps);
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

    /// Сервер «с мёртвым DNS» (инцидент XR-094): mux-handshake и ConnectAck
    /// работают, но каждый стрим тут же закрывается причиной resolve-сбоя,
    /// как настоящий mux_handler, у которого упал резолвер.
    async fn spawn_relay_failing_server() -> SocketAddr {
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
                    let Ok(Some(caps)) = mux_handshake_server(&mut sock, &codec, &init).await
                    else {
                        return;
                    };
                    let mux = Multiplexer::new_server(sock, codec.clone(), caps);
                    let Some(mut rx) = mux.take_new_stream_rx().await else { return };
                    while let Some(ns) = rx.recv().await {
                        let _ = mux
                            .send_frame(ns.stream_id, Command::ConnectAck, vec![0])
                            .await;
                        let _ = mux
                            .send_frame(
                                ns.stream_id,
                                Command::Close,
                                vec![crate::protocol::CLOSE_REASON_RESOLVE_FAIL],
                            )
                            .await;
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

    /// Отказ со своей причиной у каждого сервера: по ней видно, чей отказ
    /// доехал до текста ошибки пула.
    fn failing_connect_msg(msg: &'static str) -> ConnectFn {
        failing_connect_kind(io::ErrorKind::ConnectionRefused, msg)
    }

    fn failing_connect_kind(kind: io::ErrorKind, msg: &'static str) -> ConnectFn {
        Arc::new(move || Box::pin(async move { Err(io::Error::new(kind, msg)) }))
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

    /// Молчащий primary (blackhole): TCP-коннект зависает без ответа и без RST.
    /// Без границы времени `open_stream` висел бы на нём десятки секунд и увёл бы
    /// соединение в Direct. Проверяем, что пул отдаёт стрим *резерва* (а не Err),
    /// то есть трафик уходит на backup, а не мимо прокси. Тест реально ждёт
    /// `PER_SERVER_OPEN_TIMEOUT` (детерминированно), успех-путь на живом IO
    /// backup'а несовместим с `start_paused` (авто-проматывание клока срубило бы
    /// и его таймаут).
    #[tokio::test]
    async fn test_silent_primary_bounds_and_fails_over_to_backup() {
        let backup_addr = spawn_test_server().await;
        // primary «съедает» коннект: SYN ушёл, ответа нет, RST нет.
        let hang: ConnectFn =
            Arc::new(|| Box::pin(std::future::pending::<io::Result<TcpStream>>()));

        let pool = ServerPool::new(
            vec![
                slot("primary", hang),
                slot("backup", connect_to(backup_addr, Arc::new(AtomicU32::new(0)))),
            ],
            PoolProfile::router(),
            None,
        );

        let stream = pool
            .open_stream(&target())
            .await
            .expect("backup must serve when primary is silent (no Direct)");
        assert!(stream.is_alive());
        assert_eq!(pool.active_index(), 1, "active must move to the backup");
        assert!(pool.is_backup_active());
    }

    /// Некодируемый адрес это ошибка ввода, а не отказ сервера. Пул обязан
    /// вернуть её сразу и оставить серверы здоровыми, иначе одно длинное имя
    /// из LAN гасит весь пул разом (XR-205).
    #[tokio::test]
    async fn test_bad_target_keeps_servers_healthy() {
        let primary_addr = spawn_test_server().await;
        let backup_addr = spawn_test_server().await;

        let pool = ServerPool::new(
            vec![
                slot("primary", connect_to(primary_addr, Arc::new(AtomicU32::new(0)))),
                slot("backup", connect_to(backup_addr, Arc::new(AtomicU32::new(0)))),
            ],
            PoolProfile::mobile(),
            None,
        );

        let bad = TargetAddr::Domain("a".repeat(MAX_DOMAIN_LEN + 1), 443);
        let err = pool.open_stream(&bad).await.expect_err("bad target must not open");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!pool.slots[0].is_down(), "primary must stay healthy");
        assert!(!pool.slots[1].is_down(), "backup must stay healthy");
        assert_eq!(pool.active_index(), 0, "active must not move");

        // Нормальный адрес после этого обслуживается как ни в чём не бывало.
        let stream = pool.open_stream(&target()).await.expect("pool still serves");
        assert!(stream.is_alive());
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

    /// Пул лёг целиком (мобильный оператор режет все VPS разом). Ошибка обязана
    /// сказать, что недоступны все, и назвать каждый сервер со своей причиной,
    /// primary в том числе: раньше наружу уезжал отказ последнего по обходу, и
    /// диагностика упиралась в случайный резерв (XR-131).
    #[tokio::test]
    async fn test_exhausted_pool_names_all_servers() {
        let pool = ServerPool::new(
            vec![
                slot("msk", failing_connect_msg("primary refused")),
                slot("fra", failing_connect_msg("first backup refused")),
                slot(
                    "aeza",
                    failing_connect_kind(io::ErrorKind::NotConnected, "last backup refused"),
                ),
            ],
            PoolProfile::mobile(),
            None,
        );

        let err = pool.open_stream(&target()).await.unwrap_err();
        let text = err.to_string();
        assert!(text.contains("все 3 сервера недоступны"), "{}", text);
        for (label, cause) in [
            ("msk", "primary refused"),
            ("fra", "first backup refused"),
            ("aeza", "last backup refused"),
        ] {
            assert!(text.contains(label), "нет сервера {} в «{}»", label, text);
            assert!(text.contains(cause), "нет причины {} в «{}»", cause, text);
        }
        // Вид ошибки достаётся от primary, а не от последнего резерва: по виду
        // движок выбирает уровень записи в журнале.
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    /// Обход начинается с активного, поэтому при работе через резерв primary
    /// попадает в список не первым. В тексте порядок обязан быть приоритетным:
    /// пользователь читает его сверху вниз и ждёт основной сервер первым.
    #[tokio::test]
    async fn test_exhausted_pool_lists_primary_first_from_backup() {
        let pool = ServerPool::new(
            vec![
                slot("msk", failing_connect_msg("primary refused")),
                slot("aeza", failing_connect_msg("backup refused")),
            ],
            PoolProfile::mobile(),
            None,
        );
        // Работаем через резерв: обход пойдёт aeza -> msk.
        pool.active.store(1, Ordering::Relaxed);

        let text = pool.open_stream(&target()).await.unwrap_err().to_string();
        let msk = text.find("msk").unwrap_or_else(|| panic!("нет primary в «{}»", text));
        let aeza = text.find("aeza").unwrap_or_else(|| panic!("нет резерва в «{}»", text));
        assert!(msk < aeza, "primary обязан идти первым: «{}»", text);
    }

    /// Молчащий пул: ни один сервер не отвечает и каждый упирается в свою
    /// границу ожидания. Такой отказ тоже обязан называть всех, а не последнего,
    /// и сохранять маркер "open timed out", по которому зависания ищут в логах
    /// роутера (XR-086).
    #[tokio::test(start_paused = true)]
    async fn test_silent_pool_names_all_servers() {
        let hang: ConnectFn =
            Arc::new(|| Box::pin(std::future::pending::<io::Result<TcpStream>>()));
        let pool = ServerPool::new(
            vec![slot("msk", hang.clone()), slot("aeza", hang)],
            PoolProfile::mobile(),
            None,
        );

        let err = pool.open_stream(&target()).await.unwrap_err();
        let text = err.to_string();
        assert!(text.contains("все 2 сервера недоступны"), "{}", text);
        assert!(text.contains("msk") && text.contains("aeza"), "{}", text);
        assert_eq!(text.matches("open timed out").count(), 2, "{}", text);
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    /// Тот же диагноз на холодном прогреве (телефон, старт и смена сети):
    /// именно этот путь питает «Сервер недоступен» на экране.
    #[tokio::test]
    async fn test_cold_warmup_exhausted_names_all_servers() {
        let pool = ServerPool::new(
            vec![
                slot("msk", failing_connect_msg("primary refused")),
                slot("aeza", failing_connect_msg("backup refused")),
            ],
            PoolProfile::mobile(),
            None,
        );

        let text = pool.warmup().await.unwrap_err().to_string();
        assert!(text.contains("все 2 сервера недоступны"), "{}", text);
        assert!(text.contains("msk") && text.contains("primary refused"), "{}", text);
        assert!(text.contains("aeza"), "{}", text);
    }

    /// Тёплый прогрев (роутер) ходит ко всем серверам параллельно, и порядок
    /// завершения проб ничего не значит: отказ primary в тексте обязателен.
    #[tokio::test]
    async fn test_warm_warmup_exhausted_keeps_primary() {
        let pool = ServerPool::new(
            vec![
                slot("msk", failing_connect_msg("primary refused")),
                slot("fra", failing_connect_msg("first backup refused")),
                slot("aeza", failing_connect_msg("last backup refused")),
            ],
            PoolProfile::router(),
            None,
        );

        let text = pool.warmup().await.unwrap_err().to_string();
        assert!(text.contains("все 3 сервера недоступны"), "{}", text);
        assert!(text.contains("msk") && text.contains("primary refused"), "{}", text);
        assert!(text.contains("aeza"), "{}", text);
    }

    /// Единственный сервер пересказывать нечем: его отказ уходит наружу как
    /// есть, и «Сервер недоступен: ...» на экране остаётся прежним.
    #[tokio::test]
    async fn test_single_server_error_passes_through() {
        let pool = ServerPool::new(
            vec![slot("msk", failing_connect_msg("primary refused"))],
            PoolProfile::mobile(),
            None,
        );

        let err = pool.warmup().await.unwrap_err();
        assert_eq!(err.to_string(), "primary refused");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[test]
    fn test_servers_word_declension() {
        assert_eq!(servers_word(1), "сервер");
        assert_eq!(servers_word(2), "сервера");
        assert_eq!(servers_word(4), "сервера");
        assert_eq!(servers_word(5), "серверов");
        assert_eq!(servers_word(11), "серверов");
        assert_eq!(servers_word(21), "сервер");
        assert_eq!(servers_word(112), "серверов");
    }

    #[tokio::test]
    async fn test_failback_after_holddown() {
        let server_addr = spawn_test_server().await;
        let primary_dead = Arc::new(AtomicBool::new(true));

        // Запас hold-down с большой форой над задержкой реальной пробы
        // (`probe_fresh` теперь делает настоящий localhost-коннект на тик, а не
        // мгновенную проверку `is_alive`), иначе тест ловил бы ложный failback.
        let profile = PoolProfile {
            warm_backups: false,
            probe_interval: Duration::from_millis(10),
            failback_hold: Duration::from_millis(300),
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
        tokio::time::sleep(Duration::from_millis(350)).await;
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
        // Формулировка проверяется вместе с фактом события: `on_event` ведёт в
        // журнал приложения, и запись там русская, как у остальных источников
        // (XR-089). Английское `server failover: ...` осталось только в tracing.
        assert!(
            log.iter().any(|m| {
                m.starts_with("сервер не ответил, перешёл на резерв") && m.contains("timeweb")
            }),
            "failover must be reported to the host log, got: {:?}",
            *log
        );
    }

    /// Регрессия XR-094 (инцидент 2026-07-10): у сервера с мёртвым DNS туннель
    /// и ConnectAck живы, но каждый relay кончается resolve-ошибкой, и обычный
    /// failover (по ошибке/таймауту open_stream) не срабатывает. Пул обязан
    /// увести активность на резерв по статистике relay-сбоев из live-трафика.
    /// На коде без учёта relay-исходов health_tick оставляет active=primary.
    #[tokio::test]
    async fn test_relay_degraded_active_fails_over_to_backup() {
        let dns_dead = spawn_relay_failing_server().await;
        let healthy = spawn_test_server().await;
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let events_cb = events.clone();
        let pool = ServerPool::new(
            vec![
                slot("dns-dead", connect_to(dns_dead, Arc::new(AtomicU32::new(0)))),
                slot("backup", connect_to(healthy, Arc::new(AtomicU32::new(0)))),
            ],
            PoolProfile::mobile(),
            Some(Arc::new(move |msg: &str| {
                events_cb.lock().unwrap().push(msg.to_string());
            })),
        );

        // Живой трафик: открытия «успешны» (ConnectAck приходит), но каждый
        // стрим закрывается relay-ошибкой без единого байта данных.
        for _ in 0..8 {
            let mut s = pool
                .open_stream(&target())
                .await
                .expect("open проходит, падает только relay");
            assert!(
                s.recv().await.is_none(),
                "данных нет, сервер закрыл стрим relay-ошибкой"
            );
        }
        assert_eq!(
            pool.active_index(),
            0,
            "сам по себе open_stream деградацию не ловит (в этом и дыра)"
        );

        pool.health_tick().await;
        assert_eq!(
            pool.active_index(),
            1,
            "health_tick обязан увести трафик с деградировавшего по relay primary"
        );
        assert!(pool.is_backup_active());
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|m| m.contains("не идёт трафик") && m.contains("backup")),
            "failover по деградации должен уйти событием в журнал"
        );
    }

    /// XR-094 + XR-082: деградация relay сразу после failback (primary «мигает»
    /// живым туннелем при мёртвом relay) получает тот же анти-флаппинг-штраф,
    /// что и обычное мигание, чтобы качели failback -> деградация -> failover
    /// замедлялись экспоненциально.
    #[tokio::test]
    async fn test_relay_degradation_after_failback_is_penalized() {
        let addr = spawn_test_server().await;
        let pool = ServerPool::new(
            vec![
                slot("primary", connect_to(addr, Arc::new(AtomicU32::new(0)))),
                slot("backup", connect_to(addr, Arc::new(AtomicU32::new(0)))),
            ],
            PoolProfile::mobile(),
            None,
        );
        // Свежий failback: primary только что стал активным.
        pool.slots[0].note_became_active(SwitchReason::Failback);
        // Relay-сбои по live-трафику (так их записал бы mux из Close с причиной).
        for _ in 0..10 {
            pool.slots[0].pool.relay_health().record_resolve_fail();
        }

        pool.health_tick().await;
        assert_eq!(pool.active_index(), 1);
        assert!(
            pool.slots[0].failback_suppressed(),
            "деградировавший сразу после failback слот попадает под штраф"
        );
    }

    /// Единственному серверу деградация relay ничего не меняет: уходить
    /// некуда, он продолжает хотя бы туннелировать IP-таргеты.
    #[tokio::test]
    async fn test_relay_degradation_single_server_stays() {
        let addr = spawn_test_server().await;
        let pool = ServerPool::new(
            vec![slot("only", connect_to(addr, Arc::new(AtomicU32::new(0))))],
            PoolProfile::mobile(),
            None,
        );
        for _ in 0..10 {
            pool.slots[0].pool.relay_health().record_resolve_fail();
        }
        pool.health_tick().await;
        assert_eq!(pool.active_index(), 0);
        assert_eq!(
            pool.server_health(0),
            Some(HealthState::Up),
            "единственный сервер не помечается Down: уходить некуда"
        );
    }

    /// Когда все резервы уже Down, деградировавший активный остаётся на месте
    /// (хромать лучше, чем переключиться на заведомо мёртвое).
    #[tokio::test]
    async fn test_relay_degradation_all_backups_down_stays() {
        let addr = spawn_test_server().await;
        let pool = ServerPool::new(
            vec![
                slot("primary", connect_to(addr, Arc::new(AtomicU32::new(0)))),
                slot("backup", connect_to(addr, Arc::new(AtomicU32::new(0)))),
            ],
            PoolProfile::mobile(),
            None,
        );
        pool.slots[1].mark_down();
        for _ in 0..10 {
            pool.slots[0].pool.relay_health().record_resolve_fail();
        }
        pool.health_tick().await;
        assert_eq!(pool.active_index(), 0, "уходить некуда, активный остаётся");
    }

    /// XR-082: если только что сделанный failback тут же откатывается реальным
    /// failover'ом (primary «мигает»), слот получает анти-флаппинг-штраф.
    #[tokio::test]
    async fn test_premature_failback_is_penalized() {
        let backup_addr = spawn_test_server().await;
        let pool = ServerPool::new(
            vec![
                slot("primary", failing_connect(Arc::new(AtomicU32::new(0)))),
                slot("backup", connect_to(backup_addr, Arc::new(AtomicU32::new(0)))),
            ],
            PoolProfile::router(),
            None,
        );
        // Симулируем свежий failback: primary только что стал активным.
        pool.slots[0].note_became_active(SwitchReason::Failback);
        assert!(!pool.slots[0].failback_suppressed());

        // Реальный open с active=primary: primary падает, уходим на backup.
        let _ = pool.open_stream(&target()).await.expect("backup serves");
        assert_eq!(pool.active_index(), 1);
        assert!(
            pool.slots[0].failback_suppressed(),
            "a primary that fails right after failback must be suppressed"
        );
    }

    /// XR-082: пока слот под штрафом, health_loop НЕ делает failback на него,
    /// даже когда проба держит его «живым» дольше hold-down. После снятия
    /// штрафа failback возобновляется.
    #[tokio::test]
    async fn test_suppressed_slot_skips_failback() {
        let addr = spawn_test_server().await;
        let profile = PoolProfile {
            warm_backups: false,
            probe_interval: Duration::from_millis(10),
            failback_hold: Duration::from_millis(50),
        };
        let pool = ServerPool::new(
            vec![
                slot("primary", connect_to(addr, Arc::new(AtomicU32::new(0)))),
                slot("backup", connect_to(addr, Arc::new(AtomicU32::new(0)))),
            ],
            profile,
            None,
        );

        // Уводим активность на backup и штрафуем primary (как будто мигнул).
        pool.switch_active(0, 1, SwitchReason::Failover);
        assert_eq!(pool.active_index(), 1);
        pool.slots[0].penalize_flap(Duration::from_secs(3600));

        // primary здоров для проб (реальный сервер), но failback подавлен.
        for _ in 0..5 {
            pool.health_tick().await;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert_eq!(
            pool.active_index(),
            1,
            "suppressed primary must not steal active back"
        );

        // Штраф снят -> failback снова разрешён.
        pool.slots[0].clear_flap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        pool.health_tick().await;
        assert_eq!(
            pool.active_index(),
            0,
            "after the penalty clears, failback resumes"
        );
    }

    fn collecting_events() -> (PoolEventFn, Arc<Mutex<Vec<String>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let cb: PoolEventFn = Arc::new(move |s: &str| sink.lock().unwrap().push(s.to_string()));
        (cb, events)
    }

    /// XR-296: признак «аплинка нет» от ОС замораживает пул. Отказ активного
    /// под заморозкой не двигает активный, не трогает здоровье и не обходит
    /// резерв, а после возврата аплинка работа продолжается с прежнего сервера.
    /// Вход и выход видны строками журнала.
    #[tokio::test]
    async fn test_uplink_down_freezes_pool_and_resumes_on_same_server() {
        let addr = spawn_test_server().await;
        let primary_dead = Arc::new(AtomicBool::new(false));
        let backup_calls = Arc::new(AtomicU32::new(0));
        let (cb, events) = collecting_events();
        let pool = ServerPool::new(
            vec![
                slot("primary", switchable_connect(addr, primary_dead.clone())),
                slot("backup", connect_to(addr, backup_calls.clone())),
            ],
            PoolProfile::mobile(),
            Some(cb),
        );
        let _ = pool.open_stream(&target()).await.expect("primary serves");
        assert_eq!(pool.active_index(), 0);

        // Радио пропало: ОС сообщила раньше, чем упал первый стрим, а сокеты
        // живого mux на мёртвом интерфейсе рвутся вместе с ним.
        pool.uplink_lost();
        pool.uplink_lost();
        primary_dead.store(true, Ordering::Relaxed);
        pool.slots[0].pool.recycle().await;
        for _ in 0..3 {
            let err = pool.open_stream(&target()).await.err();
            assert!(err.is_some(), "frozen pool must not serve via the backup");
        }
        assert_eq!(pool.active_index(), 0, "frozen pool must not move the active slot");
        assert_eq!(backup_calls.load(Ordering::Relaxed), 0, "frozen pool must not walk the list");
        assert_eq!(
            pool.server_health(0),
            Some(HealthState::Up),
            "a failure without uplink says nothing about the server"
        );
        // Пробер тоже молчит: живой резерв не забирает активность.
        pool.health_tick().await;
        assert_eq!(pool.active_index(), 0);

        // Радио вернулось: тот же сервер, без обхода и без смены активного.
        primary_dead.store(false, Ordering::Relaxed);
        assert!(pool.uplink_restored().await);
        assert!(!pool.uplink_restored().await, "second restore is a no-op");
        let stream = pool.open_stream(&target()).await.expect("primary serves again");
        assert!(stream.is_alive());
        assert_eq!(pool.active_index(), 0);
        assert_eq!(backup_calls.load(Ordering::Relaxed), 0);

        let events = events.lock().unwrap();
        assert!(
            events.iter().any(|e| e.contains("сети нет") && e.contains("primary")),
            "freeze entry must be logged: {:?}",
            events
        );
        assert_eq!(
            events.iter().filter(|e| e.contains("сети нет")).count(),
            1,
            "repeated uplink_lost must not spam the journal"
        );
        assert!(
            events.iter().any(|e| e.contains("сеть вернулась") && e.contains("primary")),
            "freeze exit must be logged: {:?}",
            events
        );
    }

    /// XR-296: под заморозкой прогрев тоже не переводит активный на резерв.
    #[tokio::test]
    async fn test_uplink_down_warmup_keeps_active() {
        let addr = spawn_test_server().await;
        let pool = ServerPool::new(
            vec![
                slot("primary", failing_connect_msg("no route")),
                slot("backup", connect_to(addr, Arc::new(AtomicU32::new(0)))),
            ],
            PoolProfile::mobile(),
            None,
        );
        pool.uplink_lost();
        assert!(pool.warmup().await.is_err(), "frozen warmup tries only the active");
        assert_eq!(pool.active_index(), 0);
    }

    /// XR-296: после failover резерв держит активность минимальное время.
    /// Его отказ в этот срок отдаёт стрим соседу, но активный не двигает;
    /// по истечении срока отказ снова переключает как раньше.
    #[tokio::test]
    async fn test_failover_dwell_keeps_active_on_fresh_backup() {
        let addr = spawn_test_server().await;
        let primary_dead = Arc::new(AtomicBool::new(true));
        let backup_dead = Arc::new(AtomicBool::new(false));
        let pool = ServerPool::new(
            vec![
                slot("primary", switchable_connect(addr, primary_dead.clone())),
                slot("backup", switchable_connect(addr, backup_dead.clone())),
            ],
            PoolProfile::mobile(),
            None,
        );
        // Первый уход в резерв без задержки.
        let _ = pool.open_stream(&target()).await.expect("backup serves");
        assert_eq!(pool.active_index(), 1);

        // Роли поменялись сразу после failover: primary ожил, backup лёг
        // вместе со своим mux.
        primary_dead.store(false, Ordering::Relaxed);
        backup_dead.store(true, Ordering::Relaxed);
        pool.slots[1].pool.recycle().await;
        pool.slots[0].pool.probe_fresh().await.expect("primary is reachable again");
        let stream = pool.open_stream(&target()).await.expect("primary serves the stream");
        assert!(stream.is_alive());
        assert!(
            matches!(pool.server_health(1), Some(HealthState::Down { .. })),
            "the backup was really tried and failed"
        );
        assert_eq!(pool.active_index(), 1, "the fresh backup must keep the active slot");
        assert!(pool.is_backup_active());

        // Минимальное время прошло: тот же отказ переключает.
        pool.slots[1].state.lock().unwrap().became_active_at =
            Some(Instant::now() - MIN_ACTIVE_DWELL);
        let _ = pool.open_stream(&target()).await.expect("primary serves");
        assert_eq!(pool.active_index(), 0, "after the dwell the failure moves the active slot");
    }

    /// XR-296: возврат того же аплинка пересобирает mux, но оставляет
    /// `flap_count`, `failback_suppressed_until` и активный слот на месте.
    /// Настоящая смена сети (`recycle`) по-прежнему обнуляет всё и возвращает
    /// активного на primary.
    #[tokio::test]
    async fn test_same_uplink_return_keeps_flap_penalty_and_active() {
        let addr = spawn_test_server().await;
        let pool = ServerPool::new(
            vec![
                slot("primary", failing_connect(Arc::new(AtomicU32::new(0)))),
                slot("backup", connect_to(addr, Arc::new(AtomicU32::new(0)))),
            ],
            PoolProfile::mobile(),
            None,
        );
        let _ = pool.open_stream(&target()).await.expect("backup serves");
        assert_eq!(pool.active_index(), 1);
        pool.slots[0].penalize_flap(Duration::from_secs(3600));
        assert!(pool.slots[0].failback_suppressed());

        pool.uplink_lost();
        assert!(pool.uplink_restored().await);
        assert_eq!(pool.active_index(), 1, "same uplink must not move the active slot");
        {
            let st = pool.slots[0].state.lock().unwrap();
            assert_eq!(st.flap_count, 1, "same uplink must keep the flap counter");
            assert!(st.failback_suppressed_until.is_some(), "same uplink must keep the penalty");
            assert!(
                matches!(st.health, HealthState::Down { .. }),
                "same uplink must keep the health knowledge"
            );
        }
        let stream = pool.open_stream(&target()).await.expect("backup still serves");
        assert!(stream.is_alive());
        assert_eq!(pool.active_index(), 1);

        // Настоящая смена сети ведёт себя как раньше и заодно снимает заморозку.
        pool.uplink_lost();
        pool.recycle().await;
        assert!(!pool.is_uplink_down(), "a real network change lifts the freeze");
        assert_eq!(pool.active_index(), 0, "a real network change resets to primary");
        let st = pool.slots[0].state.lock().unwrap();
        assert_eq!(st.flap_count, 0);
        assert!(st.failback_suppressed_until.is_none());
        assert_eq!(st.health, HealthState::Up);
    }

    /// XR-296, стенд эмулятора: ОС объявляет сеть раньше, чем у неё встают
    /// маршруты, и первый коннект после возврата аплинка падает. Такой отказ
    /// не двигает активный: стрим уходит резерву, активный остаётся, а по
    /// истечении окна отказ снова переключает как раньше.
    #[tokio::test]
    async fn test_resume_grace_keeps_active_on_first_failure_after_uplink_return() {
        let addr = spawn_test_server().await;
        let primary_dead = Arc::new(AtomicBool::new(false));
        let pool = ServerPool::new(
            vec![
                slot("primary", switchable_connect(addr, primary_dead.clone())),
                slot("backup", connect_to(addr, Arc::new(AtomicU32::new(0)))),
            ],
            PoolProfile::mobile(),
            None,
        );
        let _ = pool.open_stream(&target()).await.expect("primary serves");

        pool.uplink_lost();
        primary_dead.store(true, Ordering::Relaxed);
        assert!(pool.uplink_restored().await);

        // Сеть объявлена, маршрутов ещё нет: primary падает, прогрев и стрим
        // уходят резерву без смены активного.
        assert!(pool.warmup().await.is_ok(), "warmup succeeds via the backup");
        assert_eq!(pool.active_index(), 0, "warmup right after the return must not switch");
        let stream = pool.open_stream(&target()).await.expect("backup serves the stream");
        assert!(stream.is_alive());
        assert_eq!(pool.active_index(), 0, "a failure right after the return must not switch");

        // Окно вышло, сервер всё ещё лежит: теперь это его отказ.
        *pool.resumed_at.lock().unwrap() = Some(Instant::now() - MIN_ACTIVE_DWELL);
        let _ = pool.open_stream(&target()).await.expect("backup serves");
        assert_eq!(pool.active_index(), 1, "after the grace the failure moves the active slot");
    }

    /// XR-296, ревью: в окне выдержки статус показывает активный, а стримы
    /// идут через соседа. Подмена названа в журнале ровно один раз на окно,
    /// второй стрим под breaker'ом активного строку не повторяет.
    #[tokio::test]
    async fn test_dwell_substitution_is_journaled_once_per_window() {
        let addr = spawn_test_server().await;
        let primary_dead = Arc::new(AtomicBool::new(true));
        let backup_dead = Arc::new(AtomicBool::new(false));
        let (cb, events) = collecting_events();
        let pool = ServerPool::new(
            vec![
                slot("primary", switchable_connect(addr, primary_dead.clone())),
                slot("backup", switchable_connect(addr, backup_dead.clone())),
            ],
            PoolProfile::mobile(),
            Some(cb),
        );
        let _ = pool.open_stream(&target()).await.expect("backup serves");
        assert_eq!(pool.active_index(), 1);

        primary_dead.store(false, Ordering::Relaxed);
        backup_dead.store(true, Ordering::Relaxed);
        pool.slots[1].pool.recycle().await;
        pool.slots[0].pool.probe_fresh().await.expect("primary is reachable again");
        for _ in 0..3 {
            let _ = pool.open_stream(&target()).await.expect("primary serves the stream");
        }
        assert_eq!(pool.active_index(), 1, "the dwell keeps the active slot");
        let noted = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.contains("стримы идут через primary") && e.contains("сервер backup"))
            .count();
        assert_eq!(noted, 1, "the substitution must be journaled exactly once per dwell");

        // Новое окно после смены активного: строка приходит снова.
        pool.slots[1].state.lock().unwrap().became_active_at =
            Some(Instant::now() - MIN_ACTIVE_DWELL);
        let _ = pool.open_stream(&target()).await.expect("primary serves");
        assert_eq!(pool.active_index(), 0);
        primary_dead.store(true, Ordering::Relaxed);
        backup_dead.store(false, Ordering::Relaxed);
        pool.slots[0].pool.recycle().await;
        pool.slots[1].pool.probe_fresh().await.expect("backup is reachable again");
        let _ = pool.open_stream(&target()).await.expect("backup serves the stream");
        let noted_backup = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.contains("стримы идут через backup"))
            .count();
        assert_eq!(noted_backup, 1, "a new dwell window journals its own substitution");
    }
}
