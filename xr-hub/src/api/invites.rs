use std::path::Path;
use std::sync::Arc;

use axum::extract::{self, State};
use axum::http::StatusCode;
use axum::Json;
use base64::Engine;
use serde::Deserialize;
use xr_proto::preset::{Invite, InviteInfo, InvitePayload};

use crate::config::InviteDefaults;
use crate::state::AppState;
use crate::storage;

// ── Public ──────────────────────────────────────────────────────────

/// GET /invite/:token — return metadata without secrets. Does NOT consume.
pub async fn get_invite_info(
    State(state): State<Arc<AppState>>,
    extract::Path(token): extract::Path<String>,
) -> Result<Json<InviteInfo>, (StatusCode, String)> {
    let invites = state.invites.read().await;
    let invite = invites
        .get(&token)
        .ok_or((StatusCode::NOT_FOUND, "invite not found".into()))?;

    let now = chrono::Utc::now().to_rfc3339();
    let status = if invite.consumed_at.is_some() {
        "consumed"
    } else if invite.expires_at <= now {
        "expired"
    } else {
        "active"
    };

    Ok(Json(InviteInfo {
        token: invite.token.clone(),
        preset: invite.payload.preset.clone(),
        comment: invite.comment.clone(),
        status: status.into(),
        expires_at: invite.expires_at.clone(),
    }))
}

/// POST /invite/:token/claim — return full payload and consume one-time invites.
pub async fn claim_invite(
    State(state): State<Arc<AppState>>,
    extract::Path(token): extract::Path<String>,
) -> Result<Json<InvitePayload>, (StatusCode, String)> {
    let mut invites = state.invites.write().await;
    let invite = invites
        .get_mut(&token)
        .ok_or((StatusCode::NOT_FOUND, "invite not found".into()))?;

    let now = chrono::Utc::now().to_rfc3339();
    if invite.expires_at <= now {
        return Err((StatusCode::GONE, "invite expired".into()));
    }
    if invite.consumed_at.is_some() {
        return Err((StatusCode::GONE, "invite already used".into()));
    }

    let payload = invite.payload.clone();

    // Consume one-time invites (unless dev_mode).
    if invite.one_time && !state.config.invites.dev_mode {
        invite.consumed_at = Some(now);
        let data_dir = Path::new(&state.config.server.data_dir);
        let _ = storage::save_invite(data_dir, invite);
    }

    Ok(Json(payload))
}

// ── Admin ───────────────────────────────────────────────────────────

pub async fn list_invites(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<Invite>> {
    let invites = state.invites.read().await;
    let mut list: Vec<Invite> = invites.values().cloned().collect();
    list.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    list.truncate(1000);
    Json(list)
}

/// GET /admin/invite-defaults — return default payload values from config.
pub async fn get_invite_defaults(
    State(state): State<Arc<AppState>>,
) -> Json<InviteDefaults> {
    Json(state.config.invites.defaults.clone())
}

#[derive(Debug, Deserialize)]
pub struct CreateInviteRequest {
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    #[serde(default = "default_true")]
    pub one_time: bool,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub payload: Option<InvitePayload>,
}

fn default_true() -> bool {
    true
}

pub async fn create_invite(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateInviteRequest>,
) -> Result<(StatusCode, Json<Invite>), (StatusCode, String)> {
    let ttl = req.ttl_seconds.unwrap_or(state.config.invites.default_ttl_seconds);
    if ttl > state.config.invites.max_ttl_seconds {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("TTL exceeds maximum of {} seconds", state.config.invites.max_ttl_seconds),
        ));
    }

    // Build payload from explicit values or defaults.
    let defaults = &state.config.invites.defaults;
    let payload = if let Some(p) = req.payload {
        p
    } else {
        let preset_name = req.preset.unwrap_or_default();
        InvitePayload {
            server_address: defaults.server_address.clone(),
            server_port: defaults.server_port,
            obfuscation_key: defaults.obfuscation_key.clone(),
            modifier: defaults.modifier.clone(),
            salt: defaults.salt,
            preset: preset_name,
            hub_url: defaults.hub_url.clone(),
        }
    };

    // Generate random 16-byte token, base64url without padding.
    let mut token_bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut token_bytes);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token_bytes);

    let now = chrono::Utc::now();
    let expires = now + chrono::Duration::seconds(ttl as i64);

    let invite = Invite {
        token,
        created_at: now.to_rfc3339(),
        expires_at: expires.to_rfc3339(),
        consumed_at: None,
        one_time: req.one_time,
        comment: req.comment,
        payload,
    };

    let data_dir = Path::new(&state.config.server.data_dir);
    storage::save_invite(data_dir, &invite)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut invites = state.invites.write().await;
    invites.insert(invite.token.clone(), invite.clone());

    Ok((StatusCode::CREATED, Json(invite)))
}

pub async fn revoke_invite(
    State(state): State<Arc<AppState>>,
    extract::Path(token): extract::Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let mut invites = state.invites.write().await;
    let invite = invites
        .get_mut(&token)
        .ok_or((StatusCode::NOT_FOUND, "invite not found".into()))?;

    let now = chrono::Utc::now().to_rfc3339();
    invite.consumed_at = Some(now);

    let data_dir = Path::new(&state.config.server.data_dir);
    storage::save_invite(data_dir, invite)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}
