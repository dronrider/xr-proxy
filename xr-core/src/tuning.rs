//! Профиль тюнинга памяти движка (LLD-39, раздел 4.5).
//!
//! Дефолты те же, что стояли литералами: роутер и Android их не замечают.
//! Профиль `ne` ужимает буферы, капает одновременные сессии, режет очереди
//! и начальное окно отправки под жёсткий лимит памяти Network Extension.
//! Счёт бюджета лежит в `xr-core/tests/ios_memory_budget.rs`.

use std::time::Duration;

/// Буферы сессии smoltcp по умолчанию (Android, роутер-совместимый путь).
pub const DEFAULT_SESSION_RX_BUF: usize = 65535;
pub const DEFAULT_SESSION_TX_BUF: usize = 65535;
/// Глубина каналов сессии (tun <-> relay) по умолчанию.
pub const DEFAULT_SESSION_CHANNEL: usize = 512;
/// Потолок очередей пакетов по умолчанию.
pub const DEFAULT_QUEUE_CAPACITY: usize = 4096;

/// Значения профиля Network Extension (LLD-39, раздел 4.5, пункты 1-5).
/// Пункт 6 (пул буферов relay) относится к коду relay-петли, а не к конфигу.
impl TuningProfile {
    /// Память к 15 МБ: буферы 16/16 КБ, кап 128 сессий, каналы 64, очереди 512,
    /// окно отправки 256 КиБ. Токи рантайма задаёт хост (NE-расширение
    /// поднимает свой tokio на 1-2 воркерах).
    pub fn ne() -> Self {
        Self {
            session_rx_buf: 16 * 1024,
            session_tx_buf: 16 * 1024,
            max_sessions: 128,
            session_channel: 64,
            queue_capacity: 512,
            recv_window: 256 * 1024,
            relay_flow_timeout: Duration::from_secs(120),
            relay_max_flows: crate::udp_relay::DEFAULT_MAX_FLOWS,
        }
    }
}

/// Рычаги тюнинга. `Default` повторяет прежние литералы, так что дефолтный
/// `VpnConfig` ведёт себя как до появления профиля.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuningProfile {
    /// smoltcp RX-буфер одной TCP-сессии.
    pub session_rx_buf: usize,
    /// smoltcp TX-буфер одной TCP-сессии.
    pub session_tx_buf: usize,
    /// Потолок одновременных клиентских сессий; 0 означает без капа.
    pub max_sessions: usize,
    /// Глубина mpsc-каналов сессии (в обе стороны).
    pub session_channel: usize,
    /// Потолок каждой очереди пакетов TUN.
    pub queue_capacity: usize,
    /// Начальное окно отправки mux-стрима в байтах; 0 берёт константу
    /// протокола. Касается upload: сервер своё окно не перенастраивает,
    /// download-всплеск остаётся на константе `STREAM_RECV_WINDOW`.
    pub recv_window: u32,
    /// Таймаут UDP-флоу реле.
    pub relay_flow_timeout: Duration,
    /// Потолок таблицы UDP-флоу реле.
    pub relay_max_flows: usize,
}

impl Default for TuningProfile {
    fn default() -> Self {
        Self {
            session_rx_buf: DEFAULT_SESSION_RX_BUF,
            session_tx_buf: DEFAULT_SESSION_TX_BUF,
            max_sessions: 0,
            session_channel: DEFAULT_SESSION_CHANNEL,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            recv_window: 0,
            relay_flow_timeout: crate::udp_relay::DEFAULT_FLOW_TIMEOUT,
            relay_max_flows: crate::udp_relay::DEFAULT_MAX_FLOWS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_keeps_legacy_limits() {
        let p = TuningProfile::default();
        assert_eq!(p.session_rx_buf, 65535);
        assert_eq!(p.session_tx_buf, 65535);
        assert_eq!(p.session_channel, 512);
        assert_eq!(p.queue_capacity, 4096);
        assert_eq!(p.max_sessions, 0);
        assert_eq!(p.recv_window, 0);
    }

    #[test]
    fn ne_profile_fits_memory_budget_story() {
        let p = TuningProfile::ne();
        // 128 сессий * 32 КБ = 4 МБ, очереди 3 * 512 * 1500 ~ 2.3 МБ.
        assert_eq!(p.session_rx_buf + p.session_tx_buf, 32 * 1024);
        assert_eq!(p.max_sessions, 128);
        assert_eq!(p.session_channel, 64);
        assert_eq!(p.queue_capacity, 512);
        assert_eq!(p.recv_window, 256 * 1024);
    }

    #[test]
    fn ne_buffers_smaller_than_default() {
        let ne = TuningProfile::ne();
        let d = TuningProfile::default();
        assert!(ne.session_rx_buf < d.session_rx_buf);
        assert!(ne.queue_capacity < d.queue_capacity);
    }
}
