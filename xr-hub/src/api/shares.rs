//! Share index endpoints (LLD-19, XR-027).
//!
//! The hub is a *telephone book*: it stores a [`ShareRecord`] (name + address +
//! the agent's pinned public key) and mints short-lived [`ShareToken`]s signed
//! with the hub's ed25519 key. It never stores or relays file bytes — the agent
//! (`xr-share`) holds the data and verifies tokens offline. Routes:
//!
//! - Public:  `GET  /api/v1/shares`              → consumer view (name+addr+key)
//! - Admin:   `GET  /api/v1/admin/shares`        → full records
//!            `POST /api/v1/admin/shares`        → register address:port + pubkey
//!            `DELETE /api/v1/admin/shares/:id`  → unregister
//!            `POST /api/v1/admin/shares/:id/token` → mint a signed access token

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{self, State};
use axum::http::StatusCode;
use axum::Json;
use base64::Engine;
use serde::Deserialize;
use xr_proto::share::{sign_share_token, ShareInfo, ShareRecord, ShareToken};

use crate::state::AppState;
use crate::storage;

/// Default token lifetime when the request omits `ttl_seconds`: 7 days.
const DEFAULT_TOKEN_TTL_SECONDS: u64 = 7 * 24 * 3600;
/// Hard cap on token lifetime: 30 days. A share token is a bearer capability,
/// so the TTL is the primary revocation lever (§5.6) — keep it bounded.
const MAX_TOKEN_TTL_SECONDS: u64 = 30 * 24 * 3600;

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── Public ──────────────────────────────────────────────────────────

/// GET /api/v1/shares — the consumer-facing index: for every registered share,
/// just enough to reach and pin the agent (name + addr:port + pubkey). No
/// owner-side bookkeeping, no file listing (that comes from the agent).
pub async fn list_shares(State(state): State<Arc<AppState>>) -> Json<Vec<ShareInfo>> {
    let shares = state.shares.read().await;
    let mut list: Vec<ShareInfo> = shares.values().map(ShareRecord::info).collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    Json(list)
}

// ── Admin ───────────────────────────────────────────────────────────

/// GET /api/v1/admin/shares — full records (incl. owner/comment/created_at).
pub async fn admin_list_shares(State(state): State<Arc<AppState>>) -> Json<Vec<ShareRecord>> {
    let shares = state.shares.read().await;
    let mut list: Vec<ShareRecord> = shares.values().cloned().collect();
    list.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Json(list)
}

#[derive(Debug, Deserialize)]
pub struct CreateShareRequest {
    pub name: String,
    #[serde(default)]
    pub owner: String,
    /// Reachable host or IP of the agent (manual entry — owner owns reachability).
    pub addr: String,
    pub port: u16,
    /// Base64 (standard) ed25519 public key the consumer will pin (TOFU).
    pub agent_pubkey: String,
    #[serde(default)]
    pub comment: String,
}

/// POST /api/v1/admin/shares — register a share. Validates the address and the
/// agent pubkey, generates an opaque `share_id`, and persists the record.
pub async fn create_share(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateShareRequest>,
) -> Result<(StatusCode, Json<ShareRecord>), (StatusCode, String)> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name must not be empty".into()));
    }
    let addr = req.addr.trim();
    if addr.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "addr must not be empty".into()));
    }
    if req.port == 0 {
        return Err((StatusCode::BAD_REQUEST, "port must be non-zero".into()));
    }
    // The pubkey is what the consumer pins — reject anything that is not a
    // 32-byte ed25519 key so a typo can't be saved as an unpinnable identity.
    validate_ed25519_pubkey(&req.agent_pubkey)?;

    // Opaque, filename-safe id (16 random bytes, base64url no-pad — same shape
    // as invite tokens).
    let mut id_bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut id_bytes);
    let share_id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id_bytes);

    let share = ShareRecord {
        share_id,
        name: name.to_string(),
        owner: req.owner.trim().to_string(),
        addr: addr.to_string(),
        port: req.port,
        agent_pubkey: req.agent_pubkey.trim().to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        comment: req.comment.trim().to_string(),
    };

    let data_dir = Path::new(&state.config.server.data_dir);
    storage::save_share(data_dir, &share)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut shares = state.shares.write().await;
    shares.insert(share.share_id.clone(), share.clone());

    Ok((StatusCode::CREATED, Json(share)))
}

/// DELETE /api/v1/admin/shares/:id — unregister a share.
pub async fn delete_share(
    State(state): State<Arc<AppState>>,
    extract::Path(share_id): extract::Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let mut shares = state.shares.write().await;
    // Only act on ids we actually hold — this also keeps a crafted id (`../…`)
    // from ever reaching the filesystem helper.
    if shares.remove(&share_id).is_none() {
        return Err((StatusCode::NOT_FOUND, "share not found".into()));
    }
    let data_dir = Path::new(&state.config.server.data_dir);
    storage::delete_share_file(data_dir, &share_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct MintTokenRequest {
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

/// POST /api/v1/admin/shares/:id/token — mint a share-access token signed with
/// the hub key. The owner hands this token out-of-band to a consumer, who
/// presents it to the agent; the agent verifies it offline (the hub is never in
/// the data-path).
pub async fn mint_token(
    State(state): State<Arc<AppState>>,
    extract::Path(share_id): extract::Path<String>,
    Json(req): Json<MintTokenRequest>,
) -> Result<Json<ShareToken>, (StatusCode, String)> {
    // Signing must be configured (same key as presets/app-update, LLD-01).
    let signing = state.signing.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "hub signing key not configured".into(),
    ))?;

    {
        let shares = state.shares.read().await;
        if !shares.contains_key(&share_id) {
            return Err((StatusCode::NOT_FOUND, "share not found".into()));
        }
    }

    let ttl = req.ttl_seconds.unwrap_or(DEFAULT_TOKEN_TTL_SECONDS);
    if ttl == 0 || ttl > MAX_TOKEN_TTL_SECONDS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("ttl_seconds must be 1..={MAX_TOKEN_TTL_SECONDS}"),
        ));
    }
    let exp = now_unix().saturating_add(ttl);

    let token = sign_share_token(&signing.signing_key, &share_id, exp);
    Ok(Json(token))
}

/// Reject anything that is not a standard-base64 32-byte ed25519 public key.
fn validate_ed25519_pubkey(b64: &str) -> Result<(), (StatusCode, String)> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "agent_pubkey must be valid base64".into(),
            )
        })?;
    if bytes.len() != 32 {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("agent_pubkey must be 32 bytes, got {}", bytes.len()),
        ));
    }
    Ok(())
}
