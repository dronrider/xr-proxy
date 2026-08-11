//! Здоровье сессии: во что складываются счётчики предупреждений и ошибок
//! движка на карточке подключения (LLD-06, раздел 3.5a).
//!
//! Логика жила в приложении (Kotlin `HealthTracker`) и была бы переписана
//! слово в слово под каждую новую платформу, хотя ничего платформенного в ней
//! нет: на вход идут накопительные счётчики движка, на выход одна из пяти
//! ступеней (XR-271). Окна и пороги здесь, а имена ступеней и их картинки
//! остаются делом экрана.

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

/// Ступени здоровья, от спокойной к тревожной. Порядок значим: ухудшение
/// применяется сразу, улучшение придерживается [`DOWNSHIFT_HOLD_MS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HealthLevel {
    /// Ни одной ошибки и ни одного предупреждения за последние 30 секунд.
    Healthy,
    /// Ошибок нет, предупреждений до десятка (фоновый шум).
    Good,
    /// Ошибок нет, предупреждений больше десятка.
    Watching,
    /// Три и больше ошибки в окне 30 секунд, но не всплеск.
    Hurt,
    /// Пять и больше ошибок за последние 5 секунд (всплеск отказов).
    Critical,
}

impl HealthLevel {
    /// Имя ступени для обвязки платформы: по нему экран выбирает свою
    /// картинку и подпись.
    pub fn as_str(self) -> &'static str {
        match self {
            HealthLevel::Healthy => "healthy",
            HealthLevel::Good => "good",
            HealthLevel::Watching => "watching",
            HealthLevel::Hurt => "hurt",
            HealthLevel::Critical => "critical",
        }
    }
}

/// Окно, за которое считаются события.
const WINDOW_MS: u64 = 30_000;
/// Окно всплеска.
const BURST_WINDOW_MS: u64 = 5_000;
/// Сколько ошибок в окне всплеска дают Critical.
const BURST_THRESHOLD: usize = 5;
/// Сколько держится ступень перед улучшением: иначе Critical и Hurt мигали бы
/// друг в друга каждый тик.
const DOWNSHIFT_HOLD_MS: u64 = 5_000;
/// Выше этого числа предупреждений в окне здоровье уходит в Watching.
const WARN_NOISE_THRESHOLD: usize = 10;
/// Сколько ошибок в окне дают Hurt.
const ERROR_HURT_THRESHOLD: usize = 3;
/// Потолок на одну дельту: подряд идущий скачок счётчика на тысячи событий не
/// должен раздувать очередь меток.
const MAX_EVENTS_PER_TICK: u64 = 100;

/// Скользящее окно по накопительным счётчикам движка. Кормится снимком
/// статистики раз в тик (около секунды) и отдаёт текущую ступень.
#[derive(Debug, Default)]
pub struct HealthTracker {
    last_seen_errors: u64,
    last_seen_warns: u64,
    error_timestamps: VecDeque<u64>,
    warn_timestamps: VecDeque<u64>,
    current_level: Option<HealthLevel>,
    last_worse_ms: u64,
}

impl HealthTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Обновить по свежим счётчикам, взяв текущее время у системы.
    pub fn update(&mut self, relay_errors: u64, relay_warnings: u64) -> HealthLevel {
        self.update_at(now_ms(), relay_errors, relay_warnings)
    }

    /// То же с явным временем: так тест видит окна и придержку, не завися от
    /// того, как быстро он бежит.
    pub fn update_at(&mut self, now: u64, relay_errors: u64, relay_warnings: u64) -> HealthLevel {
        // Счётчики движка только растут, но перезапуск движка обнуляет их, и
        // отрицательная дельта означает новую сессию, а не поток событий.
        let delta_err = relay_errors.saturating_sub(self.last_seen_errors);
        let delta_warn = relay_warnings.saturating_sub(self.last_seen_warns);
        self.last_seen_errors = relay_errors;
        self.last_seen_warns = relay_warnings;

        for _ in 0..delta_err.min(MAX_EVENTS_PER_TICK) {
            self.error_timestamps.push_back(now);
        }
        for _ in 0..delta_warn.min(MAX_EVENTS_PER_TICK) {
            self.warn_timestamps.push_back(now);
        }

        let window_cutoff = now.saturating_sub(WINDOW_MS);
        evict_older_than(&mut self.error_timestamps, window_cutoff);
        evict_older_than(&mut self.warn_timestamps, window_cutoff);

        let burst_cutoff = now.saturating_sub(BURST_WINDOW_MS);
        let recent_burst = self
            .error_timestamps
            .iter()
            .filter(|t| **t >= burst_cutoff)
            .count();
        let raw = if recent_burst >= BURST_THRESHOLD {
            HealthLevel::Critical
        } else if self.error_timestamps.len() >= ERROR_HURT_THRESHOLD {
            HealthLevel::Hurt
        } else if self.warn_timestamps.len() > WARN_NOISE_THRESHOLD {
            HealthLevel::Watching
        } else if !self.warn_timestamps.is_empty() {
            HealthLevel::Good
        } else {
            HealthLevel::Healthy
        };

        let current = self.current_level.unwrap_or(HealthLevel::Healthy);
        if raw >= current {
            self.current_level = Some(raw);
            if raw > HealthLevel::Healthy {
                self.last_worse_ms = now;
            }
        } else if now.saturating_sub(self.last_worse_ms) >= DOWNSHIFT_HOLD_MS {
            self.current_level = Some(raw);
        }
        self.current_level.unwrap_or(HealthLevel::Healthy)
    }

    /// Сеть пропала (авиарежим): без аплинка движок сыплет ошибками relay, но
    /// это не деградация прокси, а отсутствие связи, и портить по ним здоровье
    /// нельзя (XR-183). Базовая линия подтягивается к текущим счётчикам, окно
    /// чистится, ступень остаётся Healthy: вернувшаяся сеть считает дельту от
    /// этой точки, без ложного всплеска из накопленного за офлайн.
    pub fn freeze_at(&mut self, relay_errors: u64, relay_warnings: u64) -> HealthLevel {
        self.last_seen_errors = relay_errors;
        self.last_seen_warns = relay_warnings;
        self.reset_window();
        HealthLevel::Healthy
    }

    /// Сброс перед новой сессией.
    pub fn reset(&mut self) {
        self.last_seen_errors = 0;
        self.last_seen_warns = 0;
        self.reset_window();
    }

    fn reset_window(&mut self) {
        self.error_timestamps.clear();
        self.warn_timestamps.clear();
        self.current_level = Some(HealthLevel::Healthy);
        self.last_worse_ms = 0;
    }
}

