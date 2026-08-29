use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use argon2::Argon2;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use axum::Json;
use base64::Engine;
use password_hash::{PasswordHash, PasswordVerifier};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub username: String,
}

/// Сходится ли пароль с хэшем учётки админки. Неизвестное имя всё равно гоняет
/// проверку по фиктивному хэшу, чтобы «нет такого» не отличалось по времени от
/// «пароль не тот». Этим же вердиктом отвечает служебная ручка браузерного
/// входа (LLD-38 п. 3.2): второго пароля владельцу не заводим и второго места
/// хранения хэшей тоже.
pub fn password_matches(
    users: &[crate::config::UserConfig],
    username: &str,
    password: &str,
) -> bool {
    let Some(user) = users.iter().find(|u| u.username == username) else {
        let _ = Argon2::default().verify_password(
            password.as_bytes(),
            &PasswordHash::new(DUMMY_HASH).expect("built-in dummy hash parses"),
        );
        return false;
    };
    let Ok(parsed) = PasswordHash::new(&user.password_hash) else {
        tracing::error!("плохой хэш пароля в конфиге у {username}");
        return false;
    };
    Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok()
}

/// Фиктивный argon2-хэш для равного по времени отказа неизвестному имени.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// Счётчик попыток входа на источник (XR-195). Окно фиксированное: попытки
/// одного IP копятся `login_window_secs`, после `login_max_attempts` промахов
/// вход из этого источника отбивается сразу, без argon2, до конца окна.
/// Верный пароль снимает счётчик. Живёт в памяти: рестарт обнуляет, и это
/// осознанно, перебор через рестарт чужого процесса не ускоряется.
#[derive(Default)]
pub struct LoginAttempts {
    inner: std::sync::Mutex<HashMap<IpAddr, Window>>,
    max_attempts: u32,
    window_ms: u64,
}

#[derive(Default)]
struct Window {
    /// Начало текущего окна, мс от эпохи.
    start_ms: u64,
    attempts: u32,
}

impl LoginAttempts {
    pub fn new(max_attempts: u32, window_ms: u64) -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
            max_attempts,
            window_ms,
        }
    }

    /// Разрешена ли попытка из этого источника прямо сейчас. Нулевой лимит
    /// снимает ограничение целиком.
    pub fn allowed(&self, ip: &IpAddr, now_ms: u64) -> bool {
        if self.max_attempts == 0 {
            return true;
        }
        let map = self.inner.lock().expect("login attempts lock");
        map.get(ip).is_none_or(|w| {
            w.attempts < self.max_attempts || now_ms.saturating_sub(w.start_ms) >= self.window_ms
        })
    }

    /// Ещё одна попытка из источника: в свежем окне счётчик начинается заново.
    pub fn attempted(&self, ip: &IpAddr, now_ms: u64) {
        if self.max_attempts == 0 {
            return;
        }
        let mut map = self.inner.lock().expect("login attempts lock");
        let w = map.entry(*ip).or_default();
        if now_ms.saturating_sub(w.start_ms) >= self.window_ms {
            w.start_ms = now_ms;
            w.attempts = 0;
        }
        w.attempts = w.attempts.saturating_add(1);
    }

    /// Верный пароль снимает счётчик источника.
    pub fn succeeded(&self, ip: &IpAddr) {
        self.inner.lock().expect("login attempts lock").remove(ip);
    }
}

/// Адрес источника запроса. Слушатель кладёт его в соединение через
/// `into_make_service_with_connect_info`; источник без адреса (ручка позвана
/// мимо слушателя) сидит отдельной меткой, а не обходит лимит: тестовые
/// вызовы и любые обходные пути остаются под тем же ограничением.
pub struct SourceIp(pub IpAddr);

