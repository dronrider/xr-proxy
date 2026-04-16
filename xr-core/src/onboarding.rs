//! One-shot HTTP calls against xr-hub used during Android onboarding.
//!
//! Two flows are exposed:
//! - [`fetch_invite_info`] — `GET /api/v1/invite/<token>`, metadata only,
//!   does NOT consume the invite. Used to render the confirmation screen.
//! - [`apply_invite`] — `POST /api/v1/invite/<token>/claim` (consumes
//!   one-time invites) + TOFU `GET /api/v1/public-key` + pre-warm fetch
//!   of the preset into the local cache. Used when the user taps Apply.
//!
//! Both flows are platform-agnostic; the JNI crate wraps them for Kotlin.

use std::path::Path;
use std::time::Duration;

use reqwest::StatusCode;
use xr_proto::preset::{InviteInfo, InvitePayload, Preset};

use crate::presets::PresetCache;

#[derive(Debug)]
pub struct ApplyInviteResult {
    pub payload: Option<InvitePayload>,
    pub public_key: Option<String>,
    pub preset_cached: bool,
    pub errors: Vec<String>,
}

/// GET the invite metadata. Does not consume.
pub async fn fetch_invite_info(
    hub_url: &str,
    token: &str,
    timeout: Duration,
) -> Result<InviteInfo, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    let url = format!("{}/api/v1/invite/{}", hub_url.trim_end_matches('/'), token);
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;

    match resp.status() {
        s if s.is_success() => resp
            .json::<InviteInfo>()
            .await
            .map_err(|e| format!("parse: {e}")),
        StatusCode::NOT_FOUND => Err("not_found".into()),
        StatusCode::GONE => Err("gone".into()),
        s => Err(format!("http_{}", s.as_u16())),
    }
}

/// Claim the invite, fetch the public key and preset, and cache the
/// preset to `cache_dir`. Non-fatal errors are recorded in
/// `result.errors`; `payload` being `Some(_)` means the claim succeeded
/// and the user can proceed even if preset fetch failed.
pub async fn apply_invite(
    hub_url: &str,
    token: &str,
    preset_name: &str,
    cache_dir: &Path,
    per_request_timeout: Duration,
) -> ApplyInviteResult {
    let mut errors = Vec::new();

    let client = match reqwest::Client::builder()
        .timeout(per_request_timeout)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return ApplyInviteResult {
                payload: None,
                public_key: None,
                preset_cached: false,
                errors: vec![format!("http client: {e}")],
            };
        }
    };

    let hub = hub_url.trim_end_matches('/');

    // 1. Claim the invite (may consume, per server policy).
    let claim_url = format!("{}/api/v1/invite/{}/claim", hub, token);
    let payload = match client.post(&claim_url).send().await {
        Ok(resp) => match resp.status() {
            s if s.is_success() => match resp.json::<InvitePayload>().await {
                Ok(p) => Some(p),
                Err(e) => {
                    errors.push(format!("claim parse: {e}"));
                    None
                }
            },
            StatusCode::NOT_FOUND => {
                errors.push("claim: not_found".into());
                None
            }
            StatusCode::GONE => {
                errors.push("claim: gone".into());
                None
            }
            s => {
                errors.push(format!("claim: http_{}", s.as_u16()));
                None
            }
        },
        Err(e) => {
            errors.push(format!("claim network: {e}"));
            None
        }
    };

    // If the claim failed there is nothing to apply — bail early.
    if payload.is_none() {
        return ApplyInviteResult {
            payload: None,
            public_key: None,
            preset_cached: false,
            errors,
        };
    }

    // 2. TOFU public key — best-effort.
    let pubkey_url = format!("{}/api/v1/public-key", hub);
    let public_key = match client.get(&pubkey_url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.text().await {
            Ok(body) => {
                let key = body.trim().to_string();
                if key.is_empty() {
                    None
                } else {
                    Some(key)
                }
            }
            Err(e) => {
                errors.push(format!("public-key read: {e}"));
                None
            }
        },
        Ok(resp) if resp.status() == StatusCode::NOT_FOUND => None,
        Ok(resp) => {
            errors.push(format!("public-key: http_{}", resp.status().as_u16()));
            None
        }
        Err(e) => {
            errors.push(format!("public-key network: {e}"));
            None
        }
    };

    // 3. Pre-warm the preset cache — best-effort.
    let preset_url = format!("{}/api/v1/presets/{}", hub, preset_name);
    let preset_cached = match client.get(&preset_url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.json::<Preset>().await {
            Ok(preset) => match PresetCache::write_to_disk(cache_dir, &preset) {
                Ok(()) => true,
                Err(e) => {
                    errors.push(format!("preset cache write: {e}"));
                    false
                }
            },
            Err(e) => {
                errors.push(format!("preset parse: {e}"));
                false
            }
        },
        Ok(resp) => {
            errors.push(format!("preset: http_{}", resp.status().as_u16()));
            false
        }
        Err(e) => {
            errors.push(format!("preset network: {e}"));
            false
        }
    };

    ApplyInviteResult {
        payload,
        public_key,
        preset_cached,
        errors,
    }
}
