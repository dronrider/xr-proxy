/// Shared data types for xr-hub control-plane: presets and invites.
use serde::{Deserialize, Serialize};

use crate::config::RoutingConfig;

/// Full preset with routing rules, versioning, and optional signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub version: u64,
    pub updated_at: String,
    #[serde(default)]
    pub description: String,
    pub rules: RoutingConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// Lightweight summary for listing presets (version check without full rules).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetSummary {
    pub name: String,
    pub version: u64,
    pub updated_at: String,
    pub rules_count: usize,
}

impl Preset {
    pub fn summary(&self) -> PresetSummary {
        PresetSummary {
            name: self.name.clone(),
            version: self.version,
            updated_at: self.updated_at.clone(),
            rules_count: self.rules.rules.len(),
        }
    }
}

// Подпись пресета (XR-207).
// Канонизация и проверка живут рядом с Preset, потому что их двое: хаб
// подписывает, клиент сверяет, и разъезд двух реализаций испортил бы
// подпись молча. Крипта едет фичей `share` (см. Cargo.toml).

#[cfg(any(feature = "share", test))]
use base64::Engine as _;
#[cfg(any(feature = "share", test))]
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

/// Canonical form of a preset for signing (without the signature field).
#[cfg(any(feature = "share", test))]
#[derive(Serialize)]
struct CanonicalPreset<'a> {
    description: &'a str,
    name: &'a str,
    rules: &'a RoutingConfig,
    updated_at: &'a str,
    version: u64,
}

/// Deterministic JSON for signing: fields in alphabetical order, no signature.
#[cfg(any(feature = "share", test))]
pub fn canonical_json(preset: &Preset) -> Vec<u8> {
    let canonical = CanonicalPreset {
        description: &preset.description,
        name: &preset.name,
        rules: &preset.rules,
        updated_at: &preset.updated_at,
        version: preset.version,
    };
    // serde_json serializes struct fields in declaration order.
    // CanonicalPreset fields are declared alphabetically.
    serde_json::to_vec(&canonical).expect("canonical JSON serialization cannot fail")
}

/// Проверить подпись пресета публичным ключом. `Ok(false)` значит «подписи
/// нет или она не сошлась», `Err` значит, что подпись не разобрать как
/// base64 из 64 байт.
#[cfg(any(feature = "share", test))]
pub fn verify_preset(preset: &Preset, verifying_key: &VerifyingKey) -> Result<bool, String> {
    let Some(sig_str) = preset.signature.as_deref() else {
        return Ok(false);
    };
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(sig_str)
        .map_err(|e| format!("signature base64: {e}"))?;
    let sig_arr: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|v: Vec<u8>| format!("signature must be 64 bytes, got {}", v.len()))?;
    let signature = Signature::from_bytes(&sig_arr);
    let bytes = canonical_json(preset);
    Ok(verifying_key.verify(&bytes, &signature).is_ok())
}

/// Публичный ключ из base64 (32 байта): в таком виде его отдаёт хаб
/// (`/api/v1/public-key`) и хранят конфиг клиента и профиль приложения.
#[cfg(any(feature = "share", test))]
pub fn decode_verifying_key(key_b64: &str) -> Result<VerifyingKey, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(key_b64.trim())
        .map_err(|e| format!("pubkey base64: {e}"))?;
    let key_arr: [u8; 32] = bytes
        .try_into()
        .map_err(|v: Vec<u8>| format!("pubkey must be 32 bytes, got {}", v.len()))?;
    VerifyingKey::from_bytes(&key_arr).map_err(|e| format!("invalid pubkey: {e}"))
}

/// One-time (or reusable) invite for client onboarding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invite {
    pub token: String,
    pub created_at: String,
    pub expires_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by_ip: Option<String>,
    /// Ключ клиента, забравшего инвайт (XR-216). Одноразовый инвайт потребляется
    /// на сервере в момент выдачи payload'а, поэтому любой сбой уже после этого
    /// (тело не разобралось, оборвалась сеть на чтении) оставлял бы получателя
    /// ни с чем. С ключом повтор того же клиента отдаёт payload снова, а чужой
    /// повтор по-прежнему упирается в 410. `default` держит старые файлы инвайтов
    /// читаемыми, у них ключа нет и повтор невозможен.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    pub one_time: bool,
    #[serde(default)]
    pub comment: String,
    pub payload: InvitePayload,
    /// share_ids attached to this invite (LLD-19 §9.5, XR-031). The invite is a
    /// durable access anchor: whoever holds it reaches every share listed here.
    /// `default` so invites stored before this field still load.
    #[serde(default)]
    pub share_ids: Vec<String>,
    /// Subset of `share_ids` this invite may write to (LLD-28). The hub mints a
    /// `share:write` scope only for a share listed here (and marked `writable` on
    /// the record); a read binding keeps the share in `share_ids` but not here.
    /// The invariant "subset of `share_ids`" is held by the hub's attach/detach.
    /// `default` keeps invites stored before this field loadable (empty = no write).
    #[serde(default)]
    pub write_share_ids: Vec<String>,
}

