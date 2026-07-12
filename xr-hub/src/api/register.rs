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

/// Sign a fresh registration token with the given TTL, returning the blob and its
/// expiry. Shared by the admin reg-token endpoint and the combined setup-token
/// (XR-127), so both go through one signing path.
pub(crate) fn sign_reg_token(
    signing: &SigningContext,
    ttl: u64,
) -> Result<(String, u64), (StatusCode, String)> {
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
    Ok((token, exp))
}

/// `POST /api/v1/admin/shares/reg-token` signs a short-lived registration token.
pub async fn create_reg_token(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateRegTokenReq>,
) -> Result<Json<CreateRegTokenResp>, (StatusCode, String)> {
    let signing = state.signing.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "hub signing key not configured".into(),
    ))?;
    let ttl = req.ttl_seconds.unwrap_or(DEFAULT_REG_TTL);
    let (token, exp) = sign_reg_token(signing, ttl)?;
    Ok(Json(CreateRegTokenResp { token, exp }))
}

// Admin: mint a combined setup token (reg-token + invite), XR-127.

#[derive(Debug, Deserialize)]
pub struct CreateSetupTokenReq {
    /// Reg-token lifetime: how long the operator has to run the installer.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    /// Invite lifetime; defaults to the hub's invite default TTL.
    #[serde(default)]
    pub invite_ttl_seconds: Option<u64>,
    /// One-time invite. Default false: a setup token you hand to a colleague is
    /// meant to be reusable while it lives.
    #[serde(default)]
    pub one_time: bool,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub preset: Option<String>,
}

#[derive(Serialize)]
pub struct CreateSetupTokenResp {
    /// The single blob for `xr-share install --setup`: base64url("<reg>.<invite>").
    pub setup_token: String,
    pub reg_token: String,
    pub invite_token: String,
    pub reg_exp: u64,
}

/// Pack a reg-token and an invite token into one opaque setup token. The client
/// splits it back with [`unpack_setup_token`].
pub(crate) fn pack_setup_token(reg_token: &str, invite_token: &str) -> String {
    let joined = format!("{reg_token}.{invite_token}");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(joined.as_bytes())
}

/// `POST /api/v1/admin/shares/setup-token` mints a reg-token AND an invite and
/// bundles them into one blob (XR-127), so onboarding needs a single secret. The
/// agent redeems the reg-token for a mandate and auto-attaches its shares to the
/// invite.
pub async fn create_setup_token(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateSetupTokenReq>,
) -> Result<Json<CreateSetupTokenResp>, (StatusCode, String)> {
    let signing = state.signing.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "hub signing key not configured".into(),
    ))?;
    let ttl = req.ttl_seconds.unwrap_or(DEFAULT_REG_TTL);
    let (reg_token, reg_exp) = sign_reg_token(signing, ttl)?;

    let invite = crate::api::invites::build_invite(
        &state,
        req.invite_ttl_seconds,
        req.one_time,
        req.comment.clone(),
        req.preset.clone(),
        None,
    )
    .await?;
    let invite_token = invite.token;

    let setup_token = pack_setup_token(&reg_token, &invite_token);
    Ok(Json(CreateSetupTokenResp { setup_token, reg_token, invite_token, reg_exp }))
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ed25519_dalek::SigningKey;
    use tokio::sync::RwLock;

    use super::*;
    use crate::config::HubConfig;

    fn state_with_signing() -> Arc<AppState> {
        let dir = std::env::temp_dir().join(format!("xr-hub-setup-{}", std::process::id()));
        let toml = format!("[server]\ndata_dir = \"{}\"\n[admin]\nusers = []\n", dir.display());
        let config: HubConfig = toml::from_str(&toml).unwrap();
        Arc::new(AppState {
            presets: RwLock::new(HashMap::new()),
            invites: RwLock::new(HashMap::new()),
            shares: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            config,
            signing: Some(crate::signing::SigningContext {
                signing_key: SigningKey::from_bytes(&[42u8; 32]),
            }),
        })
    }

    /// The setup token packs a reg-token and an invite into one blob; splitting it
    /// back yields exactly the two tokens the response reports, the reg half
    /// verifies against the hub key, and the invite half is stored (XR-127).
    #[tokio::test]
    async fn setup_token_bundles_reg_and_invite() {
        let state = state_with_signing();
        let resp = create_setup_token(
            State(state.clone()),
            Json(CreateSetupTokenReq {
                ttl_seconds: Some(3600),
                invite_ttl_seconds: Some(3600),
                one_time: false,
                comment: "colleague".into(),
                preset: None,
            }),
        )
        .await
        .expect("mint setup token")
        .0;

        // The opaque blob splits back into the same two tokens.
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&resp.setup_token)
            .expect("setup token is base64url");
        let joined = String::from_utf8(raw).unwrap();
        let (reg, inv) = joined.split_once('.').expect("reg.invite");
        assert_eq!(reg, resp.reg_token);
        assert_eq!(inv, resp.invite_token);

        // The reg half is a valid registration token against the hub key.
        let signing = state.signing.as_ref().unwrap();
        verify_reg_token(signing, reg, now_unix()).expect("reg half verifies");

        // The invite half is registered and reachable by its token.
        assert!(state.invites.read().await.contains_key(inv));
    }
}
