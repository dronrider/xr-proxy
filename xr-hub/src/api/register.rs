//! Agent self-registration for the "no-hands" install (XR-028).
//!
//! Flow: the owner generates a short-lived **registration token** in the admin
//! UI (signed with the hub key — no separate store). The agent presents it to
//! `POST /api/v1/share/register` during `xr-share init`, and the hub creates the
//! `ShareRecord` itself — defaulting the address to the request's source IP — so
//! nothing has to be pasted by hand. The token authorizes *one install run*; it
//! stays valid for its short TTL (re-running creates another share — the owner
//! deletes duplicates).

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use base64::Engine;
use ed25519_dalek::{Signer, Verifier};
use serde::{Deserialize, Serialize};
use xr_proto::share::ShareRecord;

use crate::signing::SigningContext;
use crate::state::AppState;
use crate::storage;

const DEFAULT_REG_TTL: u64 = 3600; // 1h — enough to run the installer
const MAX_REG_TTL: u64 = 7 * 24 * 3600;

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Bytes the registration token signs over — single source for sign+verify.
fn reg_signing_bytes(exp: u64) -> Vec<u8> {
    format!("xr-share-reg\nv1\n{exp}").into_bytes()
}

/// The token blob (base64url of this JSON) the agent carries as `--token`.
#[derive(Serialize, Deserialize)]
struct RegToken {
    exp: u64,
    signature: String,
}

/// Verify a registration-token blob (signature + expiry) against the hub key.
/// Shared by `register` (v1) and the v2 `share/exchange` endpoint so the
/// reg-token check has exactly one implementation.
pub(crate) fn verify_reg_token(
    signing: &SigningContext,
    token: &str,
    now: u64,
) -> Result<(), (StatusCode, String)> {
    let blob = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token.trim())
        .map_err(|_| (StatusCode::BAD_REQUEST, "malformed registration token".into()))?;
    let rt: RegToken = serde_json::from_slice(&blob)
        .map_err(|_| (StatusCode::BAD_REQUEST, "malformed registration token".into()))?;
    if rt.exp <= now {
        return Err((StatusCode::FORBIDDEN, "registration token expired".into()));
    }
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(rt.signature.trim())
        .map_err(|_| (StatusCode::BAD_REQUEST, "malformed token signature".into()))?;
    let sig_arr: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| (StatusCode::BAD_REQUEST, "malformed token signature".into()))?;
    signing
        .verifying_key()
        .verify(
            &reg_signing_bytes(rt.exp),
            &ed25519_dalek::Signature::from_bytes(&sig_arr),
        )
        .map_err(|_| (StatusCode::FORBIDDEN, "invalid registration token".into()))
}

// ── Admin: mint a registration token ────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateRegTokenReq {
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

#[derive(Serialize)]
pub struct CreateRegTokenResp {
    pub token: String,
    pub exp: u64,
}

/// `POST /api/v1/admin/shares/reg-token` — sign a short-lived registration token.
pub async fn create_reg_token(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateRegTokenReq>,
) -> Result<Json<CreateRegTokenResp>, (StatusCode, String)> {
    let signing = state.signing.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "hub signing key not configured".into(),
    ))?;
    let ttl = req.ttl_seconds.unwrap_or(DEFAULT_REG_TTL);
    if ttl == 0 || ttl > MAX_REG_TTL {
        return Err((StatusCode::BAD_REQUEST, format!("ttl_seconds must be 1..={MAX_REG_TTL}")));
    }
    let exp = now_unix().saturating_add(ttl);
    let sig = signing.signing_key.sign(&reg_signing_bytes(exp));
    let rt = RegToken {
        exp,
        signature: base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()),
    };
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&rt).expect("serialize reg token"));
    Ok(Json(CreateRegTokenResp { token, exp }))
}

// ── Public: agent self-registers ────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RegisterReq {
    pub token: String,
    #[serde(default)]
    pub name: String,
    /// Reachable address; if omitted the hub uses the request's source IP.
    #[serde(default)]
    pub addr: Option<String>,
    pub port: u16,
    pub agent_pubkey: String,
    #[serde(default)]
    pub owner: String,
}

#[derive(Serialize)]
pub struct RegisterResp {
    pub share_id: String,
    pub addr: String,
    pub port: u16,
}

/// `POST /api/v1/share/register` — create a share from a valid registration
/// token. Address defaults to the source IP so the agent needn't know its own.
pub async fn register(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<RegisterReq>,
) -> Result<Json<RegisterResp>, (StatusCode, String)> {
    let signing = state.signing.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "hub signing key not configured".into(),
    ))?;

    // Verify the registration token (signature + expiry) against the hub key.
    verify_reg_token(signing, &req.token, now_unix())?;

    // Validate agent identity + port.
    validate_ed25519_pubkey(&req.agent_pubkey)?;
    if req.port == 0 {
        return Err((StatusCode::BAD_REQUEST, "port must be non-zero".into()));
    }

    // Address: explicit, else the source IP the hub sees (public IP / forward).
    let addr = req
        .addr
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| client_ip(&headers))
        .filter(|s| !s.is_empty())
        .ok_or((StatusCode::BAD_REQUEST, "could not determine address — pass addr".into()))?;

    let mut id_bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut id_bytes);
    let share_id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id_bytes);
    let name = if req.name.trim().is_empty() {
        format!("agent {addr}")
    } else {
        req.name.trim().to_string()
    };

    let share = ShareRecord {
        share_id: share_id.clone(),
        name,
        owner: req.owner.trim().to_string(),
        addr: addr.clone(),
        port: req.port,
        agent_pubkey: req.agent_pubkey.trim().to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        comment: "self-registered".into(),
        via_relay: false,
    };
    storage::save_share(Path::new(&state.config.server.data_dir), &share)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state.shares.write().await.insert(share_id.clone(), share);

    Ok(Json(RegisterResp { share_id, addr, port: req.port }))
}

/// Source IP from the reverse proxy headers (nginx) or X-Forwarded-For.
pub(crate) fn client_ip(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(',').next())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

pub(crate) fn validate_ed25519_pubkey(b64: &str) -> Result<(), (StatusCode, String)> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| (StatusCode::BAD_REQUEST, "agent_pubkey must be valid base64".into()))?;
    if bytes.len() != 32 {
        return Err((StatusCode::BAD_REQUEST, format!("agent_pubkey must be 32 bytes, got {}", bytes.len())));
    }
    Ok(())
}
