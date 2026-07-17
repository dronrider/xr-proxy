use std::path::Path;
use std::sync::Arc;

use axum::extract::{self, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::Json;
use base64::Engine;
use serde::Deserialize;
use xr_proto::invite_url::build_https_url;
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

    // QR кодирует каноническую ссылку https://<host>/invite/<token> (LLD-04):
    // относительный путь приложение не парсит. Хост берём из hub_url инвайта,
    // при пустом из дефолтов конфига хаба.
    let hub_url = if invite.payload.hub_url.is_empty() {
        state.config.invites.defaults.hub_url.as_str()
    } else {
        invite.payload.hub_url.as_str()
    };
    let qr_data = build_https_url(hub_url, &token);

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
    let invite = build_invite(
        &state,
        req.ttl_seconds,
        req.one_time,
        req.comment,
        req.preset,
        req.payload,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(invite)))
}

/// Build, persist and register an invite. Shared by the admin endpoint and the
/// combined setup-token (XR-127). With `payload` given it is used verbatim;
/// otherwise the payload is filled from the hub's invite defaults.
pub(crate) async fn build_invite(
    state: &AppState,
    ttl_seconds: Option<u64>,
    one_time: bool,
    comment: String,
    preset: Option<String>,
    payload: Option<InvitePayload>,
) -> Result<Invite, (StatusCode, String)> {
    let ttl = ttl_seconds.unwrap_or(state.config.invites.default_ttl_seconds);
    if ttl > state.config.invites.max_ttl_seconds {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("TTL exceeds maximum of {} seconds", state.config.invites.max_ttl_seconds),
        ));
    }

    // Build payload from explicit values or defaults.
    let defaults = &state.config.invites.defaults;
    let payload = if let Some(p) = payload {
        p
    } else {
        let preset_name = preset.unwrap_or_default();
        // Пул серверов из конфига хаба (LLD-10 п. 2.8); legacy-поля всегда
        // несут primary, чтобы старое приложение работало по ним как раньше.
        let servers = defaults.sorted_servers();
        let (server_address, server_port) = servers
            .first()
            .map(|s| (s.address.clone(), s.port))
            .unwrap_or_else(|| (defaults.server_address.clone(), defaults.server_port));
        InvitePayload {
            server_address,
            server_port,
            obfuscation_key: defaults.obfuscation_key.clone(),
            modifier: defaults.modifier.clone(),
            salt: defaults.salt,
            preset: preset_name,
            hub_url: defaults.hub_url.clone(),
            servers,
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
        one_time,
        comment,
        payload,
        share_ids: Vec::new(),
        write_share_ids: Vec::new(),
    };

    let data_dir = Path::new(&state.config.server.data_dir);
    storage::save_invite(data_dir, &invite)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    state.invites.write().await.insert(invite.token.clone(), invite.clone());

    Ok(invite)
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tokio::sync::RwLock;

    use super::*;

    const TOKEN: &str = "abcdefghij0123456789AB";

    fn state_with_invite(payload_hub_url: &str, default_hub_url: &str) -> Arc<AppState> {
        let mut config: crate::config::HubConfig =
            toml::from_str("[server]\n[admin]\nusers = []").unwrap();
        config.invites.defaults.hub_url = default_hub_url.into();

        let invite = Invite {
            token: TOKEN.into(),
            created_at: "2026-01-01T00:00:00+00:00".into(),
            expires_at: "2099-01-01T00:00:00+00:00".into(),
            consumed_at: None,
            claimed_by_ip: None,
            one_time: true,
            comment: String::new(),
            payload: InvitePayload {
                server_address: "203.0.113.10".into(),
                server_port: 8443,
                obfuscation_key: String::new(),
                modifier: "positional_xor_rotate".into(),
                salt: 0,
                preset: "russia".into(),
                hub_url: payload_hub_url.into(),
                servers: Vec::new(),
            },
            share_ids: Vec::new(),
            write_share_ids: Vec::new(),
        };

        let mut invites = HashMap::new();
        invites.insert(invite.token.clone(), invite);

        Arc::new(AppState {
            presets: RwLock::new(HashMap::new()),
            invites: RwLock::new(invites),
            shares: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            config,
            signing: None,
        })
    }

    /// Вытащить содержимое QR из HTML страницы /view: значение параметра
    /// data в src картинки qrserver, percent-декодированное обратно.
    fn qr_data_from_view(html: &str) -> String {
        let start = html.find("&amp;data=").expect("no qr data in html") + "&amp;data=".len();
        let end = start + html[start..].find('"').expect("unterminated img src");
        percent_decode(&html[start..end])
    }

    fn percent_decode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                out.push(u8::from_str_radix(hex, 16).unwrap());
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    async fn view_html(state: Arc<AppState>) -> String {
        let Html(html) = view_invite(State(state), extract::Path(TOKEN.to_string()))
            .await
            .expect("view_invite failed");
        html
    }

    // Регрессия XR-032: QR кодировал относительный claim-путь, который
    // parse_invite_link не принимает (нет схемы и хоста).
    #[tokio::test]
    async fn qr_encodes_canonical_invite_url() {
        let state = state_with_invite("https://hub.example.com", "");
        let qr = qr_data_from_view(&view_html(state).await);

        assert_eq!(qr, format!("https://hub.example.com/invite/{TOKEN}"));
        let link = xr_proto::invite_url::parse_invite_link(&qr).expect("app must parse qr");
        assert_eq!(link.hub_url(), "https://hub.example.com");
        assert_eq!(link.token(), TOKEN);
    }

    #[tokio::test]
    async fn qr_host_falls_back_to_hub_config() {
        let state = state_with_invite("", "https://fallback.example.com/");
        let qr = qr_data_from_view(&view_html(state).await);

        assert_eq!(qr, format!("https://fallback.example.com/invite/{TOKEN}"));
    }
}
