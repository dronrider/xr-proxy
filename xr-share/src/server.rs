//! HTTP(S) surface of the agent (LLD-19 §3.1, §9.1).
//!
//! v2 serves **many** shares, routed by `share_id`:
//!
//! - `GET /{share_id}/manifest`        — the listing for that share
//! - `GET /{share_id}/file/{*path}`    — its bytes (range-capable)
//! - `GET /manifest` / `GET /file/...` — legacy single-share aliases; the share
//!   is selected by the **token's** `share_id`, so the v1 consumer keeps working
//! - `GET /healthz`                    — unauthenticated liveness
//!
//! A share path is a directory (its tree is served) or a single file (a one-entry
//! manifest). The hub is never contacted: tokens are verified offline against the
//! pinned hub key, and must be bound to the share being accessed.
//!
//! The share table lives behind an `RwLock<Arc<..>>` so the hot-reload task
//! (`main`) can swap in a new set without restarting the server.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Path as AxPath, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ed25519_dalek::VerifyingKey;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use xr_proto::share::{verify_share_token, ShareManifest};

use crate::auth::extract_token;
use crate::manifest::{build_manifest, build_manifest_for_file};
use crate::safepath::resolve_within;

/// One served share: a canonical path that is either a directory tree or a
/// single file.
pub struct ShareRoot {
    pub path: PathBuf,
    pub is_file: bool,
}

impl ShareRoot {
    /// Build this share's manifest (directory walk, or a single-file entry).
    fn manifest(&self) -> anyhow::Result<ShareManifest> {
        if self.is_file {
            build_manifest_for_file(&self.path)
        } else {
            build_manifest(&self.path)
        }
    }

    /// Resolve a manifest-relative request path to a real file inside this share.
    /// For a file share the only valid request is the file's own name.
    fn resolve(&self, rel: &str) -> Option<PathBuf> {
        if self.is_file {
            let name = self.path.file_name()?.to_string_lossy().into_owned();
            // Tolerate a leading slash / "./", reject anything else.
            let asked = rel.trim_start_matches('/').trim_start_matches("./");
            (asked == name).then(|| self.path.clone())
        } else {
            resolve_within(&self.path, rel).ok()
        }
    }
}

pub type SharesMap = HashMap<String, ShareRoot>;

/// Resolve config share entries into a live share table. Each path is
/// canonicalized (fail-fast on a bad path) and classified file vs directory. A
/// bad entry is **skipped with a warning**, never fatal — one broken share must
/// not take the whole agent down (and hot-reload keeps the previous set on a
/// fully unparseable config).
pub fn build_shares(entries: &[crate::config::ShareEntry]) -> SharesMap {
    let mut map = HashMap::new();
    for e in entries {
        match std::fs::canonicalize(&e.path) {
            Ok(canon) => {
                let is_file = canon.is_file();
                if !is_file && !canon.is_dir() {
                    tracing::warn!("share {}: path is neither file nor directory, skipping: {}", e.share_id, e.path);
                    continue;
                }
                map.insert(e.share_id.clone(), ShareRoot { path: canon, is_file });
            }
            Err(err) => {
                tracing::warn!("share {}: path unreadable ({err}), skipping: {}", e.share_id, e.path)
            }
        }
    }
    map
}

/// Runtime state. `shares` is swappable for hot reload; `hub_key` is fixed.
pub struct AgentState {
    pub shares: RwLock<Arc<SharesMap>>,
    pub hub_key: VerifyingKey,
}

impl AgentState {
    /// Cheap snapshot of the current share table (clones the `Arc`, not the map).
    fn snapshot(&self) -> Arc<SharesMap> {
        self.shares.read().expect("shares lock poisoned").clone()
    }
}

