use std::path::Path;
use std::sync::Arc;

use axum::extract::{self, State};
use axum::http::StatusCode;
use axum::response::Html;
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

/// GET /invite/:token/view — HTML page with invite info and QR code.
pub async fn view_invite(
    State(state): State<Arc<AppState>>,
    extract::Path(token): extract::Path<String>,
) -> Result<Html<String>, (StatusCode, String)> {
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

    let preset = &invite.payload.preset;
    let comment = &invite.comment;
    let expires = format_datetime(&invite.expires_at);
    let status_badge = match status {
        "active" => r#"<span style="color:#2e7d32;font-weight:600">Active</span>"#,
        "expired" => r#"<span style="color:#999">Expired</span>"#,
        "consumed" => r#"<span style="color:#f57c00">Already used</span>"#,
        _ => status,
    };

    // QR code via inline SVG from qrserver API (simple, no JS needed)
    let qr_data = format!("/api/v1/invite/{}/claim", token);

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>xr-proxy Invite</title>
<style>
  body {{ font-family: -apple-system, system-ui, sans-serif; background: #f5f5f5; color: #333; display: flex; justify-content: center; padding: 2rem; }}
  @media (prefers-color-scheme: dark) {{ body {{ background: #1a1a2e; color: #e0e0e0; }} .card {{ background: #16213e; }} }}
  .card {{ background: #fff; border-radius: 12px; padding: 2rem; max-width: 400px; width: 100%; box-shadow: 0 2px 8px rgba(0,0,0,0.1); text-align: center; }}
  h1 {{ font-size: 1.5rem; margin-bottom: 0.5rem; }}
  .meta {{ color: #888; font-size: 0.9rem; margin-bottom: 1rem; }}
  .field {{ text-align: left; margin-bottom: 0.75rem; }}
  .field-label {{ font-size: 0.75rem; color: #888; text-transform: uppercase; }}
  .field-value {{ font-size: 1rem; }}
  .qr {{ margin: 1.5rem 0; }}
  .qr img {{ border-radius: 8px; }}
  .btn {{ display: inline-block; padding: 0.75rem 2rem; background: #1a1a2e; color: #fff; border: none; border-radius: 6px; font-size: 1rem; text-decoration: none; cursor: pointer; }}
  .btn:disabled, .btn.disabled {{ opacity: 0.4; pointer-events: none; }}
</style>
</head>
<body>
<div class="card">
  <h1>xr-proxy Invite</h1>
  <p class="meta">Scan this QR code in the xr-proxy app to connect</p>
  <div class="field"><div class="field-label">Preset</div><div class="field-value">{preset}</div></div>
  <div class="field"><div class="field-label">Status</div><div class="field-value">{status_badge}</div></div>
  <div class="field"><div class="field-label">Expires</div><div class="field-value">{expires}</div></div>
  {comment_html}
  <div class="qr">
    <img src="https://api.qrserver.com/v1/create-qr-code/?size=200x200&amp;data={qr_data_encoded}" width="200" height="200" alt="QR Code">
  </div>
</div>
</body>
</html>"#,
        preset = preset,
        status_badge = status_badge,
        expires = expires,
        comment_html = if comment.is_empty() {
            String::new()
        } else {
            format!(r#"<div class="field"><div class="field-label">Comment</div><div class="field-value">{comment}</div></div>"#)
        },
        qr_data_encoded = urlencoding(&qr_data),
    );

    Ok(Html(html))
}

/// Format RFC3339 datetime to human-readable "YYYY-MM-DD HH:MM:SS UTC".
fn format_datetime(rfc3339: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|_| rfc3339.to_string())
}

fn urlencoding(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' | '/' => c.to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}

/// POST /invite/:token/claim — return full payload and consume one-time invites.
pub async fn claim_invite(
    State(state): State<Arc<AppState>>,
    extract::Path(token): extract::Path<String>,
    headers: axum::http::HeaderMap,
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

    // Extract client IP (X-Real-IP from nginx, or direct connection).
    let client_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .or_else(|| {
            headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(',').next())
                .map(|s| s.trim().to_string())
        })
        ;

    let payload = invite.payload.clone();

    // Consume one-time invites (unless dev_mode).
    if invite.one_time && !state.config.invites.dev_mode {
        invite.consumed_at = Some(now);
        invite.claimed_by_ip = client_ip;
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
        claimed_by_ip: None,
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
