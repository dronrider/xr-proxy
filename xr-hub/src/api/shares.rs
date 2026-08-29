//! Share index endpoints (LLD-19, XR-027).
//!
//! The hub is a *telephone book*: it stores a [`ShareRecord`] (name + address +
//! the agent's pinned public key) and mints short-lived [`ShareToken`]s signed
//! with the hub's ed25519 key. It never stores or relays file bytes — the agent
//! (`xr-share`) holds the data and verifies tokens offline. Routes:
//!
//! - Public:  `GET  /api/v1/shares`              -> consumer view (name+addr+key),
//!           auth: a live grant or invite (XR-193)
//! - Admin:   `GET  /api/v1/admin/shares`        -> full records
//!            `POST /api/v1/admin/shares`        -> register address:port + pubkey
//!            `DELETE /api/v1/admin/shares/:id`  -> unregister
//!            `POST /api/v1/admin/shares/:id/token` -> mint a signed access token

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{self, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use base64::Engine;
use serde::Deserialize;
use xr_proto::share::{
    sign_share_token, verify_share_token, ShareInfo, ShareRecord, ShareToken, SCOPE_READ,
};

use crate::state::AppState;
use crate::storage;

/// Default token lifetime when the request omits `ttl_seconds`: 7 days.
const DEFAULT_TOKEN_TTL_SECONDS: u64 = 7 * 24 * 3600;
/// Hard cap on token lifetime: 30 days. A share token is a bearer capability,
/// so the TTL is the primary revocation lever (§5.6) — keep it bounded.
const MAX_TOKEN_TTL_SECONDS: u64 = 30 * 24 * 3600;

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── Public ──────────────────────────────────────────────────────────

/// `GET /api/v1/shares` auth (XR-193): the index lists every agent of the
/// fleet, so a reader must present either a live invite token (the same
/// liveness rule as `invite_shares`) or a share-access grant, the base64url
/// [`ShareToken`] blob, verified against the hub key with the `share:read`
/// scope. Any valid grant opens the whole index: the token binds its own
/// `share_id`, and refusing an otherwise-live grant over a deleted share
/// would only lock a legitimate reader out. An absent credential is `401`,
/// a presented-but-dead one is `403`/`410`.
async fn authorize_index_reader(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, String)> {
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("")
        .trim();
    if provided.is_empty() {
        return Err((
            StatusCode::UNAUTHORIZED,
            "share index requires a grant or invite token".into(),
        ));
    }

    {
        let invites = state.invites.read().await;
        if let Some(invite) = invites.get(provided) {
            let now_rfc = chrono::Utc::now().to_rfc3339();
            if invite.expires_at <= now_rfc {
                return Err((StatusCode::GONE, "invite expired".into()));
            }
            if invite.consumed_at.is_some() {
                return Err((StatusCode::GONE, "invite revoked".into()));
            }
            return Ok(());
        }
    }

    let Some(signing) = state.signing.as_ref() else {
        return Err((StatusCode::FORBIDDEN, "unknown credential".into()));
    };
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(provided)
        .map_err(|_| (StatusCode::FORBIDDEN, "unknown credential".into()))?;
    let token: ShareToken = serde_json::from_slice(&json)
        .map_err(|_| (StatusCode::FORBIDDEN, "unknown credential".into()))?;
    verify_share_token(
        &token,
        &signing.verifying_key(),
        &token.share_id,
        SCOPE_READ,
        now_unix(),
    )
    .map_err(|e| (StatusCode::FORBIDDEN, e.to_string()))?;
    Ok(())
}

/// GET /api/v1/shares — the consumer-facing index: for every registered share,
/// just enough to reach and pin the agent (name + addr:port + pubkey). No
/// owner-side bookkeeping, no file listing (that comes from the agent).
pub async fn list_shares(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<ShareInfo>>, (StatusCode, String)> {
    authorize_index_reader(&state, &headers).await?;
    let shares = state.shares.read().await;
    let mut list: Vec<ShareInfo> = shares.values().map(ShareRecord::info).collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(list))
}

// ── Admin ───────────────────────────────────────────────────────────

/// GET /api/v1/admin/shares — full records (incl. owner/comment/created_at).
pub async fn admin_list_shares(State(state): State<Arc<AppState>>) -> Json<Vec<ShareRecord>> {
    let shares = state.shares.read().await;
    let mut list: Vec<ShareRecord> = shares.values().cloned().collect();
    list.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Json(list)
}

#[derive(Debug, Deserialize)]
pub struct CreateShareRequest {
    pub name: String,
    #[serde(default)]
    pub owner: String,
    /// Reachable host or IP of the agent (manual entry — owner owns reachability).
    pub addr: String,
    pub port: u16,
    /// Base64 (standard) ed25519 public key the consumer will pin (TOFU).
    pub agent_pubkey: String,
    #[serde(default)]
    pub comment: String,
}

/// POST /api/v1/admin/shares — register a share. Validates the address and the
/// agent pubkey, generates an opaque `share_id`, and persists the record.
pub async fn create_share(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateShareRequest>,
) -> Result<(StatusCode, Json<ShareRecord>), (StatusCode, String)> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name must not be empty".into()));
    }
    let addr = req.addr.trim();
    if addr.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "addr must not be empty".into()));
    }
    if req.port == 0 {
        return Err((StatusCode::BAD_REQUEST, "port must be non-zero".into()));
    }
    // The pubkey is what the consumer pins — reject anything that is not a
    // 32-byte ed25519 key so a typo can't be saved as an unpinnable identity.
    validate_ed25519_pubkey(&req.agent_pubkey)?;

    // Opaque, filename-safe id (16 random bytes, base64url no-pad — same shape
    // as invite tokens).
    let mut id_bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut id_bytes);
    let share_id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id_bytes);

    let share = ShareRecord {
        share_id,
        name: name.to_string(),
        owner: req.owner.trim().to_string(),
        addr: addr.to_string(),
        // Admin entry is a single address; the multi-address list (XR-050) is
        // populated only through the self-service `xr-share share` path.
        addrs: Vec::new(),
        port: req.port,
        agent_pubkey: req.agent_pubkey.trim().to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        comment: req.comment.trim().to_string(),
        via_relay: false,
        // Admin-registered shares are read-only by default; write is opted into
        // through the self-service `xr-share share --writable` path (LLD-28).
        writable: false,
    };

    let data_dir = Path::new(&state.config.server.data_dir);
    storage::save_share(data_dir, &share)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mut shares = state.shares.write().await;
    shares.insert(share.share_id.clone(), share.clone());

    Ok((StatusCode::CREATED, Json(share)))
}

