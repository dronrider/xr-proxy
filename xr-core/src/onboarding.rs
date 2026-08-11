//! One-shot HTTP calls against xr-hub used during Android onboarding.
//!
//! Two flows are exposed:
//! - [`fetch_invite_info`]: `GET /api/v1/invite/<token>`, metadata only,
//!   does NOT consume the invite. Used to render the confirmation screen.
//! - [`apply_invite`]: `POST /api/v1/invite/<token>/claim` (consumes
//!   one-time invites) + TOFU `GET /api/v1/public-key` + pre-warm fetch
//!   of the preset into the local cache. Used when the user taps Apply.
//!   Claim идёт с ключом этой установки в `X-Claim-Id` (XR-216), поэтому
//!   повторная попытка после сорванного разбора ответа снова получает
//!   payload вместо 410.
//!
//! Both flows are platform-agnostic; the JNI crate wraps them for Kotlin.
//!
//! Оба запроса идут обычным reqwest'ом, без `protect(fd)`, и на Android под
//! поднятым туннелем уезжают в TUN, то есть к хабу приходят через прокси
//! (XR-088). Так и задумано: сведения об инвайте тем же путём ходят с самого
//! начала, а хаб в стране пользователя бывает доступен как раз только через
//! туннель. Обход TUN потребовал бы своего коннектора с андроидным
//! `VpnService.protect` внутри общего крейта, а туннель на паузе TUN закрывает,
//! так что запрос всё равно уходит по обычной сети.

use std::path::Path;
use std::time::Duration;

use base64::Engine;
use reqwest::StatusCode;
use xr_proto::preset::{InviteInfo, InvitePayload, Preset};

use crate::presets::PresetCache;

/// Имя файла с ключом клиента рядом с кэшем пресетов.
const CLAIM_ID_FILE: &str = "claim-id";

#[derive(Debug)]
pub struct ApplyInviteResult {
    pub payload: Option<InvitePayload>,
    pub public_key: Option<String>,
    pub preset_cached: bool,
    pub errors: Vec<String>,
}

/// GET the invite metadata. Does not consume.
///
/// Запрос несёт тот же ключ установки, что и claim (XR-216): хаб по нему
/// отмечает в `InviteInfo.reclaimable`, что потреблённый инвайт принадлежит
/// спрашивающему. Без ключа экран подтверждения гасит кнопку по статусу
/// `consumed`, и до повторного claim дело не доходит вовсе.
pub async fn fetch_invite_info(
    hub_url: &str,
    token: &str,
    cache_dir: &Path,
    timeout: Duration,
) -> Result<InviteInfo, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    // Сведения об инвайте ошибок наружу не несут, поэтому о несохранившемся
    // ключе говорим в журнал; в Apply та же неудача попадает в errors.
    let mut key_errors = Vec::new();
    let claim_id = claim_id(cache_dir, &mut key_errors);
    for e in &key_errors {
        tracing::warn!("{e}");
    }

    let url = format!("{}/api/v1/invite/{}", hub_url.trim_end_matches('/'), token);
    let resp = client
        .get(&url)
        .header("X-Claim-Id", &claim_id)
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;

    match resp.status() {
        s if s.is_success() => resp
            .json::<InviteInfo>()
            .await
            .map_err(|e| format!("parse: {e}")),
        StatusCode::NOT_FOUND => Err("not_found".into()),
        StatusCode::GONE => Err("gone".into()),
        s => Err(format!("http_{}", s.as_u16())),
    }
}

/// Ключ этой установки для claim (XR-216). Хаб потребляет одноразовый инвайт в
/// момент выдачи payload'а, поэтому повторная попытка обычно упирается в 410, и
/// сорванный на нашей стороне разбор ответа (тело не легло в `InvitePayload`,
/// оборвалась сеть на чтении) запирал получателя до нового инвайта от админа. С
/// ключом хаб узнаёт того же клиента и отдаёт payload снова. Ключ лежит рядом с
/// кэшем пресетов и переживает перезапуск приложения: повторить Apply можно и
/// после обновления, из-за которого разбор наконец сойдётся.
fn claim_id(cache_dir: &Path, errors: &mut Vec<String>) -> String {
    let path = cache_dir.join(CLAIM_ID_FILE);
    if let Ok(saved) = std::fs::read_to_string(&path) {
        let saved = saved.trim();
        if !saved.is_empty() {
            return saved.to_string();
        }
    }

    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);

    // Не сохранился, значит второй попытки по этому же ключу не будет: говорим
    // об этом вслух, иначе потеря повтора выглядела бы как обычная ошибка сети.
    if let Err(e) = std::fs::create_dir_all(cache_dir).and_then(|()| std::fs::write(&path, &id)) {
        errors.push(format!("claim id save: {e}"));
    }
    id
}

