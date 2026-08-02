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

/// GET /invite/:token: return metadata without secrets. Does NOT consume.
pub async fn get_invite_info(
    State(state): State<Arc<AppState>>,
    extract::Path(token): extract::Path<String>,
    headers: axum::http::HeaderMap,
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
        // Приложение спрашивает сведения до применения и гасит кнопку по
        // статусу, поэтому владельцу ключа мало идемпотентного claim: без
        // этого поля он до claim просто не доходит (XR-216).
        reclaimable: can_reclaim(invite, &claim_id_header(&headers), &now),
    }))
}

/// Инвайт потреблён, но помнит, кто его забрал, и ещё не истёк: кому-то повтор
/// возможен. Кому именно, знает только владелец ключа, поэтому страница
/// приглашения дальше этого не заглядывает. Отзыв ключ стирает, и отозванный
/// инвайт сюда не попадает.
fn has_live_claim(invite: &Invite, now: &str) -> bool {
    invite.consumed_at.is_some()
        && invite.claim_id.is_some()
        && invite.expires_at.as_str() > now
}

/// Может ли спрашивающий забрать payload повторно: инвайт потреблён этим же
/// ключом и ещё не истёк. Одна проверка на ручку сведений и на claim, чтобы
/// экран подтверждения не расходился с тем, что ответит сам claim.
fn can_reclaim(invite: &Invite, claim_id: &Option<String>, now: &str) -> bool {
    if !has_live_claim(invite, now) {
        return false;
    }
    match (invite.claim_id.as_deref(), claim_id.as_deref()) {
        (Some(stored), Some(given)) => constant_time_eq(stored, given),
        _ => false,
    }
}

/// GET /invite/:token и /invite/:token/view на верхнем уровне -> HTML-view.
///
/// Красивый путь `https://<host>/invite/<token>` это то, что кодирует QR и что
/// уходит получателю (build_https_url). Сами ручки инвайта висят под /api/v1, а
/// голый путь без редиректа проваливается в SPA-заглушку админки. Ведём его на
/// страницу-воронку, чтобы ссылка открывалась в браузере у того, кто её получил.
pub async fn redirect_to_view(
    extract::Path(token): extract::Path<String>,
) -> axum::response::Redirect {
    // Токен percent-кодируем: своим набором (base64url) он проходит насквозь, но
    // подставленные в путь CR/LF так не утекут в заголовок Location.
    axum::response::Redirect::temporary(&format!("/api/v1/invite/{}/view", urlencoding(&token)))
}

/// Ответ страницы /view: тело плюс заголовок Content-Security-Policy.
pub type ViewResponse = ([(axum::http::HeaderName, String); 1], Html<String>);

