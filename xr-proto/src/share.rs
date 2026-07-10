//! Shared types for the file-sharing agent (LLD-19, XR-027).
//!
//! Trust model in one paragraph: the **hub** is an *index of addresses* — it
//! signs short-lived [`ShareToken`]s with its ed25519 key but never stores or
//! relays bytes. The **agent** (`xr-share`) holds the actual files, serves a
//! [`ShareManifest`] (listing only — path/size/mtime/SHA-256), and verifies the
//! token **offline** against the hub's pinned public key (the hub is not in the
//! data-path). The **consumer** (Android) pins the agent's identity via the hub
//! (TOFU, LLD-04) and downloads straight from the agent.
//!
//! The manifest itself is **signed by the agent's identity key** (XR-046): the
//! data-path is plain HTTP by default, and an unsigned manifest would let a
//! MITM rewrite both a file and its hash, making the SHA-256 check vacuous. The
//! consumer verifies the signature with the `agent_pubkey` it pinned from the
//! grant, which turns the per-file hashes into a chain anchored at the pinned
//! key. See [`manifest_signing_bytes`].
//!
//! This module holds the wire types plus the token sign/verify pair. The types
//! are always available (pure serde); the crypto lives behind the `share`
//! cargo feature so the size-constrained OpenWRT `xr-client` never links
//! `ed25519-dalek`. Tests compile the crypto unconditionally (`cfg(test)` +
//! a dev-dependency) so `cargo test -p xr-proto` is self-contained.
//!
//! Signing follows the `app_update` rule — sign over one canonical byte string
//! produced by a single function ([`token_signing_bytes`]) used by both signer
//! and verifier, so the two can never drift.

use serde::{Deserialize, Serialize};

/// One file in a share, as listed by the agent. Carries **metadata only** — the
/// bytes are fetched directly from the agent over a range request, never from
/// the hub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareManifestEntry {
    /// Path relative to the share root, forward-slash separated, no leading
    /// slash and no `..` (the agent guarantees this — see anti-traversal in
    /// `xr-share`). This is the identity used by the sync diff.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Last-modified time, unix seconds. Used as a cheap pre-filter before the
    /// SHA-256 comparison in the sync planner.
    pub mtime: i64,
    /// Lowercase hex SHA-256 of the file contents — the integrity anchor for
    /// downloads and the change signal for sync.
    pub sha256: String,
}

/// The full listing the agent serves for a share. No bytes — `xr-core`'s sync
/// planner diffs this against local state to decide what to fetch/delete.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ShareManifest {
    pub entries: Vec<ShareManifestEntry>,
}

/// A capability the hub mints and the agent checks. Bound to a single
/// `share_id`, expires at `exp`, and carries a detached ed25519 signature over
/// [`token_signing_bytes`]. The agent verifies it offline with the hub's pinned
/// public key — the hub is never contacted at access time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareToken {
    pub share_id: String,
    /// Expiry, unix seconds.
    pub exp: u64,
    /// Base64 (standard) of the 64-byte ed25519 signature.
    pub signature: String,
}

/// A long-lived **bearer mandate** the hub issues to an agent once at install
/// time (§9.2), so the agent can self-register shares and mint access tokens
/// without an admin action each time. It is the hub's ed25519 signature over
/// `{agent_pubkey, exp}`; the hub verifies it **statelessly** (its own key) and
/// keeps no trusted-agent store. Bearer semantics: whoever holds it can register
/// shares under `agent_pubkey`, so the agent stores it `0600`. Expiry (~1 year)
/// is the only revocation lever, same as a [`ShareToken`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCredential {
    /// Base64 (standard) ed25519 public key this mandate binds to — the identity
    /// the agent's shares will pin (TOFU). Shares created with this credential
    /// carry exactly this key.
    pub agent_pubkey: String,
    /// Expiry, unix seconds (the hub sets it ~1 year out).
    pub exp: u64,
    /// Base64 (standard) of the 64-byte ed25519 signature over
    /// [`agent_credential_signing_bytes`].
    pub signature: String,
}

