use std::path::Path;
use std::sync::Arc;

use axum::extract::{self, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::Json;
use base64::Engine;
use serde::Deserialize;
use xr_proto::invite_url::{build_custom_url, build_https_url};
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

/// GET /invite/:token/view - HTML page with invite info and QR code.
pub async fn view_invite(
    State(state): State<Arc<AppState>>,
    extract::Path(token): extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Html<String>, (StatusCode, String)> {
    // Приложение есть только под Android, поэтому «Открыть в приложении»
    // (deep link) показываем лишь там. На iOS/десктопе кнопка вела бы в никуда,
    // получателю остаётся отсканировать QR телефоном или скачать APK.
    let is_android = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|ua| ua.contains("Android"))
        .unwrap_or(false);

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

    let comment = &invite.comment;
    let expires = format_datetime(&invite.expires_at);
    let active = status == "active";
    let status_badge = match status {
        "active" => r#"<span class="badge badge-active">Активно</span>"#,
        "expired" => r#"<span class="badge badge-expired">Истекло</span>"#,
        "consumed" => r#"<span class="badge badge-consumed">Уже использовано</span>"#,
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
    // «Открыть в приложении» это гарантированный deep link на кастомной схеме:
    // на странице /view приложение заведомо не дефолтный обработчик (иначе
    // получатель не смотрел бы её в браузере), а xr:// перехватит установленный
    // клиент напрямую, без chooser'а. Если приложения нет, спасает «Скачать APK».
    let deep_link = build_custom_url(hub_url, &token);
    // Абсолютный от корня путь: страница живёт под /api/v1/..., а раздача APK по
    // /api/v1/app/download (LLD-12), латест-алиас всегда тянет свежий релиз.
    let apk_url = "/api/v1/app/download/latest";
    let open_class = if active { "btn primary" } else { "btn primary disabled" };

    // «Открыть в приложении» только на Android (см. is_android выше).
    let open_in_app = if is_android {
        format!(r#"<a class="{open_class}" href="{deep_link}">Открыть в приложении</a>"#)
    } else {
        String::new()
    };
    // На не-Android нет ни deep link, ни смысла в APK: подсказываем QR.
    let platform_note = if is_android {
        String::new()
    } else {
        r#"<p class="note">Приложение доступно для Android. Отсканируйте QR телефоном или откройте эту ссылку на Android-устройстве.</p>"#.to_string()
    };

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="ru">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Приглашение xr-proxy</title>
<style>
  * {{ box-sizing: border-box; }}
  body {{ font-family: -apple-system, system-ui, sans-serif; margin: 0; min-height: 100vh; display: flex; align-items: center; justify-content: center; padding: 1.5rem; background: #eceef2; }}
  /* Цвет текста задаём вместе с фоном карточки в каждом правиле: если вебвью
     применит один override и не применит другой (наблюдалось с тёмной темой),
     текст и фон не разъедутся в светлый-на-белом. */
  .card {{ background: #fff; color: #1a1a2e; border-radius: 16px; padding: 2.25rem 2.5rem; max-width: 760px; width: 100%; box-shadow: 0 6px 28px rgba(0,0,0,0.10); }}
  h1 {{ font-size: 1.7rem; margin: 0 0 0.35rem; text-align: center; color: #12121c; }}
  .meta {{ color: #5a5f6e; font-size: 0.95rem; text-align: center; margin: 0 0 1.75rem; }}
  .main {{ display: flex; gap: 2.5rem; flex-wrap: wrap; align-items: center; justify-content: center; }}
  .col-info {{ flex: 1 1 300px; min-width: min(100%, 300px); }}
  .col-qr {{ flex: 0 0 auto; text-align: center; }}
  .field {{ display: flex; justify-content: space-between; align-items: baseline; gap: 1rem; padding: 0.7rem 0; border-bottom: 1px solid #ececf0; }}
  .field-label {{ color: #6b7080; font-size: 0.8rem; text-transform: uppercase; letter-spacing: 0.03em; }}
  .field-value {{ color: #12121c; font-size: 1rem; font-weight: 600; text-align: right; }}
  .badge {{ font-weight: 700; }}
  .badge-active {{ color: #1e8e3e; }}
  .badge-expired {{ color: #8a8f9e; }}
  .badge-consumed {{ color: #e8710a; }}
  .actions {{ display: flex; flex-direction: column; gap: 0.75rem; margin: 1.5rem 0 0; }}
  .btn {{ display: block; width: 100%; padding: 0.95rem 1.5rem; border-radius: 12px; font-size: 1.05rem; font-weight: 600; text-align: center; text-decoration: none; cursor: pointer; border: 2px solid transparent; }}
  .btn.primary {{ background: #4f46e5; color: #fff; }}
  .btn.disabled {{ opacity: 0.4; pointer-events: none; }}
  /* Кнопка скачивания в стиле магазина: иконка Android плюс подпись в две строки. */
  .store-btn {{ display: flex; align-items: center; justify-content: center; gap: 0.75rem; width: 100%; padding: 0.7rem 1.25rem; background: #12121c; color: #fff; border: 1px solid rgba(255,255,255,0.08); border-radius: 12px; text-decoration: none; }}
  .store-ic {{ fill: #3ddc84; flex: 0 0 auto; }}
  .store-tx {{ display: flex; flex-direction: column; line-height: 1.15; text-align: left; }}
  .store-tx small {{ font-size: 0.72rem; opacity: 0.85; }}
  .store-tx b {{ font-size: 1.05rem; font-weight: 700; }}
  .note {{ color: #5a5f6e; font-size: 0.85rem; line-height: 1.45; margin: 1rem 0 0; }}
  .col-qr img {{ background: #fff; padding: 10px; border-radius: 12px; width: 220px; height: 220px; max-width: 100%; }}
  .qr-cap {{ color: #7a7f8e; font-size: 0.82rem; margin: 0.6rem 0 0; }}
  @media (prefers-color-scheme: dark) {{
    body {{ background: #0f1017; }}
    .card {{ background: #1b1e2b; color: #e6e7ee; box-shadow: 0 6px 28px rgba(0,0,0,0.45); }}
    h1 {{ color: #f2f3f8; }}
    .meta, .note, .qr-cap {{ color: #9ea3b4; }}
    .field {{ border-bottom-color: #2b2f40; }}
    .field-label {{ color: #9096a8; }}
    .field-value {{ color: #f2f3f8; }}
    .badge-active {{ color: #4ade80; }}
    .badge-expired {{ color: #9096a8; }}
    .badge-consumed {{ color: #fb923c; }}
    .btn.primary {{ background: #6366f1; }}
    .store-btn {{ background: #0d0e14; border-color: #2b2f40; }}
  }}
</style>
</head>
<body>
<div class="card">
  <h1>Приглашение xr-proxy</h1>
  <p class="meta">Подключение к xr-proxy в пару касаний</p>
  <div class="main">
    <div class="col-info">
      <div class="field"><div class="field-label">Статус</div><div class="field-value">{status_badge}</div></div>
      <div class="field"><div class="field-label">Действует до</div><div class="field-value">{expires}</div></div>
      {comment_html}
      <div class="actions">
        {open_in_app}
        <a class="store-btn" href="{apk_url}">
          <svg class="store-ic" viewBox="0 0 24 24" width="30" height="30" aria-hidden="true"><path d="M6 18c0 .55.45 1 1 1h1v3.5c0 .83.67 1.5 1.5 1.5s1.5-.67 1.5-1.5V19h2v3.5c0 .83.67 1.5 1.5 1.5s1.5-.67 1.5-1.5V19h1c.55 0 1-.45 1-1V8H6v10zM3.5 8C2.67 8 2 8.67 2 9.5v7c0 .83.67 1.5 1.5 1.5S5 17.33 5 16.5v-7C5 8.67 4.33 8 3.5 8zm17 0c-.83 0-1.5.67-1.5 1.5v7c0 .83.67 1.5 1.5 1.5s1.5-.67 1.5-1.5v-7c0-.83-.67-1.5-1.5-1.5zm-4.97-5.84l1.3-1.3c.2-.2.2-.51 0-.71-.2-.2-.51-.2-.71 0l-1.48 1.48C13.85 1.23 12.95 1 12 1c-.96 0-1.86.23-2.66.63L7.85.15c-.2-.2-.51-.2-.71 0-.2.2-.2.51 0 .71l1.31 1.31C6.97 3.26 6 5.01 6 7h12c0-1.99-.97-3.75-2.47-4.84zM10 5H9V4h1v1zm5 0h-1V4h1v1z"/></svg>
          <span class="store-tx"><small>Скачать</small><b>APK для Android</b></span>
        </a>
      </div>
      {platform_note}
    </div>
    <div class="col-qr">
      <img src="https://api.qrserver.com/v1/create-qr-code/?size=300x300&amp;data={qr_data_encoded}" width="220" height="220" alt="QR-код приглашения">
      <p class="qr-cap">Отсканируйте телефоном</p>
    </div>
  </div>
</div>
</body>
</html>"#,
        status_badge = status_badge,
        expires = expires,
        comment_html = if comment.is_empty() {
            String::new()
        } else {
            format!(r#"<div class="field"><div class="field-label">Комментарий</div><div class="field-value">{comment}</div></div>"#)
        },
        open_in_app = open_in_app,
        platform_note = platform_note,
        apk_url = apk_url,
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

    async fn view_html_ua(state: Arc<AppState>, ua: &str) -> String {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(axum::http::header::USER_AGENT, ua.parse().unwrap());
        let Html(html) = view_invite(State(state), extract::Path(TOKEN.to_string()), headers)
            .await
            .expect("view_invite failed");
        html
    }

    /// По умолчанию рендерим как Android: там показывается deep link.
    async fn view_html(state: Arc<AppState>) -> String {
        view_html_ua(state, "Mozilla/5.0 (Linux; Android 14; Pixel)").await
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

    // XR-033: /view это воронка для получателя без приложения. Кнопка «Открыть
    // в приложении» несёт гарантированный deep link на кастомной схеме, кнопка
    // «Скачать APK» ведёт на раздачу релиза.
    #[tokio::test]
    async fn view_offers_deep_link_and_apk() {
        let html = view_html(state_with_invite("https://hub.example.com", "")).await;

        assert!(
            html.contains(&format!(r#"href="xr://invite/{TOKEN}?hub=hub.example.com""#)),
            "нет deep link на кастомной схеме"
        );
        assert!(
            html.contains(r#"href="/api/v1/app/download/latest""#),
            "нет кнопки скачать APK"
        );
    }

    // Просроченный инвайт применять нечем: кнопку «Открыть в приложении»
    // гасим, чтобы не вести в claim, который вернёт 410.
    #[tokio::test]
    async fn view_disables_open_for_expired_invite() {
        let state = state_with_invite("https://hub.example.com", "");
        state
            .invites
            .write()
            .await
            .get_mut(TOKEN)
            .unwrap()
            .expires_at = "2000-01-01T00:00:00+00:00".into();

        let html = view_html(state).await;
        assert!(
            html.contains(&format!(r#"class="btn primary disabled" href="xr://invite/{TOKEN}"#)),
            "у просроченного инвайта кнопка открытия должна быть погашена"
        );
    }

    // «Открыть в приложении» только на Android: приложение есть лишь под него,
    // на десктопе и iOS deep link вёл бы в никуда. APK-кнопка остаётся везде.
    #[tokio::test]
    async fn view_hides_open_in_app_on_non_android() {
        let html = view_html_ua(
            state_with_invite("https://hub.example.com", ""),
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15)",
        )
        .await;

        assert!(
            !html.contains("xr://invite/"),
            "на не-Android deep link показывать не должны"
        );
        assert!(
            !html.contains("Открыть в приложении"),
            "на не-Android кнопки открытия быть не должно"
        );
        assert!(
            html.contains(r#"href="/api/v1/app/download/latest""#),
            "кнопка скачивания APK нужна и на не-Android"
        );
    }
}
