//! Мост `tracing` в журнал приложения (XR-237).
//!
//! Движок и протокол пишут диагностику через `tracing`, а тот по умолчанию
//! уходит в stdout процесса. На Android stdout выбрасывается, в logcat не
//! попадает ничего, и любой `warn!` движка (отказ разбора, деградация) не виден
//! ни пользователю на вкладке Log, ни при разборе инцидента. Лента приложения
//! до этой правки наполнялась только прямыми вызовами [`Journal::append`] из
//! JNI и `Stats`, то есть слоем обвязки, а не самим движком.
//!
//! Мост это слой `tracing_subscriber`, который кладёт подходящие события в тот
//! же журнал. Живёт он в `xr-core`, а не в JNI, потому что причина
//! android-специфична, а нужда общая: iOS-порт берёт тот же слой без единой
//! строки на своей стороне.
//!
//! Что проходит в ленту:
//!
//! - `WARN` и `ERROR` от любого источника. Это события, про которые
//!   пользователю есть что сказать, и их немного.
//! - `INFO` только с явно перечисленных target ([`JournalLayer::
//!   with_info_targets`]). По умолчанию список пуст: `info!` в движке стоит на
//!   каждом выбранном пути соединения, и лента утонет в них за минуту.
//! - `DEBUG` и `TRACE` не проходят никогда, это developer-логи.
//!
//! Лента пользовательская, поэтому кроме фильтра по уровню у моста есть
//! потолок частоты ([`DEFAULT_RATE_LIMIT`] записей в секунду). Цикл повторов,
//! который сыплет `warn!` с разным текстом, журнальной свёрткой дубликатов не
//! ужимается и вытеснил бы из хвоста всё остальное. Придержанные записи не
//! исчезают молча: как только окно сменилось, мост пишет строку о том, сколько
//! их было.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

use crate::journal::Journal;

/// Потолок частоты по умолчанию: записей моста в секунду.
pub const DEFAULT_RATE_LIMIT: u32 = 20;

/// Источник служебных записей самого моста в ленте.
const BRIDGE_SOURCE: &str = "trace";

const WINDOW: Duration = Duration::from_secs(1);

static FORWARDED: AtomicU64 = AtomicU64::new(0);
static SUPPRESSED: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// Защита от рекурсии: запись в журнал делает файловые операции, и любой
    /// `warn!` изнутри этого пути (свой или чужой, из зависимости) вернулся бы
    /// в мост на том же потоке и закрутил бы стек.
    static IN_BRIDGE: Cell<bool> = const { Cell::new(false) };
}

/// Сколько событий мост доставил в журнал и сколько придержал по потолку
/// частоты за всё время жизни процесса. Счётчики общие на процесс: разбор
/// инцидента по ним отличает молчащий мост от моста без событий.
pub fn counters() -> (u64, u64) {
    (FORWARDED.load(Ordering::Relaxed), SUPPRESSED.load(Ordering::Relaxed))
}

/// Отметить в ленте, что мост встал. Молчащий мост неотличим от отсутствующего,
/// поэтому строка пишется всегда, даже когда событий движка ещё не было.
pub fn announce(journal: &Journal) {
    journal.append(
        "INFO",
        BRIDGE_SOURCE,
        "мост диагностики движка включён: в ленту идут WARN и выше",
    );
}

/// Поставить мост единственным слоем подписчика и отметиться в ленте.
/// Возвращает `false`, если глобальный подписчик уже стоял (тогда мост не
/// встал, и ставить его надо в составе того подписчика, см. `xr-android-jni`).
pub fn install(journal: Journal) -> bool {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let ok = tracing_subscriber::registry()
        .with(JournalLayer::new(journal.clone()))
        .try_init()
        .is_ok();
    if ok {
        announce(&journal);
    }
    ok
}

/// Слой `tracing_subscriber`, кладущий события в журнал приложения.
pub struct JournalLayer {
    journal: Journal,
    info_targets: Vec<String>,
    limit: u32,
    window: Mutex<Window>,
}