/// GET /invite/:token/view - HTML page with invite info and QR code.
pub async fn view_invite(
    State(state): State<Arc<AppState>>,
    extract::Path(token): extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<ViewResponse, (StatusCode, String)> {
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

    let comment = escape_html(&invite.comment);
    let expires = escape_html(&format_datetime(&invite.expires_at));
    let active = status == "active";
    let (badge_class, badge_label) = match status {
        "expired" => ("badge-expired", "Истекло"),
        "consumed" => ("badge-consumed", "Уже использовано"),
        _ => ("badge-active", "Активно"),
    };
    let status_badge = format!(r#"<span class="badge {badge_class}">{badge_label}</span>"#);

    // QR кодирует каноническую ссылку https://<host>/invite/<token> (LLD-04):
    // относительный путь приложение не парсит. Хост берём из hub_url инвайта,
    // при пустом из дефолтов конфига хаба.
    let hub_url = if invite.payload.hub_url.is_empty() {
        state.config.invites.defaults.hub_url.as_str()
    } else {
        invite.payload.hub_url.as_str()
    };
    let qr_data = build_https_url(hub_url, &token);
    let qr_svg = render_qr_svg(&qr_data);
    // «Открыть в приложении» это гарантированный deep link на кастомной схеме:
    // на странице /view приложение заведомо не дефолтный обработчик (иначе
    // получатель не смотрел бы её в браузере), а xr:// перехватит установленный
    // клиент напрямую, без chooser'а. Если приложения нет, спасает «Скачать APK».
    let deep_link = escape_html(&build_custom_url(hub_url, &token));
    // Абсолютный от корня путь: страница живёт под /api/v1/..., а раздача APK по
    // /api/v1/app/download (LLD-12), латест-алиас всегда тянет свежий релиз.
    let apk_url = "/api/v1/app/download/latest";
    // Потреблённый инвайт кнопку не гасит (XR-216): применял его, возможно, тот
    // же человек, у которого сорвалось применение, и на его устройстве инвайт
    // откроется снова. Браузер ключа установки не держит и решить это за
    // приложение не может, поэтому кнопка живая, а под ней объяснение. Обещаем
    // это только там, где повтор вправду возможен: у истёкшего и отозванного
    // (отзыв стирает ключ) кнопка гаснет как раньше, а обещание было бы враньём.
    let reclaimable = has_live_claim(invite, &now);
    let open_class = if active || reclaimable { "btn primary" } else { "btn primary disabled" };

    // «Открыть в приложении» только на Android (см. is_android выше).
    let open_in_app = if is_android {
        format!(r#"<a class="{open_class}" href="{deep_link}">Открыть в приложении</a>"#)
    } else {
        String::new()
    };
    let consumed_note = if reclaimable {
        r#"<p class="note">Инвайт уже использован. Если применяли его вы, откройте в приложении: на том же устройстве он применится снова.</p>"#
    } else {
        ""
    };
    // На не-Android нет ни deep link, ни смысла в APK: подсказываем QR.
    let platform_note = if is_android {
        String::new()
    } else {
        r#"<p class="note">Приложение доступно для Android. Отсканируйте QR телефоном или откройте эту ссылку на Android-устройстве.</p>"#.to_string()
    };

    // Стили на странице инлайновые, поэтому CSP пускает их по одноразовому
    // nonce, а не по 'unsafe-inline': разрешение по слову открыло бы дорогу и
    // любому чужому <style>, если что-то всё-таки просочится в разметку.
    let nonce = csp_nonce();

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="ru">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Приглашение xr-proxy</title>
<style nonce="{nonce}">
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
  .col-qr svg {{ background: #fff; padding: 10px; border-radius: 12px; width: 220px; height: 220px; max-width: 100%; }}
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
      {consumed_note}
      {platform_note}
    </div>
    <div class="col-qr">
      {qr_svg}
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
        consumed_note = consumed_note,
        platform_note = platform_note,
        apk_url = apk_url,
        qr_svg = qr_svg,
        nonce = nonce,
    );

    Ok((
        [(
            axum::http::header::CONTENT_SECURITY_POLICY,
            content_security_policy(&nonce),
        )],
        Html(html),
    ))
}

/// Страница живёт целиком в своём ответе: ни картинок, ни скриптов, ни шрифтов
/// со стороны она не тянет, поэтому всё запрещаем по умолчанию и открываем
/// только инлайновые стили по nonce. Токен инвайта из адресной строки после
/// этого никуда не утечёт: уносить его наружу нечему.
fn content_security_policy(nonce: &str) -> String {
    format!(
        "default-src 'none'; style-src 'nonce-{nonce}'; img-src 'none'; \
         form-action 'none'; base-uri 'none'; frame-ancestors 'none'"
    )
}

fn csp_nonce() -> String {
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// QR рисуем сами инлайновым SVG. Раньше картинка тянулась с api.qrserver.com,
/// то есть одноразовый токен инвайта уезжал в логи чужого сервиса, а без
/// внешней сети страница оставалась без QR вовсе.
fn render_qr_svg(data: &str) -> String {
    let svg = match qrcode::QrCode::new(data.as_bytes()) {
        Ok(code) => code
            .render::<qrcode::render::svg::Color>()
            .min_dimensions(300, 300)
            .quiet_zone(true)
            .dark_color(qrcode::render::svg::Color("#000000"))
            .light_color(qrcode::render::svg::Color("#ffffff"))
            .build(),
        // Ссылка на инвайт в лимиты QR укладывается с запасом, но падать
        // страницей из-за неё не стоит: остальные способы подключиться на месте.
        Err(_) => return String::new(),
    };
    // Рендерер начинает с XML-декларации, в HTML она лишняя: там инлайновый SVG
    // это обычный элемент разметки, а <?...?> разбирается как мусорный комментарий.
    let svg = svg.strip_prefix(r#"<?xml version="1.0" standalone="yes"?>"#).unwrap_or(&svg);
    svg.replacen("<svg ", r#"<svg role="img" aria-label="QR-код приглашения" "#, 1)
}

/// Экранирование для текста и для значений атрибутов в кавычках.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
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

/// Ключ клиента из заголовка `X-Claim-Id` (XR-216). Пустой и слишком длинный
/// отбрасываем: первый не отличает клиентов, второй некуда писать в файл инвайта.
fn claim_id_header(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers.get("x-claim-id")?.to_str().ok()?.trim();
    if raw.is_empty() || raw.len() > 128 {
        return None;
    }
    Some(raw.to_string())
}

/// Ключ сверяем в постоянное время: подбирать его на потреблённом инвайте
/// пришлось бы вслепую, и по времени ответа подсказки быть не должно.
fn constant_time_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// POST /invite/:token/claim: return full payload and consume one-time invites.
pub async fn claim_invite(
    State(state): State<Arc<AppState>>,
    extract::Path(token): extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Json<InvitePayload>, (StatusCode, String)> {
    let mut invites = state.invites.write().await;
    let invite = invites
        .get_mut(&token)
        .ok_or((StatusCode::NOT_FOUND, "invite not found".into()))?;

    let claim_id = claim_id_header(&headers);

    let now = chrono::Utc::now().to_rfc3339();
    if invite.expires_at <= now {
        return Err((StatusCode::GONE, "invite expired".into()));
    }
    if invite.consumed_at.is_some() {
        // Повтор того же клиента (XR-216): инвайт уже потреблён, но получатель
        // остался без payload'а, потому что ответ до него не доехал целым.
        // Отдаём тот же payload, пока инвайт не истёк; чужой повтор и отозванный
        // инвайт (там ключа нет) уходят в 410 как раньше.
        if can_reclaim(invite, &claim_id, &now) {
            tracing::info!("invite re-claimed by the same client, serving payload again");
            return Ok(Json(invite.payload.clone()));
        }
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

    // Consume one-time invites (unless dev_mode). Пометка о потреблении сперва
    // ложится на диск и только потом в память: раньше ошибка записи глоталась,
    // клиент получал payload и 200, а поднявшийся после рестарта хаб читал из
    // файла всё тот же активный инвайт и отдавал его второй раз. Отказ клиенту
    // честнее: одноразовая ссылка не расходится надвое, а получатель приходит
    // за payload'ом ещё раз по той же ссылке.
    if invite.one_time && !state.config.invites.dev_mode {
        let mut consumed = invite.clone();
        consumed.consumed_at = Some(now);
        consumed.claimed_by_ip = client_ip;
        consumed.claim_id = claim_id;
        let data_dir = Path::new(&state.config.server.data_dir);
        storage::save_invite(data_dir, &consumed).map_err(|e| {
            // Ручка публичная, и в теле ответа устройство каталогов хаба
            // постороннему ни к чему; подробности с путём уходят в лог.
            tracing::error!("не сохранилось потребление инвайта: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to persist invite consumption".to_string(),
            )
        })?;
        *invite = consumed;
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
        claim_id: None,
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
    // Ключ клиента снимаем: с ним повтор проходил бы и после отзыва (XR-216).
    invite.claim_id = None;

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
        state_with_invite_in(payload_hub_url, default_hub_url, "/nonexistent")
    }

    fn state_with_invite_in(
        payload_hub_url: &str,
        default_hub_url: &str,
        data_dir: &str,
    ) -> Arc<AppState> {
        let mut config: crate::config::HubConfig =
            toml::from_str("[server]\n[admin]\nusers = []").unwrap();
        config.invites.defaults.hub_url = default_hub_url.into();
        config.server.data_dir = data_dir.into();

        let invite = Invite {
            token: TOKEN.into(),
            created_at: "2026-01-01T00:00:00+00:00".into(),
            expires_at: "2099-01-01T00:00:00+00:00".into(),
            consumed_at: None,
            claimed_by_ip: None,
            claim_id: None,
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

    /// Страница несёт QR картинкой из своих же байт, поэтому «что закодировано»
    /// проверяем сравнением с QR ожидаемой ссылки, а ссылку в тесте пишем руками.
    fn html_carries_qr_for(html: &str, data: &str) -> bool {
        html.contains(&render_qr_svg(data))
    }

    /// Инвайт живёт на диске, и claim его туда переписывает; тесты про ключ
    /// смотрят в файл, поэтому data_dir у них свой временный.
    fn claim_state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_invite_in(
            "https://hub.example.com",
            "",
            dir.path().to_str().unwrap(),
        );
        (state, dir)
    }

    async fn claim(
        state: &Arc<AppState>,
        claim_id: Option<&str>,
    ) -> Result<InvitePayload, StatusCode> {
        let mut headers = axum::http::HeaderMap::new();
        if let Some(id) = claim_id {
            headers.insert("x-claim-id", id.parse().unwrap());
        }
        claim_invite(State(state.clone()), extract::Path(TOKEN.to_string()), headers)
            .await
            .map(|Json(payload)| payload)
            .map_err(|(status, _)| status)
    }

    async fn invite_info(state: &Arc<AppState>, claim_id: Option<&str>) -> InviteInfo {
        let mut headers = axum::http::HeaderMap::new();
        if let Some(id) = claim_id {
            headers.insert("x-claim-id", id.parse().unwrap());
        }
        let Json(info) =
            get_invite_info(State(state.clone()), extract::Path(TOKEN.to_string()), headers)
                .await
                .expect("get_invite_info failed");
        info
    }

    fn stored_invite(dir: &tempfile::TempDir) -> Invite {
        stored_invite_in(dir.path())
    }

    fn stored_invite_in(data_dir: &Path) -> Invite {
        let path = data_dir.join("invites").join(format!("{TOKEN}.json"));
        let data = std::fs::read_to_string(&path).expect("инвайт не сохранён на диск");
        serde_json::from_str(&data).unwrap()
    }

    // XR-216: одноразовый инвайт потребляется на выдаче payload'а, и получатель,
    // у которого ответ не доехал целым (не разобралось тело, оборвалась сеть),
    // оставался ни с чем до нового инвайта от админа. Повтор с тем же ключом
    // должен отдать тот же payload.
    #[tokio::test]
    async fn repeat_claim_with_same_key_returns_payload() {
        let (state, dir) = claim_state();

        let first = claim(&state, Some("client-key-1")).await.expect("первый claim");
        let second = claim(&state, Some("client-key-1")).await.expect("повтор того же клиента");

        assert_eq!(first.server_address, "203.0.113.10");
        assert_eq!(second.server_address, first.server_address);
        assert_eq!(second.server_port, first.server_port);

        // Инвайт при этом остаётся потреблённым, и ключ переживает рестарт хаба.
        let stored = stored_invite(&dir);
        assert!(stored.consumed_at.is_some(), "повтор не должен снимать пометку");
        assert_eq!(stored.claim_id.as_deref(), Some("client-key-1"));
    }

    // XR-216, провал проверки на проде: приложение спрашивает сведения об
    // инвайте до применения и гасит кнопку по статусу, поэтому владелец ключа
    // до повторного claim просто не доходил. Сведения обязаны признать инвайт
    // своим, не выдавая его при этом за активный.
    #[tokio::test]
    async fn invite_info_marks_reclaimable_for_the_key_owner() {
        let (state, _dir) = claim_state();

        let before = invite_info(&state, Some("client-key-1")).await;
        assert_eq!(before.status, "active");
        assert!(!before.reclaimable, "живой инвайт незачем помечать повторным");

        claim(&state, Some("client-key-1")).await.expect("первый claim");

        let mine = invite_info(&state, Some("client-key-1")).await;
        assert_eq!(mine.status, "consumed", "статус остаётся честным");
        assert!(mine.reclaimable, "владелец ключа не увидит кнопку применения");

        // Сведения не расходятся с тем, что ответит сам claim.
        claim(&state, Some("client-key-1")).await.expect("повтор владельца ключа");
    }

    // Постороннему потреблённый инвайт по-прежнему закрыт, и сведения об этом
    // не должны намекать на обратное.
    #[tokio::test]
    async fn invite_info_hides_reclaimable_from_others() {
        let (state, _dir) = claim_state();
        claim(&state, Some("client-key-1")).await.expect("первый claim");

        for key in [Some("client-key-2"), None] {
            let info = invite_info(&state, key).await;
            assert_eq!(info.status, "consumed");
            assert!(!info.reclaimable, "чужой ключ {key:?} получил право на повтор");
        }
    }

    // Истёкший инвайт мёртв и для владельца ключа: повтор ему всё равно
    // ответит 410, и обещать применение на экране нельзя.
    #[tokio::test]
    async fn invite_info_hides_reclaimable_after_expiry() {
        let (state, _dir) = claim_state();
        claim(&state, Some("client-key-1")).await.expect("первый claim");
        state.invites.write().await.get_mut(TOKEN).unwrap().expires_at =
            "2000-01-01T00:00:00+00:00".into();

        let info = invite_info(&state, Some("client-key-1")).await;
        assert!(!info.reclaimable, "истёкший инвайт обещает повтор");
        assert_eq!(claim(&state, Some("client-key-1")).await.unwrap_err(), StatusCode::GONE);
    }

    // Отзыв закрывает инвайт и для владельца ключа, сведения обязаны это
    // показать: иначе кнопка живая, а claim отвечает отказом.
    #[tokio::test]
    async fn invite_info_hides_reclaimable_after_revoke() {
        let (state, _dir) = claim_state();
        claim(&state, Some("client-key-1")).await.expect("первый claim");
        revoke_invite(State(state.clone()), extract::Path(TOKEN.to_string()))
            .await
            .expect("revoke failed");

        assert!(!invite_info(&state, Some("client-key-1")).await.reclaimable);
    }

    // Ключ и есть весь пропуск к повтору: без него и с чужим одноразовый инвайт
    // закрыт, иначе пересланная в мессенджере ссылка сработала бы дважды.
    #[tokio::test]
    async fn repeat_claim_with_foreign_key_is_gone() {
        let (state, _dir) = claim_state();
        claim(&state, Some("client-key-1")).await.expect("первый claim");

        assert_eq!(claim(&state, Some("client-key-2")).await.unwrap_err(), StatusCode::GONE);
        assert_eq!(claim(&state, None).await.unwrap_err(), StatusCode::GONE);
    }

    // Пустой ключ не сохраняем: он совпал бы у всех, и одноразовый инвайт
    // открылся бы любому, кто пришлёт тот же пустой заголовок.
    #[tokio::test]
    async fn blank_key_is_not_remembered() {
        let (state, dir) = claim_state();
        claim(&state, Some("   ")).await.expect("первый claim");
        assert!(stored_invite(&dir).claim_id.is_none());
        assert_eq!(claim(&state, Some("   ")).await.unwrap_err(), StatusCode::GONE);
    }

    // Граница длины ключа: 128 символов ещё ключ клиента, 129 это уже попытка
    // раздуть файл инвайта. Проверяем оба края, иначе сдвиг условия на символ
    // прошёл бы незамеченным.
    #[tokio::test]
    async fn key_of_128_is_remembered_and_129_is_not() {
        let (state, dir) = claim_state();
        let limit = "k".repeat(128);
        claim(&state, Some(&limit)).await.expect("первый claim");
        assert_eq!(
            stored_invite(&dir).claim_id.as_deref(),
            Some(limit.as_str()),
            "ключ ровно в 128 символов должен запоминаться"
        );
        claim(&state, Some(&limit)).await.expect("повтор с ключом в 128 символов");

        let (state, dir) = claim_state();
        let over = "k".repeat(129);
        claim(&state, Some(&over)).await.expect("первый claim");
        assert!(stored_invite(&dir).claim_id.is_none(), "ключ в 129 символов запомнили");
        assert_eq!(claim(&state, Some(&over)).await.unwrap_err(), StatusCode::GONE);
    }

    // Отзыв инвайта закрывает его и для того, кто уже забирал: иначе «отозвать»
    // не отменяло бы ничего, пока у клиента на руках ключ.
    #[tokio::test]
    async fn revoked_invite_is_gone_for_its_claimer() {
        let (state, _dir) = claim_state();
        claim(&state, Some("client-key-1")).await.expect("первый claim");

        revoke_invite(State(state.clone()), extract::Path(TOKEN.to_string()))
            .await
            .expect("revoke failed");

        assert_eq!(claim(&state, Some("client-key-1")).await.unwrap_err(), StatusCode::GONE);
    }

    // Срок инвайта важнее ключа: после истечения повтор такой же мёртвый, как
    // и первая попытка.
    #[tokio::test]
    async fn expired_invite_is_gone_for_its_claimer() {
        let (state, _dir) = claim_state();
        claim(&state, Some("client-key-1")).await.expect("первый claim");
        state.invites.write().await.get_mut(TOKEN).unwrap().expires_at =
            "2000-01-01T00:00:00+00:00".into();

        assert_eq!(claim(&state, Some("client-key-1")).await.unwrap_err(), StatusCode::GONE);
    }

    // XR-211: ошибка записи потребления глоталась (`let _ =`), клиент получал
    // payload и 200, а поднявшийся после рестарта хаб читал из файла всё тот же
    // активный инвайт: одноразовая ссылка срабатывала второй раз, уже у другого
    // человека. Диск тут единственная память хаба, поэтому не легло на диск
    // значит не потреблено, и отвечать успехом нельзя.
    #[tokio::test]
    async fn claim_fails_when_consumption_is_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        // На месте каталога данных обычный файл: save_invite спотыкается на
        // создании подкаталога invites, как споткнулся бы на полном диске.
        let data_dir = dir.path().join("data");
        std::fs::write(&data_dir, b"not a directory").unwrap();
        let state = state_with_invite_in("https://hub.example.com", "", data_dir.to_str().unwrap());

        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-claim-id", "client-key-1".parse().unwrap());
        let (status, message) =
            claim_invite(State(state.clone()), extract::Path(TOKEN.to_string()), headers)
                .await
                .map(|Json(p)| p)
                .expect_err("клиент получил payload, хотя потребление на диск не легло");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        // Ручка публичная, и устройство каталогов хаба постороннему ни к чему.
        assert!(
            !message.contains(dir.path().to_str().unwrap()),
            "путь каталога данных уехал в ответ: {message}"
        );

        // Память не должна разойтись с диском: там инвайт остаётся активным, и
        // получатель заберёт его по той же ссылке, когда запись пройдёт.
        let invite = state.invites.read().await.get(TOKEN).cloned().unwrap();
        assert!(invite.consumed_at.is_none(), "инвайт потреблён мимо диска");
        assert!(invite.claim_id.is_none(), "ключ клиента запомнен мимо диска");

        std::fs::remove_file(&data_dir).unwrap();
        claim(&state, Some("client-key-1")).await.expect("повтор по починенному диску");
        assert!(
            stored_invite_in(&data_dir).consumed_at.is_some(),
            "удачный claim не отметил потребление"
        );
    }

    // Многоразовый инвайт ключом не ограничен: он не потребляется вовсе.
    #[tokio::test]
    async fn reusable_invite_serves_every_client() {
        let (state, _dir) = claim_state();
        state.invites.write().await.get_mut(TOKEN).unwrap().one_time = false;

        claim(&state, Some("client-key-1")).await.expect("первый claim");
        claim(&state, Some("client-key-2")).await.expect("другой клиент");
        claim(&state, None).await.expect("клиент без ключа");
    }

    async fn view_ua(state: Arc<AppState>, ua: &str) -> (String, String) {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(axum::http::header::USER_AGENT, ua.parse().unwrap());
        let ([(_, csp)], Html(html)) =
            view_invite(State(state), extract::Path(TOKEN.to_string()), headers)
                .await
                .expect("view_invite failed");
        (csp, html)
    }

    async fn view_html_ua(state: Arc<AppState>, ua: &str) -> String {
        view_ua(state, ua).await.1
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
        let html = view_html(state).await;
        let canonical = format!("https://hub.example.com/invite/{TOKEN}");

        assert!(html_carries_qr_for(&html, &canonical), "на странице не QR канонической ссылки");
        assert!(
            !html_carries_qr_for(&html, &format!("https://hub.example.com/invite/{TOKEN}x")),
            "QR не зависит от того, что в него кладут"
        );
        let link = xr_proto::invite_url::parse_invite_link(&canonical).expect("app must parse qr");
        assert_eq!(link.hub_url(), "https://hub.example.com");
        assert_eq!(link.token(), TOKEN);
    }

    #[tokio::test]
    async fn qr_host_falls_back_to_hub_config() {
        let html = view_html(state_with_invite("", "https://fallback.example.com/")).await;

        assert!(
            html_carries_qr_for(&html, &format!("https://fallback.example.com/invite/{TOKEN}")),
            "при пустом hub_url в инвайте QR должен брать хост из дефолтов хаба"
        );
    }

    // XR-192: QR грузился картинкой с api.qrserver.com, то есть одноразовый
    // токен инвайта уходил в чужой сервис (и в его логи) при каждом открытии
    // страницы, а без внешней сети получатель оставался без QR.
    #[tokio::test]
    async fn view_does_not_leak_token_to_third_party() {
        let html = view_html(state_with_invite("https://hub.example.com", "")).await;

        assert!(!html.contains("qrserver.com"), "QR всё ещё тянется со стороннего сервиса");
        assert!(
            !html.contains("src=\"http"),
            "страница не должна грузить ничего по внешним ссылкам: {html}"
        );
        assert!(html.contains("<svg role=\"img\" aria-label=\"QR-код приглашения\""), "нет своего QR");
    }

    // XR-192: комментарий инвайта задаёт админ, и в HTML он подставлялся сырьём.
    #[tokio::test]
    async fn view_escapes_invite_comment() {
        let state = state_with_invite("https://hub.example.com", "");
        state.invites.write().await.get_mut(TOKEN).unwrap().comment =
            r#"<script>alert(1)</script>"#.into();

        let html = view_html(state).await;
        assert!(!html.contains("<script>"), "комментарий уехал в разметку как есть");
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"), "комментарий не экранирован");
    }

    // XR-192: hub_url приезжает из payload инвайта и попадал в href без
    // экранирования, кавычка в нём разрывала атрибут.
    #[tokio::test]
    async fn view_escapes_deep_link_attribute() {
        let html = view_html(state_with_invite(r#"https://evil"onerror="x"#, "")).await;

        assert!(!html.contains(r#""onerror=""#), "кавычка из hub_url разорвала атрибут: {html}");
    }

    // XR-192: у страницы не было CSP, хотя это единственная публичная HTML-ручка
    // хаба и открывает её получатель инвайта по ссылке из мессенджера. Политику
    // сверяем целиком, а не по паре директив: ослабление любой из них это тихая
    // дыра, а расширять политику всё равно осознанная правка, и пусть она
    // проходит через этот тест.
    #[tokio::test]
    async fn view_sets_strict_csp() {
        let (csp, html) = view_ua(
            state_with_invite("https://hub.example.com", ""),
            "Mozilla/5.0 (Linux; Android 14; Pixel)",
        )
        .await;

        let nonce = csp
            .split("style-src 'nonce-")
            .nth(1)
            .and_then(|rest| rest.split('\'').next())
            .expect("в CSP нет nonce для инлайновых стилей");
        assert!(!nonce.is_empty(), "nonce пустой: {csp}");
        assert_eq!(
            csp,
            format!(
                "default-src 'none'; style-src 'nonce-{nonce}'; img-src 'none'; \
                 form-action 'none'; base-uri 'none'; frame-ancestors 'none'"
            ),
            "политика разошлась с ожидаемой"
        );
        assert!(
            html.contains(&format!(r#"<style nonce="{nonce}">"#)),
            "nonce из заголовка не проставлен тегу style"
        );
    }

    // Nonce на то и одноразовый: постоянное значение пускало бы чужой <style>
    // на любую следующую страницу.
    #[tokio::test]
    async fn csp_nonce_is_fresh_for_every_response() {
        let (first, _) = view_ua(
            state_with_invite("https://hub.example.com", ""),
            "Mozilla/5.0 (Linux; Android 14; Pixel)",
        )
        .await;
        let (second, _) = view_ua(
            state_with_invite("https://hub.example.com", ""),
            "Mozilla/5.0 (Linux; Android 14; Pixel)",
        )
        .await;

        assert_ne!(first, second, "nonce не меняется от ответа к ответу");
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

    // XR-216: со страницы получатель и возвращается в приложение после
    // сорванного применения, поэтому у потреблённого инвайта кнопка остаётся
    // живой, а рядом объяснение, кому она поможет. Бейдж при этом не врёт.
    #[tokio::test]
    async fn view_keeps_open_button_for_consumed_invite() {
        let (state, _dir) = claim_state();
        claim(&state, Some("client-key-1")).await.expect("первый claim");

        let html = view_html(state).await;
        assert!(
            html.contains(&format!(r#"class="btn primary" href="xr://invite/{TOKEN}"#)),
            "у потреблённого инвайта кнопка открытия погашена: {html}"
        );
        assert!(html.contains("Если применяли его вы"), "нет объяснения про повтор");
        assert!(html.contains("Уже использовано"), "бейдж должен остаться честным");
    }

    // Живому инвайту обещать нечего: он и так применяется первый раз, а фраза
    // про «уже применяли» сбивала бы с толку.
    #[tokio::test]
    async fn view_says_nothing_about_reclaim_for_active_invite() {
        let html = view_html(state_with_invite("https://hub.example.com", "")).await;
        assert!(!html.contains("Если применяли его вы"), "живому инвайту обещан повтор");
    }

    // Отзыв стирает ключ, повторить нечем (XR-216, замечание ревью). Обещание
    // повтора тут враньё: страница зовёт в приложение, а там отказ без причины.
    #[tokio::test]
    async fn view_promises_nothing_for_revoked_invite() {
        let (state, _dir) = claim_state();
        claim(&state, Some("client-key-1")).await.expect("первый claim");
        revoke_invite(State(state.clone()), extract::Path(TOKEN.to_string()))
            .await
            .expect("revoke failed");

        let html = view_html(state).await;
        assert!(!html.contains("Если применяли его вы"), "отозванному инвайту обещан повтор");
        assert!(
            html.contains(&format!(r#"class="btn primary disabled" href="xr://invite/{TOKEN}"#)),
            "у отозванного инвайта кнопка открытия должна быть погашена: {html}"
        );
    }

    // Истёкший инвайт не оживить ничем, даже с запомненным ключом.
    #[tokio::test]
    async fn view_says_nothing_about_reclaim_for_expired_invite() {
        let (state, _dir) = claim_state();
        claim(&state, Some("client-key-1")).await.expect("первый claim");
        state.invites.write().await.get_mut(TOKEN).unwrap().expires_at =
            "2000-01-01T00:00:00+00:00".into();

        let html = view_html(state).await;
        assert!(!html.contains("Если применяли его вы"), "истёкшему инвайту обещан повтор");
        assert!(
            html.contains(&format!(r#"class="btn primary disabled" href="xr://invite/{TOKEN}"#)),
            "у истёкшего инвайта кнопка открытия должна быть погашена: {html}"
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

