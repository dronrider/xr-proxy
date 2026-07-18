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
    sign_agent_credential, sign_relay_token, sign_share_token, verify_agent_credential,
    AgentCredential, RelayDescriptor, RelayGrant, ShareGrant, ShareRecord, SCOPE_IMPORT, SCOPE_READ,
    SCOPE_WRITE,
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
    /// The relay this hub advertises (LLD-23 §2.4), so the agent can bring up its
    /// reverse tunnel at install. `None` if the hub has no relay configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayDescriptor>,
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
    let relay = state.config.relay.as_ref().map(|r| r.descriptor());
    Ok(Json(ExchangeResp { credential: encode_blob(&cred), exp, relay }))
}

/// `GET /api/v1/relay`: the hub's relay descriptor (LLD-23), or `null`.
/// Public, the descriptor (addr/port/obfuscation) is not secret since consumers
/// already get it in every grant. The agent fetches it at startup to bring up
/// its reverse tunnel without re-exchanging a token or hand-editing the config
/// (XR-123), so a plain binary update is enough to switch an agent onto relay.
pub async fn get_relay(State(state): State<Arc<AppState>>) -> Json<Option<RelayDescriptor>> {
    Json(state.config.relay.as_ref().map(|r| r.descriptor()))
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
    /// Mark the share reachable through the hub's relay (LLD-23 §2.4). Set by
    /// `xr-share share --relay` for an agent behind NAT.
    #[serde(default)]
    pub via_relay: bool,
    /// Mark the share writable (LLD-28), the hub-side master switch. Set by
    /// `xr-share share --writable`; a share never becomes writable by accident.
    #[serde(default)]
    pub writable: bool,
}