/// What the hub stores for a registered share: a name and an address, nothing
/// more. **No file listing, no bytes** — that lives on the agent (§3.1, the
/// legal-cleanliness requirement). `test_share_record_has_no_content` guards
/// this shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareRecord {
    /// Stable opaque id (also the token binding and storage filename).
    pub share_id: String,
    /// Human label shown in the consumer's share list.
    pub name: String,
    /// Free-form owner label (who registered it).
    #[serde(default)]
    pub owner: String,
    /// Reachable host or IP of the agent (manual entry in MVP — owner is
    /// responsible for reachability; no heartbeat yet).
    pub addr: String,
    /// Agent listen port.
    pub port: u16,
    /// Base64 ed25519 public key the consumer pins (TOFU, LLD-04). Pinning is
    /// on the *key*, not the address, so a dynamic IP doesn't reset trust.
    pub agent_pubkey: String,
    /// Registration timestamp (free-form, e.g. "2026-06-24T12:00:00Z").
    #[serde(default)]
    pub created_at: String,
    /// Optional note.
    #[serde(default)]
    pub comment: String,
}

/// The public view of a share handed to a consumer by the hub: enough to reach
/// and pin the agent, without owner-side bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInfo {
    pub share_id: String,
    pub name: String,
    pub addr: String,
    pub port: u16,
    pub agent_pubkey: String,
}

/// One reachable share granted to an invite holder (LLD-19 §9.5, XR-031): where
/// the agent is, the key to pin, and a hub-minted access token the agent verifies
/// offline. Returned by `GET /api/v1/invite/{token}/shares`. The `token` here is
/// the URL-safe base64 blob the agent expects as a bearer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareGrant {
    pub share_id: String,
    pub name: String,
    pub addr: String,
    pub port: u16,
    pub agent_pubkey: String,
    pub token: String,
    pub exp: u64,
}

impl ShareRecord {
    /// Project to the consumer-facing [`ShareInfo`] (drops owner/comment/time).
    pub fn info(&self) -> ShareInfo {
        ShareInfo {
            share_id: self.share_id.clone(),
            name: self.name.clone(),
            addr: self.addr.clone(),
            port: self.port,
            agent_pubkey: self.agent_pubkey.clone(),
        }
    }
}

/// The exact bytes a [`ShareToken`] signature covers. Single source of truth for
/// both [`sign_share_token`] and [`verify_share_token`] — they can never drift
/// because there is only one definition of the signed form. Newline-delimited,
/// versioned, so the format can evolve without ambiguity.
///
/// `share_id` cannot contain a newline in practice (it is an opaque id the hub
/// generates); the version prefix and fixed field order keep this injective.
pub fn token_signing_bytes(share_id: &str, exp: u64) -> Vec<u8> {
    format!("xr-share-token\nv1\n{share_id}\n{exp}").into_bytes()
}

/// The exact bytes an [`AgentCredential`] signature covers. As with
/// [`token_signing_bytes`], a single definition shared by signer and verifier.
/// A distinct domain prefix (`xr-share-agent-cred`) keeps an agent credential
/// from ever being confused with a share token, even though both are hub
/// signatures over a `(string, exp)` pair.
pub fn agent_credential_signing_bytes(agent_pubkey: &str, exp: u64) -> Vec<u8> {
    format!("xr-share-agent-cred\nv1\n{agent_pubkey}\n{exp}").into_bytes()
}

/// Response header carrying the agent's detached manifest signature (base64 of
/// the 64-byte ed25519 signature). Paired with [`MANIFEST_SIGNED_AT_HEADER`];
/// the body of the response is the manifest JSON the signature covers.
pub const MANIFEST_SIG_HEADER: &str = "x-xr-manifest-sig";

/// Response header with the unix-seconds moment the manifest was signed. Part
/// of the signed bytes, so it cannot be altered in flight.
pub const MANIFEST_SIGNED_AT_HEADER: &str = "x-xr-manifest-signed-at";

/// The exact bytes a manifest signature covers (XR-046). Domain prefix and
/// newline-delimited fields follow [`token_signing_bytes`]. `share_id` binds
/// the signature to one share: an agent signs every share it serves with the
/// same identity key, so without the binding a captured manifest of share A
/// would verify as share B. `signed_at` records the signing moment (kept in
/// the signed form so future freshness checks need no format change). The
/// manifest JSON is appended verbatim: the signature covers the exact bytes
/// the agent serves, with no canonicalization step to keep in sync between
/// signer and verifier (the `app_update` rule).
pub fn manifest_signing_bytes(share_id: &str, signed_at: u64, manifest_json: &[u8]) -> Vec<u8> {
    let mut bytes = format!("xr-share-manifest\nv1\n{share_id}\n{signed_at}\n").into_bytes();
    bytes.extend_from_slice(manifest_json);
    bytes
}

