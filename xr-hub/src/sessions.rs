use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Одна admin-сессия: кому выдана, когда истекает и какой по счёту вошла.
/// Порядок входа нужен лимиту: вытеснять положено старейшую, а порядок
/// `HashMap` ничего не значит.
struct Session {
    username: String,
    expires_at: Instant,
    seq: u64,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, Session>,
    next_seq: u64,
}

/// Хранилище admin-сессий (XR-194). Раньше токен из `auth/login` жил до
/// рестарта процесса, а карта сессий только росла: утёкший Bearer нельзя
/// было погасить ничем. Теперь сессия истекает по TTL, `auth/logout` гасит
/// её по требованию, а лимит на оператора вытесняет старейшую вместо
/// бесконечного роста.
pub struct SessionStore {
    inner: RwLock<Inner>,
    ttl: Duration,
    max_per_user: usize,
    now: Clock,
}

impl SessionStore {
    pub fn new(ttl: Duration, max_per_user: usize) -> Self {
        Self::with_clock(ttl, max_per_user, Arc::new(Instant::now))
    }

    /// Часы приходят аргументом ради тестов: истечение проверяется сдвигом
    /// времени, а не сном на длину TTL.
    pub fn with_clock(ttl: Duration, max_per_user: usize, now: Clock) -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
            ttl,
            max_per_user,
            now,
        }
    }

    /// Записывает новую сессию. Протухшие вычищаются заодно, а когда у
    /// оператора сессий уже столько, сколько allows лимит, старейшая его
    /// сессия вытесняется. `max_per_user == 0` снимает ограничение.
    pub async fn insert(&self, session_token: String, username: String) {
        let now = (self.now)();
        let mut inner = self.inner.write().await;
        inner.sessions.retain(|_, s| s.expires_at > now);
        if self.max_per_user > 0 {
            let oldest = inner
                .sessions
                .iter()
                .filter(|(_, s)| s.username == username)
                .min_by_key(|(_, s)| s.seq)
                .map(|(token, _)| token.clone());
            let count = inner
                .sessions
                .values()
                .filter(|s| s.username == username)
                .count();
            if count >= self.max_per_user {
                if let Some(oldest) = oldest {
                    inner.sessions.remove(&oldest);
                }
            }
        }
        let seq = inner.next_seq;
        inner.next_seq += 1;
        inner.sessions.insert(
            session_token,
            Session {
                username,
                expires_at: now + self.ttl,
                seq,
            },
        );
    }

    /// Возвращает оператора живой сессии. Протухшая снимается с карты на
    /// месте, чтобы отказ по TTL заодно возвращал память.
    pub async fn validate(&self, token: &str) -> Option<String> {
        let now = (self.now)();
        let mut inner = self.inner.write().await;
        match inner.sessions.get(token) {
            Some(s) if s.expires_at > now => Some(s.username.clone()),
            Some(_) => {
                inner.sessions.remove(token);
                None
            }
            None => None,
        }
    }

    /// Гасит сессию logout'ом. Отвечает, была ли она жива.
    pub async fn remove(&self, token: &str) -> bool {
        self.inner.write().await.sessions.remove(token).is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// Часы со сдвигаемым временем: `advance` двигает «сейчас» на секунды
    /// вперёд, не ночуя в тесте.
    struct ShiftedClock(Arc<AtomicU64>);

    impl ShiftedClock {
        fn new() -> (Self, Clock) {
            let offset = Arc::new(AtomicU64::new(0));
            let clock = {
                let offset = offset.clone();
                Arc::new(move || Instant::now() + Duration::from_secs(offset.load(Ordering::SeqCst)))
                    as Clock
            };
            (Self(offset), clock)
        }

        fn advance(&self, secs: u64) {
            self.0.fetch_add(secs, Ordering::SeqCst);
        }
    }

    fn store(ttl: Duration, max: usize) -> (ShiftedClock, SessionStore) {
        let (clock, now) = ShiftedClock::new();
        (clock, SessionStore::with_clock(ttl, max, now))
    }

    #[tokio::test]
    async fn expired_token_is_rejected_and_dropped() {
        let (clock, sessions) = store(Duration::from_secs(60), 5);
        sessions.insert("tok".into(), "root".into()).await;

        assert_eq!(sessions.validate("tok").await.as_deref(), Some("root"));
        clock.advance(61);
        assert_eq!(sessions.validate("tok").await, None);
        assert!(
            !sessions.remove("tok").await,
            "протухшая сессия обязана уйти из карты при проверке"
        );
    }

    #[tokio::test]
    async fn limit_evicts_oldest_session_of_same_user_only() {
        let (_clock, sessions) = store(Duration::from_secs(3600), 2);
        sessions.insert("first".into(), "root".into()).await;
        sessions.insert("other".into(), "peer".into()).await;
        sessions.insert("second".into(), "root".into()).await;
        sessions.insert("third".into(), "root".into()).await;

        assert_eq!(sessions.validate("first").await, None, "старейшая вытеснена");
        assert_eq!(sessions.validate("second").await.as_deref(), Some("root"));
        assert_eq!(sessions.validate("third").await.as_deref(), Some("root"));
        assert_eq!(
            sessions.validate("other").await.as_deref(),
            Some("peer"),
            "лимит одного оператора чужие сессии не трогает"
        );
    }

    #[tokio::test]
    async fn insert_sweeps_expired_sessions() {
        let (clock, sessions) = store(Duration::from_secs(60), 0);
        sessions.insert("stale".into(), "root".into()).await;
        clock.advance(61);
        sessions.insert("fresh".into(), "root".into()).await;

        assert_eq!(sessions.validate("stale").await, None);
        assert_eq!(sessions.validate("fresh").await.as_deref(), Some("root"));
    }

    #[tokio::test]
    async fn logout_removes_only_target_session() {
        let (_clock, sessions) = store(Duration::from_secs(3600), 5);
        sessions.insert("one".into(), "root".into()).await;
        sessions.insert("two".into(), "root".into()).await;

        assert!(sessions.remove("one").await);
        assert_eq!(sessions.validate("one").await, None);
        assert_eq!(sessions.validate("two").await.as_deref(), Some("root"));
    }
}
