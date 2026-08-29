//! Реестр публикаций и мандаты к ним (LLD-38 п. 2.1, п. 3.4, п. 3.5).
//!
//! Публикация это «имя, агент, локальный адрес», и хаб помнит из этой тройки
//! только первые две части: апстрим остаётся в конфиге агента, посреднику
//! внутренний адрес машины владельца знать незачем. Имя уникально в пределах
//! хаба, потому что оно же поддомен браузерного входа, а поддомен адресует
//! ровно одну машину.
//!
//! Ручки заведения и снятия авторизуются мандатом агента ([`AgentCredential`]),
//! тем же, что `share/add`: доска админки для этого не нужна, публикацию
//! заводит владелец со своей машины одной командой. Мандат публикации
//! ([`ExposeToken`]) выписывает только хаб и проверяет офлайн только агент.
//! Держателю relay-токена мандат не выдаётся ни при каких условиях: транзит к
//! машине и выбор того, что на ней открыто, это разные права (п. 3.4).

use std::path::Path;
use std::sync::Arc;

use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use base64::Engine;
use serde::{Deserialize, Serialize};
use xr_proto::share::{
    sign_expose_token, sign_relay_token, valid_publication_name, verify_agent_credential,
    web_share_id, AgentCredential, ExposeRecord, ExposeToken, WebRoute,
};

use crate::api::register::now_unix;
use crate::signing::SigningContext;
use crate::state::AppState;

/// Срок мандата публикации по умолчанию: сутки. Мандат живёт на посреднике в
/// памяти и перевыписывается запросом маршрута, поэтому долгий срок ему ни к
/// чему, а короткий ограничивает окно у утёкшего.
const DEFAULT_MANDATE_TTL: u64 = 24 * 3600;
/// Потолок срока мандата: неделя.
const MAX_MANDATE_TTL: u64 = 7 * 24 * 3600;

fn signing_or_503(state: &AppState) -> Result<&SigningContext, (StatusCode, String)> {
    state.signing.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "hub signing key not configured".into(),
    ))
}

/// Разобрать и проверить мандат агента. Любой отказ (декод, подпись, срок) это
/// чистый «не авторизован», а не 500.
fn verify_credential_blob(
    signing: &SigningContext,
    blob: &str,
    now: u64,
) -> Result<AgentCredential, (StatusCode, String)> {
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(blob.trim())
        .map_err(|_| (StatusCode::BAD_REQUEST, "malformed agent credential".into()))?;
    let cred: AgentCredential = serde_json::from_slice(&json)
        .map_err(|_| (StatusCode::BAD_REQUEST, "malformed agent credential".into()))?;
    verify_agent_credential(&cred, &signing.verifying_key(), now)
        .map_err(|e| (StatusCode::FORBIDDEN, e.to_string()))?;
    Ok(cred)
}

/// Мандат агента из заголовка `Authorization: Bearer <blob>`. Так авторизуются
/// ручки без тела (список и снятие): тело у GET и DELETE неуместно, а второго
/// вида авторизации заводить незачем.
fn credential_from_header(
    signing: &SigningContext,
    headers: &HeaderMap,
    now: u64,
) -> Result<AgentCredential, (StatusCode, String)> {
    let blob = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .ok_or((
            StatusCode::UNAUTHORIZED,
            "expected Authorization: Bearer <agent credential>".to_string(),
        ))?;
    verify_credential_blob(signing, blob, now)
}

/// Проверить имя публикации до всего остального: имя едет в `Host`, и всё, что
/// не DNS-метка, до агента просто не доберётся.
fn checked_name(raw: &str) -> Result<String, (StatusCode, String)> {
    let name = raw.trim().to_string();
    if !valid_publication_name(&name) {
        return Err((
            StatusCode::BAD_REQUEST,
            "имя публикации это DNS-метка: строчные буквы, цифры и дефис, 1..63 символа, дефис не с краю"
                .into(),
        ));
    }
    Ok(name)
}

fn clamp_mandate_ttl(ttl: Option<u64>) -> Result<u64, (StatusCode, String)> {
    let ttl = ttl.unwrap_or(DEFAULT_MANDATE_TTL);
    if ttl == 0 || ttl > MAX_MANDATE_TTL {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("ttl_seconds must be 1..={MAX_MANDATE_TTL}"),
        ));
    }
    Ok(ttl)
}

// Заведение публикации.

