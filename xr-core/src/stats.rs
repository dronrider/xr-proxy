//! VPN traffic statistics.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

/// Format current wall-clock time as YYYY-MM-DD HH:MM:SS UTC.
fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Days since epoch → date (simplified, no leap second handling).
    let days = (secs / 86400) as i64;
    let time = secs % 86400;
    let h = time / 3600;
    let m = (time % 3600) / 60;
    let s = time % 60;

    // Civil date from days since 1970-01-01 (Rata Die algorithm).
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };

    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, m, s)
}

/// Thread-safe traffic counters.
#[derive(Clone)]
pub struct Stats {
    inner: Arc<StatsInner>,
}

struct StatsInner {
    bytes_up: AtomicU64,
    bytes_down: AtomicU64,
    active_connections: AtomicU32,
    total_connections: AtomicU64,
    started_at: std::sync::Mutex<Option<Instant>>,
    // Debug counters.
    dns_queries: AtomicU64,
    tcp_syns: AtomicU64,
    smol_recv: AtomicU64,
    smol_send: AtomicU64,
    /// Cumulative WARN-level event count (policy drops: fake IPs, private IPs, blocked DoT, etc.).
    /// Grows monotonically, not affected by drain of the `recent_errors` ring.
    relay_warns: AtomicU64,
    /// Cumulative ERROR-level event count (real I/O failures: mux open fail, timeouts).
    /// Grows monotonically, not affected by drain of the `recent_errors` ring.
    relay_errors: AtomicU64,
    debug_msg: std::sync::Mutex<String>,
    recent_errors: std::sync::Mutex<Vec<String>>,
}

/// Snapshot of current statistics.
#[derive(Debug, Clone)]
pub struct StatsSnapshot {
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub active_connections: u32,
    pub total_connections: u64,
    pub uptime_seconds: u64,
    pub dns_queries: u64,
    pub tcp_syns: u64,
    pub smol_recv: u64,
    pub smol_send: u64,
    /// Cumulative WARN count (policy drops).
    pub relay_warns: u64,
    /// Cumulative ERROR count (real failures).
    pub relay_errors: u64,
    pub debug_msg: String,
}

