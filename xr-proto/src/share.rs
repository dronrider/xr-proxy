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

#[cfg(any(feature = "share", test))]
mod crypto {
    use super::{
        agent_credential_signing_bytes, token_signing_bytes, AgentCredential,
        AgentCredentialError, ShareToken, ShareTokenError,
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
}

#[cfg(any(feature = "share", test))]
pub use crypto::{
    sign_agent_credential, sign_share_token, verify_agent_credential, verify_share_token,
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

        // Malformed signature → MalformedSignature, not a panic.
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
