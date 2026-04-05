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

    pub fn add_log(&self, msg: &str) {
        self.append_entry("INFO", msg);
    }

    pub fn add_relay_error(&self, msg: &str) {
        self.inner.relay_errors.fetch_add(1, Ordering::Relaxed);
        self.append_entry("WARN", msg);
    }

    fn append_entry(&self, level: &str, msg: &str) {
        let mut entries = self.inner.recent_errors.lock().unwrap();
        if entries.len() >= 200 { entries.drain(0..50); }
        entries.push(format!("{} {:>4} {}", timestamp(), level, msg));
    }

    pub fn recent_errors(&self) -> Vec<String> {
        self.inner.recent_errors.lock().unwrap().clone()
    }

    pub fn clear_errors(&self) {
        self.inner.recent_errors.lock().unwrap().clear();
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
