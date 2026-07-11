//! Relay configuration (LLD-23 §2.4, §3.2).
//!
//! A standalone `[relay]` block: where to listen, the obfuscation params (shared
//! with the deployment so the relay's mux looks like the proxy's), the hub's
//! public key (to verify agent credentials and relay tokens offline), and the
//! transit limits (§5.2).

use std::path::Path;

use serde::Deserialize;
use xr_proto::share::RelayObf;

fn default_listen() -> String {
    "0.0.0.0".to_string()
}
fn default_max_connections() -> usize {
    512
}
fn default_max_streams() -> usize {
    4096
}
fn default_max_reg_per_ip() -> usize {
    8
}
fn default_splice_lifetime_secs() -> u64 {
    3600
}
fn default_counter_log_secs() -> u64 {
    300
}
fn default_log_level() -> String {
    "info".to_string()
}

#[derive(Debug, Deserialize)]
pub struct RelayConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    pub port: u16,
    /// Base64 (standard) ed25519 hub public key. The relay verifies credentials
    /// and tokens against it offline and never holds the hub's private key
    /// (LLD-23 §5.8: a compromised relay can't mint access).
    pub hub_pubkey: String,
    pub obfuscation: RelayObf,
    /// Concurrent TCP connections (agents + consumers). Backpressure, not a hard
    /// reject: over the cap, accept waits for a slot.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    /// Concurrent consumer transit streams across all connections.
    #[serde(default = "default_max_streams")]
    pub max_streams: usize,
    /// Live registrations allowed from one source IP (anti-abuse, §5.2).
    #[serde(default = "default_max_reg_per_ip")]
    pub max_registrations_per_ip: usize,
    /// Hard cap on one spliced transfer before the relay tears it down.
    #[serde(default = "default_splice_lifetime_secs")]
    pub splice_lifetime_secs: u64,
    /// How often the per-share byte totals are logged (§2.6).
    #[serde(default = "default_counter_log_secs")]
    pub counter_log_secs: u64,
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

#[derive(Debug, Deserialize)]
struct RelayFile {
    relay: RelayConfig,
}

impl RelayConfig {
    /// Load and parse the `[relay]` block from a TOML file.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
        let file: RelayFile = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
        Ok(file.relay)
    }

    /// Build the mux obfuscation codec from the shared params.
    pub fn codec(&self) -> anyhow::Result<xr_proto::protocol::Codec> {
        self.obfuscation
            .codec()
            .map_err(|e| anyhow::anyhow!("relay obfuscation: {e}"))
    }

    /// Parse the pinned hub verifying key.
    pub fn hub_key(&self) -> anyhow::Result<ed25519_dalek::VerifyingKey> {
        xr_proto::share::parse_agent_pubkey(&self.hub_pubkey)
            .map_err(|e| anyhow::anyhow!("hub_pubkey: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn parses_full_config_and_defaults() {
        let key = base64::engine::general_purpose::STANDARD.encode(b"relay-obf-key-32-bytes-long!!!!!");
        let hub = base64::engine::general_purpose::STANDARD
            .encode(ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]).verifying_key().as_bytes());
        let toml_text = format!(
            r#"
            [relay]
            port = 8444
            hub_pubkey = "{hub}"
            [relay.obfuscation]
            key = "{key}"
            salt = 12345
            modifier = "positional_xor_rotate"
            padding_min = 16
            padding_max = 128
            "#
        );
        let cfg: RelayFile = toml::from_str(&toml_text).unwrap();
        let cfg = cfg.relay;
        assert_eq!(cfg.port, 8444);
        assert_eq!(cfg.listen, "0.0.0.0"); // default
        assert_eq!(cfg.max_connections, 512); // default
        assert_eq!(cfg.max_registrations_per_ip, 8); // default
        assert!(cfg.codec().is_ok());
        assert!(cfg.hub_key().is_ok());
    }
}