/// DELETE /api/v1/admin/shares/:id — unregister a share.
pub async fn delete_share(
    State(state): State<Arc<AppState>>,
    extract::Path(share_id): extract::Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let mut shares = state.shares.write().await;
    // Only act on ids we actually hold — this also keeps a crafted id (`../…`)
    // from ever reaching the filesystem helper.
    if shares.remove(&share_id).is_none() {
        return Err((StatusCode::NOT_FOUND, "share not found".into()));
    }
    let data_dir = Path::new(&state.config.server.data_dir);
    storage::delete_share_file(data_dir, &share_id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct MintTokenRequest {
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

/// POST /api/v1/admin/shares/:id/token — mint a share-access token signed with
/// the hub key. The owner hands this token out-of-band to a consumer, who
/// presents it to the agent; the agent verifies it offline (the hub is never in
/// the data-path).
pub async fn mint_token(
    State(state): State<Arc<AppState>>,
    extract::Path(share_id): extract::Path<String>,
    Json(req): Json<MintTokenRequest>,
) -> Result<Json<ShareToken>, (StatusCode, String)> {
    // Signing must be configured (same key as presets/app-update, LLD-01).
    let signing = state.signing.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "hub signing key not configured".into(),
    ))?;

    {
        let shares = state.shares.read().await;
        if !shares.contains_key(&share_id) {
            return Err((StatusCode::NOT_FOUND, "share not found".into()));
        }
    }

    let ttl = req.ttl_seconds.unwrap_or(DEFAULT_TOKEN_TTL_SECONDS);
    if ttl == 0 || ttl > MAX_TOKEN_TTL_SECONDS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("ttl_seconds must be 1..={MAX_TOKEN_TTL_SECONDS}"),
        ));
    }
    let exp = now_unix().saturating_add(ttl);

    // The admin hand-out token is read-only, like the self-service link: write
    // scope only ever comes from a write-binding on an invite (LLD-28 п. 2.2).
    let token = sign_share_token(&signing.signing_key, &share_id, SCOPE_READ, exp);
    Ok(Json(token))
}