fn evict_older_than(queue: &mut VecDeque<u64>, cutoff: u64) {
    while queue.front().is_some_and(|t| *t < cutoff) {
        queue.pop_front();
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_700_000_000_000;

    /// Тишина это Healthy, и первый же тик не должен принять стартовые
    /// счётчики за поток событий.
    #[test]
    fn quiet_session_is_healthy() {
        let mut t = HealthTracker::new();
        assert_eq!(t.update_at(T0, 0, 0), HealthLevel::Healthy);
        assert_eq!(t.update_at(T0 + 1_000, 0, 0), HealthLevel::Healthy);
    }

    /// Единичные предупреждения это фоновый шум, десяток с лишним уже повод
    /// присмотреться.
    #[test]
    fn warnings_walk_from_good_to_watching() {
        let mut t = HealthTracker::new();
        assert_eq!(t.update_at(T0, 0, 3), HealthLevel::Good);
        assert_eq!(t.update_at(T0 + 1_000, 0, 11), HealthLevel::Watching);
    }

    /// Три ошибки в окне это Hurt, пять за пять секунд уже Critical.
    #[test]
    fn errors_reach_hurt_then_critical() {
        let mut t = HealthTracker::new();
        assert_eq!(t.update_at(T0, 3, 0), HealthLevel::Hurt);
        assert_eq!(t.update_at(T0 + 1_000, 8, 0), HealthLevel::Critical);
    }

    /// Улучшение придерживается: сразу после всплеска ступень не падает, а
    /// через придержку сходит на спокойную.
    #[test]
    fn downshift_is_held() {
        let mut t = HealthTracker::new();
        assert_eq!(t.update_at(T0, 6, 0), HealthLevel::Critical);
        // Всплеск ещё в своём окне, ступень та же.
        assert_eq!(t.update_at(T0 + 3_000, 6, 0), HealthLevel::Critical);
        // Всплеск вышел из окна 5 секунд, придержка истекла, но сами ошибки
        // всё ещё в окне 30 секунд.
        assert_eq!(t.update_at(T0 + 8_000, 6, 0), HealthLevel::Hurt);
        // Ошибки вышли и из окна 30 секунд.
        assert_eq!(t.update_at(T0 + 40_000, 6, 0), HealthLevel::Healthy);
    }

    /// События стареют: тот же счётчик через минуту снова Healthy.
    #[test]
    fn events_age_out_of_the_window() {
        let mut t = HealthTracker::new();
        assert_eq!(t.update_at(T0, 0, 5), HealthLevel::Good);
        assert_eq!(t.update_at(T0 + 60_000, 0, 5), HealthLevel::Healthy);
    }

    /// Пропавшая сеть (XR-183): накопленные за офлайн ошибки не должны
    /// всплеском обрушить здоровье, когда связь вернётся.
    #[test]
    fn freeze_swallows_offline_errors() {
        let mut t = HealthTracker::new();
        assert_eq!(t.update_at(T0, 4, 0), HealthLevel::Hurt);
        assert_eq!(t.freeze_at(900, 900), HealthLevel::Healthy);
        // Первый тик после возвращения сети считает дельту от точки заморозки.
        assert_eq!(t.update_at(T0 + 1_000, 900, 900), HealthLevel::Healthy);
        assert_eq!(t.update_at(T0 + 2_000, 901, 900), HealthLevel::Healthy);
    }

    /// Перезапуск движка обнуляет счётчики: упавшая дельта это новая сессия, а
    /// не поток событий.
    #[test]
    fn counters_reset_does_not_spike() {
        let mut t = HealthTracker::new();
        t.update_at(T0, 50, 50);
        t.reset();
        assert_eq!(t.update_at(T0 + 1_000, 0, 0), HealthLevel::Healthy);
    }

    /// Скачок счётчика на тысячи событий подрезается: очередь меток не растёт
    /// без границ, а ступень и так уже на потолке.
    #[test]
    fn huge_delta_is_capped() {
        let mut t = HealthTracker::new();
        assert_eq!(t.update_at(T0, 100_000, 0), HealthLevel::Critical);
        assert_eq!(t.error_timestamps.len(), MAX_EVENTS_PER_TICK as usize);
    }

    #[test]
    fn level_names_are_stable() {
        assert_eq!(HealthLevel::Healthy.as_str(), "healthy");
        assert_eq!(HealthLevel::Critical.as_str(), "critical");
    }
}
