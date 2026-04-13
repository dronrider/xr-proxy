use std::sync::Arc;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use subtle::ConstantTimeEq;

use crate::state::AppState;

/// Middleware that requires a valid admin Bearer token.
pub async fn require_admin_token(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let provided = header.strip_prefix("Bearer ").unwrap_or("");
    let expected = &state.config.admin.token;

    if provided.as_bytes().ct_eq(expected.as_bytes()).into() {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}
