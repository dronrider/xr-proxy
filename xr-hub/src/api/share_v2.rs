//! Self-service share endpoints for the universal multishare agent (LLD-19 §9,
//! XR-029).
//!
//! These let an installed agent register shares and mint access tokens **without
//! an admin action each time**. The chain:
//!
//! 1. `POST /api/v1/share/exchange` — once at install: trade a short-lived
//!    reg-token (admin-issued) for a long-lived [`AgentCredential`] bound to the
//!    agent's pubkey. The hub signs it and forgets it (stateless, no agent store).
//! 2. `POST /api/v1/share/add` — per `xr-share share <path>`: present the
//!    credential, the hub creates a [`ShareRecord`] under the credential's pubkey
//!    and returns the new `share_id` plus a ready access token.
//! 3. `POST /api/v1/share/mint` — another access token for an existing share the
//!    same agent owns.
//!
//! All three are unauthenticated routes (no admin session): the bearer credential
//! *is* the authorization, verified against the hub key. Bytes never touch the
//! hub — it stays a pure address index (§3.1).

use std::path::Path;
use std::sync::Arc;

use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use base64::Engine;
use serde::{Deserialize, Serialize};
use xr_proto::share::{
    sign_agent_credential, sign_share_token, verify_agent_credential, AgentCredential, ShareGrant,
    ShareRecord,
};

use crate::api::register::{client_ip, now_unix, validate_ed25519_pubkey, verify_reg_token};
use crate::signing::SigningContext;
use crate::state::AppState;
use crate::storage;

/// Agent mandate lifetime: ~1 year. Long-lived by design (§9.2) — the agent gets
/// it once at install and the TTL is the only revocation lever.
const AGENT_CREDENTIAL_TTL: u64 = 365 * 24 * 3600;
/// Default access-token lifetime when `ttl_seconds` is omitted: 7 days.
const DEFAULT_TOKEN_TTL: u64 = 7 * 24 * 3600;
/// Hard cap on access-token lifetime: 30 days (matches the admin mint path).
const MAX_TOKEN_TTL: u64 = 30 * 24 * 3600;

fn signing_or_503(state: &AppState) -> Result<&SigningContext, (StatusCode, String)> {
    state.signing.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "hub signing key not configured".into(),
    ))
}

/// base64url-no-pad of a JSON value — the blob form used for both the agent
/// credential and the access token (the agent's `auth.rs` decodes tokens this way).
fn encode_blob<T: Serialize>(value: &T) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(value).expect("serialize blob"))
}

fn clamp_token_ttl(ttl: Option<u64>) -> Result<u64, (StatusCode, String)> {
    let ttl = ttl.unwrap_or(DEFAULT_TOKEN_TTL);
    if ttl == 0 || ttl > MAX_TOKEN_TTL {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("ttl_seconds must be 1..={MAX_TOKEN_TTL}"),
        ));
    }
    Ok(ttl)
}

/// Decode an agent-credential blob and verify it against the hub key. Maps to a
/// `403` on any failure (decode, signature, expiry) so a bad mandate is a clean
/// "not authorized", never a 500.
fn verify_credential_blob(
    signing: &SigningContext,
    blob: &str,
    now: u64,
) -> Result<AgentCredential, (StatusCode, String)> {
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(blob.trim())
        .map_err(|_| (StatusCode::BAD_REQUEST, "malformed agent credential".into()))?;
    let cred: AgentCredential = serde_json::from_slice(&json)
        .map_err(|_| (StatusCode::BAD_REQUEST, "malformed agent credential".into()))?;
    verify_agent_credential(&cred, &signing.verifying_key(), now)
        .map_err(|e| (StatusCode::FORBIDDEN, e.to_string()))?;
    Ok(cred)
}

fn random_share_id() -> String {
    let mut id_bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut id_bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id_bytes)
}

// ── exchange: reg-token → agent credential ──────────────────────────

#[derive(Debug, Deserialize)]
pub struct ExchangeReq {
    /// The short-lived registration token from the admin "install command".
    pub token: String,
    /// The agent's own ed25519 public key (standard base64, 32 bytes) — the
    /// identity the mandate binds to.
    pub agent_pubkey: String,
}

#[derive(Serialize)]
pub struct ExchangeResp {
    /// base64url blob of the [`AgentCredential`]; the agent stores it `0600`.
    pub credential: String,
    pub exp: u64,
}

