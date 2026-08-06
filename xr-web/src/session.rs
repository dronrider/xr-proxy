//! Сессии браузера и их cookie (LLD-38 п. 2.2, п. 3.2).
//!
//! Своей учётной базы у фронта нет: пароль проверяет хаб, а здесь живёт только
//! то, чего хаб не умеет, cookie. Сессии лежат в памяти процесса, рестарт
//! разлогинивает всех, на диск не пишется ничего.
//!
//! Cookie host-only: атрибута `Domain` в ней нет, поэтому браузер не отдаёт
//! сессию публикации `dash` публикации `notes`. Сервер держит тот же рубеж
//! своими руками: сессия помнит, на какой публикации её открыли, и на чужом
//! поддомене не считается. Одного браузера тут мало, cookie ходят и без него.

use std::collections::HashMap;
use std::sync::Mutex;

use base64::Engine;
use rand::RngCore;

/// Имя cookie сессии.
pub const COOKIE_NAME: &str = "xrweb";

/// Атрибуты cookie: `Domain` тут нет намеренно (host-only, п. 3.2),
/// `SameSite=Lax` режет cross-site POST, `HttpOnly` прячет токен от скриптов
/// проксируемого приложения.
fn cookie_attrs(max_age_secs: u64) -> String {
    format!("HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age={max_age_secs}")
}

/// Заголовок `Set-Cookie` для свежей сессии.
pub fn set_cookie(token: &str, ttl_secs: u64) -> String {
    format!("{COOKIE_NAME}={token}; {}", cookie_attrs(ttl_secs))
}

/// Заголовок `Set-Cookie`, снимающий сессию у браузера.
pub fn clear_cookie() -> String {
    format!("{COOKIE_NAME}=; {}", cookie_attrs(0))
}

/// Токен сессии из заголовка `Cookie`. Чужие cookie приложения тут не мешают:
/// разбираются все пары, берётся своя.
pub fn token_from_cookie_header(raw: &str) -> Option<String> {
    cookie_pairs(raw)
        .find(|(name, _)| *name == COOKIE_NAME)
        .and_then(|(_, value)| value)
        .map(str::to_string)
}

/// Заголовок `Cookie` без своей пары: браузерная сессия фронта приложению за
/// апстримом не нужна, а его собственные cookie обязаны доехать целыми.
/// `None` значит от заголовка не осталось ничего и его надо снять.
pub fn cookie_header_without_session(raw: &str) -> Option<String> {
    let kept: Vec<String> = cookie_pairs(raw)
        .filter(|(name, _)| *name != COOKIE_NAME)
        .map(|(name, value)| match value {
            Some(value) => format!("{name}={value}"),
            None => name.to_string(),
        })
        .collect();
    (!kept.is_empty()).then(|| kept.join("; "))
}

fn cookie_pairs(raw: &str) -> impl Iterator<Item = (&str, Option<&str>)> {
    raw.split(';').filter_map(|pair| {
        let pair = pair.trim();
        if pair.is_empty() {
            return None;
        }
        match pair.split_once('=') {
            Some((name, value)) => Some((name.trim(), Some(value.trim()))),
            None => Some((pair, None)),
        }
    })
}

struct Session {
    publication: String,
    username: String,
    /// Момент, после которого сессия мертва, unix-секунды. Двигается при
    /// каждой активности: владелец, который держит вкладку открытой, не
    /// разлогинивается посреди работы.
    expires: u64,
}

/// Сессии в памяти с TTL и продлением при активности.
pub struct Sessions {
    ttl_secs: u64,
    inner: Mutex<HashMap<String, Session>>,
}

impl Sessions {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            ttl_secs,
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn ttl_secs(&self) -> u64 {
        self.ttl_secs
    }