struct Window {
    started: Instant,
    used: u32,
    held: u64,
}

/// Решение потолка частоты по конкретному событию.
enum Slot {
    Pass,
    /// Пропустить, но сперва сказать про `n` придержанных в прошлом окне.
    PassAfterHold(u64),
    Drop,
}

impl JournalLayer {
    pub fn new(journal: Journal) -> Self {
        Self {
            journal,
            info_targets: Vec::new(),
            limit: DEFAULT_RATE_LIMIT,
            window: Mutex::new(Window { started: Instant::now(), used: 0, held: 0 }),
        }
    }

    /// Пропускать в ленту ещё и `INFO` с этих target (сравнение по префиксу,
    /// то есть `xr_core::update` берёт и вложенные модули).
    pub fn with_info_targets<I, S>(mut self, targets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.info_targets = targets.into_iter().map(Into::into).collect();
        self
    }

    /// Поменять потолок частоты; ноль выключает ограничение.
    pub fn with_rate_limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }

    fn passes_filter(&self, level: &Level, target: &str) -> bool {
        if *level <= Level::WARN {
            return true;
        }
        *level == Level::INFO && self.info_targets.iter().any(|t| target.starts_with(t.as_str()))
    }

    fn take_slot(&self, now: Instant) -> Slot {
        if self.limit == 0 {
            return Slot::Pass;
        }
        let mut w = self.window.lock().unwrap();
        if now.duration_since(w.started) >= WINDOW {
            w.started = now;
            w.used = 1;
            let held = std::mem::take(&mut w.held);
            return if held > 0 { Slot::PassAfterHold(held) } else { Slot::Pass };
        }
        if w.used < self.limit {
            w.used += 1;
            Slot::Pass
        } else {
            w.held += 1;
            Slot::Drop
        }
    }
}

impl<S: Subscriber> Layer<S> for JournalLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if reentered() {
            return;
        }
        let meta = event.metadata();
        if !self.passes_filter(meta.level(), meta.target()) {
            return;
        }

        let held = match self.take_slot(Instant::now()) {
            Slot::Drop => {
                SUPPRESSED.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Slot::Pass => 0,
            Slot::PassAfterHold(n) => n,
        };

        // Защита взводится до разбора полей: `Debug` чужого типа тоже может
        // позвать `warn!`, и это уже второй заход в мост на том же потоке.
        let _guard = ReentryGuard::enter();
        let mut visitor = EventText::default();
        event.record(&mut visitor);
        let text = visitor.finish(meta.target());
        let level = level_name(meta.level());

        if held > 0 {
            self.journal.append(
                "WARN",
                BRIDGE_SOURCE,
                &format!("мост придержал записей движка: {}", held),
            );
        }
        self.journal.append(level, source_for(meta.target()), &text);
        FORWARDED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Идёт ли на этом потоке обработка события мостом.
fn reentered() -> bool {
    IN_BRIDGE.with(Cell::get)
}

/// Взводит защиту от рекурсии на время обработки события и снимает её на
/// выходе, в том числе при панике в форматировании чужого поля.
///
/// Вложенное событие через тот же `Dispatch` не пройдёт и без неё, это делает
/// сам `tracing`. Защита нужна там, где его блокировки нет: запись в журнал
/// ходит в файловую систему, и событие оттуда может прийти по другому
/// подписчику (`tracing-log`, свой `with_default` в колбэке), а вернётся оно в
/// тот же мост на том же потоке.
struct ReentryGuard;

impl ReentryGuard {
    fn enter() -> Self {
        IN_BRIDGE.with(|f| f.set(true));
        Self
    }
}

impl Drop for ReentryGuard {
    fn drop(&mut self) {
        IN_BRIDGE.with(|f| f.set(false));
    }
}

fn level_name(level: &Level) -> &'static str {
    match *level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARN",
        _ => "INFO",
    }
}

