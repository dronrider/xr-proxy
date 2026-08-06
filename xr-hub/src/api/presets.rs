use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{self, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use xr_proto::config::RoutingConfig;
use xr_proto::preset::{Preset, PresetSummary};

use crate::state::AppState;
use crate::storage;

// ── Public ──────────────────────────────────────────────────────────

pub async fn list_presets(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let presets = state.presets.read().await;
    let mut summaries: Vec<PresetSummary> = presets.values().map(|p| p.summary()).collect();
    summaries.sort_by(|a, b| a.name.cmp(&b.name));

    let etag = format!("\"{}\"", summaries.iter().map(|s| s.version).sum::<u64>());
    let mut headers = HeaderMap::new();
    headers.insert("etag", etag.parse().unwrap());
    headers.insert(
        "x-hub-version",
        env!("CARGO_PKG_VERSION").parse().unwrap(),
    );
    (headers, Json(summaries))
}

pub async fn get_preset(
    State(state): State<Arc<AppState>>,
    extract::Path(name): extract::Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    let presets = state.presets.read().await;
    let preset = presets.get(&name).ok_or(StatusCode::NOT_FOUND)?;

    let etag = format!("\"{}\"", preset.version);

    // ETag / If-None-Match
    if let Some(inm) = headers.get("if-none-match").and_then(|v| v.to_str().ok()) {
        if inm == etag {
            return Err(StatusCode::NOT_MODIFIED);
        }
    }

    let mut resp_headers = HeaderMap::new();
    resp_headers.insert("etag", etag.parse().unwrap());
    Ok((resp_headers, Json(preset.clone())))
}

/// Максимум удержания запроса ожидания и значение по умолчанию (LLD-37).
/// Минута это потолок, за которым промежуточные прокси начинают рвать
/// висящий ответ сами.
const WAIT_MAX_SECS: u64 = 60;
const WAIT_DEFAULT_SECS: u64 = 55;

#[derive(Debug, Deserialize)]
pub struct WaitQuery {
    /// Версия, которая уже есть у клиента. Без кэша он присылает 0.
    #[serde(default)]
    pub version: u64,
    pub timeout_secs: Option<u64>,
}

/// Сколько держать запрос: клиент просит своё, но дольше [`WAIT_MAX_SECS`]
/// висеть нельзя. Клиент таймаут считает от того, что попросил, а ответ
/// длиннее минуты промежуточные прокси рвут сами, и клиент видит обрыв
/// вместо честного 304.
fn wait_hold(requested_secs: Option<u64>) -> Duration {
    Duration::from_secs(requested_secs.unwrap_or(WAIT_DEFAULT_SECS).min(WAIT_MAX_SECS))
}

/// Ожидание новой версии пресета (LLD-37): пока версия совпадает с
/// клиентской, запрос висит, а публикация из админки будит его через
/// поколение в [`AppState::preset_gen`]. Сравнение идёт на неравенство, а не
/// на «больше»: откат пресета админом обязан доезжать так же мгновенно, как
/// и новая версия.
pub async fn wait_preset(
    State(state): State<Arc<AppState>>,
    extract::Path(name): extract::Path<String>,
    Query(query): Query<WaitQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    let hold = wait_hold(query.timeout_secs);
    let deadline = tokio::time::Instant::now() + hold;
    // Подписка снимается до первой сверки версий: иначе публикация в этот
    // зазор прошла бы мимо, и клиент провисел бы полное удержание впустую.
    let mut gen_rx = state.preset_gen.subscribe();

    loop {
        {
            let presets = state.presets.read().await;
            let preset = presets.get(&name).ok_or(StatusCode::NOT_FOUND)?;
            if preset.version != query.version {
                let mut headers = HeaderMap::new();
                headers.insert("etag", format!("\"{}\"", preset.version).parse().unwrap());
                return Ok((headers, Json(preset.clone())));
            }
        }

        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Err(StatusCode::NOT_MODIFIED);
        }
        tokio::select! {
            _ = tokio::time::sleep(left) => return Err(StatusCode::NOT_MODIFIED),
            changed = gen_rx.changed() => {
                if changed.is_err() {
                    return Err(StatusCode::NOT_MODIFIED);
                }
            }
        }
    }
}

pub async fn get_public_key(
    State(state): State<Arc<AppState>>,
) -> Result<String, StatusCode> {
    let ctx = state.signing.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let pubkey = ctx.verifying_key();
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(pubkey.as_bytes()))
}

// ── Admin ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreatePresetRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub rules: RoutingConfig,
}