/// `POST /api/v1/share/exchange` — one-time trade of a reg-token for a long-lived
/// agent credential bound to `agent_pubkey`.
pub async fn exchange(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ExchangeReq>,
) -> Result<Json<ExchangeResp>, (StatusCode, String)> {
    let signing = signing_or_503(&state)?;
    verify_reg_token(signing, &req.token, now_unix())?;
    validate_ed25519_pubkey(&req.agent_pubkey)?;

    let exp = now_unix().saturating_add(AGENT_CREDENTIAL_TTL);
    let cred = sign_agent_credential(&signing.signing_key, req.agent_pubkey.trim(), exp);
    Ok(Json(ExchangeResp { credential: encode_blob(&cred), exp }))
}

// ── add: credential → new share + access token ──────────────────────

#[derive(Debug, Deserialize)]
pub struct AddShareReq {
    pub credential: String,
    pub name: String,
    /// Reachable address; if omitted the hub uses the request's source IP.
    #[serde(default)]
    pub addr: Option<String>,
    pub port: u16,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

#[derive(Serialize)]
pub struct AddShareResp {
    pub share_id: String,
    pub addr: String,
    pub port: u16,
    /// Ready-to-hand-out access token blob (base64url of the [`ShareToken`]).
    pub token: String,
    pub exp: u64,
}

/// `POST /api/v1/share/add` — register a share under the credential's pubkey and
/// return a fresh access token in one round-trip (so `xr-share share` prints a
/// link immediately).
pub async fn add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<AddShareReq>,
) -> Result<Json<AddShareResp>, (StatusCode, String)> {
    let signing = signing_or_503(&state)?;
    let now = now_unix();
    let cred = verify_credential_blob(signing, &req.credential, now)?;

    let name = req.name.trim();
    if name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name must not be empty".into()));
    }
    if req.port == 0 {
        return Err((StatusCode::BAD_REQUEST, "port must be non-zero".into()));
    }
    let ttl = clamp_token_ttl(req.ttl_seconds)?;

    let addr = req
        .addr
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| client_ip(&headers))
        .filter(|s| !s.is_empty())
        .ok_or((StatusCode::BAD_REQUEST, "could not determine address — pass addr".into()))?;

    let share_id = random_share_id();
    let share = ShareRecord {
        share_id: share_id.clone(),
        name: name.to_string(),
        owner: String::new(),
        addr: addr.clone(),
        port: req.port,
        // Bind the share to the mandate's identity — consumers pin this key.
        agent_pubkey: cred.agent_pubkey.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        comment: "self-shared (v2)".into(),
    };
    storage::save_share(Path::new(&state.config.server.data_dir), &share)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state.shares.write().await.insert(share_id.clone(), share);

    let exp = now.saturating_add(ttl);
    let token = sign_share_token(&signing.signing_key, &share_id, exp);
    Ok(Json(AddShareResp { share_id, addr, port: req.port, token: encode_blob(&token), exp }))
}

// ── mint: credential → token for an existing owned share ────────────

