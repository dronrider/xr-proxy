//! XR-207: пресет хаба применяется только после проверки подписи ключом,
//! названным в конфиге или профиле. Отдельным интеграционным файлом, а не
//! модулем внутри presets.rs, тесты сидят ради regcheck: файл переносится
//! на базовую версию целиком, и там они обязаны покраснеть.

use std::time::Duration;

use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use tracing_subscriber::prelude::*;
use xr_core::presets::{cached_preset_json, read_cached, PresetCache, RefreshOutcome};
use xr_proto::preset::{self, Preset};

/// Мок-хаб на несколько запросов подряд: `respond` выбирает ответ по пути
/// запроса, а `None` оставляет соединение висеть, изображая удержание на
/// ручке ожидания.
fn serve_hub<F>(respond: F) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>)
where
    F: Fn(&str) -> Option<String> + Send + Sync + 'static,
{
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let respond = std::sync::Arc::new(respond);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let respond = respond.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let Ok(n) = sock.read(&mut buf).await else { return };
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                let _ = tx.send(path.clone());
                match respond(&path) {
                    Some(resp) => {
                        let _ = sock.write_all(resp.as_bytes()).await;
                    }
                    None => {
                        // Сокет держим живым, иначе клиент увидит обрыв
                        // вместо честного ожидания.
                        std::future::pending::<()>().await;
                    }
                }
            });
        }
    });
    (format!("http://{}", addr), rx)
}

fn json_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

const PRESET_V3: &str = r#"{"name":"russia","version":3,"updated_at":"2026-08-03T00:00:00Z","description":"","rules":{"default_action":"direct","rules":[{"action":"proxy","domains":["example.com"]}]}}"#;

// Сиды фиксированные, подписи в тестах детерминированы.
fn trusted_key_b64(seed: u8) -> String {
    let key = SigningKey::from_bytes(&[seed; 32]);
    base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes())
}

/// Подпись PRESET_V3 ключом с сидом `seed` в base64.
fn sign_preset_v3(seed: u8) -> String {
    let preset: Preset = serde_json::from_str(PRESET_V3).unwrap();
    let key = SigningKey::from_bytes(&[seed; 32]);
    let signature = key.sign(&preset::canonical_json(&preset));
    base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
}

fn preset_v3_json(signature: Option<String>) -> String {
    let mut preset: Preset = serde_json::from_str(PRESET_V3).unwrap();
    preset.signature = signature;
    serde_json::to_string(&preset).unwrap()
}

const SUMMARIES_V3: &str =
    r#"[{"name":"russia","version":3,"updated_at":"2026-08-03T00:00:00Z","rules_count":1}]"#;

/// XR-207, регрессия: пресет с чужой подписью не применяется. Раньше клиент
/// доверял одному TLS до хаба, и подменённый хаб раздавал default_action=direct
/// под видом рабочего пресета.
#[tokio::test]
async fn refresh_rejects_preset_with_foreign_signature() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _requests) = serve_hub(|path| {
        if path == "/api/v1/presets" {
            Some(json_response(SUMMARIES_V3))
        } else {
            // Подпись от ключа с сидом 8, проверка идёт по ключу с сидом 7.
            Some(json_response(&preset_v3_json(Some(sign_preset_v3(8)))))
        }
    });

    let mut cache = PresetCache::new(dir.path(), &url, "russia", Some(&trusted_key_b64(7)));
    let err = cache.refresh(Duration::from_secs(5)).await.unwrap_err();
    assert!(err.starts_with("signature"), "неожиданная ошибка: {err}");
    assert!(
        cache.routing_config().is_none(),
        "отвергнутый пресет не должен попадать в кэш"
    );
    assert!(
        !dir.path().join("russia.json").exists(),
        "отвергнутый пресет не должен писаться на диск"
    );
}

/// Хаб без секции [signing] раздаёт пресет без подписи: клиент с ключом
/// обязан такой пресет отбраковать.
#[tokio::test]
async fn refresh_rejects_unsigned_preset() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _requests) = serve_hub(|path| {
        if path == "/api/v1/presets" {
            Some(json_response(SUMMARIES_V3))
        } else {
            Some(json_response(&preset_v3_json(None)))
        }
    });

    let mut cache = PresetCache::new(dir.path(), &url, "russia", Some(&trusted_key_b64(7)));
    let err = cache.refresh(Duration::from_secs(5)).await.unwrap_err();
    assert!(err.starts_with("signature"), "неожиданная ошибка: {err}");
    assert!(cache.routing_config().is_none());
}

/// Честная подпись доверенным ключом проходит: проверка не глушит штатную
/// доставку пресета.
#[tokio::test]
async fn refresh_applies_preset_signed_by_trusted_key() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _requests) = serve_hub(|path| {
        if path == "/api/v1/presets" {
            Some(json_response(SUMMARIES_V3))
        } else {
            Some(json_response(&preset_v3_json(Some(sign_preset_v3(7)))))
        }
    });

    let mut cache = PresetCache::new(dir.path(), &url, "russia", Some(&trusted_key_b64(7)));
    assert_eq!(
        cache.refresh(Duration::from_secs(5)).await.unwrap(),
        RefreshOutcome::Updated(3)
    );
    assert!(cache.routing_config().is_some());
    assert!(dir.path().join("russia.json").exists());
}