#[derive(Serialize)]
pub struct AddShareResp {
    pub share_id: String,
    pub addr: String,
    pub port: u16,
    /// Ready-to-hand-out access token blob (base64url of the [`ShareToken`]).
    pub token: String,
    pub exp: u64,
    /// The relay descriptor, echoed when the share is `via_relay` and the hub has
    /// a relay, so the agent knows where to open its reverse tunnel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayDescriptor>,
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
        via_relay: req.via_relay,
        writable: req.writable,
    };
    storage::save_share(Path::new(&state.config.server.data_dir), &share)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state.shares.write().await.insert(share_id.clone(), share);

    let exp = now.saturating_add(ttl);
    // The hand-out token is read-only: write scope is minted only through a
    // write-binding on an invite (LLD-28 п. 2.2), never on the share link.
    let token = sign_share_token(&signing.signing_key, &share_id, SCOPE_READ, exp);
    // Give the agent the relay descriptor for a relay-reachable share, so it can
    // bring up the reverse tunnel it just promised the consumer will use.
    let relay = req
        .via_relay
        .then(|| state.config.relay.as_ref().map(|r| r.descriptor()))
        .flatten();
    Ok(Json(AddShareResp {
        share_id,
        addr,
        port: req.port,
        token: encode_blob(&token),
        exp,
        relay,
    }))
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
    // Read-only, like `add`: the owner needs no write link to their own machine,
    // and a second write-scope channel would be extra surface (LLD-28 п. 2.2).
    let token = sign_share_token(&signing.signing_key, &req.share_id, SCOPE_READ, exp);
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
    /// Grant write access on this binding (LLD-28): the share lands in the
    /// invite's `write_share_ids` too, so the hub mints `share:write`. `false`
    /// (the default, and a re-attach without it) is a read-only binding and
    /// clears any prior write binding, keeping `write_share_ids` a subset of
    /// `share_ids`.
    #[serde(default)]
    pub write: bool,
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
    let mut changed = false;
    if !invite.share_ids.contains(&req.share_id) {
        invite.share_ids.push(req.share_id.clone());
        changed = true;
    }
    // Keep write_share_ids a subset of share_ids and match the requested access:
    // --writable adds the write binding, a plain re-attach clears it.
    let has_write = invite.write_share_ids.contains(&req.share_id);
    if req.write && !has_write {
        invite.write_share_ids.push(req.share_id.clone());
        changed = true;
    } else if !req.write && has_write {
        invite.write_share_ids.retain(|s| s != &req.share_id);
        changed = true;
    }
    if changed {
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
    let wbefore = invite.write_share_ids.len();
    invite.share_ids.retain(|s| s != &req.share_id);
    // Drop the write binding too, so a detached share leaves no dangling
    // write_share_ids entry (the subset invariant).
    invite.write_share_ids.retain(|s| s != &req.share_id);
    if invite.share_ids.len() != before || invite.write_share_ids.len() != wbefore {
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

    let (share_ids, write_ids) = {
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
        (invite.share_ids.clone(), invite.write_share_ids.clone())
    };

    let now = now_unix();
    let exp = now.saturating_add(DEFAULT_TOKEN_TTL);
    let shares = state.shares.read().await;
    let mut out = Vec::new();
    for sid in &share_ids {
        // Skip shares that were unregistered after being attached.
        if let Some(rec) = shares.get(sid) {
            // Write scope needs both master switches: a write binding on the
            // invite and a writable record (LLD-28 п. 2.2, п. 3.2). Either one
            // missing keeps the grant read-only. Import rides on the same
            // binding (LLD-29 п. 2.2): a separate attach axis appears only when
            // someone actually needs import without write.
            let scope = if rec.writable && write_ids.contains(sid) {
                format!("{SCOPE_READ} {SCOPE_WRITE} {SCOPE_IMPORT}")
            } else {
                SCOPE_READ.to_string()
            };
            let token = sign_share_token(&signing.signing_key, sid, &scope, exp);
            // A relay-reachable share gets a relay leg next to the direct address
            // (LLD-23 §2.4): its own transit token, bound to this agent+share, and
            // the relay descriptor. The consumer tries direct first, relay last.
            let relay = rec.via_relay.then(|| state.config.relay.as_ref()).flatten().map(|r| {
                let relay_token =
                    sign_relay_token(&signing.signing_key, sid, &rec.agent_pubkey, exp);
                RelayGrant {
                    addr: r.addr.clone(),
                    port: r.port,
                    obf: r.obf.clone(),
                    relay_token,
                }
            });
            out.push(ShareGrant {
                share_id: rec.share_id.clone(),
                name: rec.name.clone(),
                addr: rec.addr.clone(),
                port: rec.port,
                agent_pubkey: rec.agent_pubkey.clone(),
                token: encode_blob(&token),
                exp,
                relay,
            });
        }
    }
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ed25519_dalek::SigningKey;
    use tokio::sync::RwLock;
    use xr_proto::preset::{Invite, InvitePayload};
    use xr_proto::share::verify_relay_token;

    use super::*;
    use crate::config::HubConfig;
    use crate::signing::SigningContext;

    const TOKEN: &str = "abcdefghij0123456789AB";

    fn share(id: &str, agent_pk: &str, via_relay: bool) -> ShareRecord {
        share_rec(id, agent_pk, via_relay, false)
    }

    fn share_rec(id: &str, agent_pk: &str, via_relay: bool, writable: bool) -> ShareRecord {
        ShareRecord {
            share_id: id.into(),
            name: id.into(),
            owner: String::new(),
            addr: "203.0.113.9".into(),
            port: 8443,
            agent_pubkey: agent_pk.into(),
            created_at: String::new(),
            comment: String::new(),
            via_relay,
            writable,
        }
    }

    fn state_with(config_toml: &str, hub: SigningKey, shares: Vec<ShareRecord>) -> Arc<AppState> {
        state_with_writes(config_toml, hub, shares, Vec::new())
    }

    /// Like [`state_with`] but with an explicit write-binding subset on the invite
    /// (LLD-28): `write_ids` go into `write_share_ids`, all shares into `share_ids`.
    fn state_with_writes(
        config_toml: &str,
        hub: SigningKey,
        shares: Vec<ShareRecord>,
        write_ids: Vec<String>,
    ) -> Arc<AppState> {
        let config: HubConfig = toml::from_str(config_toml).unwrap();
        let ids: Vec<String> = shares.iter().map(|s| s.share_id.clone()).collect();
        let invite = Invite {
            token: TOKEN.into(),
            created_at: "2026-01-01T00:00:00+00:00".into(),
            expires_at: "2099-01-01T00:00:00+00:00".into(),
            consumed_at: None,
            claimed_by_ip: None,
            one_time: false,
            comment: String::new(),
            payload: InvitePayload {
                server_address: "203.0.113.10".into(),
                server_port: 8443,
                obfuscation_key: String::new(),
                modifier: "positional_xor_rotate".into(),
                salt: 0,
                preset: "russia".into(),
                hub_url: String::new(),
                servers: Vec::new(),
            },
            share_ids: ids,
            write_share_ids: write_ids,
        };
        let mut invites = HashMap::new();
        invites.insert(TOKEN.to_string(), invite);
        let mut share_map = HashMap::new();
        for s in shares {
            share_map.insert(s.share_id.clone(), s);
        }
        Arc::new(AppState {
            presets: RwLock::new(HashMap::new()),
            invites: RwLock::new(invites),
            shares: RwLock::new(share_map),
            sessions: RwLock::new(HashMap::new()),
            config,
            signing: Some(SigningContext { signing_key: hub }),
        })
    }

    fn config_with_relay() -> String {
        let key = base64::engine::general_purpose::STANDARD.encode(b"relay-obf-key-32-bytes-long!!!!!");
        format!(
            "[server]\n[admin]\nusers = []\n[relay]\naddr = \"relay.example.com\"\nport = 8444\n[relay.obfuscation]\nkey = \"{key}\"\n"
        )
    }

    #[tokio::test]
    async fn invite_shares_attaches_relay_leg_only_for_via_relay() {
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let agent_pk = base64::engine::general_purpose::STANDARD
            .encode(SigningKey::from_bytes(&[7u8; 32]).verifying_key().as_bytes());
        let state = state_with(
            &config_with_relay(),
            hub.clone(),
            vec![
                share("relayed", &agent_pk, true),
                share("direct", &agent_pk, false),
            ],
        );

        let Json(grants) = invite_shares(State(state), AxPath(TOKEN.to_string()))
            .await
            .unwrap();
        let relayed = grants.iter().find(|g| g.share_id == "relayed").unwrap();
        let direct = grants.iter().find(|g| g.share_id == "direct").unwrap();

        // Direct share: no relay leg.
        assert!(direct.relay.is_none(), "a direct share must not get a relay leg");

        // Relayed share: descriptor + a valid, correctly-bound relay token.
        let relay = relayed.relay.as_ref().expect("via_relay share gets a relay leg");
        assert_eq!(relay.addr, "relay.example.com");
        assert_eq!(relay.port, 8444);
        assert!(
            verify_relay_token(
                &relay.relay_token,
                &hub.verifying_key(),
                "relayed",
                &agent_pk,
                now_unix(),
            )
            .is_ok(),
            "relay token must verify against the hub key, bound to this share+agent"
        );
    }

    #[tokio::test]
    async fn invite_shares_no_relay_leg_without_hub_relay() {
        // A via_relay share, but the hub has no [relay]: no leg, share stays direct.
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let state = state_with(
            "[server]\n[admin]\nusers = []\n",
            hub,
            vec![share("relayed", "QQ==", true)],
        );
        let Json(grants) = invite_shares(State(state), AxPath(TOKEN.to_string()))
            .await
            .unwrap();
        assert!(grants[0].relay.is_none(), "no hub relay => no relay leg");
    }

    #[tokio::test]
    async fn get_relay_returns_descriptor_or_null() {
        // XR-123: the agent fetches this at startup. With a relay configured it
        // returns the descriptor; without, null (agent falls back to direct).
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let state = state_with(&config_with_relay(), hub.clone(), vec![]);
        let Json(d) = get_relay(State(state)).await;
        let d = d.expect("relay configured");
        assert_eq!(d.addr, "relay.example.com");
        assert_eq!(d.port, 8444);

        let state = state_with("[server]\n[admin]\nusers = []\n", hub, vec![]);
        let Json(d) = get_relay(State(state)).await;
        assert!(d.is_none(), "no relay configured => null");
    }

    #[test]
    fn relay_obf_config_parses() {
        // The [relay] block round-trips into the descriptor the hub hands out.
        let cfg: HubConfig = toml::from_str(&config_with_relay()).unwrap();
        let r = cfg.relay.expect("relay parsed");
        let desc = r.descriptor();
        assert_eq!(desc.addr, "relay.example.com");
        assert_eq!(desc.obf, r.obf);
    }

    /// The scope carried by a grant's token blob (base64url JSON).
    fn grant_scope(blob: &str) -> String {
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(blob).unwrap();
        serde_json::from_slice::<xr_proto::share::ShareToken>(&json).unwrap().scope
    }

    #[tokio::test]
    async fn test_import_scope_minted_with_write() {
        // LLD-28: write scope is minted only when the record is writable AND the
        // invite has a write binding. Missing either keeps the grant read-only.
        // LLD-29: share:import rides on the same write binding, so a read-only
        // grant carries neither.
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let agent_pk = base64::engine::general_purpose::STANDARD
            .encode(SigningKey::from_bytes(&[7u8; 32]).verifying_key().as_bytes());
        let state = state_with_writes(
            "[server]\n[admin]\nusers = []\n",
            hub.clone(),
            vec![
                share_rec("w", &agent_pk, false, true),   // writable + write binding
                share_rec("wr", &agent_pk, false, true),  // writable, read binding only
                share_rec("nw", &agent_pk, false, false), // write binding, not writable
            ],
            vec!["w".into(), "nw".into()],
        );

        let Json(grants) = invite_shares(State(state), AxPath(TOKEN.to_string()))
            .await
            .unwrap();
        let scope = |id: &str| grant_scope(&grants.iter().find(|g| g.share_id == id).unwrap().token);

        assert_eq!(scope("w"), "share:read share:write share:import");
        assert_eq!(scope("wr"), "share:read", "writable record but no write binding stays read");
        assert_eq!(scope("nw"), "share:read", "write binding but not writable stays read");

        // The write token actually verifies as writable against the hub key.
        let tok_blob = &grants.iter().find(|g| g.share_id == "w").unwrap().token;
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(tok_blob).unwrap();
        let tok: xr_proto::share::ShareToken = serde_json::from_slice(&json).unwrap();
        assert!(xr_proto::share::verify_share_token(
            &tok, &hub.verifying_key(), "w", SCOPE_WRITE, now_unix()
        )
        .is_ok());
    }

    #[tokio::test]
    async fn test_attach_write_subset() {
        // attach --writable adds a write binding, a plain re-attach clears it, and
        // detach drops the share from both lists: write_share_ids stays a subset
        // of share_ids throughout (LLD-28).
        let dir = tempfile::tempdir().unwrap();
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let identity = SigningKey::from_bytes(&[7u8; 32]);
        let agent_pk = base64::engine::general_purpose::STANDARD
            .encode(identity.verifying_key().as_bytes());
        let cred = encode_blob(&sign_agent_credential(&hub, &agent_pk, now_unix() + 3600));
        let config = format!(
            "[server]\ndata_dir = {:?}\n[admin]\nusers = []\n",
            dir.path().display().to_string()
        );
        let state = state_with(&config, hub, vec![share_rec("s", &agent_pk, false, true)]);
        let req = |write: bool| AttachReq {
            credential: cred.clone(),
            share_id: "s".into(),
            invite_token: TOKEN.into(),
            write,
        };
        let snapshot = |state: &Arc<AppState>| {
            let state = state.clone();
            async move {
                let invites = state.invites.read().await;
                let inv = invites.get(TOKEN).unwrap();
                (inv.share_ids.clone(), inv.write_share_ids.clone())
            }
        };

        // Start: state_with put "s" in share_ids as a read binding.
        assert_eq!(snapshot(&state).await, (vec!["s".to_string()], vec![]));

        attach(State(state.clone()), Json(req(true))).await.unwrap();
        assert_eq!(snapshot(&state).await, (vec!["s".to_string()], vec!["s".to_string()]));

        // A plain re-attach downgrades to read only; share_ids unchanged (idempotent).
        attach(State(state.clone()), Json(req(false))).await.unwrap();
        assert_eq!(snapshot(&state).await, (vec!["s".to_string()], vec![]));

        // detach removes it from both.
        attach(State(state.clone()), Json(req(true))).await.unwrap();
        detach(State(state.clone()), Json(req(true))).await.unwrap();
        assert_eq!(snapshot(&state).await, (vec![], vec![]));
    }
}