/// Why a [`verify_share_token`] check failed. Distinct variants so the agent can
/// log/diagnose without leaking the token itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareTokenError {
    /// `signature` was not valid base64 or not 64 bytes.
    MalformedSignature,
    /// Signature did not verify against the pinned hub key (tampered claims or
    /// wrong signer).
    BadSignature,
    /// `exp` is at or before `now` — the token has lapsed.
    Expired,
    /// The token is for a different `share_id` than the one being accessed.
    WrongShare,
}

impl core::fmt::Display for ShareTokenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Self::MalformedSignature => "malformed token signature",
            Self::BadSignature => "token signature does not verify",
            Self::Expired => "token has expired",
            Self::WrongShare => "token is for a different share",
        };
        f.write_str(s)
    }
}

impl std::error::Error for ShareTokenError {}

/// Why a [`verify_agent_credential`] check failed. Mirrors [`ShareTokenError`]
/// but without a share-binding variant (a credential is not share-scoped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCredentialError {
    /// `signature` was not valid base64 or not 64 bytes.
    MalformedSignature,
    /// Signature did not verify against the hub key (tampered or wrong signer).
    BadSignature,
    /// `exp` is at or before `now` — the mandate has lapsed.
    Expired,
}

impl core::fmt::Display for AgentCredentialError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Self::MalformedSignature => "malformed agent-credential signature",
            Self::BadSignature => "agent-credential signature does not verify",
            Self::Expired => "agent-credential has expired",
        };
        f.write_str(s)
    }
}

impl std::error::Error for AgentCredentialError {}

/// Why a [`verify_share_manifest`] check failed. As with [`ShareTokenError`],
/// distinct variants for diagnostics; every variant means "do not trust the
/// listing".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestSigError {
    /// The pinned `agent_pubkey` was not valid base64, not 32 bytes, or not a
    /// valid ed25519 point.
    MalformedKey,
    /// The signature was not valid base64 or not 64 bytes.
    MalformedSignature,
    /// Signature did not verify: tampered manifest bytes or timestamp, a
    /// different share's manifest replayed, or a wrong signer.
    BadSignature,
}

impl core::fmt::Display for ManifestSigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Self::MalformedKey => "malformed agent public key",
            Self::MalformedSignature => "malformed manifest signature",
            Self::BadSignature => "manifest signature does not verify",
        };
        f.write_str(s)
    }
}

impl std::error::Error for ManifestSigError {}