/// То же самое на длинном опросе: подмена, приехавшая ручкой ожидания,
/// отбраковывается так же, как обычный фетч.
#[tokio::test]
async fn wait_for_update_rejects_invalid_signature() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _requests) = serve_hub(|_| {
        Some(json_response(&preset_v3_json(Some(sign_preset_v3(8)))))
    });

    let mut cache = PresetCache::new(dir.path(), &url, "russia", Some(&trusted_key_b64(7)));
    let err = cache
        .wait_for_update(Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(err.starts_with("signature"), "неожиданная ошибка: {err}");
    assert!(cache.routing_config().is_none());
    assert!(!dir.path().join("russia.json").exists());
}

/// Кэш на диске проходит ту же проверку: пресет, лежавший до включения ключа
/// или подменённый на роутере, при старте не применяется.
#[test]
fn load_from_disk_rejects_unverified_preset() {
    let dir = tempfile::tempdir().unwrap();
    let unsigned = preset_v3_json(None);
    let unsigned: Preset = serde_json::from_str(&unsigned).unwrap();
    PresetCache::write_to_disk(dir.path(), &unsigned).unwrap();

    let mut cache = PresetCache::new(dir.path(), "", "russia", Some(&trusted_key_b64(7)));
    assert!(cache.load_from_disk().is_none());
    assert!(cache.routing_config().is_none());

    let mut signed = unsigned.clone();
    signed.signature = Some(sign_preset_v3(7));
    PresetCache::write_to_disk(dir.path(), &signed).unwrap();
    let mut cache = PresetCache::new(dir.path(), "", "russia", Some(&trusted_key_b64(7)));
    assert!(cache.load_from_disk().is_some());
}

/// Неразборная строка ключа это тоже «ключ задан»: пресет отбраковывается,
/// опечатка в конфиге не выключает проверку молча.
#[tokio::test]
async fn refresh_with_unparsable_key_rejects_the_preset() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _requests) = serve_hub(|path| {
        if path == "/api/v1/presets" {
            Some(json_response(SUMMARIES_V3))
        } else {
            Some(json_response(PRESET_V3))
        }
    });

    let mut cache = PresetCache::new(dir.path(), &url, "russia", Some("не base64"));
    let err = cache.refresh(Duration::from_secs(5)).await.unwrap_err();
    assert!(err.starts_with("trusted key"), "неожиданная ошибка: {err}");
    assert!(cache.routing_config().is_none());
}

/// Карточка и превью читают кэш через ту же проверку: показывать отвергнутый
/// движком пресет нельзя. Без ключа прежнее поведение.
#[test]
fn read_cached_rejects_unverified_preset() {
    let dir = tempfile::tempdir().unwrap();
    let preset: Preset = serde_json::from_str(&preset_v3_json(None)).unwrap();
    PresetCache::write_to_disk(dir.path(), &preset).unwrap();

    assert!(read_cached(dir.path(), "russia", Some(&trusted_key_b64(7))).is_none());
    assert!(cached_preset_json(dir.path(), "russia", Some(&trusted_key_b64(7))).is_none());
    assert!(read_cached(dir.path(), "russia", None).is_some());
    // Пустая строка ключа значит «ключа нет» и для прямого читателя кэша.
    assert!(read_cached(dir.path(), "russia", Some("  ")).is_some());
}

/// Пустая строка ключа значит «ключа нет»: раскомментированная, но не
/// заполненная строка конфига не включает проверку, неподписанный пресет
/// применяется как раньше.
#[tokio::test]
async fn blank_key_keeps_old_behavior() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _requests) = serve_hub(|path| {
        if path == "/api/v1/presets" {
            Some(json_response(SUMMARIES_V3))
        } else {
            Some(json_response(&preset_v3_json(None)))
        }
    });

    let mut cache = PresetCache::new(dir.path(), &url, "russia", Some("  "));
    assert_eq!(
        cache.refresh(Duration::from_secs(5)).await.unwrap(),
        RefreshOutcome::Updated(3)
    );
    assert!(cache.routing_config().is_some());
}

/// Слой, собирающий текст событий трейсинга: предупреждение это видимый
/// снаружи исход, и его приходится проверять так же, как возвращаемое
/// значение.
struct Capture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

struct MsgVisitor(String);

impl tracing::field::Visit for MsgVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut visitor = MsgVisitor(String::new());
        event.record(&mut visitor);
        self.0.lock().unwrap().push(visitor.0);
    }
}

/// Без ключа проверки на старте уходит предупреждение: молчаливая работа без
/// проверки не должна выглядеть как включённая проверка. С ключом тихо.
#[test]
fn missing_key_warns_on_start() {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(Capture(seen.clone())),
    );

    xr_core::presets::warn_if_unverified(None);
    assert!(
        seen.lock().unwrap().iter().any(|m| m.contains("trusted_public_key")),
        "ожидалось предупреждение о работе без проверки, событий: {:?}",
        seen.lock().unwrap()
    );

    seen.lock().unwrap().clear();
    xr_core::presets::warn_if_unverified(Some(&trusted_key_b64(7)));
    assert!(
        seen.lock().unwrap().is_empty(),
        "с ключом предупреждения быть не должно"
    );
}
