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

/// POST /api/v1/auth/login — validate credentials, return session token.
pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, (StatusCode, String)> {
    // Find user by username.
    let user = state
        .config
        .admin
        .users
        .iter()
        .find(|u| u.username == req.username);

    let user = match user {
        Some(u) => u,
        None => {
            // Constant-time: still do a dummy hash verify to prevent timing attacks.
            let _ = Argon2::default().verify_password(
                req.password.as_bytes(),
                &PasswordHash::new("$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap(),
            );
            return Err((StatusCode::UNAUTHORIZED, "invalid credentials".into()));
        }
    };

    // Verify password against stored hash.
    let parsed_hash = PasswordHash::new(&user.password_hash)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("bad hash in config: {e}")))?;

    if Argon2::default()
        .verify_password(req.password.as_bytes(), &parsed_hash)
        .is_err()
    {
        return Err((StatusCode::UNAUTHORIZED, "invalid credentials".into()));
    }

    // Generate session token.
    let mut token_bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut token_bytes);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token_bytes);

    // Store session.
    let mut sessions = state.sessions.write().await;
    sessions.insert(token.clone(), req.username.clone());

    Ok(Json(LoginResponse {
        token,
        username: req.username,
    }))
}

/// Middleware that requires a valid session token.
pub async fn require_admin(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let provided = header.strip_prefix("Bearer ").unwrap_or("");

    if provided.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let sessions = state.sessions.read().await;
    if sessions.contains_key(provided) {
        drop(sessions);
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
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