#[cfg(any(feature = "share", test))]
mod crypto {
    use super::{
        agent_credential_signing_bytes, manifest_signing_bytes, token_signing_bytes,
        AgentCredential, AgentCredentialError, ManifestSigError, ShareToken, ShareTokenError,
    };
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};

    /// Mint a share token: sign `(share_id, exp)` with the hub's key. The caller
    /// (hub) owns the `SigningKey`; `exp` is an absolute unix-seconds deadline.
    pub fn sign_share_token(key: &SigningKey, share_id: &str, exp: u64) -> ShareToken {
        let sig = key.sign(&token_signing_bytes(share_id, exp));
        ShareToken {
            share_id: share_id.to_string(),
            exp,
            signature: base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()),
        }
    }

    /// Verify a token offline against the pinned hub public key, for access to
    /// `expected_share_id` at wall-clock `now_unix`. Fails closed: any decode
    /// error, signature mismatch, share mismatch, or expiry returns `Err`.
    ///
    /// Order matters for diagnostics: share binding and expiry are cheap and
    /// checked first, then the signature. (All paths still fully reject — none
    /// of the early returns grant access.)
    pub fn verify_share_token(
        token: &ShareToken,
        hub_key: &VerifyingKey,
        expected_share_id: &str,
        now_unix: u64,
    ) -> Result<(), ShareTokenError> {
        if token.share_id != expected_share_id {
            return Err(ShareTokenError::WrongShare);
        }
        if token.exp <= now_unix {
            return Err(ShareTokenError::Expired);
        }
        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(token.signature.trim())
            .map_err(|_| ShareTokenError::MalformedSignature)?;
        let sig_arr: [u8; 64] = sig_bytes
            .try_into()
            .map_err(|_| ShareTokenError::MalformedSignature)?;
        let signature = ed25519_dalek::Signature::from_bytes(&sig_arr);
        hub_key
            .verify(&token_signing_bytes(&token.share_id, token.exp), &signature)
            .map_err(|_| ShareTokenError::BadSignature)
    }

    /// Issue an agent mandate: sign `(agent_pubkey, exp)` with the hub key. The
    /// hub calls this once per agent at install time (the reg-token exchange).
    pub fn sign_agent_credential(key: &SigningKey, agent_pubkey: &str, exp: u64) -> AgentCredential {
        let sig = key.sign(&agent_credential_signing_bytes(agent_pubkey, exp));
        AgentCredential {
            agent_pubkey: agent_pubkey.to_string(),
            exp,
            signature: base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()),
        }
    }

    /// Verify an agent mandate against the hub key at wall-clock `now_unix`.
    /// Stateless: a valid hub signature over a non-expired `{agent_pubkey, exp}`
    /// is the whole proof — no trusted-agent store. Fails closed. Expiry is
    /// checked before the signature (cheap first), but every path still rejects.
    pub fn verify_agent_credential(
        cred: &AgentCredential,
        hub_key: &VerifyingKey,
        now_unix: u64,
    ) -> Result<(), AgentCredentialError> {
        if cred.exp <= now_unix {
            return Err(AgentCredentialError::Expired);
        }
        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(cred.signature.trim())
            .map_err(|_| AgentCredentialError::MalformedSignature)?;
        let sig_arr: [u8; 64] = sig_bytes
            .try_into()
            .map_err(|_| AgentCredentialError::MalformedSignature)?;
        let signature = ed25519_dalek::Signature::from_bytes(&sig_arr);
        hub_key
            .verify(
                &agent_credential_signing_bytes(&cred.agent_pubkey, cred.exp),
                &signature,
            )
            .map_err(|_| AgentCredentialError::BadSignature)
    }

    /// Sign a manifest as served (XR-046): the **agent's identity key** over
    /// [`manifest_signing_bytes`]. Returns the base64 signature for the
    /// [`MANIFEST_SIG_HEADER`](super::MANIFEST_SIG_HEADER) response header;
    /// `manifest_json` must be the exact response body bytes.
    pub fn sign_share_manifest(
        key: &SigningKey,
        share_id: &str,
        signed_at: u64,
        manifest_json: &[u8],
    ) -> String {
        let sig = key.sign(&manifest_signing_bytes(share_id, signed_at, manifest_json));
        base64::engine::general_purpose::STANDARD.encode(sig.to_bytes())
    }

    /// Decode a pinned base64 `agent_pubkey` (as carried by a `ShareGrant` /
    /// `ShareInfo`) into a verifying key. Single decode point for consumers.
    pub fn parse_agent_pubkey(b64: &str) -> Result<VerifyingKey, ManifestSigError> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|_| ManifestSigError::MalformedKey)?;
        let arr: [u8; 32] = bytes.try_into().map_err(|_| ManifestSigError::MalformedKey)?;
        VerifyingKey::from_bytes(&arr).map_err(|_| ManifestSigError::MalformedKey)
    }

    /// Verify a manifest signature against the pinned agent key, for the share
    /// the consumer actually requested (`share_id` is what binds the reply to
    /// the request) and the served body bytes. Fails closed: any decode error
    /// or mismatch rejects the listing.
    pub fn verify_share_manifest(
        sig_b64: &str,
        agent_key: &VerifyingKey,
        share_id: &str,
        signed_at: u64,
        manifest_json: &[u8],
    ) -> Result<(), ManifestSigError> {
        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(sig_b64.trim())
            .map_err(|_| ManifestSigError::MalformedSignature)?;
        let sig_arr: [u8; 64] = sig_bytes
            .try_into()
            .map_err(|_| ManifestSigError::MalformedSignature)?;
        let signature = ed25519_dalek::Signature::from_bytes(&sig_arr);
        agent_key
            .verify(&manifest_signing_bytes(share_id, signed_at, manifest_json), &signature)
            .map_err(|_| ManifestSigError::BadSignature)
    }
}