pub fn router(state: Arc<AgentState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        // v2: share selected by the URL.
        .route("/{share_id}/manifest", get(get_manifest))
        .route("/{share_id}/file/{*path}", get(serve_file))
        // legacy: share selected by the token (single-share v1 consumers).
        .route("/manifest", get(get_manifest_legacy))
        .route("/file/{*path}", get(serve_file_legacy))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

/// Verify the request's token is valid, unexpired, and bound to `share_id`.
fn check_token(state: &AgentState, share_id: &str, req: &Request) -> Result<(), (StatusCode, &'static str)> {
    let token = extract_token(req.headers(), req.uri())
        .ok_or((StatusCode::UNAUTHORIZED, "missing or malformed token"))?;
    verify_share_token(&token, &state.hub_key, share_id, now_unix())
        .map_err(|_| (StatusCode::FORBIDDEN, "token rejected"))
}

/// The `share_id` the request's token is bound to (for the legacy routes, which
/// carry no id in the URL). `401` if absent/malformed.
fn token_share_id(req: &Request) -> Result<String, (StatusCode, &'static str)> {
    extract_token(req.headers(), req.uri())
        .map(|t| t.share_id)
        .ok_or((StatusCode::UNAUTHORIZED, "missing or malformed token"))
}

// ── v2: share id from the URL ───────────────────────────────────────

async fn get_manifest(
    State(state): State<Arc<AgentState>>,
    AxPath(share_id): AxPath<String>,
    req: Request,
) -> Result<Json<ShareManifest>, (StatusCode, &'static str)> {
    manifest_response(&state, &share_id, &req)
}

async fn serve_file(
    State(state): State<Arc<AgentState>>,
    AxPath((share_id, rel)): AxPath<(String, String)>,
    req: Request,
) -> Response {
    file_response(&state, &share_id, &rel, req).await
}

// ── legacy: share id from the token ─────────────────────────────────

async fn get_manifest_legacy(
    State(state): State<Arc<AgentState>>,
    req: Request,
) -> Result<Json<ShareManifest>, (StatusCode, &'static str)> {
    let share_id = token_share_id(&req)?;
    manifest_response(&state, &share_id, &req)
}

async fn serve_file_legacy(
    State(state): State<Arc<AgentState>>,
    AxPath(rel): AxPath<String>,
    req: Request,
) -> Response {
    let share_id = match token_share_id(&req) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    file_response(&state, &share_id, &rel, req).await
}

// ── shared bodies ───────────────────────────────────────────────────

fn manifest_response(
    state: &AgentState,
    share_id: &str,
    req: &Request,
) -> Result<Json<ShareManifest>, (StatusCode, &'static str)> {
    let shares = state.snapshot();
    let share = shares
        .get(share_id)
        .ok_or((StatusCode::NOT_FOUND, "no such share"))?;
    check_token(state, share_id, req)?;
    let manifest = share.manifest().map_err(|e| {
        tracing::error!("manifest build failed for {share_id}: {e:#}");
        (StatusCode::INTERNAL_SERVER_ERROR, "manifest error")
    })?;
    Ok(Json(manifest))
}

async fn file_response(state: &AgentState, share_id: &str, rel: &str, req: Request) -> Response {
    let shares = state.snapshot();
    let Some(share) = shares.get(share_id) else {
        return (StatusCode::NOT_FOUND, "no such share").into_response();
    };
    if let Err(e) = check_token(state, share_id, &req) {
        return e.into_response();
    }
    let Some(safe) = share.resolve(rel) else {
        // Don't distinguish escape vs bad-component vs wrong-file to the client.
        return (StatusCode::FORBIDDEN, "path rejected").into_response();
    };
    // ServeFile handles Range / Content-Type / Last-Modified / 404.
    match ServeFile::new(&safe).oneshot(req).await {
        Ok(resp) => resp.map(Body::new),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "io error").into_response(),
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use tower::ServiceExt;
    use xr_proto::share::{sign_share_token, ShareToken};

    fn blob(t: &ShareToken) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(t).unwrap())
    }

    fn state_with(shares: SharesMap, key: &SigningKey) -> Arc<AgentState> {
        Arc::new(AgentState {
            shares: RwLock::new(Arc::new(shares)),
            hub_key: key.verifying_key(),
        })
    }

    fn get_with_token(uri: &str, tok: Option<&ShareToken>) -> HttpRequest<Body> {
        let mut b = HttpRequest::get(uri);
        if let Some(t) = tok {
            b = b.header("authorization", format!("Bearer {}", blob(t)));
        }
        b.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn test_router_share_id() {
        // Two directory shares; a token for one must not open the other.
        let key = SigningKey::from_bytes(&[5u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hi").unwrap();
        let canon = dir.path().canonicalize().unwrap();
        let mut shares = SharesMap::new();
        shares.insert("A".into(), ShareRoot { path: canon.clone(), is_file: false });
        shares.insert("B".into(), ShareRoot { path: canon, is_file: false });
        let app = router(state_with(shares, &key));

        let tok_a = sign_share_token(&key, "A", now_unix() + 1000);

        // Right share → 200.
        let r = app.clone().oneshot(get_with_token("/A/manifest", Some(&tok_a))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        // Token for A presented to B's URL → rejected (WrongShare → 403).
        let r = app.clone().oneshot(get_with_token("/B/manifest", Some(&tok_a))).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // Unknown share id → 404.
        let tok_x = sign_share_token(&key, "X", now_unix() + 1000);
        let r = app.clone().oneshot(get_with_token("/X/manifest", Some(&tok_x))).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);

        // No token → 401.
        let r = app.oneshot(get_with_token("/A/manifest", None)).await.unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_share_path_file() {
        // A single-file share: one-entry manifest, the file fetched by its name.
        let key = SigningKey::from_bytes(&[6u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("report.pdf");
        std::fs::write(&file, b"hello").unwrap();
        let mut shares = SharesMap::new();
        shares.insert("F".into(), ShareRoot { path: file.canonicalize().unwrap(), is_file: true });
        let app = router(state_with(shares, &key));
        let tok = sign_share_token(&key, "F", now_unix() + 1000);

        let r = app.clone().oneshot(get_with_token("/F/manifest", Some(&tok))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let m: ShareManifest = serde_json::from_slice(&body).unwrap();
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.entries[0].path, "report.pdf");

        // Fetch the file by its manifest name → contents.
        let r = app.clone().oneshot(get_with_token("/F/file/report.pdf", Some(&tok))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        assert_eq!(&body[..], b"hello");

        // Any other path inside a file share is refused.
        let r = app.oneshot(get_with_token("/F/file/other.txt", Some(&tok))).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn legacy_route_selects_share_by_token() {
        // The v1 `/manifest` alias must resolve the share from the token's id.
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hi").unwrap();
        let mut shares = SharesMap::new();
        shares.insert("only".into(), ShareRoot { path: dir.path().canonicalize().unwrap(), is_file: false });
        let app = router(state_with(shares, &key));

        let tok = sign_share_token(&key, "only", now_unix() + 1000);
        let r = app.clone().oneshot(get_with_token("/manifest", Some(&tok))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        // A token for a share this agent doesn't hold → 404 via the legacy path.
        let bad = sign_share_token(&key, "missing", now_unix() + 1000);
        let r = app.oneshot(get_with_token("/manifest", Some(&bad))).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }
}
