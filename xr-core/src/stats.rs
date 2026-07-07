//! VPN traffic statistics.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::journal::Journal;

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
    /// Grows monotonically, not affected by clearing the journal.
    relay_warns: AtomicU64,
    /// Cumulative ERROR-level event count (real I/O failures: mux open fail, timeouts).
    /// Grows monotonically, not affected by clearing the journal.
    relay_errors: AtomicU64,
    debug_msg: std::sync::Mutex<String>,
    /// Куда уходят записи `add_log`/`add_warn`/`add_error`. По умолчанию
    /// memory-only журнал; Android-обвязка подменяет его на общий
    /// персистентный (XR-042), и тогда лента переживает перезапуск движка.
    journal: std::sync::Mutex<Journal>,
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
                journal: std::sync::Mutex::new(Journal::memory()),
            }),
        }
    }

    /// Подключить внешний журнал (общий персистентный буфер приложения).
    /// Вызывается сразу после создания движка, до первых записей.
    pub fn set_journal(&self, journal: Journal) {
        *self.inner.journal.lock().unwrap() = journal;
    }

    fn journal(&self) -> Journal {
        self.inner.journal.lock().unwrap().clone()
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
        self.journal().append("INFO", "vpn", msg);
    }

    /// Ожидаемое срабатывание policy (fake IP без домена, private IP, blocked DoT).
    /// Инкрементит `relay_warns`. Это не баг, а настроенная защитная реакция.
    pub fn add_warn(&self, msg: &str) {
        self.inner.relay_warns.fetch_add(1, Ordering::Relaxed);
        self.journal().append("WARN", "vpn", msg);
    }

    /// Реальный отказ (VPS недоступен, I/O ошибка, таймаут). Инкрементит `relay_errors`.
    pub fn add_error(&self, msg: &str) {
        self.inner.relay_errors.fetch_add(1, Ordering::Relaxed);
        self.journal().append("ERROR", "vpn", msg);
    }

    pub fn recent_errors(&self) -> Vec<String> {
        self.journal().tail()
    }

    pub fn clear_errors(&self) {
        self.journal().clear();
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
    fn coalesce_does_not_affect_counters() {
        let stats = Stats::new();
        stats.add_warn("same warn");
        stats.add_warn("same warn");
        stats.add_warn("same warn");
        stats.add_error("same err");
        stats.add_error("same err");

        let snap = stats.snapshot();
        // Счётчики считают КАЖДОЕ событие, даже если в журнале они свёрнуты.
        assert_eq!(snap.relay_warns, 3);
        assert_eq!(snap.relay_errors, 2);

        let entries = stats.recent_errors();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].ends_with(" (\u{00D7}3)"));
        assert!(entries[1].ends_with(" (\u{00D7}2)"));
    }

    #[test]
    fn external_journal_survives_stats_recreation() {
        // Регрессия XR-042: раньше лента жила в Stats и обнулялась вместе с
        // движком при каждом перезапуске (смена сети). С внешним журналом
        // новый экземпляр Stats продолжает ту же ленту.
        let journal = Journal::memory();

        let stats1 = Stats::new();
        stats1.set_journal(journal.clone());
        stats1.add_error("ошибка до перезапуска движка");
        drop(stats1);

        let stats2 = Stats::new();
        stats2.set_journal(journal.clone());
        stats2.add_log("запись после перезапуска");

        let entries = stats2.recent_errors();
        assert_eq!(entries.len(), 2, "entries: {:?}", entries);
        assert!(entries[0].contains("ошибка до перезапуска движка"));
        assert!(entries[1].contains("запись после перезапуска"));
    }
}