#[cfg(any(feature = "share", test))]
pub use crypto::{
    parse_agent_pubkey, sign_agent_credential, sign_share_manifest, sign_share_token,
    verify_agent_credential, verify_share_manifest, verify_share_token,
};

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use ed25519_dalek::SigningKey;

    fn hub_key() -> SigningKey {
        SigningKey::from_bytes(&[42u8; 32])
    }

    #[test]
    fn token_signing_bytes_is_deterministic() {
        let a = token_signing_bytes("share-1", 1000);
        let b = token_signing_bytes("share-1", 1000);
        assert_eq!(a, b);
        // Distinct inputs produce distinct bytes (no field collision).
        assert_ne!(token_signing_bytes("share-1", 1000), token_signing_bytes("share-1", 1001));
        assert_ne!(token_signing_bytes("share-1", 1000), token_signing_bytes("share-2", 1000));
    }

    #[test]
    fn test_share_token_sign_verify() {
        let key = hub_key();
        let vk = key.verifying_key();
        let token = sign_share_token(&key, "share-1", 5000);

        // Valid: right key, right share, not yet expired.
        assert!(verify_share_token(&token, &vk, "share-1", 4999).is_ok());

        // Wrong signer key → BadSignature.
        let other = SigningKey::from_bytes(&[7u8; 32]).verifying_key();
        assert_eq!(
            verify_share_token(&token, &other, "share-1", 4999),
            Err(ShareTokenError::BadSignature)
        );

        // Expired exp (now == exp, and now > exp) → Expired.
        assert_eq!(
            verify_share_token(&token, &vk, "share-1", 5000),
            Err(ShareTokenError::Expired)
        );
        assert_eq!(
            verify_share_token(&token, &vk, "share-1", 5001),
            Err(ShareTokenError::Expired)
        );

        // Bound to a different share → WrongShare.
        assert_eq!(
            verify_share_token(&token, &vk, "share-2", 4999),
            Err(ShareTokenError::WrongShare)
        );
    }

    #[test]
    fn verify_rejects_tampered_claims() {
        let key = hub_key();
        let vk = key.verifying_key();
        let mut token = sign_share_token(&key, "share-1", 5000);
        // Push out the expiry without re-signing → signature no longer matches.
        token.exp = 9999;
        assert_eq!(
            verify_share_token(&token, &vk, "share-1", 4999),
            Err(ShareTokenError::BadSignature)
        );
    }

    #[test]
    fn verify_rejects_malformed_signature() {
        let key = hub_key();
        let vk = key.verifying_key();
        let mut token = sign_share_token(&key, "share-1", 5000);
        token.signature = "not-base64-@@@".into();
        assert_eq!(
            verify_share_token(&token, &vk, "share-1", 4999),
            Err(ShareTokenError::MalformedSignature)
        );
        // Valid base64 but wrong length is also malformed, not a panic.
        token.signature = base64::engine::general_purpose::STANDARD.encode([0u8; 10]);
        assert_eq!(
            verify_share_token(&token, &vk, "share-1", 4999),
            Err(ShareTokenError::MalformedSignature)
        );
    }

    #[test]
    fn test_agent_credential_verify() {
        let key = hub_key();
        let vk = key.verifying_key();
        let agent_pk = "QQ=="; // opaque label here; real callers pass a 32-byte key
        let cred = sign_agent_credential(&key, agent_pk, 5000);

        // Valid: right hub key, not expired.
        assert!(verify_agent_credential(&cred, &vk, 4999).is_ok());
        assert_eq!(cred.agent_pubkey, agent_pk);

        // Expired (now == exp and now > exp) → Expired.
        assert_eq!(
            verify_agent_credential(&cred, &vk, 5000),
            Err(AgentCredentialError::Expired)
        );
        assert_eq!(
            verify_agent_credential(&cred, &vk, 6000),
            Err(AgentCredentialError::Expired)
        );

        // Wrong signer → BadSignature.
        let other = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        assert_eq!(
            verify_agent_credential(&cred, &other, 4999),
            Err(AgentCredentialError::BadSignature)
        );

        // Tampered pubkey without re-signing → BadSignature (the signed bytes
        // bind the key, so swapping it invalidates the mandate).
        let mut forged = cred.clone();
        forged.agent_pubkey = "WW==".into();
        assert_eq!(
            verify_agent_credential(&forged, &vk, 4999),
            Err(AgentCredentialError::BadSignature)
        );

        // Malformed signature -> MalformedSignature, not a panic.
        let mut bad = cred.clone();
        bad.signature = "@@@".into();
        assert_eq!(
            verify_agent_credential(&bad, &vk, 4999),
            Err(AgentCredentialError::MalformedSignature)
        );
    }

    #[test]
    fn agent_credential_domain_separated_from_token() {
        // A share token and an agent credential are both hub signatures over a
        // (string, exp) pair; the distinct domain prefixes must keep their signed
        // bytes disjoint so one can never be replayed as the other.
        assert_ne!(
            agent_credential_signing_bytes("x", 1),
            token_signing_bytes("x", 1)
        );
    }

    #[test]
    fn test_manifest_sign_verify() {
        let agent = SigningKey::from_bytes(&[3u8; 32]);
        let vk = agent.verifying_key();
        let body = br#"{"entries":[{"path":"a.txt","size":5,"mtime":1,"sha256":"aa"}]}"#;
        let sig = sign_share_manifest(&agent, "share-1", 7000, body);

        // Valid: right key, right share, exact bytes.
        assert!(verify_share_manifest(&sig, &vk, "share-1", 7000, body).is_ok());

        // Tampered body (a MITM swapping a hash) -> BadSignature.
        let forged = br#"{"entries":[{"path":"a.txt","size":5,"mtime":1,"sha256":"bb"}]}"#;
        assert_eq!(
            verify_share_manifest(&sig, &vk, "share-1", 7000, forged),
            Err(ManifestSigError::BadSignature)
        );

        // Same agent's manifest for another share replayed here -> BadSignature.
        assert_eq!(
            verify_share_manifest(&sig, &vk, "share-2", 7000, body),
            Err(ManifestSigError::BadSignature)
        );

        // Shifted timestamp without re-signing -> BadSignature.
        assert_eq!(
            verify_share_manifest(&sig, &vk, "share-1", 7001, body),
            Err(ManifestSigError::BadSignature)
        );

        // Wrong signer (a MITM's own key) -> BadSignature.
        let other = SigningKey::from_bytes(&[4u8; 32]).verifying_key();
        assert_eq!(
            verify_share_manifest(&sig, &other, "share-1", 7000, body),
            Err(ManifestSigError::BadSignature)
        );

        // Malformed signature -> MalformedSignature, not a panic.
        assert_eq!(
            verify_share_manifest("@@@", &vk, "share-1", 7000, body),
            Err(ManifestSigError::MalformedSignature)
        );
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 10]);
        assert_eq!(
            verify_share_manifest(&short, &vk, "share-1", 7000, body),
            Err(ManifestSigError::MalformedSignature)
        );
    }

    #[test]
    fn parse_agent_pubkey_rejects_junk() {
        let good = base64::engine::general_purpose::STANDARD
            .encode(SigningKey::from_bytes(&[3u8; 32]).verifying_key().as_bytes());
        assert!(parse_agent_pubkey(&good).is_ok());
        for bad in ["", "not-base64-@@@", "QQ=="] {
            assert_eq!(parse_agent_pubkey(bad), Err(ManifestSigError::MalformedKey), "{bad:?}");
        }
    }

    #[test]
    fn manifest_domain_separated_from_token_and_cred() {
        // All three are ed25519 signatures over newline-joined fields; the
        // distinct prefixes keep the signed byte spaces disjoint.
        let m = manifest_signing_bytes("x", 1, b"");
        assert_ne!(m, token_signing_bytes("x", 1));
        assert_ne!(m, agent_credential_signing_bytes("x", 1));
    }

    #[test]
    fn test_share_record_has_no_content() {
        // The hub record is an index entry: address + identity metadata only.
        // Serialize and assert no field carries file bytes or a listing — this
        // is the legal-cleanliness invariant (§3.1), enforced structurally.
        let rec = ShareRecord {
            share_id: "abc".into(),
            name: "Photos".into(),
            owner: "andrew".into(),
            addr: "203.0.113.7".into(),
            port: 8443,
            agent_pubkey: "QQ==".into(),
            created_at: "2026-06-24T00:00:00Z".into(),
            comment: "vacation".into(),
        };
        let json = serde_json::to_string(&rec).unwrap();
        for forbidden in ["entries", "files", "content", "data", "manifest", "sha256", "bytes"] {
            assert!(
                !json.contains(forbidden),
                "ShareRecord JSON must not carry content-bearing field `{forbidden}`: {json}"
            );
        }
    }

    #[test]
    fn share_info_drops_owner_side_fields() {
        let rec = ShareRecord {
            share_id: "abc".into(),
            name: "Photos".into(),
            owner: "andrew".into(),
            addr: "203.0.113.7".into(),
            port: 8443,
            agent_pubkey: "QQ==".into(),
            created_at: "2026-06-24T00:00:00Z".into(),
            comment: "secret note".into(),
        };
        let json = serde_json::to_string(&rec.info()).unwrap();
        assert!(!json.contains("owner"));
        assert!(!json.contains("secret note"));
        assert!(json.contains("agent_pubkey"));
    }
}
