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
    pub one_time: bool,
    #[serde(default)]
    pub comment: String,
    pub payload: InvitePayload,
    /// share_ids attached to this invite (LLD-19 §9.5, XR-031). The invite is a
    /// durable access anchor: whoever holds it reaches every share listed here.
    /// `default` so invites stored before this field still load.
    #[serde(default)]
    pub share_ids: Vec<String>,
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
}