/// Connection details delivered to a client via invite.
///
/// `server_address`/`server_port` это legacy-поля с primary-сервером: старое
/// приложение читает только их, новое при пустом `servers` строит пул из
/// одного легаси-адреса. Ломающих комбинаций version skew нет (LLD-10 §2.8).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvitePayload {
    pub server_address: String,
    pub server_port: u16,
    pub obfuscation_key: String,
    pub modifier: String,
    pub salt: u64,
    pub preset: String,
    pub hub_url: String,
    /// Пул серверов профиля (LLD-10). Ключ/salt/modifier общие на профиль
    /// и приходят в полях выше, per-server ключей в инвайте нет by design.
    #[serde(default)]
    pub servers: Vec<PayloadServer>,
}

/// Один сервер в составе invite-payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayloadServer {
    #[serde(default)]
    pub name: String,
    pub address: String,
    pub port: u16,
    /// Меньше = выше приоритет; 0 = primary.
    #[serde(default)]
    pub priority: u32,
}

/// Public invite metadata (no secrets). Returned by GET /invite/:token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteInfo {
    pub token: String,
    pub preset: String,
    pub comment: String,
    pub status: String,
    pub expires_at: String,
    /// Инвайт потреблён, но потребил его тот, кто спрашивает: claim с тем же
    /// ключом снова отдаст payload (XR-216). Статус при этом честный,
    /// `consumed`, врать «активен» посторонним нельзя. `default` оставляет поле
    /// пустым для хабов и клиентов, которые про ключ не знают.
    #[serde(default)]
    pub reclaimable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Version skew хаба и приложения (LLD-10 §5.8): payload без `servers`
    /// (старый хаб) парсится в пустой список, payload со списком отдаёт его
    /// целиком, а legacy-поля в обоих случаях несут primary.
    #[test]
    fn test_payload_legacy_roundtrip() {
        // Старый payload, как его сериализует хаб до LLD-10.
        let legacy = r#"{
            "server_address": "1.2.3.4",
            "server_port": 8443,
            "obfuscation_key": "a2V5",
            "modifier": "positional_xor_rotate",
            "salt": 7,
            "preset": "russia",
            "hub_url": "https://hub.example"
        }"#;
        let p: InvitePayload = serde_json::from_str(legacy).unwrap();
        assert!(p.servers.is_empty(), "legacy payload -> empty pool list");
        assert_eq!(p.server_address, "1.2.3.4");

        // Новый payload: список + зеркальные legacy-поля с primary.
        let full = InvitePayload {
            servers: vec![
                PayloadServer {
                    name: "aeza".into(),
                    address: "1.2.3.4".into(),
                    port: 8443,
                    priority: 0,
                },
                PayloadServer {
                    name: "timeweb".into(),
                    address: "5.6.7.8".into(),
                    port: 8443,
                    priority: 1,
                },
            ],
            ..p
        };
        let json = serde_json::to_string(&full).unwrap();
        let back: InvitePayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back.servers.len(), 2);
        assert_eq!(back.servers[0].name, "aeza");
        assert_eq!(back.server_address, "1.2.3.4", "legacy field keeps primary");
    }

    /// XR-216: ключ клиента добавлен к уже лежащим на диске инвайтам, и хаб
    /// читает их файлы при старте. Инвайт без поля должен загружаться, иначе
    /// апгрейд хаба потеряет весь выданный парк.
    #[test]
    fn test_stored_invite_without_claim_id_loads() {
        let stored = r#"{
            "token": "abc",
            "created_at": "2026-01-01T00:00:00+00:00",
            "expires_at": "2099-01-01T00:00:00+00:00",
            "one_time": true,
            "payload": {
                "server_address": "1.2.3.4",
                "server_port": 8443,
                "obfuscation_key": "a2V5",
                "modifier": "positional_xor_rotate",
                "salt": 7,
                "preset": "russia",
                "hub_url": "https://hub.example"
            }
        }"#;
        let invite: Invite = serde_json::from_str(stored).expect("старый инвайт не читается");
        assert!(invite.claim_id.is_none());

        // Пустой ключ в файле не появляется: повтор сверяется по равенству, и
        // пустая строка в обеих частях сравнения открыла бы инвайт всем подряд.
        let json = serde_json::to_string(&invite).unwrap();
        assert!(!json.contains("claim_id"), "ключ без значения не сериализуем: {json}");
    }

    // Подпись пресета (XR-207): каноничная форма одинакова у подписывающего
    // хаба и проверяющего клиента, поэтому тесты на неё живут рядом.
    use crate::config::RoutingRule;
    use ed25519_dalek::{Signer, SigningKey};

    fn plain_preset() -> Preset {
        Preset {
            name: "russia".into(),
            version: 1,
            updated_at: "2026-01-01T00:00:00Z".into(),
            description: "Test preset".into(),
            rules: crate::config::RoutingConfig {
                default_action: "direct".into(),
                rules: vec![RoutingRule {
                    name: None,
                    action: "proxy".into(),
                    domains: vec!["youtube.com".into()],
                    ip_ranges: vec![],
                    geoip: vec![],
                }],
            },
            signature: None,
        }
    }

    fn sign_with(seed: u8, preset: &Preset) -> String {
        let key = SigningKey::from_bytes(&[seed; 32]);
        let signature = key.sign(&canonical_json(preset));
        base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
    }

    #[test]
    fn canonical_json_is_deterministic() {
        let a = canonical_json(&plain_preset());
        let b = canonical_json(&plain_preset());
        assert_eq!(a, b);
    }

    /// XR-117: имя группы у правила это часть подписываемых данных, а его
    /// отсутствие ничего к ним не добавляет. Подписи пресетов, выданные до
    /// появления поля, поэтому остаются действительными.
    #[test]
    fn rule_name_enters_canonical_json_only_when_set() {
        let preset = plain_preset();
        assert_eq!(
            String::from_utf8(canonical_json(&preset)).unwrap(),
            r#"{"description":"Test preset","name":"russia","rules":{"default_action":"direct","rules":[{"action":"proxy","domains":["youtube.com"],"ip_ranges":[],"geoip":[]}]},"updated_at":"2026-01-01T00:00:00Z","version":1}"#
        );

        let mut named = plain_preset();
        named.rules.rules[0].name = Some("YouTube".into());
        assert_ne!(canonical_json(&preset), canonical_json(&named));
    }

    #[test]
    fn verify_preset_accepts_own_signature() {
        let mut preset = plain_preset();
        preset.signature = Some(sign_with(7, &preset));
        let key = decode_verifying_key(
            &base64::engine::general_purpose::STANDARD.encode(SigningKey::from_bytes(&[7; 32]).verifying_key().as_bytes()),
        )
        .unwrap();
        assert_eq!(verify_preset(&preset, &key), Ok(true));
    }

    /// Подпись чужим ключом и подпись от изменённого пресета обязаны
    /// расходиться с ключом: первый случай это хаб без своего ключа, второй
    /// подмена правил поверх честной подписи.
    #[test]
    fn verify_preset_rejects_wrong_key_and_tampered_rules() {
        let mut preset = plain_preset();
        preset.signature = Some(sign_with(7, &preset));
        let other = decode_verifying_key(
            &base64::engine::general_purpose::STANDARD.encode(SigningKey::from_bytes(&[8; 32]).verifying_key().as_bytes()),
        )
        .unwrap();
        assert_eq!(verify_preset(&preset, &other), Ok(false));

        preset.rules.default_action = "proxy".into();
        let own = decode_verifying_key(
            &base64::engine::general_purpose::STANDARD.encode(SigningKey::from_bytes(&[7; 32]).verifying_key().as_bytes()),
        )
        .unwrap();
        assert_eq!(verify_preset(&preset, &own), Ok(false));
    }

    #[test]
    fn verify_preset_rejects_missing_and_malformed_signature() {
        let key = decode_verifying_key(
            &base64::engine::general_purpose::STANDARD.encode(SigningKey::from_bytes(&[7; 32]).verifying_key().as_bytes()),
        )
        .unwrap();
        // Пресет хаба без секции [signing]: поля подписи нет вовсе.
        assert_eq!(verify_preset(&plain_preset(), &key), Ok(false));

        let mut broken = plain_preset();
        broken.signature = Some("не base64".into());
        assert!(verify_preset(&broken, &key).is_err());

        let mut short = plain_preset();
        short.signature = Some(base64::engine::general_purpose::STANDARD.encode([1u8; 8]));
        assert!(verify_preset(&short, &key).is_err());
    }

    #[test]
    fn decode_verifying_key_checks_shape() {
        let b64 = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        assert!(decode_verifying_key(&b64).is_ok());
        assert!(decode_verifying_key(&format!(" {b64} ")).is_ok(), "пробелы по краям не должны мешать");
        assert!(decode_verifying_key("не base64").is_err());
        let short = base64::engine::general_purpose::STANDARD.encode([1u8; 31]);
        assert!(decode_verifying_key(&short).is_err());
    }
}
