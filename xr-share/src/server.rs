//! HTTP(S) surface of the agent (LLD-19 §3.1). Two token-gated reads —
//! `GET /manifest` (the listing) and `GET /file/{*path}` (the bytes, with range
//! support for resume) — plus an unauthenticated `GET /healthz`. The hub is
//! never contacted: tokens are verified offline against the pinned hub key.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Path as AxPath, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ed25519_dalek::VerifyingKey;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use xr_proto::share::{verify_share_token, ShareManifest};

use crate::auth::extract_token;
use crate::manifest::build_manifest;
use crate::safepath::resolve_within;

/// Shared, immutable runtime state.
pub struct AgentState {
    /// Canonical share root (validated at startup).
    pub root: PathBuf,
    /// The share id a token must be bound to.
    pub share_id: String,
    /// Pinned hub key tokens are verified against.
    pub hub_key: VerifyingKey,
}

pub fn router(state: Arc<AgentState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/manifest", get(get_manifest))
        .route("/file/{*path}", get(serve_file))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

/// Verify the request carries a valid, unexpired token bound to this share.
fn authorize(state: &AgentState, req: &Request) -> Result<(), (StatusCode, &'static str)> {
    let token = extract_token(req.headers(), req.uri())
        .ok_or((StatusCode::UNAUTHORIZED, "missing or malformed token"))?;
    verify_share_token(&token, &state.hub_key, &state.share_id, now_unix())
        .map_err(|_| (StatusCode::FORBIDDEN, "token rejected"))
}

async fn get_manifest(
    State(state): State<Arc<AgentState>>,
    req: Request,
) -> Result<Json<ShareManifest>, (StatusCode, &'static str)> {
    authorize(&state, &req)?;
    let manifest = build_manifest(&state.root).map_err(|e| {
        tracing::error!("manifest build failed: {e:#}");
        (StatusCode::INTERNAL_SERVER_ERROR, "manifest error")
    })?;
    Ok(Json(manifest))
}

async fn serve_file(
    State(state): State<Arc<AgentState>>,
    AxPath(rel): AxPath<String>,
    req: Request,
) -> Response {
    if let Err(e) = authorize(&state, &req) {
        return e.into_response();
    }
    let safe = match resolve_within(&state.root, &rel) {
        Ok(p) => p,
        // Don't distinguish escape vs bad-component to the client.
        Err(_) => return (StatusCode::FORBIDDEN, "path rejected").into_response(),
    };

    // ServeFile handles Range / Content-Type / Last-Modified / 404, so resume
    // and content negotiation come for free once the path is proven safe.
    match ServeFile::new(&safe).oneshot(req).await {
        Ok(resp) => resp.map(Body::new),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "io error").into_response(),
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
