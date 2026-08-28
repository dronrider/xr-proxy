use anyhow::{Context, Result};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use xr_proto::preset::{canonical_json, Preset};

pub struct SigningContext {
    pub signing_key: SigningKey,
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

// Канонизация и проверка подписи живут в xr-proto рядом с Preset (XR-207):
// хаб подписывает, клиенты сверяют, реализация одна на обе стороны.

#[cfg(test)]
mod tests {
    use super::*;
    use xr_proto::config::{RoutingConfig, RoutingRule};
    use xr_proto::preset::verify_preset;

    fn test_preset() -> Preset {
        Preset {
            name: "russia".into(),
            version: 1,
            updated_at: "2026-01-01T00:00:00Z".into(),
            description: "Test preset".into(),
            rules: RoutingConfig {
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

    #[test]
    fn sign_and_verify_roundtrip() {
        let key = SigningKey::generate(&mut rand::thread_rng());
        let ctx = SigningContext { signing_key: key };
        let mut preset = test_preset();
        let sig = ctx.sign_preset(&preset);
        preset.signature = Some(sig);
        assert_eq!(verify_preset(&preset, &ctx.verifying_key()), Ok(true));
    }

    /// Переименование группы это правка пресета, и старая подпись под ней
    /// не должна проходить.
    #[test]
    fn verify_fails_on_renamed_group() {
        let key = SigningKey::generate(&mut rand::thread_rng());
        let ctx = SigningContext { signing_key: key };
        let mut preset = test_preset();
        preset.rules.rules[0].name = Some("YouTube".into());
        preset.signature = Some(ctx.sign_preset(&preset));
        assert_eq!(verify_preset(&preset, &ctx.verifying_key()), Ok(true));

        preset.rules.rules[0].name = Some("Видео".into());
        assert_eq!(verify_preset(&preset, &ctx.verifying_key()), Ok(false));
    }

    #[test]
    fn verify_fails_on_tampered_data() {
        let key = SigningKey::generate(&mut rand::thread_rng());
        let ctx = SigningContext { signing_key: key };
        let mut preset = test_preset();
        preset.signature = Some(ctx.sign_preset(&preset));
        preset.version = 999;
        assert_eq!(verify_preset(&preset, &ctx.verifying_key()), Ok(false));
    }
}