#[derive(Debug, Deserialize)]
pub struct MintReq {
    pub credential: String,
    pub share_id: String,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

#[derive(Serialize)]
pub struct MintResp {
    pub token: String,
    pub exp: u64,
}

/// `POST /api/v1/share/mint` — issue another access token for a share the
/// presenting agent owns. The share's pinned key must match the credential, so
/// one agent can never mint tokens for another agent's share.
pub async fn mint(
    State(state): State<Arc<AppState>>,
    Json(req): Json<MintReq>,
) -> Result<Json<MintResp>, (StatusCode, String)> {
    let signing = signing_or_503(&state)?;
    let now = now_unix();
    let cred = verify_credential_blob(signing, &req.credential, now)?;
    let ttl = clamp_token_ttl(req.ttl_seconds)?;

    {
        let shares = state.shares.read().await;
        let rec = shares
            .get(&req.share_id)
            .ok_or((StatusCode::NOT_FOUND, "share not found".into()))?;
        if rec.agent_pubkey != cred.agent_pubkey {
            return Err((StatusCode::FORBIDDEN, "share belongs to another agent".into()));
        }
    }

    let exp = now.saturating_add(ttl);
    let token = sign_share_token(&signing.signing_key, &req.share_id, exp);
    Ok(Json(MintResp { token: encode_blob(&token), exp }))
}

/// Drop a share the presenting agent owns (`xr-share unshare`). Same ownership
/// check as `mint`: only the agent whose key the share pins may remove it.
#[derive(Debug, Deserialize)]
pub struct UnshareReq {
    pub credential: String,
    pub share_id: String,
}

pub async fn unshare(
    State(state): State<Arc<AppState>>,
    Json(req): Json<UnshareReq>,
) -> Result<StatusCode, (StatusCode, String)> {
    let signing = signing_or_503(&state)?;
    let cred = verify_credential_blob(signing, &req.credential, now_unix())?;

    let mut shares = state.shares.write().await;
    match shares.get(&req.share_id) {
        None => return Err((StatusCode::NOT_FOUND, "share not found".into())),
        Some(rec) if rec.agent_pubkey != cred.agent_pubkey => {
            return Err((StatusCode::FORBIDDEN, "share belongs to another agent".into()))
        }
        Some(_) => {}
    }
    shares.remove(&req.share_id);
    drop(shares);
    storage::delete_share_file(Path::new(&state.config.server.data_dir), &req.share_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

// ── attach / detach a share to an invite (access anchor, §9.5) ──────

#[derive(Debug, Deserialize)]
pub struct AttachReq {
    pub credential: String,
    pub share_id: String,
    pub invite_token: String,
}

/// Confirm the presenting agent owns `share_id` (the share's pinned key matches
/// the mandate). Shared by attach/detach.
async fn assert_owns_share(
    state: &AppState,
    cred: &AgentCredential,
    share_id: &str,
) -> Result<(), (StatusCode, String)> {
    let shares = state.shares.read().await;
    let rec = shares
        .get(share_id)
        .ok_or((StatusCode::NOT_FOUND, "share not found".into()))?;
    if rec.agent_pubkey != cred.agent_pubkey {
        return Err((StatusCode::FORBIDDEN, "share belongs to another agent".into()));
    }
    Ok(())
}

/// `POST /api/v1/share/attach` — hang one of the agent's shares on an invite, so
/// everyone holding that invite reaches it. Idempotent.
pub async fn attach(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AttachReq>,
) -> Result<StatusCode, (StatusCode, String)> {
    let signing = signing_or_503(&state)?;
    let cred = verify_credential_blob(signing, &req.credential, now_unix())?;
    assert_owns_share(&state, &cred, &req.share_id).await?;

    let mut invites = state.invites.write().await;
    let invite = invites
        .get_mut(&req.invite_token)
        .ok_or((StatusCode::NOT_FOUND, "invite not found".into()))?;
    if !invite.share_ids.contains(&req.share_id) {
        invite.share_ids.push(req.share_id.clone());
        storage::save_invite(Path::new(&state.config.server.data_dir), invite)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/share/detach` — remove the share from the invite.
pub async fn detach(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AttachReq>,
) -> Result<StatusCode, (StatusCode, String)> {
    let signing = signing_or_503(&state)?;
    let cred = verify_credential_blob(signing, &req.credential, now_unix())?;
    assert_owns_share(&state, &cred, &req.share_id).await?;

    let mut invites = state.invites.write().await;
    let invite = invites
        .get_mut(&req.invite_token)
        .ok_or((StatusCode::NOT_FOUND, "invite not found".into()))?;
    let before = invite.share_ids.len();
    invite.share_ids.retain(|s| s != &req.share_id);
    if invite.share_ids.len() != before {
        storage::save_invite(Path::new(&state.config.server.data_dir), invite)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ── consumer: list the shares on my invite (auth = the invite) ──────

/// `GET /api/v1/invite/{token}/shares` — authenticate by the invite (today mere
/// possession; later OIDC/JWT, XR-030) and return every attached share as a
/// [`ShareGrant`] with a minted access token. Not consuming: share access is
/// durable. The hub stays off the data-path; the token is verified by the agent
/// offline.
pub async fn invite_shares(
    State(state): State<Arc<AppState>>,
    AxPath(token): AxPath<String>,
) -> Result<Json<Vec<ShareGrant>>, (StatusCode, String)> {
    let signing = signing_or_503(&state)?;

    let share_ids = {
        let invites = state.invites.read().await;
        let invite = invites
            .get(&token)
            .ok_or((StatusCode::NOT_FOUND, "invite not found".into()))?;
        let now_rfc = chrono::Utc::now().to_rfc3339();
        if invite.expires_at <= now_rfc {
            return Err((StatusCode::GONE, "invite expired".into()));
        }
        // consumed_at is set both by a one-time onboarding claim and by revoke;
        // for durable share access use a non-one-time invite, so a set value
        // here means revoked.
        if invite.consumed_at.is_some() {
            return Err((StatusCode::GONE, "invite revoked".into()));
        }
        invite.share_ids.clone()
    };

    let now = now_unix();
    let exp = now.saturating_add(DEFAULT_TOKEN_TTL);
    let shares = state.shares.read().await;
    let mut out = Vec::new();
    for sid in &share_ids {
        // Skip shares that were unregistered after being attached.
        if let Some(rec) = shares.get(sid) {
            let token = sign_share_token(&signing.signing_key, sid, exp);
            out.push(ShareGrant {
                share_id: rec.share_id.clone(),
                name: rec.name.clone(),
                addr: rec.addr.clone(),
                port: rec.port,
                agent_pubkey: rec.agent_pubkey.clone(),
                token: encode_blob(&token),
                exp,
            });
        }
    }
    Ok(Json(out))
}
