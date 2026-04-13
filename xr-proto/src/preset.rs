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
    pub one_time: bool,
    #[serde(default)]
    pub comment: String,
    pub payload: InvitePayload,
}

/// Connection details delivered to a client via invite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvitePayload {
    pub server_address: String,
    pub server_port: u16,
    pub obfuscation_key: String,
    pub modifier: String,
    pub salt: u64,
    pub preset: String,
    pub hub_url: String,
}
