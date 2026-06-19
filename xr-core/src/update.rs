//! Android APK self-update: fetch + verify the signed release manifest, and
//! verify a downloaded APK's SHA-256 (LLD-12).
//!
//! Trust model (LLD-12 §2.2/§3.1): the manifest is signed by an offline
//! **release key** whose public half is *pinned into the app at build time*
//! (`BuildConfig.RELEASE_PUBLIC_KEY`) — never fetched, never TOFU'd. A
//! compromised VPS can swap the APK and rewrite the manifest, but cannot forge
//! the signature without the private release key (which is not on the VPS), so
//! a tampered update is rejected right here. Verification therefore lives in
//! Rust (this module, unit-tested) rather than in Kotlin; Kotlin only does the
//! download + `PackageInstaller` parts it cannot delegate.
//!
//! Reuses the reqwest-client style from [`crate::onboarding`].

use std::path::Path;
use std::time::Duration;

use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

pub use xr_proto::app_update::{AppManifest, SignedManifest};

/// GET the signed release manifest from the hub. Does **not** verify — the
/// caller MUST run [`verify_manifest`] with the pinned key before trusting any
/// field. `Err("no_release")` when the hub has no release published yet.
pub async fn fetch_manifest(hub_url: &str, timeout: Duration) -> Result<SignedManifest, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    let url = format!("{}/api/v1/app/latest", hub_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;

    match resp.status() {
        s if s.is_success() => resp
            .json::<SignedManifest>()
            .await
            .map_err(|e| format!("parse: {e}")),
        reqwest::StatusCode::NOT_FOUND => Err("no_release".into()),
        s => Err(format!("http_{}", s.as_u16())),
    }
}

/// Verify the manifest's signature with the pinned release public key
/// (base64, 32 bytes), then parse and return the manifest.
///
/// Any failure — bad base64 key, wrong key length, malformed signature,
/// signature mismatch (tampered manifest bytes or wrong signer), or
/// unparseable manifest JSON — returns `Err`, and the caller MUST NOT offer
/// the update (LLD-12 §2.4: bad signature → no update, the installed version
/// keeps running, no partial install).
pub fn verify_manifest(
    signed: &SignedManifest,
    pinned_pubkey_b64: &str,
) -> Result<AppManifest, String> {
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(pinned_pubkey_b64.trim())
        .map_err(|e| format!("pubkey base64: {e}"))?;
    let key_arr: [u8; 32] = key_bytes
        .try_into()
        .map_err(|v: Vec<u8>| format!("pubkey must be 32 bytes, got {}", v.len()))?;
    let verifying_key =
        VerifyingKey::from_bytes(&key_arr).map_err(|e| format!("invalid pubkey: {e}"))?;

    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(signed.signature.trim())
        .map_err(|e| format!("signature base64: {e}"))?;
    let sig_arr: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| "signature must be 64 bytes".to_string())?;
    let signature = Signature::from_bytes(&sig_arr);

    // The signature covers the EXACT bytes of the manifest string as served by
    // the hub — no canonicalization, so the signer/verifier can never drift.
    verifying_key
        .verify(signed.manifest.as_bytes(), &signature)
        .map_err(|_| "signature verification failed".to_string())?;

    serde_json::from_str::<AppManifest>(&signed.manifest)
        .map_err(|e| format!("manifest parse: {e}"))
}

/// True if `manifest` advertises a strictly newer build than `current_code`.
/// A signed-but-older (or equal) manifest — e.g. a replay of a previous
/// release — is not offered (LLD-12 §5.6).
pub fn manifest_offers_update(manifest: &AppManifest, current_code: u64) -> bool {
    manifest.version_code > current_code
}

/// Stream `path` and compare its SHA-256 against `expected` (hex, any case).
/// Returns `false` on any I/O error or mismatch — a truncated/corrupt download
/// (cf. bug C2) fails the check, and the caller discards the file (LLD-12
/// §2.4/§5.5).
pub fn verify_apk_sha256(path: &Path, expected_hex: &str) -> bool {
    match sha256_file(path) {
        Ok(actual) => actual.eq_ignore_ascii_case(expected_hex.trim()),
        Err(_) => false,
    }
}