pub async fn create_preset(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreatePresetRequest>,
) -> Result<(StatusCode, Json<Preset>), (StatusCode, String)> {
    validate_slug(&req.name)?;
    validate_rules_size(&req.rules)?;

    let mut presets = state.presets.write().await;
    if presets.contains_key(&req.name) {
        return Err((StatusCode::CONFLICT, format!("preset '{}' already exists", req.name)));
    }

    let now = chrono::Utc::now().to_rfc3339();
    let mut preset = Preset {
        name: req.name,
        version: 1,
        updated_at: now,
        description: req.description,
        rules: req.rules,
        signature: None,
    };

    if let Some(ctx) = &state.signing {
        preset.signature = Some(ctx.sign_preset(&preset));
    }

    let data_dir = Path::new(&state.config.server.data_dir);
    storage::save_preset(data_dir, &preset)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    presets.insert(preset.name.clone(), preset.clone());
    bump_generation(&state);
    Ok((StatusCode::CREATED, Json(preset)))
}

pub async fn update_preset(
    State(state): State<Arc<AppState>>,
    extract::Path(name): extract::Path<String>,
    Json(req): Json<CreatePresetRequest>,
) -> Result<Json<Preset>, (StatusCode, String)> {
    validate_rules_size(&req.rules)?;

    let mut presets = state.presets.write().await;
    let existing = presets
        .get(&name)
        .ok_or((StatusCode::NOT_FOUND, format!("preset '{name}' not found")))?;

    let now = chrono::Utc::now().to_rfc3339();
    let mut preset = Preset {
        name: name.clone(),
        version: existing.version + 1,
        updated_at: now,
        description: req.description,
        rules: req.rules,
        signature: None,
    };

    if let Some(ctx) = &state.signing {
        preset.signature = Some(ctx.sign_preset(&preset));
    }

    let data_dir = Path::new(&state.config.server.data_dir);
    storage::save_preset(data_dir, &preset)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    presets.insert(name, preset.clone());
    bump_generation(&state);
    Ok(Json(preset))
}

pub async fn delete_preset(
    State(state): State<Arc<AppState>>,
    extract::Path(name): extract::Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let mut presets = state.presets.write().await;
    if presets.remove(&name).is_none() {
        return Err((StatusCode::NOT_FOUND, format!("preset '{name}' not found")));
    }

    let data_dir = Path::new(&state.config.server.data_dir);
    storage::delete_preset_file(data_dir, &name)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    bump_generation(&state);
    Ok(StatusCode::NO_CONTENT)
}

/// Разбудить всех, кто висит на ручке ожидания. Зовётся после записи на диск
/// и вставки в мапу, чтобы проснувшийся увидел уже новую версию.
fn bump_generation(state: &AppState) {
    state.preset_gen.send_modify(|gen| *gen += 1);
}

fn validate_slug(name: &str) -> Result<(), (StatusCode, String)> {
    if name.is_empty() || name.len() > 64 {
        return Err((StatusCode::BAD_REQUEST, "name must be 1-64 characters".into()));
    }
    if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
        return Err((StatusCode::BAD_REQUEST, "name must match [a-z0-9_-]+".into()));
    }
    Ok(())
}