/// Тег источника в ленте. Крейт движка и крейт протокола различаются, всё
/// остальное (зависимости) идёт общим тегом моста.
fn source_for(target: &str) -> &'static str {
    match target.split("::").next().unwrap_or("") {
        "xr_core" => "core",
        "xr_proto" => "proto",
        _ => BRIDGE_SOURCE,
    }
}

/// Сборка текста записи из полей события.
#[derive(Default)]
struct EventText {
    message: String,
    fields: String,
}

impl EventText {
    /// Строка ленты это `target: сообщение, поле=значение`. Target остаётся в
    /// тексте: `[source]` журнала различает только крейт, а место в коде при
    /// разборе инцидента нужно точнее.
    fn finish(self, target: &str) -> String {
        let mut s = String::with_capacity(target.len() + self.message.len() + self.fields.len() + 2);
        s.push_str(target);
        s.push_str(": ");
        if self.message.is_empty() {
            s.push_str("событие без текста");
        } else {
            s.push_str(&self.message);
        }
        s.push_str(&self.fields);
        s
    }
}

impl Visit for EventText {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        } else {
            self.fields.push_str(&format!(", {}={:?}", field.name(), value));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push_str(&format!(", {}={}", field.name(), value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;

    fn subscriber(layer: JournalLayer) -> impl Subscriber {
        tracing_subscriber::registry().with(layer)
    }

    #[test]
    fn warn_reaches_journal() {
        let j = Journal::memory();
        with_default(subscriber(JournalLayer::new(j.clone())), || {
            tracing::warn!("разбор правил не удался");
        });

        let tail = j.tail();
        assert_eq!(tail.len(), 1, "tail: {:?}", tail);
        assert!(tail[0].contains("  WARN ["), "unexpected: {}", tail[0]);
        assert!(tail[0].contains("разбор правил не удался"), "unexpected: {}", tail[0]);
    }

    #[test]
    fn error_keeps_level_and_fields() {
        let j = Journal::memory();
        with_default(subscriber(JournalLayer::new(j.clone())), || {
            tracing::error!(peer = "vps", "туннель не поднялся");
        });

        let e = &j.tail()[0];
        assert!(e.contains(" ERROR ["), "unexpected: {}", e);
        assert!(e.contains("туннель не поднялся, peer=vps"), "unexpected: {}", e);
    }

    #[test]
    fn debug_and_info_are_filtered_out() {
        let j = Journal::memory();
        with_default(subscriber(JournalLayer::new(j.clone())), || {
            tracing::debug!("пакет ушёл в туннель");
            tracing::info!("выбран путь: напрямую");
            tracing::trace!("smoltcp poll");
        });

        assert!(j.tail().is_empty(), "лента должна остаться пустой: {:?}", j.tail());
    }

    #[test]
    fn info_passes_for_listed_target() {
        let j = Journal::memory();
        // Target по умолчанию это путь модуля, здесь `xr_core::journal_bridge`.
        let layer = JournalLayer::new(j.clone()).with_info_targets(["xr_core::journal_bridge"]);
        with_default(subscriber(layer), || {
            tracing::info!("обновление скачано");
        });

        let tail = j.tail();
        assert_eq!(tail.len(), 1, "tail: {:?}", tail);
        assert!(tail[0].contains("  INFO [core]"), "unexpected: {}", tail[0]);
    }

    #[test]
    fn info_target_match_is_prefix_only() {
        let j = Journal::memory();
        let layer = JournalLayer::new(j.clone()).with_info_targets(["xr_core::update"]);
        with_default(subscriber(layer), || {
            tracing::info!("не тот модуль");
        });
        assert!(j.tail().is_empty(), "tail: {:?}", j.tail());
    }

    #[test]
    fn source_tag_follows_crate() {
        assert_eq!(source_for("xr_core::session"), "core");
        assert_eq!(source_for("xr_proto"), "proto");
        assert_eq!(source_for("reqwest::connect"), BRIDGE_SOURCE);
    }

    #[test]
    fn rate_limit_holds_burst_and_reports_it() {
        let j = Journal::memory();
        let layer = JournalLayer::new(j.clone()).with_rate_limit(2);
        let (before_forwarded, before_suppressed) = counters();
        with_default(subscriber(layer), || {
            // Тексты разные, журнальная свёртка дубликатов их не ужмёт.
            for i in 0..10 {
                tracing::warn!("повтор попытки {}", i);
            }
        });

        let tail = j.tail();
        assert_eq!(tail.len(), 2, "сверх потолка в ленту ничего не идёт: {:?}", tail);
        // Счётчики общие на процесс, а тесты идут параллельно, поэтому судим по
        // приросту не меньше своего вклада, а не по точному равенству.
        let (forwarded, suppressed) = counters();
        assert!(forwarded - before_forwarded >= 2, "forwarded: {}", forwarded);
        assert!(suppressed - before_suppressed >= 8, "suppressed: {}", suppressed);
    }

    #[test]
    fn held_count_is_reported_in_next_window() {
        let j = Journal::memory();
        let layer = JournalLayer::new(j.clone()).with_rate_limit(1);
        // Окно сменяем руками, без sleep: тест обязан быть детерминированным.
        {
            let mut w = layer.window.lock().unwrap();
            w.started = Instant::now() - Duration::from_secs(5);
        }
        with_default(subscriber(layer), || {
            tracing::warn!("первая");
            tracing::warn!("вторая");
            tracing::warn!("третья");
        });
        // Первая прошла, вторая с третьей придержаны.
        assert_eq!(j.tail().len(), 1);

        // Следующее окно с придержанными в прошлом: мост сперва отчитывается.
        let j3 = Journal::memory();
        let layer = JournalLayer::new(j3.clone()).with_rate_limit(1);
        {
            let mut w = layer.window.lock().unwrap();
            w.used = 1;
            w.held = 7;
            w.started = Instant::now() - Duration::from_secs(5);
        }
        with_default(subscriber(layer), || {
            tracing::warn!("после паузы");
        });
        let tail = j3.tail();
        assert_eq!(tail.len(), 2, "tail: {:?}", tail);
        assert!(tail[0].contains("мост придержал записей движка: 7"), "unexpected: {}", tail[0]);
        assert!(tail[1].contains("после паузы"), "unexpected: {}", tail[1]);
    }

    #[test]
    fn reentry_guard_blocks_nested_handling() {
        assert!(!reentered());
        {
            let _g = ReentryGuard::enter();
            assert!(reentered(), "внутри обработки события мост закрыт для себя");
        }
        assert!(!reentered(), "по выходу защита снимается");
    }

    #[test]
    fn no_recursion_when_field_formatting_logs() {
        // `warn!` из `Debug` чужого поля печатается уже из-под обработки
        // события и в ленту второй строкой не идёт.
        struct Noisy;
        impl std::fmt::Debug for Noisy {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                tracing::warn!("изнутри записи в журнал");
                f.write_str("noisy")
            }
        }

        let j = Journal::memory();
        with_default(subscriber(JournalLayer::new(j.clone())), || {
            tracing::warn!(payload = ?Noisy, "внешнее событие");
        });

        let tail = j.tail();
        assert_eq!(tail.len(), 1, "вложенное событие в ленту не идёт: {:?}", tail);
        assert!(tail[0].contains("внешнее событие"), "unexpected: {}", tail[0]);
    }

    #[test]
    fn announce_marks_bridge_alive() {
        let j = Journal::memory();
        announce(&j);
        let tail = j.tail();
        assert_eq!(tail.len(), 1);
        assert!(tail[0].contains("[trace] мост диагностики движка включён"), "unexpected: {}", tail[0]);
    }
}