/// Reject anything that is not a standard-base64 32-byte ed25519 public key.
fn validate_ed25519_pubkey(b64: &str) -> Result<(), (StatusCode, String)> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "agent_pubkey must be valid base64".into(),
            )
        })?;
    if bytes.len() != 32 {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("agent_pubkey must be 32 bytes, got {}", bytes.len()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use tokio::sync::RwLock;
    use tower::ServiceExt;
    use xr_proto::preset::{Invite, InvitePayload};

    use super::*;
    use crate::api::router;
    use crate::signing::SigningContext;
    use crate::state::AppState;

    const LIVE_UNTIL: &str = "2099-01-01T00:00:00+00:00";
    const DEAD_SINCE: &str = "2001-01-01T00:00:00+00:00";

    fn share_rec(id: &str) -> ShareRecord {
        ShareRecord {
            share_id: id.into(),
            name: id.into(),
            owner: String::new(),
            addr: "203.0.113.9".into(),
            addrs: Vec::new(),
            port: 8443,
            agent_pubkey: String::new(),
            created_at: String::new(),
            comment: String::new(),
            via_relay: false,
            writable: false,
        }
    }

    fn invite_rec(token: &str, expires_at: &str, consumed: bool) -> Invite {
        Invite {
            token: token.into(),
            created_at: DEAD_SINCE.into(),
            expires_at: expires_at.into(),
            consumed_at: consumed.then(|| DEAD_SINCE.into()),
            claimed_by_ip: None,
            claim_id: None,
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
            share_ids: Vec::new(),
            write_share_ids: Vec::new(),
        }
    }

    fn state_with(hub: Option<SigningKey>, invites: Vec<Invite>) -> Arc<AppState> {
        let config: crate::config::HubConfig =
            toml::from_str("[server]\n[admin]\nusers = []").unwrap();
        let mut invite_map = HashMap::new();
        for i in invites {
            invite_map.insert(i.token.clone(), i);
        }
        let mut share_map = HashMap::new();
        share_map.insert("s1".to_string(), share_rec("s1"));
        Arc::new(AppState {
            presets: RwLock::new(HashMap::new()),
            invites: RwLock::new(invite_map),
            shares: RwLock::new(share_map),
            exposes: RwLock::new(HashMap::new()),
            sessions: crate::sessions::SessionStore::new(
                std::time::Duration::from_secs(config.admin.session_ttl_secs),
                config.admin.max_sessions_per_user,
            ),
            config,
            signing: hub.map(|key| SigningContext { signing_key: key }),
            preset_gen: tokio::sync::watch::Sender::new(0),
            ready: std::sync::atomic::AtomicBool::new(true),
            web_attempts: Default::default(),
            login_attempts: Default::default(),
        })
    }

    fn grant_blob(hub: &SigningKey, share_id: &str, scope: &str, exp: u64) -> String {
        let token = sign_share_token(hub, share_id, scope, exp);
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&token).unwrap())
    }

    async fn get_index(state: Arc<AppState>, bearer: Option<&str>) -> axum::response::Response {
        let mut builder = Request::builder().uri("/api/v1/shares");
        if let Some(b) = bearer {
            builder = builder.header("authorization", format!("Bearer {b}"));
        }
        router(state)
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn index_body(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    // XR-193: индекс адресов агентов закрыт от анонима, пустой ответ 401,
    // а не пустой список, иначе карту флота продолжали бы читать без токена.
    #[tokio::test]
    async fn anonymous_index_request_is_unauthorized() {
        let state = state_with(None, Vec::new());
        let resp = get_index(state, None).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn live_grant_reads_index() {
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let state = state_with(Some(hub.clone()), Vec::new());
        let exp = now_unix() + 3600;
        let blob = grant_blob(&hub, "s1", SCOPE_READ, exp);
        let resp = get_index(state, Some(&blob)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = index_body(resp).await;
        assert!(body.contains("\"share_id\":\"s1\""), "body: {body}");
    }

    #[tokio::test]
    async fn expired_grant_is_forbidden() {
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let state = state_with(Some(hub.clone()), Vec::new());
        let blob = grant_blob(&hub, "s1", SCOPE_READ, now_unix() - 1);
        let resp = get_index(state, Some(&blob)).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn write_only_grant_is_forbidden() {
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let state = state_with(Some(hub.clone()), Vec::new());
        let exp = now_unix() + 3600;
        let blob = grant_blob(&hub, "s1", xr_proto::share::SCOPE_WRITE, exp);
        let resp = get_index(state, Some(&blob)).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn grant_signed_by_other_key_is_forbidden() {
        let state = state_with(Some(SigningKey::from_bytes(&[42u8; 32])), Vec::new());
        let stranger = SigningKey::from_bytes(&[7u8; 32]);
        let blob = grant_blob(&stranger, "s1", SCOPE_READ, now_unix() + 3600);
        let resp = get_index(state, Some(&blob)).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn live_invite_reads_index() {
        let state = state_with(None, vec![invite_rec("invite-live", LIVE_UNTIL, false)]);
        let resp = get_index(state, Some("invite-live")).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn expired_and_revoked_invites_are_rejected() {
        let state = state_with(
            None,
            vec![
                invite_rec("invite-expired", DEAD_SINCE, false),
                invite_rec("invite-revoked", LIVE_UNTIL, true),
            ],
        );
        let resp = get_index(state.clone(), Some("invite-expired")).await;
        assert_eq!(resp.status(), StatusCode::GONE);
        let resp = get_index(state, Some("invite-revoked")).await;
        assert_eq!(resp.status(), StatusCode::GONE);
    }

    // Грант переживает саму шару: токен подписан хабом и не отозван, а индекс
    // это общий каталог, а не одна шара из него.
    #[tokio::test]
    async fn grant_for_deleted_share_still_reads_index() {
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let state = state_with(Some(hub.clone()), Vec::new());
        let blob = grant_blob(&hub, "gone-share", SCOPE_READ, now_unix() + 3600);
        let resp = get_index(state, Some(&blob)).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