/// Claim the invite, fetch the public key and preset, and cache the
/// preset to `cache_dir`. Non-fatal errors are recorded in
/// `result.errors`; `payload` being `Some(_)` means the claim succeeded
/// and the user can proceed even if preset fetch failed.
pub async fn apply_invite(
    hub_url: &str,
    token: &str,
    preset_name: &str,
    cache_dir: &Path,
    per_request_timeout: Duration,
) -> ApplyInviteResult {
    let mut errors = Vec::new();

    let client = match reqwest::Client::builder()
        .timeout(per_request_timeout)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return ApplyInviteResult {
                payload: None,
                public_key: None,
                preset_cached: false,
                errors: vec![format!("http client: {e}")],
            };
        }
    };

    let hub = hub_url.trim_end_matches('/');

    // 1. Claim the invite (may consume, per server policy).
    let claim_url = format!("{}/api/v1/invite/{}/claim", hub, token);
    let claim_id = claim_id(cache_dir, &mut errors);
    let payload = match client.post(&claim_url).header("X-Claim-Id", &claim_id).send().await {
        Ok(resp) => match resp.status() {
            s if s.is_success() => match resp.json::<InvitePayload>().await {
                Ok(p) => Some(p),
                Err(e) => {
                    errors.push(format!("claim parse: {e}"));
                    None
                }
            },
            StatusCode::NOT_FOUND => {
                errors.push("claim: not_found".into());
                None
            }
            StatusCode::GONE => {
                errors.push("claim: gone".into());
                None
            }
            s => {
                errors.push(format!("claim: http_{}", s.as_u16()));
                None
            }
        },
        Err(e) => {
            errors.push(format!("claim network: {e}"));
            None
        }
    };

    // If the claim failed there is nothing to apply, bail early.
    if payload.is_none() {
        return ApplyInviteResult {
            payload: None,
            public_key: None,
            preset_cached: false,
            errors,
        };
    }

    // 2. TOFU public key, best-effort.
    let pubkey_url = format!("{}/api/v1/public-key", hub);
    let public_key = match client.get(&pubkey_url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.text().await {
            Ok(body) => {
                let key = body.trim().to_string();
                if key.is_empty() {
                    None
                } else {
                    Some(key)
                }
            }
            Err(e) => {
                errors.push(format!("public-key read: {e}"));
                None
            }
        },
        Ok(resp) if resp.status() == StatusCode::NOT_FOUND => None,
        Ok(resp) => {
            errors.push(format!("public-key: http_{}", resp.status().as_u16()));
            None
        }
        Err(e) => {
            errors.push(format!("public-key network: {e}"));
            None
        }
    };

    // 3. Pre-warm the preset cache, best-effort.
    let preset_url = format!("{}/api/v1/presets/{}", hub, preset_name);
    let preset_cached = match client.get(&preset_url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<Preset>().await {
            Ok(preset) => match PresetCache::write_to_disk(cache_dir, &preset) {
                Ok(()) => true,
                Err(e) => {
                    errors.push(format!("preset cache write: {e}"));
                    false
                }
            },
            Err(e) => {
                errors.push(format!("preset parse: {e}"));
                false
            }
        },
        Ok(resp) => {
            errors.push(format!("preset: http_{}", resp.status().as_u16()));
            false
        }
        Err(e) => {
            errors.push(format!("preset network: {e}"));
            false
        }
    };

    ApplyInviteResult {
        payload,
        public_key,
        preset_cached,
        errors,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    const TOKEN: &str = "abcdefghij0123456789AB";
    const PUBLIC_KEY: &str = "3b5f8c1d2e4a6b7c8d9e0f1a2b3c4d5e";
    const PAYLOAD: &str = r#"{"server_address":"203.0.113.10","server_port":8443,
        "obfuscation_key":"a2V5","modifier":"positional_xor_rotate","salt":7,
        "preset":"russia","hub_url":"https://hub.example"}"#;
    // Хаб отдал 200, но тело в InvitePayload не ложится: так выглядит и версия
    // хаба со сменившимся форматом, и обрезанный ответ.
    const BROKEN_PAYLOAD: &str = r#"{"server_address":"203.0.113.10"}"#;
    const PRESET: &str = r#"{"name":"russia","version":3,"updated_at":"2026-01-01T00:00:00+00:00",
        "rules":{"default_action":"direct","rules":[]}}"#;
    // Сведения о потреблённом инвайте, который хаб признал нашим (XR-216).
    const INFO: &str = r#"{"token":"abcdefghij0123456789AB","preset":"russia","comment":"",
        "status":"consumed","expires_at":"2099-01-01T00:00:00+00:00","reclaimable":true}"#;

    #[derive(Clone, Debug)]
    struct SeenRequest {
        path: String,
        claim_id: Option<String>,
    }

    /// Хаб-заглушка на живом TCP: reqwest ходит к нему по-настоящему, поэтому
    /// в тест попадает и заголовок запроса, и разбор ответа целиком.
    struct FakeHub {
        url: String,
        seen: Arc<Mutex<Vec<SeenRequest>>>,
    }

    impl FakeHub {
        /// `claim_bodies` это очередь тел на последовательные claim'ы.
        async fn start(claim_bodies: Vec<&'static str>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let seen = Arc::new(Mutex::new(Vec::new()));
            let bodies: VecDeque<String> = claim_bodies.into_iter().map(String::from).collect();
            let bodies = Arc::new(Mutex::new(bodies));

            let seen_task = seen.clone();
            tokio::spawn(async move {
                while let Ok((mut sock, _)) = listener.accept().await {
                    let seen = seen_task.clone();
                    let bodies = bodies.clone();
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        let mut chunk = [0u8; 1024];
                        loop {
                            match sock.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => head.extend_from_slice(&chunk[..n]),
                            }
                            if head.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        let head = String::from_utf8_lossy(&head).to_string();
                        let path = head
                            .lines()
                            .next()
                            .and_then(|line| line.split(' ').nth(1))
                            .unwrap_or("")
                            .to_string();
                        let claim_id = head
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("x-claim-id:"))
                            .and_then(|l| l.split_once(':'))
                            .map(|(_, v)| v.trim().to_string());
                        seen.lock().unwrap().push(SeenRequest {
                            path: path.clone(),
                            claim_id,
                        });

                        let (ctype, body) = if path.ends_with("/claim") {
                            let body = bodies.lock().unwrap().pop_front().unwrap_or_default();
                            ("application/json", body)
                        } else if path.starts_with("/api/v1/invite/") {
                            ("application/json", INFO.to_string())
                        } else if path.ends_with("/public-key") {
                            ("text/plain", PUBLIC_KEY.to_string())
                        } else {
                            ("application/json", PRESET.to_string())
                        };
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        let _ = sock.shutdown().await;
                    });
                }
            });

            FakeHub { url, seen }
        }

        fn claims(&self) -> Vec<SeenRequest> {
            self.requests(|path| path.ends_with("/claim"))
        }

        /// Запросы сведений об инвайте: тот же путь, что у claim, но без хвоста.
        fn infos(&self) -> Vec<SeenRequest> {
            self.requests(|path| path.starts_with("/api/v1/invite/") && !path.ends_with("/claim"))
        }

        fn requests(&self, matches: impl Fn(&str) -> bool) -> Vec<SeenRequest> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|r| matches(&r.path))
                .cloned()
                .collect()
        }
    }

    async fn apply(hub: &FakeHub, cache_dir: &Path) -> ApplyInviteResult {
        apply_invite(&hub.url, TOKEN, "russia", cache_dir, Duration::from_secs(5)).await
    }

    // XR-216: одноразовый инвайт сгорает на сервере в момент выдачи payload'а,
    // поэтому вторая попытка возможна, только если хаб узнаёт того же клиента.
    // Ключ этой установки едет в claim и не меняется между попытками.
    #[tokio::test]
    async fn claim_carries_a_stable_client_key() {
        let hub = FakeHub::start(vec![BROKEN_PAYLOAD, PAYLOAD]).await;
        let dir = tempfile::tempdir().unwrap();

        let first = apply(&hub, dir.path()).await;
        assert!(first.payload.is_none(), "битое тело не должно раскладываться в payload");
        assert!(
            first.errors.iter().any(|e| e.starts_with("claim parse")),
            "провал разбора не назван в errors: {:?}",
            first.errors
        );

        let second = apply(&hub, dir.path()).await;
        let payload = second.payload.expect("повтор должен принести payload");
        assert_eq!(payload.server_address, "203.0.113.10");

        let claims = hub.claims();
        assert_eq!(claims.len(), 2, "claim'ов должно быть ровно два: {claims:?}");
        let key = claims[0]
            .claim_id
            .clone()
            .expect("claim ушёл без ключа, хаб не узнает клиента при повторе");
        assert!(!key.is_empty(), "ключ пустой");
        assert_eq!(
            claims[1].claim_id.as_deref(),
            Some(key.as_str()),
            "ключ не пережил вторую попытку, повтор упрётся в 410"
        );
    }

    // XR-216, провал проверки на проде: приложение спрашивает сведения об
    // инвайте до применения и по ним решает, показывать ли кнопку. Запрос
    // сведений обязан нести тот же ключ, что и claim, иначе хаб не признает
    // потреблённый инвайт нашим и до повтора дело не дойдёт.
    #[tokio::test]
    async fn invite_info_request_carries_the_same_key_as_claim() {
        let hub = FakeHub::start(vec![PAYLOAD]).await;
        let dir = tempfile::tempdir().unwrap();

        let info = fetch_invite_info(&hub.url, TOKEN, dir.path(), Duration::from_secs(5))
            .await
            .expect("сведения об инвайте не разобрались");
        assert_eq!(info.status, "consumed");
        assert!(info.reclaimable, "признак повторного применения не доехал до клиента");

        apply(&hub, dir.path()).await;

        let info_key = hub.infos()[0]
            .claim_id
            .clone()
            .expect("запрос сведений ушёл без ключа, хаб не отличит владельца от чужого");
        assert!(!info_key.is_empty(), "ключ пустой");
        assert_eq!(
            hub.claims()[0].claim_id.as_deref(),
            Some(info_key.as_str()),
            "сведения и claim ушли с разными ключами"
        );
    }

    // Ключ у каждой установки свой, иначе чужой claim того же инвайта проходил
    // бы по общему значению.
    #[tokio::test]
    async fn client_key_is_unique_per_install() {
        let hub = FakeHub::start(vec![PAYLOAD, PAYLOAD]).await;
        let one = tempfile::tempdir().unwrap();
        let two = tempfile::tempdir().unwrap();

        apply(&hub, one.path()).await;
        apply(&hub, two.path()).await;

        let claims = hub.claims();
        assert_ne!(claims[0].claim_id, claims[1].claim_id, "ключ одинаковый у разных установок");
        assert!(one.path().join(CLAIM_ID_FILE).exists(), "ключ не сохранён рядом с кэшем");
    }

    // Штатный проход онбординга: payload, TOFU-ключ и пресет в кэше, ошибок нет.
    #[tokio::test]
    async fn apply_invite_brings_payload_key_and_preset() {
        let hub = FakeHub::start(vec![PAYLOAD]).await;
        let dir = tempfile::tempdir().unwrap();

        let result = apply(&hub, dir.path()).await;

        assert_eq!(result.payload.expect("нет payload").server_port, 8443);
        assert_eq!(result.public_key.as_deref(), Some(PUBLIC_KEY));
        assert!(result.preset_cached, "пресет не лёг в кэш: {:?}", result.errors);
        assert!(result.errors.is_empty(), "лишние ошибки: {:?}", result.errors);
        assert!(dir.path().join("russia.json").exists(), "файла пресета нет в кэше");
    }
}