    /// Открыть сессию на публикации и отдать её токен.
    pub fn open(&self, publication: &str, username: &str, now: u64) -> String {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let mut guard = self.inner.lock().expect("sessions lock");
        // Протухшие выметаются на входе: своего таймера у хранилища нет, а
        // расти без границы память фронта не должна.
        guard.retain(|_, s| s.expires > now);
        guard.insert(
            token.clone(),
            Session {
                publication: publication.to_string(),
                username: username.to_string(),
                expires: now.saturating_add(self.ttl_secs),
            },
        );
        token
    }

    /// Живая ли это сессия на этой публикации. Живую заодно продлевает и
    /// возвращает имя владельца.
    pub fn touch(&self, token: &str, publication: &str, now: u64) -> Option<String> {
        let mut guard = self.inner.lock().expect("sessions lock");
        let session = guard.get_mut(token)?;
        if session.expires <= now {
            guard.remove(token);
            return None;
        }
        // Сессия одной публикации не открывает другую, даже если токен принесли
        // руками мимо браузера.
        if session.publication != publication {
            return None;
        }
        session.expires = now.saturating_add(self.ttl_secs);
        Some(session.username.clone())
    }

    /// Снять сессию (логаут). Возвращает `true`, если было что снимать.
    pub fn close(&self, token: &str) -> bool {
        self.inner.lock().expect("sessions lock").remove(token).is_some()
    }

    /// Сколько сессий помнит фронт: только для тестов и диагностики.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("sessions lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: u64 = 7 * 24 * 3600;

    #[test]
    fn cookie_carries_host_only_flags() {
        let header = set_cookie("tok", TTL);
        assert!(header.starts_with("xrweb=tok;"), "{header}");
        for flag in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/"] {
            assert!(header.contains(flag), "в cookie нет {flag}: {header}");
        }
        assert!(
            !header.to_ascii_lowercase().contains("domain"),
            "cookie обязана быть host-only, без Domain: {header}"
        );
        assert!(clear_cookie().contains("Max-Age=0"), "логаут снимает cookie");
    }

    #[test]
    fn token_is_taken_from_a_crowd_of_cookies() {
        let raw = "app_sid=abc; xrweb=tok-1; theme=dark";
        assert_eq!(token_from_cookie_header(raw).as_deref(), Some("tok-1"));
        assert_eq!(token_from_cookie_header("app_sid=abc").as_deref(), None);
        // Cookie приложения едут дальше, своя снимается: приложению за
        // апстримом сессия фронта не нужна.
        assert_eq!(
            cookie_header_without_session(raw).as_deref(),
            Some("app_sid=abc; theme=dark")
        );
        assert_eq!(cookie_header_without_session("xrweb=tok-1"), None);
    }

    #[test]
    fn session_expires_and_activity_renews_it() {
        let s = Sessions::new(100);
        let token = s.open("dash", "owner", 1_000);
        assert_eq!(s.touch(&token, "dash", 1_099).as_deref(), Some("owner"));
        // Продление сдвинуло срок: без него на 1_150 сессия была бы мертва.
        assert_eq!(s.touch(&token, "dash", 1_150).as_deref(), Some("owner"));
        assert_eq!(s.touch(&token, "dash", 1_400), None, "протухшая не пускает");
        assert!(s.is_empty(), "протухшая сессия не остаётся в памяти");
    }

    #[test]
    fn session_of_one_publication_does_not_open_another() {
        let s = Sessions::new(TTL);
        let token = s.open("dash", "owner", 0);
        assert_eq!(s.touch(&token, "notes", 1), None);
        assert_eq!(s.touch(&token, "dash", 1).as_deref(), Some("owner"));
    }

    #[test]
    fn logout_kills_the_session() {
        let s = Sessions::new(TTL);
        let token = s.open("dash", "owner", 0);
        assert!(s.close(&token));
        assert_eq!(s.touch(&token, "dash", 1), None);
        assert!(!s.close(&token), "повторный логаут снимать уже нечего");
    }

    #[test]
    fn tokens_do_not_repeat() {
        let s = Sessions::new(TTL);
        let a = s.open("dash", "owner", 0);
        let b = s.open("dash", "owner", 0);
        assert_ne!(a, b);
        assert_eq!(s.len(), 2);
    }
}