#[derive(Debug, Deserialize)]
pub struct AddExposeReq {
    /// Мандат агента (base64url blob), как у `share/add`.
    pub credential: String,
    /// Имя публикации, оно же поддомен.
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct ExposeResp {
    pub name: String,
    pub agent_pubkey: String,
    pub created: String,
}

impl From<&ExposeRecord> for ExposeResp {
    fn from(r: &ExposeRecord) -> Self {
        Self {
            name: r.name.clone(),
            agent_pubkey: r.agent_pubkey.clone(),
            created: r.created.clone(),
        }
    }
}

/// `POST /api/v1/expose/add` - завести публикацию под ключом предъявленного
/// мандата. Повтор своей же публикации проходит молча (идемпотентно), занятое
/// чужим агентом имя отвергается: поддомен адресует ровно одну машину.
pub async fn add(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AddExposeReq>,
) -> Result<Json<ExposeResp>, (StatusCode, String)> {
    let signing = signing_or_503(&state)?;
    let now = now_unix();
    let cred = verify_credential_blob(signing, &req.credential, now)?;
    let name = checked_name(&req.name)?;

    let mut exposes = state.exposes.write().await;
    if let Some(rec) = exposes.get(&name) {
        if rec.agent_pubkey != cred.agent_pubkey {
            return Err((
                StatusCode::CONFLICT,
                format!("имя {name} уже занято другим агентом"),
            ));
        }
        return Ok(Json(ExposeResp::from(rec)));
    }
    let rec = ExposeRecord {
        name: name.clone(),
        agent_pubkey: cred.agent_pubkey.clone(),
        created: chrono::Utc::now().to_rfc3339(),
    };
    crate::storage::save_expose(Path::new(&state.config.server.data_dir), &rec)
        .map_err(|e| crate::api::persist_failed("запись публикации", e))?;
    let resp = ExposeResp::from(&rec);
    exposes.insert(name, rec);
    Ok(Json(resp))
}

/// `GET /api/v1/expose` - публикации предъявившего мандат агента. Чужих в
/// ответе нет: список публикаций это карта чужих машин, посторонним она ни к
/// чему.
pub async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<ExposeResp>>, (StatusCode, String)> {
    let signing = signing_or_503(&state)?;
    let cred = credential_from_header(signing, &headers, now_unix())?;
    let exposes = state.exposes.read().await;
    let mut out: Vec<ExposeResp> = exposes
        .values()
        .filter(|r| r.agent_pubkey == cred.agent_pubkey)
        .map(ExposeResp::from)
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(out))
}

