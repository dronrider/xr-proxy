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
    sign_expose_token, valid_publication_name, verify_agent_credential, AgentCredential,
    ExposeRecord, ExposeToken,
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
/// агент разбирает её тем же способом.
pub fn encode_mandate(token: &ExposeToken) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(token).expect("serialize expose token"))
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
        let text = format!(
            "[server]\ndata_dir = {:?}\n[admin]\nusers = []\n",
            dir.display().to_string()
        );
        let config: HubConfig = toml::from_str(&text).unwrap();
        Arc::new(AppState {
            presets: RwLock::new(HashMap::new()),
            invites: RwLock::new(HashMap::new()),
            shares: RwLock::new(HashMap::new()),
            exposes: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            config,
            signing: Some(SigningContext { signing_key: hub }),
            preset_gen: tokio::sync::watch::Sender::new(0),
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
}