impl<S> axum::extract::FromRequestParts<S> for SourceIp
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let ip = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip())
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        Ok(SourceIp(ip))
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// POST /api/v1/auth/login validates credentials and returns a session token.
/// Попытки ограничены на источник, а argon2 уезжает в spawn_blocking: проверка
/// пароля занимает десятки миллисекунд CPU и 19 MiB памяти, и прямо в хендлере
/// шторм логина съедал бы воркеры рантайма (XR-195).
pub async fn login(
    State(state): State<Arc<AppState>>,
    SourceIp(ip): SourceIp,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, (StatusCode, String)> {
    if !state.login_attempts.allowed(&ip, now_ms()) {
        return Err((StatusCode::TOO_MANY_REQUESTS, "too many login attempts".into()));
    }
    state.login_attempts.attempted(&ip, now_ms());

    let users = state.config.admin.users.clone();
    let LoginRequest { username, password } = req;
    let creds = (username.clone(), password);
    let matches = tokio::task::spawn_blocking(move || {
        let (name, pass) = creds;
        password_matches(&users, &name, &pass)
    })
    .await
    .unwrap_or(false);

    if !matches {
        return Err((StatusCode::UNAUTHORIZED, "invalid credentials".into()));
    }
    state.login_attempts.succeeded(&ip);

    // Generate session token.
    let mut token_bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut token_bytes);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let key = b64.encode(token_bytes);

    // Store session: TTL и лимит на оператора применяет сам стор (XR-194).
    state.sessions.insert(key.clone(), username.clone()).await;

    Ok(Json(LoginResponse {
        token: key,
        username,
    }))
}

/// Bearer-токен из заголовка или пустая строка, если его нет.
fn bearer_token(request: &Request) -> &str {
    request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("")
}

/// Middleware that requires a valid session token. Просроченную сессию
/// отвергает и снимает с карты сам стор (XR-194).
pub async fn require_admin(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let provided = bearer_token(&request);

    if provided.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    if state.sessions.validate(provided).await.is_some() {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// POST /api/v1/auth/logout гасит Bearer из заголовка (XR-194). Ручка сидит
/// за `require_admin`, поэтому сюда доезжают только живые токены, и повторный
/// logout тем же токеном уже не проходит.
pub async fn logout(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> StatusCode {
    if state.sessions.remove(bearer_token(&request)).await {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::UNAUTHORIZED
    }
}

/// Hash a password for config file. Used by --hash-password CLI.
pub fn hash_password(password: &str) -> Result<String, String> {
    use argon2::password_hash::rand_core::OsRng;
    use argon2::password_hash::SaltString;
    use argon2::PasswordHasher;

    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| format!("hashing failed: {e}"))?;
    Ok(hash.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, last))
    }

    // XR-195: после исчерпания попыток источник сидит в отказе до конца окна,
    // а не навсегда.
    #[test]
    fn window_expires_and_allows_again() {
        let limiter = LoginAttempts::new(2, 60_000);
        let ip = ip(1);

        limiter.attempted(&ip, 1_000);
        limiter.attempted(&ip, 2_000);
        assert!(!limiter.allowed(&ip, 3_000), "лимит исчерпан внутри окна");
        assert!(limiter.allowed(&ip, 61_000), "новое окно открывает счётчик");

        limiter.attempted(&ip, 61_000);
        limiter.attempted(&ip, 62_000);
        assert!(!limiter.allowed(&ip, 62_500), "в новом окне лимит один и тот же");
    }

    // XR-195: верный пароль снимает счётчик источника, повторный вход не
    // упирается в чужие промахи.
    #[test]
    fn success_resets_counter() {
        let limiter = LoginAttempts::new(2, 60_000);
        let ip = ip(1);

        limiter.attempted(&ip, 1_000);
        limiter.succeeded(&ip);
        limiter.attempted(&ip, 1_002);
        assert!(limiter.allowed(&ip, 1_004), "после успеха счётчик начат заново");
        limiter.attempted(&ip, 1_005);
        assert!(!limiter.allowed(&ip, 1_006), "второй промах после успеха исчерпывает лимит");
    }

    // XR-195: источники не делят счётчик, лимит одного не запирает другого.
    #[test]
    fn sources_are_independent() {
        let limiter = LoginAttempts::new(1, 60_000);
        limiter.attempted(&ip(1), 1_000);

        assert!(!limiter.allowed(&ip(1), 1_001));
        assert!(limiter.allowed(&ip(2), 1_001));
    }

    // XR-195: нулевой лимит снимает ограничение целиком.
    #[test]
    fn zero_limit_disables_gate() {
        let limiter = LoginAttempts::new(0, 60_000);
        for _ in 0..10 {
            limiter.attempted(&ip(1), 1_000);
        }
        assert!(limiter.allowed(&ip(1), 1_001));
    }
}