/// Streaming SHA-256 of a file → lowercase hex. Reads in 64 KiB chunks so a
/// multi-megabyte APK is never held in memory at once.
fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{:02x}", b);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn make_manifest(code: u64) -> AppManifest {
        AppManifest {
            version_code: code,
            version_name: "0.2.0".into(),
            min_sdk: 29,
            apk_url: "https://hub.example/api/v1/app/download/0.2.0".into(),
            apk_sha256: "abc123".into(),
            size_bytes: 7_340_032,
            release_notes: "Multi-VPS failover".into(),
            released_at: "2026-06-20".into(),
        }
    }

    /// Build a SignedManifest exactly the way `xr-hub sign-release` does:
    /// serialize the manifest, sign those raw bytes, base64 the signature.
    fn sign(manifest: &AppManifest, key: &SigningKey) -> SignedManifest {
        let body = serde_json::to_string(manifest).unwrap();
        let sig = key.sign(body.as_bytes());
        SignedManifest {
            manifest: body,
            signature: base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()),
        }
    }

    fn pubkey_b64(key: &SigningKey) -> String {
        base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes())
    }

    #[test]
    fn test_verify_manifest_good_sig() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let signed = sign(&make_manifest(12), &key);
        let m = verify_manifest(&signed, &pubkey_b64(&key)).unwrap();
        assert_eq!(m.version_code, 12);
        assert_eq!(m.version_name, "0.2.0");
    }

    #[test]
    fn test_verify_manifest_bad_sig_rejected() {
        // §6.2: corrupt a byte inside manifest.json WITHOUT re-signing.
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let mut signed = sign(&make_manifest(12), &key);
        signed.manifest = signed
            .manifest
            .replace("\"version_code\":12", "\"version_code\":13");
        assert!(verify_manifest(&signed, &pubkey_b64(&key)).is_err());
    }

    #[test]
    fn test_verify_manifest_wrong_key_rejected() {
        // §6.3: validly signed by one key, verified against a different key.
        let signer = SigningKey::from_bytes(&[7u8; 32]);
        let attacker_pub = pubkey_b64(&SigningKey::from_bytes(&[9u8; 32]));
        let signed = sign(&make_manifest(12), &signer);
        assert!(verify_manifest(&signed, &attacker_pub).is_err());
    }

    #[test]
    fn test_verify_manifest_bad_pubkey_input_rejected() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let signed = sign(&make_manifest(12), &key);
        // An empty / malformed pinned key must be a clean reject, not a panic.
        assert!(verify_manifest(&signed, "").is_err());
        assert!(verify_manifest(&signed, "not-base64-@@@").is_err());
    }

    #[test]
    fn test_apk_sha_mismatch_rejected() {
        // §6.4: wrong/short SHA → reject; correct SHA (any case) → accept.
        let path = std::env::temp_dir().join(format!("xr_update_test_{}.apk", std::process::id()));
        std::fs::write(&path, b"hello apk bytes").unwrap();
        let good = sha256_file(&path).unwrap();

        assert!(verify_apk_sha256(&path, &good));
        assert!(verify_apk_sha256(&path, &good.to_uppercase()));
        assert!(!verify_apk_sha256(&path, "deadbeef"));
        assert!(!verify_apk_sha256(&path, ""));

        let _ = std::fs::remove_file(&path);
        // Missing file must fail closed, not panic.
        assert!(!verify_apk_sha256(&path, &good));
    }

    #[test]
    fn test_older_version_not_offered() {
        // §6.5: only a strictly newer version_code is offered.
        let m = make_manifest(10);
        assert!(!manifest_offers_update(&m, 11)); // older than installed → no
        assert!(!manifest_offers_update(&m, 10)); // same as installed → no
        assert!(manifest_offers_update(&m, 9)); // newer → yes
    }
}
