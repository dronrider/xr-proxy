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
use xr_proto::routing::valid_cidr;
use xr_proto::user_rule::{classify_pattern, normalize_pattern, RuleKind};

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
    validate_rules_patterns(&req.rules)?;

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
    validate_rules_patterns(&req.rules)?;

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

/// Домены и CIDR-диапазоны правил пресета проходят те же проверки из
/// `xr-proto`, по которым клиент и сервер решают, применять запись (XR-308).
/// Без них хаб принимал `*.google` и битые диапазоны, клиент выкидывал запись
/// с WARN, и адресат молча не маршрутизировался. Битая запись отказывает
/// всему запросу, как серверному `validate`: частично применённый пресет это
/// та же молчаливая правка.
fn validate_rules_patterns(rules: &RoutingConfig) -> Result<(), (StatusCode, String)> {
    for rule in &rules.rules {
        for domain in &rule.domains {
            let d = normalize_pattern(domain);
            match classify_pattern(&d) {
                Ok(RuleKind::Domain) | Ok(RuleKind::Wildcard) => {}
                Ok(RuleKind::CidrV4) | Ok(RuleKind::CidrV6) => {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        format!("IP range in domains, use ip_ranges: {domain}"),
                    ));
                }
                Err(e) => {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        format!("domain does not parse: {domain}: {e}"),
                    ));
                }
            }
        }
        for cidr in &rule.ip_ranges {
            if !valid_cidr(cidr) {
                return Err((StatusCode::BAD_REQUEST, format!("broken CIDR range: {cidr}")));
            }
        }
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
    use xr_proto::config::RoutingRule;

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
            sessions: crate::sessions::SessionStore::new(
                std::time::Duration::from_secs(config.admin.session_ttl_secs),
                config.admin.max_sessions_per_user,
            ),
            config,
            signing: None,
            preset_gen: tokio::sync::watch::Sender::new(0),
            ready: std::sync::atomic::AtomicBool::new(true),
            web_attempts: Default::default(),
            login_attempts: Default::default(),
            argon2_gate: Arc::new(tokio::sync::Semaphore::new(4)),
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

    fn rules_with_domains(domains: &[&str]) -> RoutingConfig {
        RoutingConfig {
            default_action: "direct".into(),
            rules: vec![RoutingRule {
                name: Some("Google".into()),
                action: "proxy".into(),
                domains: domains.iter().map(|s| s.to_string()).collect(),
                ip_ranges: vec![],
                geoip: vec![],
                outbound: None,
            }],
        }
    }

    // Ради чего задача заведена (XR-308): домен, который клиент выкинет с
    // WARN, отказывает всему PUT, в тексте названы домен и причина, а пресет
    // остаётся прежним и версия не поднимается. `*.google` и `*.goog` это
    // случай с боевого хаба, у суффикса после звёздочки меньше двух меток.
    #[tokio::test]
    async fn update_rejects_broken_domain_and_keeps_preset() {
        for broken in ["*.google", "*.goog", "a..b"] {
            let dir = tempfile::tempdir().unwrap();
            let state = state_with_preset(dir.path());

            let err = update_preset(
                State(state.clone()),
                extract::Path("russia".to_string()),
                Json(CreatePresetRequest {
                    name: "russia".into(),
                    description: "не должно примениться".into(),
                    rules: rules_with_domains(&[broken, "*.google.com"]),
                }),
            )
            .await
            .unwrap_err();

            assert_eq!(err.0, StatusCode::BAD_REQUEST, "domain {broken}");
            assert!(
                err.1.contains(broken),
                "error names the domain: {}",
                err.1
            );
            assert!(!err.1.is_empty() && err.1.len() > broken.len());

            let presets = state.presets.read().await;
            let preset = presets.get("russia").unwrap();
            assert_eq!(preset.version, 1, "version untouched for {broken}");
            assert!(preset.rules.rules.is_empty(), "rules untouched for {broken}");
        }
    }

    // IP в списке domains клиент тоже выкидывает с WARN, поэтому отказ тот же.
    #[tokio::test]
    async fn update_rejects_ip_in_domains() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_preset(dir.path());

        let err = update_preset(
            State(state.clone()),
            extract::Path("russia".to_string()),
            Json(CreatePresetRequest {
                name: "russia".into(),
                description: String::new(),
                rules: rules_with_domains(&["192.0.2.10"]),
            }),
        )
        .await
        .unwrap_err();

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("192.0.2.10"));

        assert_eq!(state.presets.read().await.get("russia").unwrap().version, 1);
    }

    // Битый CIDR в ip_ranges отказывает PUT тем же порядком, что и битый
    // домен (ревью XR-308): клиент выкидывает такой диапазон с WARN, и адреса
    // молча не маршрутизируются. Без префикса диапазон тоже битый: одиночный
    // IP клиент в ip_ranges не разбирает, ему нужна форма a.b.c.d/32.
    #[tokio::test]
    async fn update_rejects_broken_cidr_and_keeps_preset() {
        for broken in ["192.0.2.0/33", "2001:db8::/129", "192.0.2.0"] {
            let dir = tempfile::tempdir().unwrap();
            let state = state_with_preset(dir.path());

            let err = update_preset(
                State(state.clone()),
                extract::Path("russia".to_string()),
                Json(CreatePresetRequest {
                    name: "russia".into(),
                    description: String::new(),
                    rules: RoutingConfig {
                        default_action: "direct".into(),
                        rules: vec![RoutingRule {
                            name: Some("Telegram".into()),
                            action: "proxy".into(),
                            domains: vec!["*.telegram.org".into()],
                            ip_ranges: vec![broken.into()],
                            geoip: vec![],
                            outbound: None,
                        }],
                    },
                }),
            )
            .await
            .unwrap_err();

            assert_eq!(err.0, StatusCode::BAD_REQUEST, "cidr {broken}");
            assert!(err.1.contains(broken), "error names the range: {}", err.1);
            assert!(
                err.1.contains("broken CIDR range"),
                "error names the reason: {}",
                err.1
            );

            let presets = state.presets.read().await;
            assert_eq!(
                presets.get("russia").unwrap().version,
                1,
                "version untouched for {broken}"
            );
        }
    }

    // Годный домен с однобуквенным TLD нового поколения проходит: отказ
    // не шире классификации, которой пользуется клиент.
    #[tokio::test]
    async fn update_accepts_valid_domains() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_preset(dir.path());

        let updated = update_preset(
            State(state.clone()),
            extract::Path("russia".to_string()),
            Json(CreatePresetRequest {
                name: "russia".into(),
                description: "Google через резидента".into(),
                rules: RoutingConfig {
                    default_action: "direct".into(),
                    rules: vec![RoutingRule {
                        name: Some("Google".into()),
                        action: "proxy".into(),
                        domains: vec![
                            "*.antigravity.google".into(),
                            "*.google.com".into(),
                            "youtu.be".into(),
                        ],
                        ip_ranges: vec!["192.0.2.0/24".into()],
                        geoip: vec![],
                        outbound: None,
                    }],
                },
            }),
        )
        .await
        .unwrap();

        assert_eq!(updated.version, 2);
        assert_eq!(
            state.presets.read().await.get("russia").unwrap().version,
            2
        );
    }

    // Создание пресета проверяет те же домены: дыра на POST закрылась бы
    // первым же PUT, но битый пресет не должен появляться и на один шаг.
    #[tokio::test]
    async fn create_rejects_broken_domain() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_preset(dir.path());

        let err = create_preset(
            State(state.clone()),
            Json(CreatePresetRequest {
                name: "turkey".into(),
                description: String::new(),
                rules: rules_with_domains(&["*.goog"]),
            }),
        )
        .await
        .unwrap_err();

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("*.goog"));
        assert!(!state.presets.read().await.contains_key("turkey"));
    }

    // Живой эталон `configs/routing-russia.toml` принимается хабом без
    // правок: по нему заводится боевой пресет, и каждое пополнение эталона
    // обязано проходить валидацию на входе. Проверка идёт самой ручкой,
    // а не функцией валидации напрямую.
    #[tokio::test]
    async fn reference_russia_preset_passes_domain_validation() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            routing: RoutingConfig,
        }
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../configs/routing-russia.toml");
        let content = std::fs::read_to_string(path).unwrap();
        let cfg: Wrapper = toml::from_str(&content).unwrap();
        assert!(!cfg.routing.rules.is_empty());

        let dir = tempfile::tempdir().unwrap();
        let state = state_with_preset(dir.path());
        let updated = update_preset(
            State(state.clone()),
            extract::Path("russia".to_string()),
            Json(CreatePresetRequest {
                name: "russia".into(),
                description: String::new(),
                rules: cfg.routing,
            }),
        )
        .await
        .unwrap();

        assert_eq!(updated.version, 2);
        assert_eq!(
            state.presets.read().await.get("russia").unwrap().version,
            2
        );
    }
}
