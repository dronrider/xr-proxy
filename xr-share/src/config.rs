//! Agent configuration (LLD-19 §2.7).
//!
//! The agent serves exactly one share. It needs the directory to expose, the
//! `share_id` that token must be bound to, and the **hub's** public key — pinned
//! into the config — so it can verify access tokens offline without ever talking
//! to the hub. TLS is optional (provided PEM); the consumer pins the agent's own
//! identity (TOFU), so a self-signed cert is acceptable.

use anyhow::{Context, Result};
use base64::Engine;
use ed25519_dalek::VerifyingKey;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct AgentConfig {
    /// Listen address, e.g. `0.0.0.0:8443`.
    pub listen: String,
    /// Directory served read-only.
    pub dir: String,
    /// The share this agent serves; a token must be bound to this id.
    pub share_id: String,
    /// Base64 (standard) ed25519 public key of the hub — pinned. Tokens are
    /// verified against this offline.
    pub hub_pubkey: String,
    /// Optional TLS (provided cert + key PEM). Without it the agent serves
    /// plain HTTP (dev / behind a TLS terminator).
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

/// Read only by the `tls` feature; kept parseable in HTTP-only builds so a
/// `[tls]` block produces a clear error rather than an unknown-field failure.
#[cfg_attr(not(feature = "tls"), allow(dead_code))]
#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    pub cert: String,
    pub key: String,
}

impl AgentConfig {
    /// Decode and validate the pinned hub public key.
    pub fn hub_verifying_key(&self) -> Result<VerifyingKey> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(self.hub_pubkey.trim())
            .context("decoding hub_pubkey base64")?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|v: Vec<u8>| anyhow::anyhow!("hub_pubkey must be 32 bytes, got {}", v.len()))?;
        VerifyingKey::from_bytes(&arr).context("invalid hub_pubkey")
    }
}