/// `DELETE /api/v1/expose/{name}` - снять свою публикацию. Чужую снять нельзя,
/// как и чужую шару.
pub async fn remove(
    State(state): State<Arc<AppState>>,
    AxPath(name): AxPath<String>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, String)> {
    let signing = signing_or_503(&state)?;
    let cred = credential_from_header(signing, &headers, now_unix())?;

    let mut exposes = state.exposes.write().await;
    match exposes.get(&name) {
        None => return Err((StatusCode::NOT_FOUND, "публикация не найдена".into())),
        Some(rec) if rec.agent_pubkey != cred.agent_pubkey => {
            return Err((StatusCode::FORBIDDEN, "публикация принадлежит другому агенту".into()))
        }
        Some(_) => {}
    }
    exposes.remove(&name);
    drop(exposes);
    crate::storage::delete_expose_file(Path::new(&state.config.server.data_dir), &name)
        .map_err(|e| crate::api::persist_failed("снятие публикации", e))?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/admin/exposes` - все публикации хаба для админки. Владелец
/// смотрит их разделом «Публикации», не поднимая агента: реестр общий, а мандат
/// агента у админа под рукой не всегда.
pub async fn admin_list(State(state): State<Arc<AppState>>) -> Json<Vec<ExposeResp>> {
    let exposes = state.exposes.read().await;
    let mut out: Vec<ExposeResp> = exposes.values().map(ExposeResp::from).collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Json(out)
}

/// `DELETE /api/v1/admin/exposes/{name}` - снять любую публикацию из админки.
/// Это дверь владельца хаба: агента может не быть на связи вовсе, а поддомен
/// освободить надо.
pub async fn admin_remove(
    State(state): State<Arc<AppState>>,
    AxPath(name): AxPath<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let mut exposes = state.exposes.write().await;
    if exposes.remove(&name).is_none() {
        return Err((StatusCode::NOT_FOUND, "публикация не найдена".into()));
    }
    drop(exposes);
    crate::storage::delete_expose_file(Path::new(&state.config.server.data_dir), &name)
        .map_err(|e| crate::api::persist_failed("снятие публикации", e))?;
    Ok(StatusCode::NO_CONTENT)
}

// Мандат публикации.

#[derive(Debug, Deserialize)]
pub struct MandateReq {
    pub credential: String,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct MandateResp {
    /// base64url blob мандата: агент ждёт его в `Authorization: Bearer`.
    pub mandate: String,
    pub exp: u64,
}

/// `POST /api/v1/expose/{name}/mandate` - мандат на свою публикацию. Пока
/// фронта `xr-web` нет, это единственный минт, и берёт его сам владелец для
/// харнесса `expose open` (LLD-38 п. 6, фаза 1). Мандат на свою же машину не
/// даёт держателю ничего сверх того, что у него и так есть, поэтому мандата
/// агента тут достаточно; фронт же придёт за маршрутом служебной ручкой под
/// общим секретом (п. 3.5), и своего ключа подписи у него не будет.
pub async fn mandate(
    State(state): State<Arc<AppState>>,
    AxPath(name): AxPath<String>,
    Json(req): Json<MandateReq>,
) -> Result<Json<MandateResp>, (StatusCode, String)> {
    let signing = signing_or_503(&state)?;
    let now = now_unix();
    let cred = verify_credential_blob(signing, &req.credential, now)?;
    let ttl = clamp_mandate_ttl(req.ttl_seconds)?;

    {
        let exposes = state.exposes.read().await;
        let rec = exposes
            .get(&name)
            .ok_or((StatusCode::NOT_FOUND, "публикация не найдена".into()))?;
        if rec.agent_pubkey != cred.agent_pubkey {
            return Err((StatusCode::FORBIDDEN, "публикация принадлежит другому агенту".into()));
        }
    }

    let exp = now.saturating_add(ttl);
    let token = sign_expose_token(&signing.signing_key, &name, &cred.agent_pubkey, exp);
    Ok(Json(MandateResp { mandate: encode_mandate(&token), exp }))
}

/// base64url-no-pad JSON мандата: та же форма блоба, что у токенов шар, и
/// агент разбирает её тем же способом. Сама форма живёт рядом с типом в
/// `xr-proto`, чтобы минт и предъявление фронтом не разъехались.
pub fn encode_mandate(token: &ExposeToken) -> String {
    xr_proto::share::encode_expose_mandate(token)
}

// Служебные ручки браузерного фронта (LLD-38 п. 3.5).
//
// Это единственная дверь `xr-web` в хаб, и она умышленно узкая: маршрут
// публикации, вердикт по паролю и состояние публикаций. Прав админки у фронта
// нет (сессию ему не дают), приватного ключа хаба тоже, поэтому взломанный
// фронт не выпишет себе мандат на агента, которого хаб ему не отдавал.

/// Сколько живёт выданный фронту маршрут: час. Кеш маршрута на фронте живёт до
/// `exp` минус запас, поэтому чаще раза в час хаб об одной публикации не
/// спрашивают, а окно у утёкшего маршрута остаётся коротким.
const WEB_ROUTE_TTL: u64 = 3600;

/// Сколько неверных паролей подряд проходят без задержки. Владелец промахивается
/// раскладкой, перебор упирается в задержку с четвёртой попытки.
const PASSWORD_FREE_ATTEMPTS: u32 = 3;
/// Потолок задержки: пять минут. Дальше расти незачем, перебор при таком шаге
/// уже мёртв, а владелец с честной опечаткой не заперт на сутки.
const PASSWORD_MAX_DELAY_MS: u64 = 5 * 60 * 1000;

/// Проверить общий секрет служебных ручек: `Authorization: Bearer <секрет>`.
/// Сравнение постоянного времени, отказ без подробностей о том, что именно не
/// сошлось. Блок `[web]` не задан значит браузерный вход выключен, и ручка
/// говорит это прямо, а не притворяется отказом авторизации.
fn require_web_secret(state: &AppState, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    use subtle::ConstantTimeEq;
    let web = state.config.web.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "браузерный вход не настроен: нет блока [web]".to_string(),
    ))?;
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    let ok: bool = presented
        .as_bytes()
        .ct_eq(web.shared_secret.as_bytes())
        .into();
    if ok && !web.shared_secret.is_empty() {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "нужен общий секрет [web]".into()))
    }
}

#[derive(Debug, Deserialize)]
pub struct RouteReq {
    /// Имя публикации из `Host` браузерного запроса.
    pub publication: String,
}

/// `POST /api/v1/web/route` - собрать маршрут публикации для браузерного
/// фронта: агент, relay с транзитным токеном, мандат публикации и потолок жизни
/// сплайса. Минт делает хаб, потому что только у него есть ключ подписи.
pub async fn route(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<RouteReq>,
) -> Result<Json<WebRoute>, (StatusCode, String)> {
    require_web_secret(&state, &headers)?;
    let signing = signing_or_503(&state)?;
    let relay = state.config.relay.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "у хаба нет relay: браузерному входу не через что идти".to_string(),
    ))?;
    let name = checked_name(&req.publication)?;

    let agent_pubkey = {
        let exposes = state.exposes.read().await;
        exposes
            .get(&name)
            .ok_or((StatusCode::NOT_FOUND, "публикация не найдена".into()))?
            .agent_pubkey
            .clone()
    };

    let exp = now_unix().saturating_add(WEB_ROUTE_TTL);
    Ok(Json(WebRoute {
        publication: name.clone(),
        agent_pubkey: agent_pubkey.clone(),
        relay: relay.descriptor(),
        relay_token: sign_relay_token(
            &signing.signing_key,
            &web_share_id(&name),
            &agent_pubkey,
            exp,
        ),
        expose_token: sign_expose_token(&signing.signing_key, &name, &agent_pubkey, exp),
        exp,
        splice_lifetime_secs: relay.splice_lifetime_secs,
    }))
}

#[derive(Debug, Deserialize)]
pub struct VerifyPasswordReq {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct VerifyPasswordResp {
    /// Только вердикт: своей учётной базы у фронта нет, а подробности отказа
    /// ему знать незачем.
    pub ok: bool,
}

/// `POST /api/v1/web/verify-password` - вердикт по паролю владельца для входа
/// на публикацию (LLD-38 п. 3.2). Второго пароля владельцу не заводим, хэши
/// живут там же, где жили, а фронт получает `true`/`false`.
pub async fn verify_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<VerifyPasswordReq>,
) -> Result<Json<VerifyPasswordResp>, (StatusCode, String)> {
    verify_password_at(&state, &headers, &req, now_ms())
}

/// Та же проверка с явным «сейчас» в миллисекундах: лимит попыток проверяется
/// временем, и тест обязан двигать его сам, а не спать.
pub fn verify_password_at(
    state: &AppState,
    headers: &HeaderMap,
    req: &VerifyPasswordReq,
    now_ms: u64,
) -> Result<Json<VerifyPasswordResp>, (StatusCode, String)> {
    require_web_secret(state, headers)?;
    if let Some(wait_ms) = state.web_attempts.blocked_for(&req.username, now_ms) {
        let secs = wait_ms.div_ceil(1000);
        tracing::warn!(
            "вход на браузерный фронт: попытки {} упёрлись в задержку, ещё {secs} с",
            req.username
        );
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            format!("слишком много попыток, повторить через {secs} с"),
        ));
    }
    if crate::api::auth::password_matches(&state.config.admin.users, &req.username, &req.password) {
        state.web_attempts.succeeded(&req.username);
        Ok(Json(VerifyPasswordResp { ok: true }))
    } else {
        state.web_attempts.failed(&req.username, now_ms);
        tracing::warn!("вход на браузерный фронт: неверный пароль для {}", req.username);
        Ok(Json(VerifyPasswordResp { ok: false }))
    }
}

/// Состояние одной публикации: кто её держит и на связи ли он сейчас.
#[derive(Debug, Serialize)]
pub struct PublicationStatus {
    pub name: String,
    pub agent_pubkey: String,
    pub created: String,
    /// Полное имя, по которому публикация открывается в браузере. Пусто, если
    /// web-домен в конфиге не задан.
    pub host: String,
    /// `true` агент в реестре relay, `false` его там нет, `null` спросить не
    /// вышло (см. `probe_error`): «не знаю» и «выключен» это разные ответы.
    pub online: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_error: Option<String>,
}

/// `GET /api/v1/web/status` - публикации и их живость (LLD-38 п. 2.5).
/// Неработающая публикация отличима от работающей без чтения логов.
pub async fn status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<PublicationStatus>>, (StatusCode, String)> {
    require_web_secret(&state, &headers)?;
    let probe = |name: String, agent: String| {
        let state = state.clone();
        async move { probe_via_relay(&state, &name, &agent).await }
    };
    status_with(&state, probe).await.map(Json)
}

/// Та же сборка статуса с внешней проверкой живости: тест подставляет свой
/// вердикт, а настоящий поход на relay проверяется в `xr-proto`.
pub async fn status_with<F, Fut>(state: &AppState, probe: F) -> Result<Vec<PublicationStatus>, (StatusCode, String)>
where
    F: Fn(String, String) -> Fut,
    Fut: std::future::Future<Output = Result<bool, String>>,
{
    let domain = state.config.web.as_ref().map(|w| w.domain.clone()).unwrap_or_default();
    let records: Vec<ExposeRecord> = {
        let exposes = state.exposes.read().await;
        let mut v: Vec<ExposeRecord> = exposes.values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    };
    let mut out = Vec::with_capacity(records.len());
    for rec in records {
        let (online, probe_error) = match probe(rec.name.clone(), rec.agent_pubkey.clone()).await {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };
        out.push(PublicationStatus {
            host: if domain.is_empty() { String::new() } else { format!("{}.{}", rec.name, domain) },
            name: rec.name,
            agent_pubkey: rec.agent_pubkey,
            created: rec.created,
            online,
            probe_error,
        });
    }
    Ok(out)
}

/// Сколько ждём вердикта relay о живости агента. Статус это диагностика, и
/// висеть на ней дольше пары секунд незачем: неответивший relay честно уходит в
/// «спросить не вышло».
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Спросить relay, на связи ли агент: открыть транзитный стрим по свежему
/// токену и сразу закрыть. Своего состояния о живости хаб не держит и кода
/// relay не трогает (LLD-38 п. 2.5).
async fn probe_via_relay(state: &AppState, name: &str, agent_pubkey: &str) -> Result<bool, String> {
    let signing = state.signing.as_ref().ok_or("у хаба нет ключа подписи")?;
    let relay = state.config.relay.as_ref().ok_or("у хаба не настроен relay")?;
    let token = sign_relay_token(
        &signing.signing_key,
        &web_share_id(name),
        agent_pubkey,
        now_unix() + 60,
    );
    let grant = xr_proto::share::RelayGrant {
        addr: relay.addr.clone(),
        port: relay.port,
        obf: relay.obf.clone(),
        relay_token: token,
    };
    let endpoint = xr_proto::relay_client::RelayEndpoint::from_grant(&grant)?;
    match tokio::time::timeout(PROBE_TIMEOUT, xr_proto::relay_client::probe_agent_online(&endpoint))
        .await
    {
        Ok(Ok(online)) => Ok(online),
        Ok(Err(e)) => Err(format!("relay недоступен: {e}")),
        Err(_) => Err("relay не ответил вовремя".into()),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Счётчик неверных паролей на имя владельца с растущей задержкой (LLD-38
/// п. 3.2). Живёт в памяти хаба: рестарт сбрасывает счётчики, и это осознанно,
/// перебор через рестарт чужого процесса не ускоряется.
#[derive(Default)]
pub struct PasswordAttempts {
    inner: std::sync::Mutex<std::collections::HashMap<String, Attempt>>,
}

#[derive(Default)]
struct Attempt {
    failures: u32,
    /// Когда истекает задержка после последнего промаха, мс от эпохи.
    blocked_until_ms: u64,
}

impl PasswordAttempts {
    /// Сколько ещё миллисекунд имя под задержкой; `None` значит проверка
    /// разрешена прямо сейчас.
    pub fn blocked_for(&self, username: &str, now_ms: u64) -> Option<u64> {
        let map = self.inner.lock().expect("attempts lock");
        let a = map.get(username)?;
        (a.blocked_until_ms > now_ms).then(|| a.blocked_until_ms - now_ms)
    }

    /// Промах: счётчик растёт, задержка удваивается после первых свободных
    /// попыток.
    pub fn failed(&self, username: &str, now_ms: u64) {
        let mut map = self.inner.lock().expect("attempts lock");
        let a = map.entry(username.to_string()).or_default();
        a.failures = a.failures.saturating_add(1);
        let over = a.failures.saturating_sub(PASSWORD_FREE_ATTEMPTS);
        let delay = if over == 0 {
            0
        } else {
            (1000u64 << (over - 1).min(20)).min(PASSWORD_MAX_DELAY_MS)
        };
        a.blocked_until_ms = now_ms.saturating_add(delay);
    }

    /// Верный пароль снимает и счётчик, и задержку.
    pub fn succeeded(&self, username: &str) {
        self.inner.lock().expect("attempts lock").remove(username);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use ed25519_dalek::SigningKey;
    use tokio::sync::RwLock;
    use xr_proto::share::{sign_agent_credential, verify_expose_token};

    use crate::config::HubConfig;

    fn agent_pk(seed: u8) -> String {
        base64::engine::general_purpose::STANDARD
            .encode(SigningKey::from_bytes(&[seed; 32]).verifying_key().as_bytes())
    }

    fn cred_blob(hub: &SigningKey, pk: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&sign_agent_credential(hub, pk, now_unix() + 3600)).unwrap(),
        )
    }

    fn state_with(dir: &Path, hub: SigningKey) -> Arc<AppState> {
        state_from(dir, hub, "[admin]\nusers = []\n")
    }

    /// Состояние с настроенным браузерным входом: общий секрет, web-домен,
    /// relay и учётка владельца (пароль `owner-secret`).
    fn state_web(dir: &Path, hub: SigningKey) -> Arc<AppState> {
        let hash = crate::api::auth::hash_password("owner-secret").unwrap();
        state_from(
            dir,
            hub,
            &format!(
                concat!(
                    "[admin]\n[[admin.users]]\nusername = \"owner\"\npassword_hash = {hash:?}\n",
                    "[web]\ndomain = \"web.example.com\"\nshared_secret = \"s3cret\"\n",
                    "[relay]\naddr = \"relay.example.com\"\nport = 8444\n",
                    "splice_lifetime_secs = 900\n",
                    "[relay.obfuscation]\nkey = \"dGVzdC1rZXktMzItYnl0ZXMtbG9uZy1lbm91Z2ghISE=\"\n",
                ),
                hash = hash
            ),
        )
    }

    fn state_from(dir: &Path, hub: SigningKey, extra: &str) -> Arc<AppState> {
        let text = format!("[server]\ndata_dir = {:?}\n{extra}", dir.display().to_string());
        let config: HubConfig = toml::from_str(&text).unwrap();
        Arc::new(AppState {
            presets: RwLock::new(HashMap::new()),
            invites: RwLock::new(HashMap::new()),
            shares: RwLock::new(HashMap::new()),
            exposes: RwLock::new(HashMap::new()),
            sessions: crate::sessions::SessionStore::new(
                std::time::Duration::from_secs(config.admin.session_ttl_secs),
                config.admin.max_sessions_per_user,
            ),
            config,
            signing: Some(SigningContext { signing_key: hub }),
            preset_gen: tokio::sync::watch::Sender::new(0),
            ready: std::sync::atomic::AtomicBool::new(true),
            web_attempts: Default::default(),
            login_attempts: Default::default(),
        })
    }

    fn bearer(blob: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {blob}").parse().unwrap());
        h
    }

    #[tokio::test]
    async fn test_hub_expose_name_conflict() {
        // Имя публикации это поддомен, а поддомен адресует ровно одну машину:
        // второй агент на занятое имя получает отказ, а не тихую подмену
        // маршрута под чужой публикацией.
        let dir = tempfile::tempdir().unwrap();
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let state = state_with(dir.path(), hub.clone());

        let first = cred_blob(&hub, &agent_pk(7));
        let Json(resp) = add(
            State(state.clone()),
            Json(AddExposeReq { credential: first.clone(), name: "dash".into() }),
        )
        .await
        .expect("своё имя заводится");
        assert_eq!(resp.name, "dash");
        assert_eq!(resp.agent_pubkey, agent_pk(7));

        // Тот же агент повторно: идемпотентно, без конфликта.
        assert!(add(
            State(state.clone()),
            Json(AddExposeReq { credential: first, name: "dash".into() })
        )
        .await
        .is_ok());

        // Чужой агент на то же имя.
        let err = add(
            State(state.clone()),
            Json(AddExposeReq { credential: cred_blob(&hub, &agent_pk(9)), name: "dash".into() }),
        )
        .await
        .expect_err("чужое имя занимать нельзя");
        assert_eq!(err.0, StatusCode::CONFLICT);

        // И запись на диске осталась за первым агентом.
        let stored = crate::storage::load_all_exposes(dir.path()).unwrap();
        assert_eq!(stored["dash"].agent_pubkey, agent_pk(7));
    }

    #[tokio::test]
    async fn expose_list_and_remove_are_per_agent() {
        let dir = tempfile::tempdir().unwrap();
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let state = state_with(dir.path(), hub.clone());
        let mine = cred_blob(&hub, &agent_pk(7));
        let alien = cred_blob(&hub, &agent_pk(9));

        for name in ["dash", "notes"] {
            let _ = add(
                State(state.clone()),
                Json(AddExposeReq { credential: mine.clone(), name: name.into() }),
            )
            .await
            .unwrap();
        }
        let _ = add(
            State(state.clone()),
            Json(AddExposeReq { credential: alien.clone(), name: "other".into() }),
        )
        .await
        .unwrap();

        // Список это только свои публикации, отсортированные по имени.
        let Json(list_mine) = list(State(state.clone()), bearer(&mine)).await.unwrap();
        assert_eq!(
            list_mine.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["dash", "notes"]
        );

        // Чужую публикацию не снять.
        let err = remove(State(state.clone()), AxPath("other".into()), bearer(&mine))
            .await
            .expect_err("чужая публикация не снимается");
        assert_eq!(err.0, StatusCode::FORBIDDEN);

        // Свою снять можно, и файл уходит с диска.
        assert_eq!(
            remove(State(state.clone()), AxPath("notes".into()), bearer(&mine)).await.unwrap(),
            StatusCode::NO_CONTENT
        );
        assert!(!crate::storage::load_all_exposes(dir.path()).unwrap().contains_key("notes"));

        // Без мандата список не отдаётся вовсе.
        let err = list(State(state.clone()), HeaderMap::new()).await.expect_err("нужен мандат");
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn mandate_is_minted_only_for_own_publication() {
        let dir = tempfile::tempdir().unwrap();
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let state = state_with(dir.path(), hub.clone());
        let mine = cred_blob(&hub, &agent_pk(7));
        let alien = cred_blob(&hub, &agent_pk(9));
        let _ = add(
            State(state.clone()),
            Json(AddExposeReq { credential: mine.clone(), name: "dash".into() }),
        )
        .await
        .unwrap();

        let Json(resp) = mandate(
            State(state.clone()),
            AxPath("dash".into()),
            Json(MandateReq { credential: mine, ttl_seconds: None }),
        )
        .await
        .expect("мандат на свою публикацию");
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&resp.mandate).unwrap();
        let token: ExposeToken = serde_json::from_slice(&json).unwrap();
        assert!(verify_expose_token(
            &token,
            &hub.verifying_key(),
            "dash",
            &agent_pk(7),
            now_unix()
        )
        .is_ok());

        // Чужому агенту мандата на эту публикацию не выписывают.
        let err = mandate(
            State(state.clone()),
            AxPath("dash".into()),
            Json(MandateReq { credential: alien.clone(), ttl_seconds: None }),
        )
        .await
        .expect_err("чужая публикация");
        assert_eq!(err.0, StatusCode::FORBIDDEN);

        // На несуществующую публикацию тоже.
        let err = mandate(
            State(state),
            AxPath("ghost".into()),
            Json(MandateReq { credential: alien, ttl_seconds: None }),
        )
        .await
        .expect_err("публикации нет");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn bad_publication_name_is_refused() {
        // Имя едет в Host, поэтому всё, что не DNS-метка, отвергается на
        // заведении, а не оседает записью, до которой браузер не доберётся.
        let dir = tempfile::tempdir().unwrap();
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let state = state_with(dir.path(), hub.clone());
        let cred = cred_blob(&hub, &agent_pk(7));
        for name in ["", "Dash", "da.sh", "../etc", "dash-"] {
            let err = add(
                State(state.clone()),
                Json(AddExposeReq { credential: cred.clone(), name: name.into() }),
            )
            .await
            .expect_err("имя обязано быть меткой");
            assert_eq!(err.0, StatusCode::BAD_REQUEST, "{name}");
        }
        assert!(crate::storage::load_all_exposes(dir.path()).unwrap().is_empty());
    }

    fn secret(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {value}").parse().unwrap());
        h
    }

    /// Завести публикацию `dash` на агенте с ключом `agent_pk(7)`.
    async fn with_dash(state: &Arc<AppState>, hub: &SigningKey) {
        let cred = cred_blob(hub, &agent_pk(7));
        let _ = add(
            State(state.clone()),
            Json(AddExposeReq { credential: cred, name: "dash".into() }),
        )
        .await
        .unwrap();
    }

    /// Маршрут это дверь фронта в хаб, и открывает её только общий секрет:
    /// без заголовка и с чужим секретом ручка не отдаёт ничего, а с верным
    /// собирает транзит на `web:<имя>` и мандат ровно на эту публикацию
    /// (LLD-38 п. 2.3, п. 3.5).
    #[tokio::test]
    async fn web_route_is_gated_by_shared_secret() {
        use xr_proto::share::verify_relay_token;
        let dir = tempfile::tempdir().unwrap();
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let state = state_web(dir.path(), hub.clone());
        with_dash(&state, &hub).await;

        for headers in [HeaderMap::new(), secret("wrong"), secret("s3cre")] {
            let err = route(
                State(state.clone()),
                headers,
                Json(RouteReq { publication: "dash".into() }),
            )
            .await
            .expect_err("без общего секрета маршрут не выдаётся");
            assert_eq!(err.0, StatusCode::UNAUTHORIZED);
        }

        let Json(r) = route(
            State(state.clone()),
            secret("s3cret"),
            Json(RouteReq { publication: "dash".into() }),
        )
        .await
        .expect("с верным секретом маршрут собирается");
        assert_eq!(r.agent_pubkey, agent_pk(7));
        assert_eq!(r.relay.dial(), "relay.example.com:8444");
        assert_eq!(r.splice_lifetime_secs, 900);
        assert_eq!(r.relay_token.share_id, "web:dash", "расход виден отдельной строкой");
        let now = now_unix();
        assert!(verify_relay_token(
            &r.relay_token,
            &hub.verifying_key(),
            "web:dash",
            &agent_pk(7),
            now
        )
        .is_ok());
        assert!(
            verify_expose_token(&r.expose_token, &hub.verifying_key(), "dash", &agent_pk(7), now)
                .is_ok()
        );
        // Мандат бьётся ровно с этой публикацией: на соседнюю он не годится.
        assert!(
            verify_expose_token(&r.expose_token, &hub.verifying_key(), "notes", &agent_pk(7), now)
                .is_err()
        );

        // Публикации нет в реестре: 404, а не маршрут в никуда.
        let err = route(
            State(state.clone()),
            secret("s3cret"),
            Json(RouteReq { publication: "ghost".into() }),
        )
        .await
        .expect_err("публикации нет");
        assert_eq!(err.0, StatusCode::NOT_FOUND);

        // Без блока [web] браузерный вход выключен целиком, и ручка говорит
        // это прямо, а не отказом авторизации.
        let plain = state_with(dir.path(), hub);
        let err = route(
            State(plain),
            secret("s3cret"),
            Json(RouteReq { publication: "dash".into() }),
        )
        .await
        .expect_err("браузерный вход не настроен");
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    /// Перебор пароля упирается в задержку на стороне хаба, а верный пароль
    /// после её истечения проходит: владелец с опечаткой не заперт (LLD-38
    /// п. 3.2). Время двигаем сами, спать тесту незачем.
    #[tokio::test]
    async fn web_verify_password_rate_limits_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let state = state_web(dir.path(), hub);
        let wrong = VerifyPasswordReq { username: "owner".into(), password: "nope".into() };
        let right = VerifyPasswordReq { username: "owner".into(), password: "owner-secret".into() };
        let mut t = 1_000_000u64;

        // Первые промахи это просто вердикт «нет», без задержки.
        for _ in 0..PASSWORD_FREE_ATTEMPTS {
            let Json(v) = verify_password_at(&state, &secret("s3cret"), &wrong, t).unwrap();
            assert!(!v.ok);
        }
        // Следующий промах взводит задержку, и в неё упирается даже верный пароль.
        let Json(v) = verify_password_at(&state, &secret("s3cret"), &wrong, t).unwrap();
        assert!(!v.ok);
        let err = verify_password_at(&state, &secret("s3cret"), &right, t)
            .expect_err("серия неверных упирается в задержку");
        assert_eq!(err.0, StatusCode::TOO_MANY_REQUESTS);

        // Задержка прошла: верный пароль пускает и сбрасывает счётчик.
        t += 2_000;
        let Json(v) = verify_password_at(&state, &secret("s3cret"), &right, t).unwrap();
        assert!(v.ok, "после задержки верный пароль проходит");
        let Json(v) = verify_password_at(&state, &secret("s3cret"), &right, t).unwrap();
        assert!(v.ok, "успех снимает счётчик, второй вход не ждёт");

        // Чужой секрет не пускает к ручке вовсе, вердикта не видно.
        let err = verify_password_at(&state, &secret("wrong"), &right, t)
            .expect_err("нужен общий секрет");
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    /// Статус показывает публикацию и её живость: вердикт проверки едет в
    /// `online`, а неудавшаяся проверка это `null` с причиной, а не «выключен»
    /// (LLD-38 п. 2.5).
    #[tokio::test]
    async fn web_status_shows_publications_and_liveness() {
        let dir = tempfile::tempdir().unwrap();
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let state = state_web(dir.path(), hub.clone());
        with_dash(&state, &hub).await;
        let _ = add(
            State(state.clone()),
            Json(AddExposeReq { credential: cred_blob(&hub, &agent_pk(9)), name: "notes".into() }),
        )
        .await
        .unwrap();

        let out = status_with(&state, |name, _agent| async move {
            match name.as_str() {
                "dash" => Ok(true),
                _ => Err("relay недоступен".to_string()),
            }
        })
        .await
        .unwrap();
        assert_eq!(out.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), vec!["dash", "notes"]);
        assert_eq!(out[0].online, Some(true));
        assert_eq!(out[0].host, "dash.web.example.com");
        assert_eq!(out[0].agent_pubkey, agent_pk(7));
        assert_eq!(out[1].online, None, "спросить не вышло это не «выключен»");
        assert_eq!(out[1].probe_error.as_deref(), Some("relay недоступен"));

        // Агента нет в реестре relay: публикация видна и честно помечена.
        let out = status_with(&state, |_n, _a| async { Ok(false) }).await.unwrap();
        assert_eq!(out[0].online, Some(false));

        // Ручка закрыта тем же общим секретом, что и маршрут.
        let err = status(State(state), HeaderMap::new()).await.expect_err("нужен секрет");
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    /// Раздел «Публикации» в админке снимает публикацию и без агента: реестр
    /// общий, а поддомен освобождать надо и с выключенной машины.
    #[tokio::test]
    async fn admin_sees_and_removes_any_publication() {
        let dir = tempfile::tempdir().unwrap();
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let state = state_web(dir.path(), hub.clone());
        with_dash(&state, &hub).await;

        let Json(list) = admin_list(State(state.clone())).await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "dash");

        assert_eq!(
            admin_remove(State(state.clone()), AxPath("dash".into())).await.unwrap(),
            StatusCode::NO_CONTENT
        );
        assert!(crate::storage::load_all_exposes(dir.path()).unwrap().is_empty());
        let err = admin_remove(State(state), AxPath("dash".into()))
            .await
            .expect_err("снятой публикации нет");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }
}