fn validate_rules_size(rules: &RoutingConfig) -> Result<(), (StatusCode, String)> {
    if rules.rules.len() > 10_000 {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "max 10000 rules".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::body::Body;
    use axum::http::Request;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    use super::*;
    use crate::api::router;

    fn preset(version: u64) -> Preset {
        Preset {
            name: "russia".into(),
            version,
            updated_at: "2026-08-03T00:00:00Z".into(),
            description: String::new(),
            rules: RoutingConfig {
                default_action: "direct".into(),
                rules: Vec::new(),
            },
            signature: None,
        }
    }

    fn state_with_preset(dir: &Path) -> Arc<AppState> {
        let toml = format!("[server]\ndata_dir = \"{}\"\n[admin]\nusers = []\n", dir.display());
        let config: crate::config::HubConfig = toml::from_str(&toml).unwrap();
        let mut presets = HashMap::new();
        presets.insert("russia".to_string(), preset(1));
        Arc::new(AppState {
            presets: RwLock::new(presets),
            invites: RwLock::new(HashMap::new()),
            shares: RwLock::new(HashMap::new()),
            exposes: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            config,
            signing: None,
            preset_gen: tokio::sync::watch::Sender::new(0),
            web_attempts: Default::default(),
        })
    }

    async fn wait_request(state: Arc<AppState>, query: &str) -> axum::response::Response {
        router(state)
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/presets/russia/wait?{query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn body_preset(resp: axum::response::Response) -> Preset {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // Клиент приносит устаревшую версию: ждать нечего, пресет уходит сразу и
    // целиком, лишнего round trip за телом не будет.
    #[tokio::test]
    async fn wait_returns_preset_when_version_differs() {
        let dir = tempfile::tempdir().unwrap();
        let resp = wait_request(state_with_preset(dir.path()), "version=0&timeout_secs=55").await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("etag").unwrap(), "\"1\"");
        assert_eq!(body_preset(resp).await.version, 1);
    }

    // Откат версии админом это тоже изменение: сравнение идёт на неравенство,
    // и версия выше локальной доезжает так же, как новая.
    #[tokio::test]
    async fn wait_returns_preset_when_client_is_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let resp = wait_request(state_with_preset(dir.path()), "version=9&timeout_secs=55").await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_preset(resp).await.version, 1);
    }

    // Истёкшее удержание отвечает 304 без тела. Нулевой таймаут делает кейс
    // детерминированным: ждать в тесте нечего, ответ приходит сразу.
    #[tokio::test]
    async fn wait_times_out_with_not_modified() {
        let dir = tempfile::tempdir().unwrap();
        let resp = wait_request(state_with_preset(dir.path()), "version=1&timeout_secs=0").await;

        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn wait_unknown_preset_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let resp = router(state_with_preset(dir.path()))
            .oneshot(
                Request::builder()
                    .uri("/api/v1/presets/turkey/wait?version=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // Ради чего всё и заводилось: публикация будит висящий запрос, и новая
    // версия уходит клиенту, не дожидаясь конца удержания. Синхронизация без
    // sleep: тест ждёт, пока обработчик подпишется на поколение, и только
    // тогда публикует.
    #[tokio::test]
    async fn wait_wakes_up_on_update() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_preset(dir.path());

        let waiter = tokio::spawn({
            let state = state.clone();
            async move { wait_request(state, "version=1&timeout_secs=5").await }
        });
        while state.preset_gen.receiver_count() == 0 {
            tokio::task::yield_now().await;
        }

        let published = update_preset(
            State(state.clone()),
            extract::Path("russia".to_string()),
            Json(CreatePresetRequest {
                name: "russia".into(),
                description: "новая версия".into(),
                rules: RoutingConfig {
                    default_action: "direct".into(),
                    rules: Vec::new(),
                },
            }),
        )
        .await
        .unwrap();
        assert_eq!(published.0.version, 2);

        let resp = waiter.await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_preset(resp).await.version, 2);
    }
    // Публикация нового пресета тоже двигает поколение: ждущие на других
    // именах перевзведутся и не провисят удержание впустую. По имени, которого
    // в хабе ещё нет, ожидание не открыть (сразу 404), поэтому у создания
    // проверяется сам побочный эффект.
    #[tokio::test]
    async fn create_preset_bumps_generation() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_preset(dir.path());
        let before = *state.preset_gen.borrow();

        let created = create_preset(
            State(state.clone()),
            Json(CreatePresetRequest {
                name: "turkey".into(),
                description: String::new(),
                rules: RoutingConfig {
                    default_action: "direct".into(),
                    rules: Vec::new(),
                },
            }),
        )
        .await
        .unwrap();
        assert_eq!(created.0, StatusCode::CREATED);

        assert_eq!(*state.preset_gen.borrow(), before + 1);
    }

    // Удаление пресета из админки тоже будит висящий запрос, и клиент узнаёт
    // об этом сразу: проснувшись, он не находит пресета и получает 404 вместо
    // молчания до конца удержания.
    #[tokio::test]
    async fn delete_preset_wakes_waiter_with_404() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_preset(dir.path());

        let waiter = tokio::spawn({
            let state = state.clone();
            async move { wait_request(state, "version=1&timeout_secs=5").await }
        });
        while state.preset_gen.receiver_count() == 0 {
            tokio::task::yield_now().await;
        }

        delete_preset(State(state.clone()), extract::Path("russia".to_string()))
            .await
            .unwrap();

        assert_eq!(waiter.await.unwrap().status(), StatusCode::NOT_FOUND);
    }

    // Клиент вправе попросить больше потолка, но висеть дольше минуты нельзя:
    // такой ответ рвут промежуточные прокси, и клиент видит обрыв вместо 304.
    #[test]
    fn wait_hold_is_capped_and_has_default() {
        assert_eq!(wait_hold(None), Duration::from_secs(55));
        assert_eq!(wait_hold(Some(10)), Duration::from_secs(10));
        assert_eq!(wait_hold(Some(0)), Duration::ZERO);
        assert_eq!(wait_hold(Some(600)), Duration::from_secs(60));
    }
}