impl Stats {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(StatsInner {
                bytes_up: AtomicU64::new(0),
                bytes_down: AtomicU64::new(0),
                active_connections: AtomicU32::new(0),
                total_connections: AtomicU64::new(0),
                started_at: std::sync::Mutex::new(None),
                dns_queries: AtomicU64::new(0),
                tcp_syns: AtomicU64::new(0),
                smol_recv: AtomicU64::new(0),
                smol_send: AtomicU64::new(0),
                relay_warns: AtomicU64::new(0),
                relay_errors: AtomicU64::new(0),
                debug_msg: std::sync::Mutex::new(String::new()),
                recent_errors: std::sync::Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn mark_started(&self) {
        *self.inner.started_at.lock().unwrap() = Some(Instant::now());
    }

    pub fn add_bytes_up(&self, n: u64) {
        self.inner.bytes_up.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_bytes_down(&self, n: u64) {
        self.inner.bytes_down.fetch_add(n, Ordering::Relaxed);
    }

    pub fn connection_opened(&self) {
        self.inner.active_connections.fetch_add(1, Ordering::Relaxed);
        self.inner.total_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn connection_closed(&self) {
        self.inner.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn add_dns_query(&self) {
        self.inner.dns_queries.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_tcp_syn(&self) {
        self.inner.tcp_syns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add_smol_recv(&self, n: u64) {
        self.inner.smol_recv.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_smol_send(&self, n: u64) {
        self.inner.smol_send.fetch_add(n, Ordering::Relaxed);
    }

    /// Информационное событие (успешное действие), не увеличивает counters.
    pub fn add_log(&self, msg: &str) {
        self.append_entry("INFO", msg);
    }

    /// Ожидаемое срабатывание policy (fake IP без домена, private IP, blocked DoT).
    /// Инкрементит `relay_warns`. Это не баг, а настроенная защитная реакция.
    pub fn add_warn(&self, msg: &str) {
        self.inner.relay_warns.fetch_add(1, Ordering::Relaxed);
        self.append_entry("WARN", msg);
    }

    /// Реальный отказ (VPS недоступен, I/O ошибка, таймаут). Инкрементит `relay_errors`.
    pub fn add_error(&self, msg: &str) {
        self.inner.relay_errors.fetch_add(1, Ordering::Relaxed);
        self.append_entry("ERROR", msg);
    }

    fn append_entry(&self, level: &str, msg: &str) {
        let mut entries = self.inner.recent_errors.lock().unwrap();
        if entries.len() >= 200 {
            // Трёхуровневый приоритетный drain:
            //   1. Сначала 50 самых старых INFO → скидываем.
            //   2. Если всё ещё переполнено (INFO не хватило) — 50 самых старых WARN.
            //   3. В крайнем случае — drain(0..50) любых записей, не трогая порядок.
            // Это гарантирует, что ERROR никогда не вытесняются INFO-шумом
            // или даже WARN-шумом — бадж и заголовок вкладки Log в Android
            // всегда честно показывают реальные отказы.
            let mut to_drop = 50usize;
            entries.retain(|e| {
                if to_drop == 0 { return true; }
                if e.contains(" WARN ") || e.contains(" ERROR ") { return true; }
                to_drop -= 1;
                false
            });
            if entries.len() >= 200 {
                let mut to_drop_warn = 50usize;
                entries.retain(|e| {
                    if to_drop_warn == 0 { return true; }
                    if e.contains(" ERROR ") { return true; }
                    to_drop_warn -= 1;
                    false
                });
            }
            if entries.len() >= 200 {
                entries.drain(0..50);
            }
        }
        // Ширина уровня 5, чтобы "ERROR" (5) и "WARN"/"INFO" (4) выровнялись.
        entries.push(format!("{} {:>5} {}", timestamp(), level, msg));
    }

    pub fn recent_errors(&self) -> Vec<String> {
        self.inner.recent_errors.lock().unwrap().clone()
    }

    pub fn clear_errors(&self) {
        self.inner.recent_errors.lock().unwrap().clear();
        self.inner.relay_warns.store(0, Ordering::Relaxed);
        self.inner.relay_errors.store(0, Ordering::Relaxed);
    }

    pub fn set_debug(&self, msg: String) {
        *self.inner.debug_msg.lock().unwrap() = msg;
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let uptime = self
            .inner
            .started_at
            .lock()
            .unwrap()
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);

        StatsSnapshot {
            bytes_up: self.inner.bytes_up.load(Ordering::Relaxed),
            bytes_down: self.inner.bytes_down.load(Ordering::Relaxed),
            active_connections: self.inner.active_connections.load(Ordering::Relaxed),
            total_connections: self.inner.total_connections.load(Ordering::Relaxed),
            uptime_seconds: uptime,
            dns_queries: self.inner.dns_queries.load(Ordering::Relaxed),
            tcp_syns: self.inner.tcp_syns.load(Ordering::Relaxed),
            smol_recv: self.inner.smol_recv.load(Ordering::Relaxed),
            smol_send: self.inner.smol_send.load(Ordering::Relaxed),
            relay_warns: self.inner.relay_warns.load(Ordering::Relaxed),
            relay_errors: self.inner.relay_errors.load(Ordering::Relaxed),
            debug_msg: self.inner.debug_msg.lock().unwrap().clone(),
        }
    }

    pub fn reset(&self) {
        self.inner.bytes_up.store(0, Ordering::Relaxed);
        self.inner.bytes_down.store(0, Ordering::Relaxed);
        self.inner.total_connections.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_level_counters() {
        let stats = Stats::new();
        stats.add_log("info event");
        stats.add_warn("policy drop");
        stats.add_warn("another drop");
        stats.add_error("real failure");

        let snap = stats.snapshot();
        assert_eq!(snap.relay_warns, 2);
        assert_eq!(snap.relay_errors, 1);

        let entries = stats.recent_errors();
        assert_eq!(entries.len(), 4);
        assert!(entries[0].contains(" INFO "));
        assert!(entries[1].contains(" WARN "));
        assert!(entries[2].contains(" WARN "));
        assert!(entries[3].contains("ERROR"));
    }

    #[test]
    fn drain_prefers_info_over_warn_and_error() {
        let stats = Stats::new();

        // Имитируем реальное подключение: 3 serious ERROR + 2 policy WARN + потоп INFO.
        stats.add_error("mux open fail: initial1");
        stats.add_error("mux open fail: initial2");
        stats.add_error("mux open fail: initial3");
        stats.add_warn("fake IP without domain");
        stats.add_warn("private IP blocked");
        for i in 0..250 {
            stats.add_log(&format!("mux relay for target-{}", i));
        }

        let entries = stats.recent_errors();

        // Инвариант: ERROR и WARN никогда не должны быть вытеснены INFO-шумом.
        let error_count = entries.iter().filter(|e| e.contains(" ERROR ")).count();
        let warn_count = entries.iter().filter(|e| e.contains(" WARN ")).count();
        assert_eq!(error_count, 3, "все 3 ERROR должны остаться: {:?}", entries);
        assert_eq!(warn_count, 2, "все 2 WARN должны остаться: {:?}", entries);

        // Буфер не растёт неограниченно.
        assert!(entries.len() < 250, "буфер не должен хранить всё: {}", entries.len());

        // Счётчики отражают реальные значения.
        let snap = stats.snapshot();
        assert_eq!(snap.relay_errors, 3);
        assert_eq!(snap.relay_warns, 2);
    }

    #[test]
    fn drain_prefers_warn_over_error() {
        let stats = Stats::new();

        // Сначала несколько ERROR, потом поток WARN. Если INFO-drain не
        // помог (их нет), следующий приоритет — старые WARN, но ERROR
        // остаются нетронутыми.
        for i in 0..5 {
            stats.add_error(&format!("fatal-{}", i));
        }
        for i in 0..250 {
            stats.add_warn(&format!("policy-{}", i));
        }

        let entries = stats.recent_errors();

        // Все 5 ERROR выживают (они старее WARN, но drain их защищает).
        let error_count = entries.iter().filter(|e| e.contains(" ERROR ")).count();
        assert_eq!(error_count, 5, "все ERROR должны быть сохранены");

        // Часть WARN выпала через drain.
        assert!(entries.len() <= 200);
    }

    #[test]
    fn drain_falls_back_when_only_errors() {
        let stats = Stats::new();

        // Крайний случай: журнал состоит только из ERROR. Два первых уровня
        // drain не сработают, fallback drain(0..50) срезает самые старые.
        for i in 0..250 {
            stats.add_error(&format!("fatal-{}", i));
        }

        let entries = stats.recent_errors();
        assert!(entries.len() <= 200);
        assert!(entries.last().unwrap().contains("fatal-249"));
        assert!(!entries.iter().any(|e| e.contains("fatal-0")));
    }
}
