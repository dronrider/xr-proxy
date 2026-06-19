//! Shared types for the Android APK self-update channel (LLD-12).
//!
//! The release **manifest** is a small JSON document describing the latest
//! published APK (version, download URL, SHA-256, notes). It is signed with
//! the offline **release key** (ed25519) — a *different* key from the
//! server/hub preset-signing key, and whose private half never lives on the
//! VPS (LLD-12 §3.1). That separation is what makes "VPS-compromise ≠ RCE":
//! an attacker who owns the VPS can swap the APK and rewrite the manifest, but
//! cannot forge the signature, so the client rejects the tampered update.
//!
//! The signature is computed over the **raw bytes** of the serialized
//! manifest, so the hub stores and serves the manifest as an opaque string and
//! no canonicalization step has to be kept in sync between the signer
//! (`xr-hub sign-release`) and the verifier (`xr-core::update`).

use serde::{Deserialize, Serialize};

/// Release manifest for the latest published APK. Signed (detached) by the
/// offline release key; integrity of the (multi-megabyte) APK is delegated to
/// `apk_sha256` *inside* this signed document, so the binary itself is never
/// signed directly (LLD-12 §3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppManifest {
    /// Monotonic Android `versionCode`. The client offers the update only when
    /// this is strictly greater than the installed `versionCode` — guards
    /// against a replayed older (but validly signed) manifest (LLD-12 §5.6).
    pub version_code: u64,
    /// Human-readable version, e.g. "0.2.0". Doubles as the APK filename and
    /// the `/app/download/:ver` path segment.
    pub version_name: String,
    /// Minimum Android SDK the APK supports.
    pub min_sdk: u32,
    /// Absolute URL to download the APK from.
    pub apk_url: String,
    /// Lowercase hex SHA-256 of the full APK file.
    pub apk_sha256: String,
    /// APK size in bytes (shown in the UI, used for progress).
    pub size_bytes: u64,
    /// Release notes shown in the update banner.
    #[serde(default)]
    pub release_notes: String,
    /// Release date (free-form, e.g. "2026-06-20").
    #[serde(default)]
    pub released_at: String,
}

/// Wire envelope returned by `GET /api/v1/app/latest` and consumed by the
/// client: the manifest as an *opaque JSON string* plus its detached base64
/// ed25519 signature over that exact string's bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedManifest {
    pub manifest: String,
    pub signature: String,
}
