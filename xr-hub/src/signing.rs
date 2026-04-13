use anyhow::{Context, Result};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::Serialize;
use xr_proto::config::RoutingConfig;
use xr_proto::preset::Preset;

pub struct SigningContext {
    pub signing_key: SigningKey,
}

/// Canonical form of a preset for signing (without the signature field).
#[derive(Serialize)]
struct CanonicalPreset<'a> {
    description: &'a str,
    name: &'a str,
    rules: &'a RoutingConfig,
    updated_at: &'a str,
    version: u64,
}

impl SigningContext {
    /// Load signing key from a file (32 raw bytes or 44-char base64).
    pub fn from_file(path: &str) -> Result<Self> {
        let data = std::fs::read_to_string(path)
            .with_context(|| format!("reading signing key from {path}"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .context("decoding signing key base64")?;
        let key_bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|v: Vec<u8>| anyhow::anyhow!("signing key must be 32 bytes, got {}", v.len()))?;
        Ok(Self {
            signing_key: SigningKey::from_bytes(&key_bytes),
        })
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    /// Sign a preset and return base64-encoded signature.
    pub fn sign_preset(&self, preset: &Preset) -> String {
        let bytes = canonical_json(preset);
        let signature = self.signing_key.sign(&bytes);
        base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
    }
}

/// Verify a preset's signature against a public key.
#[allow(dead_code)]
pub fn verify_preset(preset: &Preset, verifying_key: &VerifyingKey) -> Result<bool> {
    let sig_str = match &preset.signature {
        Some(s) => s,
        None => return Ok(false),
    };
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(sig_str)
        .context("decoding signature base64")?;
    let signature = ed25519_dalek::Signature::from_bytes(
        &sig_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("signature must be 64 bytes"))?,
    );
    let bytes = canonical_json(preset);
    Ok(verifying_key.verify(&bytes, &signature).is_ok())
}

/// Deterministic JSON for signing: fields in alphabetical order, no signature.
fn canonical_json(preset: &Preset) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use xr_proto::config::{RoutingConfig, RoutingRule};

    fn test_preset() -> Preset {
        Preset {
            name: "russia".into(),
            version: 1,
            updated_at: "2026-01-01T00:00:00Z".into(),
            description: "Test preset".into(),
            rules: RoutingConfig {
                default_action: "direct".into(),
                rules: vec![RoutingRule {
                    action: "proxy".into(),
                    domains: vec!["youtube.com".into()],
                    ip_ranges: vec![],
                    geoip: vec![],
                }],
            },
            signature: None,
        }
    }

    #[test]
    fn canonical_json_is_deterministic() {
        let p = test_preset();
        let a = canonical_json(&p);
        let b = canonical_json(&p);
        assert_eq!(a, b);
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let key = SigningKey::generate(&mut rand::thread_rng());
        let ctx = SigningContext { signing_key: key };
        let mut preset = test_preset();
        let sig = ctx.sign_preset(&preset);
        preset.signature = Some(sig);
        assert!(verify_preset(&preset, &ctx.verifying_key()).unwrap());
    }

    #[test]
    fn verify_fails_on_tampered_data() {
        let key = SigningKey::generate(&mut rand::thread_rng());
        let ctx = SigningContext { signing_key: key };
        let mut preset = test_preset();
        preset.signature = Some(ctx.sign_preset(&preset));
        preset.version = 999;
        assert!(!verify_preset(&preset, &ctx.verifying_key()).unwrap());
    }
}
