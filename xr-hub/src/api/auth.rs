use std::sync::Arc;

use argon2::Argon2;
use axum::extract::{Request, State};
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
pub fn password_matches(config: &crate::config::HubConfig, username: &str, password: &str) -> bool {
    let Some(user) = config.admin.users.iter().find(|u| u.username == username) else {
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

/// POST /api/v1/auth/login validates credentials and returns a session token.
pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, (StatusCode, String)> {
    if !password_matches(&state.config, &req.username, &req.password) {
        return Err((StatusCode::UNAUTHORIZED, "invalid credentials".into()));
    }

    // Generate session token.
    let mut token_bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut token_bytes);
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let key = b64.encode(token_bytes);

    // Store session: TTL и лимит на оператора применяет сам стор (XR-194).
    state
        .sessions
        .insert(key.clone(), req.username.clone())
        .await;

    Ok(Json(LoginResponse {
        token: key,
        username: req.username,
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
