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
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_compression::tokio::bufread::GzipDecoder;
use axum::body::Body;
use axum::extract::{Path as AxPath, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ed25519_dalek::{SigningKey, VerifyingKey};
use http_body_util::BodyExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;
use tower_http::services::ServeFile;
use xr_proto::share::{
    sign_git_head, sign_share_manifest, verify_share_token, ShareManifest,
    MANIFEST_SIGNED_AT_HEADER, MANIFEST_SIG_HEADER, SCOPE_IMPORT, SCOPE_READ, SCOPE_WRITE,
};

use crate::auth::extract_token;
use crate::gitrepo::{GitManager, GitShare};
use crate::import::{self, ImportManager, JobSpec};
use crate::manifest::{
    build_listing, build_listing_for_file, build_manifest, build_manifest_for_file, HashCache,
    UPLOAD_TEMP_PREFIX,
};
use crate::safepath::resolve_within;

/// One served share: a canonical path that is either a directory tree or a
/// single file. A directory share may be `writable` (LLD-28): only then does the
/// agent accept `PUT`/`DELETE`. A file share is never writable.
pub struct ShareRoot {
    pub path: PathBuf,
    pub is_file: bool,
    pub writable: bool,
    /// URL-import jobs are accepted into this share (LLD-29): the local opt-in
    /// on top of `writable`, valid only for a writable directory.
    pub import: bool,
    /// The git contour is on for this share (LLD-33): auto-commit of working
    /// folder edits, smart-HTTP git transport and the signed HEAD. A local
    /// opt-in on top of `writable`, same shape as import.
    pub git: bool,
}

impl ShareRoot {
    /// Build this share's manifest (directory walk, or a single-file entry),
    /// hashing through the shared cache so unchanged files are not re-read.
    fn manifest(&self, cache: &HashCache) -> anyhow::Result<ShareManifest> {
        if self.is_file {
            build_manifest_for_file(&self.path, cache)
        } else {
            build_manifest(&self.path, cache)
        }
    }

    /// Listing without hashing (XR-039): instant even on a cold cache. Hashes are
    /// filled lazily by the warmer (which uses [`manifest`](Self::manifest)).
    fn listing(&self, cache: &HashCache) -> anyhow::Result<ShareManifest> {
        if self.is_file {
            build_listing_for_file(&self.path, cache)
        } else {
            build_listing(&self.path, cache)
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
                // Only a directory can be writable (LLD-28); a `writable` file
                // share in a hand-edited config is served read-only regardless.
                let writable = e.writable && !is_file;
                if e.writable && is_file {
                    tracing::warn!("share {}: writable ignored, a file share is read-only", e.share_id);
                }
                // Import is a kind of write (LLD-29), so it needs writable too.
                let import = e.import && writable;
                if e.import && !writable {
                    tracing::warn!("share {}: import ignored, share is not writable", e.share_id);
                }
                // So is the git contour (LLD-33): co-editing is writing by
                // definition, and its transport gates on share:write.
                let git = e.git && writable;
                if e.git && !writable {
                    tracing::warn!("share {}: git ignored, share is not writable", e.share_id);
                }
                map.insert(e.share_id.clone(), ShareRoot { path: canon, is_file, writable, import, git });
            }
            Err(err) => {
                tracing::warn!("share {}: path unreadable ({err}), skipping: {}", e.share_id, e.path)
            }
        }
    }
    map
}

/// Runtime state. `shares` is swappable for hot reload; `hub_key` is fixed;
/// `hash_cache` is shared by every manifest build (and the background warmer).
/// `identity` signs served manifests (XR-046); `None` for a legacy config
/// without a key, then manifests go out unsigned and a pinning consumer
/// rejects them.
pub struct AgentState {
    pub shares: RwLock<Arc<SharesMap>>,
    pub hub_key: VerifyingKey,
    pub hash_cache: Arc<HashCache>,
    pub identity: Option<SigningKey>,
    /// Upload size cap in mebibytes (LLD-28), `None` for no limit. Applies to the
    /// write path only; read routes are unaffected.
    pub max_file_mb: Option<u64>,
    /// URL-import job registry + plugin config (LLD-29). Always present; with no
    /// `[import]` block it just answers that import is off.
    pub import: Arc<ImportManager>,
    /// Live repositories of git-enabled shares (LLD-33): handles for the
    /// transport, refilled by the same config reload that swaps `shares`.
    pub git: Arc<GitManager>,
    /// Публикации локальных сервисов (LLD-38 п. 2.1). Меняется горячо тем же
    /// перечитыванием конфига, что и шары: `expose add` не должен требовать
    /// перезапуска агента.
    pub expose: RwLock<Arc<Vec<crate::config::ExposeEntry>>>,
}

impl AgentState {
    /// Cheap snapshot of the current share table (clones the `Arc`, not the map).
    fn snapshot(&self) -> Arc<SharesMap> {
        self.shares.read().expect("shares lock poisoned").clone()
    }

    /// Гейт публикаций на текущем состоянии: пришпиленный ключ хаба, свой
    /// identity и живой список `[[expose]]`. Нужен только обслуживанию
    /// реверс-стримов: харнесс собирает свой гейт прямо из конфига.
    #[cfg(feature = "relay")]
    pub fn expose_gate(&self) -> crate::expose::ExposeGate {
        use base64::Engine as _;
        crate::expose::ExposeGate {
            hub_key: self.hub_key,
            agent_pubkey: self.identity.as_ref().map(|k| {
                base64::engine::general_purpose::STANDARD.encode(k.verifying_key().as_bytes())
            }),
            publications: self.expose.read().expect("expose lock poisoned").clone(),
        }
    }

    /// Build every share's manifest to prime the hash cache, so a later
    /// `/manifest` request is fast even for a large share. Errors are ignored: a
    /// share that fails to build just stays cold. Blocking — call it off the
    /// async executor (a `spawn_blocking` warmer in `main`).
    pub fn warm_manifests(&self) {
        for root in self.snapshot().values() {
            let _ = root.manifest(&self.hash_cache);
        }
    }
}

pub fn router(state: Arc<AgentState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        // v2: share selected by the URL. The file route also accepts writes
        // (LLD-28); PUT/DELETE are v2-only, no legacy alias.
        .route("/{share_id}/manifest", get(get_manifest))
        .route(
            "/{share_id}/file/{*path}",
            get(serve_file).put(put_file).delete(delete_file),
        )
        // URL-import jobs (LLD-29), v2-only: start, poll, cancel.
        .route("/{share_id}/import", axum::routing::post(start_import))
        .route(
            "/{share_id}/import/{job_id}",
            get(import_status).delete(import_cancel),
        )
        // git contour (LLD-33): smart-HTTP transport plus the signed head.
        // The whole contour, fetch included, gates on share:write.
        .route("/{share_id}/git/info/refs", get(git_info_refs))
        .route("/{share_id}/git/git-upload-pack", axum::routing::post(git_upload_pack))
        .route("/{share_id}/git/git-receive-pack", axum::routing::post(git_receive_pack))
        .route("/{share_id}/git/head", get(git_head))
        // history for the web page (LLD-33 п. 2.8), the same write gate as
        // the rest of the contour.
        .route("/{share_id}/git/log", get(git_log))
        .route("/{share_id}/git/diff", get(git_diff))
        // the share's embedded web page (LLD-33 п. 2.8): read view for
        // readers, history and editing for write holders.
        .route("/{share_id}/web", get(share_web))
        // legacy: share selected by the token (single-share v1 consumers).
        .route("/manifest", get(get_manifest_legacy))
        .route("/file/{*path}", get(serve_file_legacy))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

/// Verify the request's token is valid, unexpired, bound to `share_id`, and
/// carries `required_scope`. A missing/malformed token is `401`; a token that is
/// present but rejected (wrong share, expired, bad signature, or lacking the
/// scope) is `403` (LLD-28 п. 2.3).
fn check_token(
    state: &AgentState,
    share_id: &str,
    required_scope: &str,
    req: &Request,
) -> Result<(), (StatusCode, &'static str)> {
    check_token_parts(state, share_id, required_scope, req.headers(), req.uri())
}

/// [`check_token`] for a handler that consumed the request body (the import
/// routes take a JSON extractor, so only parts remain).
fn check_token_parts(
    state: &AgentState,
    share_id: &str,
    required_scope: &str,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> Result<(), (StatusCode, &'static str)> {
    let token = extract_token(headers, uri)
        .ok_or((StatusCode::UNAUTHORIZED, "missing or malformed token"))?;
    verify_share_token(&token, &state.hub_key, share_id, required_scope, now_unix())
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
) -> Result<Response, (StatusCode, &'static str)> {
    manifest_response(state, share_id, req).await
}

async fn serve_file(
    State(state): State<Arc<AgentState>>,
    AxPath((share_id, rel)): AxPath<(String, String)>,
    req: Request,
) -> Response {
    file_response(&state, &share_id, &rel, req).await
}

async fn put_file(
    State(state): State<Arc<AgentState>>,
    AxPath((share_id, rel)): AxPath<(String, String)>,
    req: Request,
) -> Response {
    match handle_put(&state, &share_id, &rel, req).await {
        Ok(resp) => resp,
        Err(e) => e.into_response(),
    }
}

async fn delete_file(
    State(state): State<Arc<AgentState>>,
    AxPath((share_id, rel)): AxPath<(String, String)>,
    req: Request,
) -> Response {
    match handle_delete(&state, &share_id, &rel, req).await {
        Ok(resp) => resp,
        Err(e) => e.into_response(),
    }
}

// ── legacy: share id from the token ─────────────────────────────────

async fn get_manifest_legacy(
    State(state): State<Arc<AgentState>>,
    req: Request,
) -> Result<Response, (StatusCode, &'static str)> {
    let share_id = token_share_id(&req)?;
    manifest_response(state, share_id, req).await
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

async fn manifest_response(
    state: Arc<AgentState>,
    share_id: String,
    req: Request,
) -> Result<Response, (StatusCode, &'static str)> {
    if !state.snapshot().contains_key(&share_id) {
        return Err((StatusCode::NOT_FOUND, "no such share"));
    }
    check_token(&state, &share_id, SCOPE_READ, &req)?;
    // Listing never hashes (XR-039): it returns metadata plus any hash already in
    // the cache, so it is instant even on a cold cache of a huge share. The
    // warmer fills hashes in the background. Still off the async runtime because
    // the directory walk/stat is blocking I/O (a slow/network drive must not
    // stall other requests).
    let st = state.clone();
    let sid = share_id.clone();
    let built = tokio::task::spawn_blocking(move || -> anyhow::Result<ShareManifest> {
        let shares = st.snapshot();
        let share = shares
            .get(&sid)
            .ok_or_else(|| anyhow::anyhow!("share removed during build"))?;
        share.listing(&st.hash_cache)
    })
    .await;
    match built {
        Ok(Ok(manifest)) => Ok(signed_manifest_response(&state, &share_id, &manifest)),
        Ok(Err(e)) => {
            tracing::error!("manifest build failed: {e:#}");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "manifest error"))
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "manifest task failed")),
    }
}

/// Serialize the manifest **once** and sign those exact bytes (XR-046): the
/// signature and its timestamp travel as response headers, the body stays the
/// plain manifest JSON, so a pre-signing consumer keeps working while a pinning
/// one verifies the bytes it actually received. Re-serializing on the consumer
/// is never needed, hence no canonicalization to drift.
fn signed_manifest_response(state: &AgentState, share_id: &str, manifest: &ShareManifest) -> Response {
    let body = match serde_json::to_vec(manifest) {
        Ok(b) => b,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "manifest encode").into_response(),
    };
    let mut resp = Response::builder().header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = &state.identity {
        let signed_at = now_unix();
        let sig = sign_share_manifest(key, share_id, signed_at, &body);
        resp = resp
            .header(MANIFEST_SIG_HEADER, sig)
            .header(MANIFEST_SIGNED_AT_HEADER, signed_at.to_string());
    }
    resp.body(Body::from(body))
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "manifest response").into_response())
}

async fn file_response(state: &AgentState, share_id: &str, rel: &str, req: Request) -> Response {
    let shares = state.snapshot();
    let Some(share) = shares.get(share_id) else {
        return (StatusCode::NOT_FOUND, "no such share").into_response();
    };
    if let Err(e) = check_token(state, share_id, SCOPE_READ, &req) {
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

// -- write path (LLD-28) --------------------------------------------

/// Accept an upload into a writable directory share. Order of gates (LLD-28
/// п. 2.3): share exists (`404`), writable in config (`403`), token with
/// `share:write` (`401`/`403`), safepath (`403`), then target-state and
/// preconditions. The body streams into a reserved `.xr-part-<rand>` temp next
/// to the target, is hashed on the fly, fsync'd and atomically renamed over the
/// target on success; the temp is removed on any failure. `201` for a new file,
/// `204` for an overwrite.
async fn handle_put(
    state: &Arc<AgentState>,
    share_id: &str,
    rel: &str,
    req: Request,
) -> Result<Response, (StatusCode, &'static str)> {
    let (target, root) = {
        let shares = state.snapshot();
        let share = shares
            .get(share_id)
            .ok_or((StatusCode::NOT_FOUND, "no such share"))?;
        if !share.writable {
            return Err((StatusCode::FORBIDDEN, "share is read-only"));
        }
        check_token(state, share_id, SCOPE_WRITE, &req)?;
        let target = resolve_within(&share.path, rel)
            .map_err(|_| (StatusCode::FORBIDDEN, "path rejected"))?;
        (target, share.path.clone())
    };

    if target.is_dir() {
        return Err((StatusCode::CONFLICT, "target is a directory"));
    }
    let existed = target.is_file();

    // Cheapest gate first: a declared over-cap body is refused before we hash the
    // current target for a precondition (no point reading a large cold file for a
    // request that is doomed anyway).
    if let (Some(cap_mb), Some(len)) = (state.max_file_mb, content_length(req.headers())) {
        if len > cap_mb.saturating_mul(1024 * 1024) {
            return Err((StatusCode::PAYLOAD_TOO_LARGE, "file too large"));
        }
    }
    // Optimistic-concurrency preconditions (LLD-28 п. 3.7). All header-based, so
    // done before the body is consumed; current-target hashing runs off the async
    // worker like the read path, so a large cold file does not stall the runtime.
    check_put_preconditions(state, &target, existed, req.headers()).await?;
    let expected_sha = header_str(req.headers(), "x-xr-sha256").map(|s| s.trim().to_string());

    let parent = target
        .parent()
        .ok_or((StatusCode::FORBIDDEN, "path rejected"))?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "mkdir failed"))?;

    let tmp = parent.join(format!("{UPLOAD_TEMP_PREFIX}{:016x}", rand::random::<u64>()));
    let cap_bytes = state.max_file_mb.map(|m| m.saturating_mul(1024 * 1024));
    let (sha, size) = stream_to_temp(req.into_body(), &tmp, cap_bytes).await?;

    // Optional integrity check before the file is published.
    if let Some(want) = &expected_sha {
        if !sha.eq_ignore_ascii_case(want) {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err((StatusCode::UNPROCESSABLE_ENTITY, "sha256 mismatch"));
        }
    }

    if rename_replace(&tmp, &target).await.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "rename failed"));
    }

    // Seed the cache so the manifest serves the fresh file already hashed.
    if let Ok(meta) = std::fs::metadata(&target) {
        state.hash_cache.seed(&target, meta.len(), mtime_secs(&meta), sha);
    }
    // Bytes uploaded by hand did not come from whatever page used to live at
    // this path (XR-255), so the old origin goes away with the old content.
    if let Some(key) = crate::meta::rel_key(&root, &target) {
        crate::meta::forget(&root, &key);
    }

    let status = if existed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CREATED
    };
    tracing::info!("PUT share={share_id} rel={rel} size={size} -> {}", status.as_u16());
    Ok(status.into_response())
}

/// Delete a file from a writable directory share. Same gate order as
/// [`handle_put`], then `409` for a directory, `404` for a missing file, and an
/// optional `If-Match` precondition (`412`) before the removal (LLD-28 п. 2.3).
async fn handle_delete(
    state: &Arc<AgentState>,
    share_id: &str,
    rel: &str,
    req: Request,
) -> Result<Response, (StatusCode, &'static str)> {
    let (target, root) = {
        let shares = state.snapshot();
        let share = shares
            .get(share_id)
            .ok_or((StatusCode::NOT_FOUND, "no such share"))?;
        if !share.writable {
            return Err((StatusCode::FORBIDDEN, "share is read-only"));
        }
        check_token(state, share_id, SCOPE_WRITE, &req)?;
        let target = resolve_within(&share.path, rel)
            .map_err(|_| (StatusCode::FORBIDDEN, "path rejected"))?;
        (target, share.path.clone())
    };

    if target.is_dir() {
        return Err((StatusCode::CONFLICT, "target is a directory"));
    }
    if !target.is_file() {
        return Err((StatusCode::NOT_FOUND, "no such file"));
    }
    // If-Match against the current content, if the client asked (last-write-wins
    // by default). The target is known to exist here.
    if let Some(want) = if_match_hash(req.headers()) {
        let current = current_hash_blocking(state, &target).await?;
        if !current.eq_ignore_ascii_case(&want) {
            return Err((StatusCode::PRECONDITION_FAILED, "version mismatch"));
        }
    }

    tokio::fs::remove_file(&target)
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "delete failed"))?;
    // The origin goes with the file: a later upload under the same name is a
    // different file and must not inherit somebody else's page (XR-255).
    if let Some(key) = crate::meta::rel_key(&root, &target) {
        crate::meta::forget(&root, &key);
    }
    tracing::info!("DELETE share={share_id} rel={rel}");
    Ok(StatusCode::NO_CONTENT.into_response())
}

// -- import path (LLD-29) -------------------------------------------

#[derive(Deserialize)]
struct ImportReq {
    url: String,
    /// Destination directory inside the share, "" (the default) is the root.
    #[serde(default)]
    dest: String,
    /// Wanted frame height; clamped to the plugin's `max_height`.
    #[serde(default)]
    height: Option<u32>,
}

/// Gates shared by all three import routes (LLD-29 п. 2.6, steps 1-3): the
/// share exists (`404`), is writable + import-enabled with at least one plugin
/// configured (`403`), and the token carries `share:import` (`401`/`403`).
/// Returns the share's canonical root and the config snapshot the gate judged,
/// so the POST handler routes and limits against the same version.
fn import_gates(
    state: &AgentState,
    share_id: &str,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> Result<(PathBuf, Arc<crate::config::ImportConfig>), (StatusCode, &'static str)> {
    let shares = state.snapshot();
    let share = shares
        .get(share_id)
        .ok_or((StatusCode::NOT_FOUND, "no such share"))?;
    let import_cfg = state.import.config().filter(|c| !c.plugins.is_empty());
    let (true, true, Some(cfg)) = (share.writable, share.import, import_cfg) else {
        return Err((StatusCode::FORBIDDEN, "import is off for this share"));
    };
    check_token_parts(state, share_id, SCOPE_IMPORT, headers, uri)?;
    Ok((share.path.clone(), cfg))
}

/// `POST /{share_id}/import`: start a job (LLD-29 п. 2.5). After the shared
/// gates: safepath the destination (`403`), the URL gate (`400`), plugin
/// routing (`422`) and the height clamp (`400`), then enqueue (`429` full).
async fn start_import(
    State(state): State<Arc<AgentState>>,
    AxPath(share_id): AxPath<String>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    Json(req): Json<ImportReq>,
) -> Response {
    let (root, cfg) = match import_gates(&state, &share_id, &headers, &uri) {
        Ok(ok) => ok,
        Err(e) => return e.into_response(),
    };
    let Ok(dest_abs) = resolve_within(&root, &req.dest) else {
        return (StatusCode::FORBIDDEN, "path rejected").into_response();
    };
    if dest_abs.is_file() {
        return (StatusCode::CONFLICT, "dest is a file").into_response();
    }
    // Normalized share-relative destination for reporting published paths.
    let dest_rel = dest_abs
        .strip_prefix(&root)
        .map(|p| p.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
        .unwrap_or_default();

    let host = match import::check_url(&req.url).await {
        Ok(host) => host,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let Some(plugin) = import::route_plugin(&cfg.plugins, &host) else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "нет плагина под этот URL").into_response();
    };
    let height = match import::effective_height(req.height, plugin) {
        Ok(h) => h,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };

    let spec = JobSpec {
        share_id: share_id.clone(),
        share_root: root,
        dest_rel,
        url: req.url.trim().to_string(),
        height,
        plugin: plugin.clone(),
        timeout: std::time::Duration::from_secs(cfg.timeout_min.saturating_mul(60)),
        max_total_bytes: cfg.max_total_mb.map(|m| m.saturating_mul(1024 * 1024)),
        max_file_bytes: state.max_file_mb.map(|m| m.saturating_mul(1024 * 1024)),
        sandbox: cfg.sandbox.clone(),
    };
    match state.import.enqueue(spec) {
        Some(job_id) => {
            tracing::info!("import share={share_id} host={host} job={job_id}");
            (StatusCode::ACCEPTED, Json(serde_json::json!({ "job_id": job_id }))).into_response()
        }
        None => (StatusCode::TOO_MANY_REQUESTS, "import queue is full").into_response(),
    }
}

/// `GET /{share_id}/import/{job_id}`: poll a job. A `404` may equally mean an
/// unknown id or a table lost to a restart (LLD-29 п. 3.7).
async fn import_status(
    State(state): State<Arc<AgentState>>,
    AxPath((share_id, job_id)): AxPath<(String, String)>,
    req: Request,
) -> Response {
    if let Err(e) = import_gates(&state, &share_id, req.headers(), req.uri()) {
        return e.into_response();
    }
    match state.import.status(&share_id, &job_id) {
        Some(dto) => Json(dto).into_response(),
        None => (StatusCode::NOT_FOUND, "no such job").into_response(),
    }
}

/// `DELETE /{share_id}/import/{job_id}`: cancel. SIGKILL the plugin's process
/// group, forget the job (LLD-29 п. 2.5).
async fn import_cancel(
    State(state): State<Arc<AgentState>>,
    AxPath((share_id, job_id)): AxPath<(String, String)>,
    req: Request,
) -> Response {
    if let Err(e) = import_gates(&state, &share_id, req.headers(), req.uri()) {
        return e.into_response();
    }
    if state.import.cancel(&share_id, &job_id) {
        tracing::info!("import share={share_id} job={job_id} cancelled");
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "no such job").into_response()
    }
}

// -- git contour (LLD-33) -------------------------------------------

/// Whole-exchange deadline for one git RPC (fetch or push): a wedged client
/// must not pin the receive lock or a git process forever.
const GIT_RPC_DEADLINE: Duration = Duration::from_secs(5 * 60);

/// Which smart-HTTP service a git route talks to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GitRpc {
    UploadPack,
    ReceivePack,
}

impl GitRpc {
    /// The git subcommand behind the service name.
    fn sub(&self) -> &'static str {
        match self {
            GitRpc::UploadPack => "upload-pack",
            GitRpc::ReceivePack => "receive-pack",
        }
    }

    fn service(&self) -> &'static str {
        match self {
            GitRpc::UploadPack => "git-upload-pack",
            GitRpc::ReceivePack => "git-receive-pack",
        }
    }
}

/// The gate ladder every git route climbs (LLD-33 п. 2.3): the share must
/// exist (`404`), have the contour on (`403`), be writable (`403`), and the
/// token must carry `share:write` (`401`/`403`). Fetch lives under
/// `share:write` too: the repository is the owner's private history, not a
/// published binding. Resolves to the live repository handle.
fn git_gates(
    state: &AgentState,
    share_id: &str,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> Result<Arc<GitShare>, (StatusCode, &'static str)> {
    let shares = state.snapshot();
    let share = shares.get(share_id).ok_or((StatusCode::NOT_FOUND, "no such share"))?;
    if !share.git {
        return Err((StatusCode::FORBIDDEN, "git is off for this share"));
    }
    if !share.writable {
        return Err((StatusCode::FORBIDDEN, "share is not writable"));
    }
    check_token_parts(state, share_id, SCOPE_WRITE, headers, uri)?;
    state
        .git
        .get(share_id)
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "git repository unavailable"))
}

/// One value of a query parameter, if present. The git routes only take hex
/// shas and digits, so no percent-decoding is needed.
fn query_param(uri: &axum::http::Uri, name: &str) -> Option<String> {
    uri.query()?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

/// One pkt-line record of the smart-HTTP protocol: four hex digits naming the
/// payload length (itself included), then the payload.
fn pkt_line(payload: &[u8]) -> Vec<u8> {
    let len = (payload.len() + 4) as u32;
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.extend_from_slice(format!("{len:04x}").as_bytes());
    out.extend_from_slice(payload);
    out
}

/// `GET /{share_id}/git/info/refs?service=...`: the smart-HTTP ref
/// advertisement. Without `service` this is the dumb-protocol probe; the
/// contour only speaks smart HTTP, so that answers `400` naming the parameter.
async fn git_info_refs(
    State(state): State<Arc<AgentState>>,
    AxPath(share_id): AxPath<String>,
    req: Request,
) -> Response {
    let uri = req.uri().clone();
    let rpc = match query_param(&uri, "service").as_deref() {
        Some("git-upload-pack") => GitRpc::UploadPack,
        Some("git-receive-pack") => GitRpc::ReceivePack,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "smart HTTP only: pass ?service=git-upload-pack|git-receive-pack",
            )
                .into_response()
        }
    };
    let share = match git_gates(&state, &share_id, req.headers(), &uri) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let out = tokio::process::Command::new("git")
        .arg(rpc.sub())
        .arg("--stateless-rpc")
        .arg("--advertise-refs")
        .arg(&share.git_dir)
        .output()
        .await;
    let out = match out {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::error!(
                "git {} advertise-refs failed: {}",
                rpc.sub(),
                String::from_utf8_lossy(&o.stderr)
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, "git advertise failed").into_response();
        }
        Err(e) => {
            tracing::error!("git {} spawn failed: {e}", rpc.sub());
            return (StatusCode::INTERNAL_SERVER_ERROR, "git not available").into_response();
        }
    };
    // Protocol preamble: the service announcement as one pkt-line, a flush,
    // then the raw advertisement.
    let mut body = pkt_line(format!("# service={}\n", rpc.service()).as_bytes());
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&out.stdout);
    (
        [(header::CONTENT_TYPE, format!("application/x-{}-advertisement", rpc.service()))],
        body,
    )
        .into_response()
}

async fn git_upload_pack(
    State(state): State<Arc<AgentState>>,
    AxPath(share_id): AxPath<String>,
    req: Request,
) -> Response {
    git_rpc(state, share_id, req, GitRpc::UploadPack).await
}

async fn git_receive_pack(
    State(state): State<Arc<AgentState>>,
    AxPath(share_id): AxPath<String>,
    req: Request,
) -> Response {
    git_rpc(state, share_id, req, GitRpc::ReceivePack).await
}

/// `POST /{share_id}/git/git-{upload,receive}-pack`: the request body streams
/// into git's stdin, git's stdout streams back as the response, one process
/// exchange per request (`--stateless-rpc`, no connection reuse). A push is
/// serialized against the auto-commit loop by the repository's op lock, held
/// for the whole exchange and therefore moved into the finisher task that
/// outlives the handler.
async fn git_rpc(state: Arc<AgentState>, share_id: String, req: Request, rpc: GitRpc) -> Response {
    let (parts, body) = req.into_parts();
    let share = match git_gates(&state, &share_id, &parts.headers, &parts.uri) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let op_guard = if rpc == GitRpc::ReceivePack {
        Some(share.op_lock.clone().lock_owned().await)
    } else {
        None
    };
    // A client may gzip the request body (git does for large pushes when the
    // server offers it); harmless to accept unconditionally.
    let gzipped = header_str(&parts.headers, "content-encoding")
        .map(|v| v.eq_ignore_ascii_case("gzip"))
        .unwrap_or(false);
    let mut command = tokio::process::Command::new("git");
    command
        .arg(rpc.sub())
        .arg("--stateless-rpc")
        .arg(&share.git_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("git {} spawn failed: {e}", rpc.sub());
            return (StatusCode::INTERNAL_SERVER_ERROR, "git not available").into_response();
        }
    };
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");

    // Request body -> git stdin. A client that hangs up mid-body becomes a
    // broken pipe here and an EOF for git (the stdin handle drops with the
    // task), so the exchange still unwinds through the finisher.
    let reader = BodyReader::new(body);
    let mut input: Box<dyn tokio::io::AsyncRead + Unpin + Send> = if gzipped {
        Box::new(GzipDecoder::new(tokio::io::BufReader::new(reader)))
    } else {
        Box::new(reader)
    };
    let feed = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut input, &mut stdin).await;
    });

    // Keep the tail of git's stderr for the failure log.
    let err_tail = tokio::spawn(async move {
        let mut tail: Vec<u8> = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    tail.extend_from_slice(&buf[..n]);
                    let over = tail.len().saturating_sub(4096);
                    if over > 0 {
                        tail.drain(..over);
                    }
                }
            }
        }
        String::from_utf8_lossy(&tail).into_owned()
    });

    // First byte of git's answer under the deadline: a process that dies
    // before saying anything must surface as a 5xx with its stderr, not as an
    // empty 200 the client cannot diagnose.
    let mut probe = [0u8; 8192];
    let first = match tokio::time::timeout(GIT_RPC_DEADLINE, stdout.read(&mut probe)).await {
        Err(_) => {
            kill_git_tree(&mut child);
            return (StatusCode::GATEWAY_TIMEOUT, "git rpc deadline").into_response();
        }
        Ok(Err(e)) => {
            kill_git_tree(&mut child);
            tracing::error!("git {} stdout: {e}", rpc.sub());
            return (StatusCode::INTERNAL_SERVER_ERROR, "git rpc failed").into_response();
        }
        Ok(Ok(0)) => {
            let _ = feed.await;
            let tail = err_tail.await.unwrap_or_default();
            tracing::error!(
                "git {} died before answering ({:?}): {}",
                rpc.sub(),
                child.wait().await,
                tail
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, "git rpc failed").into_response();
        }
        Ok(Ok(n)) => Vec::from(&probe[..n]),
    };

    let started = std::time::Instant::now();
    // The rest of stdout streams as the response body; the Cursor replays the
    // probe read above ahead of the pipe.
    let response = Body::new(ReaderBody::new(std::io::Cursor::new(first).chain(stdout)));

    let share_fin = share.clone();
    tokio::spawn(async move {
        // Hold the receive lock until the exchange ends, not just the handler.
        let _op_guard = op_guard;
        let _ = feed.await;
        let left = GIT_RPC_DEADLINE.saturating_sub(started.elapsed());
        let status = match tokio::time::timeout(left, child.wait()).await {
            Ok(s) => s,
            Err(_) => {
                tracing::warn!("git {}: exchange overran the deadline, killed", rpc.sub());
                kill_git_tree(&mut child);
                return;
            }
        };
        let tail = err_tail.await.unwrap_or_default();
        if let Ok(st) = &status {
            if !st.success() {
                tracing::warn!("git {} exited with {st}: {}", rpc.sub(), tail);
            }
        }
        // Refs may already have moved when the report itself failed to go
        // out, so the head channel catches up on any receive outcome.
        if rpc == GitRpc::ReceivePack {
            if let Err(e) = share_fin.clone().after_receive().await {
                tracing::error!("share {share_id}: post-receive update failed: {e:#}");
            }
        }
    });

    (
        [(header::CONTENT_TYPE, format!("application/x-{}-result", rpc.service()))],
        response,
    )
        .into_response()
}

/// `GET /{share_id}/git/head?since=<sha>&wait=<sec>`: the current `main`
/// head, signed by the agent identity. `since` equal to the current head
/// parks the request for up to `wait` seconds (capped at a minute) waiting
/// for the next commit or push, so a watcher polls with second-sharp latency
/// at a request per minute. An unborn `main` reports an empty string.
async fn git_head(
    State(state): State<Arc<AgentState>>,
    AxPath(share_id): AxPath<String>,
    req: Request,
) -> Response {
    let uri = req.uri().clone();
    let share = match git_gates(&state, &share_id, req.headers(), &uri) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let mut rx = share.head_tx.subscribe();
    let since = query_param(&uri, "since");
    let wait = query_param(&uri, "wait")
        .and_then(|w| w.parse::<u64>().ok())
        .unwrap_or(0)
        .min(60);
    let head = if wait > 0 && since.as_deref() == Some(rx.borrow().as_str()) {
        match tokio::time::timeout(Duration::from_secs(wait), rx.changed()).await {
            Ok(Ok(())) => rx.borrow().clone(),
            // Timeout or the sender gone: the current state is the answer.
            _ => rx.borrow().clone(),
        }
    } else {
        rx.borrow().clone()
    };
    let signed_at = now_unix();
    let sig = state
        .identity
        .as_ref()
        .map(|k| sign_git_head(k, &share_id, signed_at, &head));
    Json(serde_json::json!({ "head": head, "signed_at": signed_at, "sig": sig })).into_response()
}

/// Commits per `git/log` response (LLD-33 п. 2.8), one JSON object each.
#[derive(serde::Serialize)]
struct GitLogRow {
    sha: String,
    author: String,
    date: String,
    subject: String,
}

/// Default and maximum number of commits `git/log` reports.
const GIT_LOG_LIMIT_DEFAULT: u32 = 50;
const GIT_LOG_LIMIT_MAX: u32 = 500;
/// A diff body past this size is cut with a marker line: the page renders it
/// as text, and a pathological rename history must not balloon the response.
const GIT_DIFF_MAX_BYTES: usize = 1024 * 1024;

/// `GET /{share_id}/git/log?path=&limit=`: `main`'s commits as a JSON array,
/// newest first, optionally narrowed to one share-relative path. The page
/// (LLD-33 п. 2.8) is the only consumer; the gate ladder is the transport's,
/// history being a co-author's tool. The path filter is validated lexically
/// but need not exist on disk: history remembers deleted files too.
async fn git_log(
    State(state): State<Arc<AgentState>>,
    AxPath(share_id): AxPath<String>,
    req: Request,
) -> Response {
    let uri = req.uri().clone();
    let share = match git_gates(&state, &share_id, req.headers(), &uri) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let path = match history_path_arg(&share, &uri) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let limit = query_param(&uri, "limit")
        .and_then(|l| l.parse::<u32>().ok())
        .unwrap_or(GIT_LOG_LIMIT_DEFAULT)
        .min(GIT_LOG_LIMIT_MAX);
    let mut args: Vec<String> = vec![
        "log".into(),
        format!("-n{limit}"),
        // Unit separators keep one `git` call parseable without quoting: \x1e
        // between commits, \x1f between fields. A subject cannot carry either.
        "--format=%H%x1f%an%x1f%aI%x1f%s%x1e".into(),
    ];
    if let Some(p) = &path {
        args.push("--".into());
        args.push(p.clone());
    }
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&share.git_dir)
        .args(&args)
        .output()
        .await;
    let out = match out {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::error!(
                "share {share_id}: git log failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return (StatusCode::INTERNAL_SERVER_ERROR, "git log failed").into_response();
        }
        Err(e) => {
            tracing::error!("share {share_id}: git log spawn failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "git not available").into_response();
        }
    };
    let rows: Vec<GitLogRow> = String::from_utf8_lossy(&out.stdout)
        .split('\x1e')
        .filter(|rec| !rec.trim().is_empty())
        .filter_map(|rec| {
            let mut f = rec.trim().split('\x1f');
            Some(GitLogRow {
                sha: f.next()?.to_string(),
                author: f.next()?.to_string(),
                date: f.next()?.to_string(),
                subject: f.next()?.to_string(),
            })
        })
        .collect();
    Json(rows).into_response()
}

/// `GET /{share_id}/git/diff?from=&to=&path=`: the unified diff between two
/// commits, as plain text for `<pre>` rendering. Both ends must be hex shas
/// (the page passes adjacent commits from `git/log`); a bad sha is `400`, not
/// a git error. Read-only, so no share op lock.
async fn git_diff(
    State(state): State<Arc<AgentState>>,
    AxPath(share_id): AxPath<String>,
    req: Request,
) -> Response {
    let uri = req.uri().clone();
    let share = match git_gates(&state, &share_id, req.headers(), &uri) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let bad_arg = |name: &str| {
        (
            StatusCode::BAD_REQUEST,
            format!("{name} must be a hex commit sha"),
        )
            .into_response()
    };
    let Some(from) = query_param(&uri, "from").filter(|v| commit_sha_arg(v)) else {
        return bad_arg("from");
    };
    let Some(to) = query_param(&uri, "to").filter(|v| commit_sha_arg(v)) else {
        return bad_arg("to");
    };
    let path = match history_path_arg(&share, &uri) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let mut args: Vec<String> = vec!["diff".into(), "--no-color".into(), from, to];
    if let Some(p) = &path {
        args.push("--".into());
        args.push(p.clone());
    }
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&share.git_dir)
        .args(&args)
        .output()
        .await;
    let out = match out {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            // A sha that names nothing in this repository is a client error.
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::warn!("share {share_id}: git diff rejected: {stderr}");
            return (StatusCode::BAD_REQUEST, "unknown commit").into_response();
        }
        Err(e) => {
            tracing::error!("share {share_id}: git diff spawn failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "git not available").into_response();
        }
    };
    let mut body = out.stdout;
    if body.len() > GIT_DIFF_MAX_BYTES {
        body.truncate(GIT_DIFF_MAX_BYTES);
        body.extend_from_slice(b"\n[diff truncated]\n");
    }
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

/// A `from`/`to` value fit to hand git on the command line: 4-40 hex digits
/// (git's own abbreviation window), nothing else.
fn commit_sha_arg(v: &str) -> bool {
    (4..=40).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The validated `path` query parameter of `git/log` and `git/diff`: a
/// share-relative path (`resolve_within`'s lexical layer rejects traversal and
/// the reserved namespace), empty meaning all of history. A leading `:` is
/// git pathspec magic and gets the same `400`.
fn history_path_arg(share: &GitShare, uri: &axum::http::Uri) -> Result<Option<String>, Response> {
    let Some(raw) = query_param(uri, "path").filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    if raw.starts_with(':') {
        return Err((StatusCode::BAD_REQUEST, "bad path").into_response());
    }
    match resolve_within(&share.worktree, &raw) {
        // The validated form need not exist; only containment matters, so the
        // original string (git's own `/`-separated notation) goes to git.
        Ok(_) => Ok(Some(raw)),
        Err(_) => Err((StatusCode::BAD_REQUEST, "bad path").into_response()),
    }
}

/// `GET /{share_id}/web`: the share's one-page web view, embedded in the
/// binary at build time (LLD-33 п. 2.8). No CDN, no external requests: the
/// page fetches the manifest, files and history from this same origin with
/// the token from its own URL. Gated like a read: a read token shows the
/// files, a write token additionally unlocks history and editing (the page
/// probes `git/log` and hides both on a `403`, so a share without the git
/// contour still gets a working read view).
async fn share_web(
    State(state): State<Arc<AgentState>>,
    AxPath(share_id): AxPath<String>,
    req: Request,
) -> Response {
    if !state.snapshot().contains_key(&share_id) {
        return (StatusCode::NOT_FOUND, "no such share").into_response();
    }
    if let Err(e) = check_token(&state, &share_id, SCOPE_READ, &req) {
        return e.into_response();
    }
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        crate::web_page::SHARE_WEB_HTML,
    )
        .into_response()
}

/// Request body as an `AsyncRead` for a git process's stdin: data frames yield
/// their bytes, a mid-body error (a client that hung up) surfaces as an io
/// error so the copy stops, and the end of the body reads as EOF.
struct BodyReader {
    body: std::pin::Pin<Box<Body>>,
    pending: axum::body::Bytes,
}

impl BodyReader {
    fn new(body: Body) -> Self {
        Self { body: Box::pin(body), pending: axum::body::Bytes::new() }
    }
}

impl tokio::io::AsyncRead for BodyReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use http_body::Body as _;
        loop {
            if !self.pending.is_empty() {
                let n = self.pending.len().min(buf.remaining());
                let chunk = self.pending.split_to(n);
                buf.put_slice(&chunk);
                return std::task::Poll::Ready(Ok(()));
            }
            match std::task::ready!(self.body.as_mut().poll_frame(cx)) {
                // End of the body: EOF for git.
                None => return std::task::Poll::Ready(Ok(())),
                // Data goes to `pending`; anything else (trailers) is skipped.
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        self.pending = data;
                    }
                }
                Some(Err(_)) => {
                    return std::task::Poll::Ready(Err(std::io::Error::other(
                        "request body failed",
                    )));
                }
            }
        }
    }
}

/// The other direction: git's stdout as an HTTP body. Each read chunk becomes
/// one data frame, EOF ends the body and with it the response.
struct ReaderBody<R: tokio::io::AsyncRead + Unpin> {
    reader: R,
    buf: Box<[u8]>,
}

impl<R: tokio::io::AsyncRead + Unpin> ReaderBody<R> {
    fn new(reader: R) -> Self {
        Self { reader, buf: vec![0u8; 16 * 1024].into_boxed_slice() }
    }
}

impl<R: tokio::io::AsyncRead + Unpin> http_body::Body for ReaderBody<R> {
    type Data = axum::body::Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<std::io::Result<http_body::Frame<axum::body::Bytes>>>> {
        let Self { reader, buf } = self.get_mut();
        let mut rb = tokio::io::ReadBuf::new(&mut **buf);
        match std::task::ready!(std::pin::Pin::new(reader).poll_read(cx, &mut rb)) {
            Ok(()) if rb.filled().is_empty() => std::task::Poll::Ready(None),
            Ok(()) => std::task::Poll::Ready(Some(Ok(http_body::Frame::data(
                axum::body::Bytes::copy_from_slice(rb.filled()),
            )))),
            Err(e) => std::task::Poll::Ready(Some(Err(e))),
        }
    }
}

/// Kill the git process and whatever it spawned (`pack-objects` and friends):
/// the child runs in its own process group, so the group signal reaches both.
#[cfg(unix)]
fn kill_git_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    }
    let _ = child.start_kill();
}

#[cfg(windows)]
fn kill_git_tree(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
}

/// Optimistic-concurrency preconditions for a `PUT` (LLD-28 п. 3.7):
/// `If-None-Match: *` requires the target not to exist; `If-Match: <sha>`
/// requires the target's current content hash to equal `<sha>`. A violated
/// precondition is `412` and the target is left untouched. The current-target
/// hash is computed off the async runtime (see [`current_hash_blocking`]).
async fn check_put_preconditions(
    state: &Arc<AgentState>,
    target: &Path,
    existed: bool,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, &'static str)> {
    if let Some(v) = header_str(headers, "if-none-match") {
        if v.trim() == "*" && existed {
            return Err((StatusCode::PRECONDITION_FAILED, "target already exists"));
        }
    }
    if let Some(want) = if_match_hash(headers) {
        if !existed {
            return Err((StatusCode::PRECONDITION_FAILED, "no current version to match"));
        }
        let current = current_hash_blocking(state, target).await?;
        if !current.eq_ignore_ascii_case(&want) {
            return Err((StatusCode::PRECONDITION_FAILED, "version mismatch"));
        }
    }
    Ok(())
}

/// The target's current content hash for an `If-Match` check, computed on a
/// blocking thread. `HashCache::hash_of` stats and (on a cold cache) reads the
/// whole file; the read/manifest path already moves this off the runtime with
/// `spawn_blocking`, and the write path must not stall a worker on a large
/// un-warmed file either.
async fn current_hash_blocking(
    state: &Arc<AgentState>,
    target: &Path,
) -> Result<String, (StatusCode, &'static str)> {
    let st = state.clone();
    let path = target.to_path_buf();
    tokio::task::spawn_blocking(move || st.hash_cache.hash_of(&path))
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "hash task failed"))?
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "hash failed"))
}

/// The `If-Match` value as a bare sha256 hex (tolerating ETag-style quotes). The
/// consumer sends the hash from the manifest, not a file-server ETag.
fn if_match_hash(headers: &HeaderMap) -> Option<String> {
    let v = header_str(headers, "if-match")?.trim().trim_matches('"');
    (!v.is_empty() && v != "*").then(|| v.to_string())
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    header_str(headers, "content-length")?.parse().ok()
}

/// Stream a request body into `tmp`, hashing on the fly and enforcing `cap_bytes`
/// (if any). Fsync before returning `(sha256_hex, size)`. On any error the temp
/// is removed and a status is returned: `413` over cap, `507` on a full disk,
/// `400` on a broken body, `500` otherwise.
async fn stream_to_temp(
    body: Body,
    tmp: &Path,
    cap_bytes: Option<u64>,
) -> Result<(String, u64), (StatusCode, &'static str)> {
    let mut file = match tokio::fs::File::create(tmp).await {
        Ok(f) => f,
        Err(e) => return Err(io_status(&e)),
    };
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    let mut body = body;
    let result = loop {
        match body.frame().await {
            Some(Ok(frame)) => {
                let Ok(data) = frame.into_data() else { continue };
                total += data.len() as u64;
                if let Some(cap) = cap_bytes {
                    if total > cap {
                        break Err((StatusCode::PAYLOAD_TOO_LARGE, "file too large"));
                    }
                }
                hasher.update(&data);
                if let Err(e) = file.write_all(&data).await {
                    break Err(io_status(&e));
                }
            }
            Some(Err(_)) => break Err((StatusCode::BAD_REQUEST, "body read error")),
            None => break Ok(()),
        }
    };
    match result {
        Ok(()) => {
            if let Err(e) = file.sync_all().await {
                drop(file);
                let _ = tokio::fs::remove_file(tmp).await;
                return Err(io_status(&e));
            }
            drop(file);
            Ok((hex_lower(&hasher.finalize()), total))
        }
        Err(status) => {
            drop(file);
            let _ = tokio::fs::remove_file(tmp).await;
            Err(status)
        }
    }
}

/// Map an IO error to a status: a full disk (`ENOSPC` on unix, `ERROR_DISK_FULL`
/// on Windows) is `507`, everything else `500`.
fn io_status(e: &std::io::Error) -> (StatusCode, &'static str) {
    match e.raw_os_error() {
        Some(28) | Some(112) => (StatusCode::INSUFFICIENT_STORAGE, "no space left"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "io error"),
    }
}

/// Rename `from` over `to`. Atomic on Unix; Windows cannot rename over an
/// existing file, so there we remove the target first (a tiny non-atomic window,
/// accepted for the Windows agent, LLD-28 risk 2).
async fn rename_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    match tokio::fs::rename(from, to).await {
        Ok(()) => Ok(()),
        Err(e) => {
            #[cfg(windows)]
            {
                let _ = tokio::fs::remove_file(to).await;
                return tokio::fs::rename(from, to).await;
            }
            #[cfg(not(windows))]
            Err(e)
        }
    }
}

/// Modification time in whole unix seconds (0 if the filesystem cannot say),
/// matching the manifest builder so a seeded hash keys on the same value.
fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
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

    use crate::gitrepo::GitSettings;

    fn blob(t: &ShareToken) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(t).unwrap())
    }

    fn state_with(shares: SharesMap, key: &SigningKey) -> Arc<AgentState> {
        state_with_cap(shares, key, None)
    }

    fn state_with_cap(shares: SharesMap, key: &SigningKey, max_file_mb: Option<u64>) -> Arc<AgentState> {
        let cache = Arc::new(HashCache::new());
        Arc::new(AgentState {
            shares: RwLock::new(Arc::new(shares)),
            hub_key: key.verifying_key(),
            hash_cache: cache.clone(),
            identity: Some(SigningKey::from_bytes(&[77u8; 32])),
            max_file_mb,
            import: ImportManager::new(None, cache),
            git: git_manager_for_tests(),
            expose: RwLock::new(Arc::new(Vec::new())),
        })
    }

    /// A git table for hand-built states: its own throwaway state dir, no
    /// shares registered (the git tests register theirs via `rebuild`, the
    /// rest of the suite never touches a repository).
    fn git_manager_for_tests() -> Arc<GitManager> {
        let state_dir = tempfile::tempdir().unwrap().keep();
        Arc::new(GitManager::new(&state_dir))
    }

    /// The contour settings the git tests build their table with.
    fn git_settings() -> GitSettings {
        GitSettings { author: ("tester".into(), "tester@xr-share".into()), max_file_mb: 10 }
    }

    /// A directory share; `writable` opts into the write path (LLD-28).
    fn dir_share(path: PathBuf, writable: bool) -> ShareRoot {
        ShareRoot { path, is_file: false, writable, import: false, git: false }
    }

    fn get_with_token(uri: &str, tok: Option<&ShareToken>) -> HttpRequest<Body> {
        let mut b = HttpRequest::get(uri);
        if let Some(t) = tok {
            b = b.header("authorization", format!("Bearer {}", blob(t)));
        }
        b.body(Body::empty()).unwrap()
    }

    /// A `PUT`/`DELETE` request with a bearer token and arbitrary extra headers.
    fn write_req(
        method: &str,
        uri: &str,
        tok: Option<&ShareToken>,
        headers: &[(&str, String)],
        body: &[u8],
    ) -> HttpRequest<Body> {
        let mut b = HttpRequest::builder().method(method).uri(uri);
        if let Some(t) = tok {
            b = b.header("authorization", format!("Bearer {}", blob(t)));
        }
        for (k, v) in headers {
            b = b.header(*k, v.clone());
        }
        b.body(Body::from(body.to_vec())).unwrap()
    }

    /// SHA-256 hex of `data`, the value a client puts in `X-Xr-Sha256`/`If-Match`.
    fn sha_hex(data: &[u8]) -> String {
        hex_lower(&Sha256::digest(data))
    }

    /// The manifest paths a share currently lists (for asserting a PUT/DELETE
    /// took effect).
    async fn manifest_paths(app: &Router, share_id: &str, tok: &ShareToken) -> Vec<String> {
        let r = app
            .clone()
            .oneshot(get_with_token(&format!("/{share_id}/manifest"), Some(tok)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let m: ShareManifest = serde_json::from_slice(&body).unwrap();
        m.entries.into_iter().map(|e| e.path).collect()
    }

    #[tokio::test]
    async fn test_router_share_id() {
        // Two directory shares; a token for one must not open the other.
        let key = SigningKey::from_bytes(&[5u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hi").unwrap();
        let canon = dir.path().canonicalize().unwrap();
        let mut shares = SharesMap::new();
        shares.insert("A".into(), dir_share(canon.clone(), false));
        shares.insert("B".into(), dir_share(canon, false));
        let app = router(state_with(shares, &key));

        let tok_a = sign_share_token(&key, "A", SCOPE_READ, now_unix() + 1000);

        // Right share → 200.
        let r = app.clone().oneshot(get_with_token("/A/manifest", Some(&tok_a))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        // Token for A presented to B's URL → rejected (WrongShare → 403).
        let r = app.clone().oneshot(get_with_token("/B/manifest", Some(&tok_a))).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // Unknown share id → 404.
        let tok_x = sign_share_token(&key, "X", SCOPE_READ, now_unix() + 1000);
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
        shares.insert("F".into(), ShareRoot { path: file.canonicalize().unwrap(), is_file: true, writable: false, import: false, git: false });
        let app = router(state_with(shares, &key));
        let tok = sign_share_token(&key, "F", SCOPE_READ, now_unix() + 1000);

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
    async fn manifest_signature_covers_served_bytes() {
        // The signature headers must verify against the pinned agent key, the
        // requested share_id and the exact body bytes, and against nothing else
        // (XR-046: a MITM rewriting a hash, or replaying another share's
        // listing, must not verify).
        let key = SigningKey::from_bytes(&[8u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hi").unwrap();
        let mut shares = SharesMap::new();
        shares.insert("A".into(), dir_share(dir.path().canonicalize().unwrap(), false));
        let state = state_with(shares, &key);
        let agent_vk = state.identity.as_ref().unwrap().verifying_key();
        let app = router(state);

        let tok = sign_share_token(&key, "A", SCOPE_READ, now_unix() + 1000);
        let r = app.oneshot(get_with_token("/A/manifest", Some(&tok))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        let sig = r.headers()[MANIFEST_SIG_HEADER].to_str().unwrap().to_string();
        let signed_at: u64 =
            r.headers()[MANIFEST_SIGNED_AT_HEADER].to_str().unwrap().parse().unwrap();
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();

        use xr_proto::share::verify_share_manifest;
        assert!(verify_share_manifest(&sig, &agent_vk, "A", signed_at, &body).is_ok());

        // Tampered body -> reject.
        let mut forged = body.to_vec();
        forged[0] ^= 1;
        assert!(verify_share_manifest(&sig, &agent_vk, "A", signed_at, &forged).is_err());
        // Replayed under a different share id -> reject.
        assert!(verify_share_manifest(&sig, &agent_vk, "B", signed_at, &body).is_err());
    }

    #[tokio::test]
    async fn manifest_unsigned_without_identity() {
        // A legacy config without an identity key still serves the listing,
        // just without signature headers (the pinning consumer then refuses it).
        let key = SigningKey::from_bytes(&[10u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hi").unwrap();
        let mut shares = SharesMap::new();
        shares.insert("A".into(), dir_share(dir.path().canonicalize().unwrap(), false));
        let cache = Arc::new(HashCache::new());
        let state = Arc::new(AgentState {
            shares: RwLock::new(Arc::new(shares)),
            hub_key: key.verifying_key(),
            hash_cache: cache.clone(),
            identity: None,
            max_file_mb: None,
            import: ImportManager::new(None, cache),
            git: git_manager_for_tests(),
            expose: RwLock::new(Arc::new(Vec::new())),
        });
        let app = router(state);

        let tok = sign_share_token(&key, "A", SCOPE_READ, now_unix() + 1000);
        let r = app.oneshot(get_with_token("/A/manifest", Some(&tok))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r.headers().get(MANIFEST_SIG_HEADER).is_none());
        assert!(r.headers().get(MANIFEST_SIGNED_AT_HEADER).is_none());
    }

    #[tokio::test]
    async fn legacy_route_selects_share_by_token() {
        // The v1 `/manifest` alias must resolve the share from the token's id.
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hi").unwrap();
        let mut shares = SharesMap::new();
        shares.insert("only".into(), dir_share(dir.path().canonicalize().unwrap(), false));
        let app = router(state_with(shares, &key));

        let tok = sign_share_token(&key, "only", SCOPE_READ, now_unix() + 1000);
        let r = app.clone().oneshot(get_with_token("/manifest", Some(&tok))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        // A token for a share this agent doesn't hold → 404 via the legacy path.
        let bad = sign_share_token(&key, "missing", SCOPE_READ, now_unix() + 1000);
        let r = app.oneshot(get_with_token("/manifest", Some(&bad))).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    // -- write path (LLD-28) --------------------------------------------

    /// A read+write scope string, as the hub mints for a write binding.
    fn rw_scope() -> String {
        format!("{SCOPE_READ} {SCOPE_WRITE}")
    }

    /// A one-share writable-directory app plus a read+write token for it.
    fn writable_app(key: &SigningKey, dir: &Path, cap: Option<u64>) -> (Router, ShareToken) {
        let mut shares = SharesMap::new();
        shares.insert("W".into(), dir_share(dir.canonicalize().unwrap(), true));
        let app = router(state_with_cap(shares, key, cap));
        let tok = sign_share_token(key, "W", &rw_scope(), now_unix() + 1000);
        (app, tok)
    }

    #[tokio::test]
    async fn test_put_creates_and_overwrites() {
        let key = SigningKey::from_bytes(&[20u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let (app, wtok) = writable_app(&key, dir.path(), None);

        // New file (nested) -> 201, visible in the manifest.
        let r = app
            .clone()
            .oneshot(write_req("PUT", "/W/file/docs/a.txt", Some(&wtok), &[], b"hello"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
        assert_eq!(std::fs::read(dir.path().join("docs/a.txt")).unwrap(), b"hello");
        assert_eq!(manifest_paths(&app, "W", &wtok).await, vec!["docs/a.txt".to_string()]);

        // Overwrite -> 204, content replaced whole.
        let r = app
            .clone()
            .oneshot(write_req("PUT", "/W/file/docs/a.txt", Some(&wtok), &[], b"world!!"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert_eq!(std::fs::read(dir.path().join("docs/a.txt")).unwrap(), b"world!!");
    }

    #[tokio::test]
    async fn test_put_requires_write_scope() {
        let key = SigningKey::from_bytes(&[21u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let (app, _wtok) = writable_app(&key, dir.path(), None);

        // No token -> 401.
        let r = app
            .clone()
            .oneshot(write_req("PUT", "/W/file/a.txt", None, &[], b"x"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

        // Read-only token -> 403 (writable share, but scope lacks share:write).
        let rtok = sign_share_token(&key, "W", SCOPE_READ, now_unix() + 1000);
        let r = app
            .oneshot(write_req("PUT", "/W/file/a.txt", Some(&rtok), &[], b"x"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert!(!dir.path().join("a.txt").exists());
    }

    #[tokio::test]
    async fn test_put_readonly_share_rejected() {
        // A valid write token against a share the agent config marks read-only:
        // the agent's own switch refuses it (LLD-28 п. 3.2).
        let key = SigningKey::from_bytes(&[22u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let mut shares = SharesMap::new();
        shares.insert("R".into(), dir_share(dir.path().canonicalize().unwrap(), false));
        let app = router(state_with(shares, &key));
        let wtok = sign_share_token(&key, "R", &rw_scope(), now_unix() + 1000);

        let r = app
            .oneshot(write_req("PUT", "/R/file/a.txt", Some(&wtok), &[], b"x"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert!(!dir.path().join("a.txt").exists());
    }

    #[tokio::test]
    async fn test_put_path_traversal_blocked() {
        let key = SigningKey::from_bytes(&[23u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let (app, wtok) = writable_app(&key, dir.path(), None);

        // Traversal and the reserved upload-temp prefix are refused (403).
        for bad in [
            "/W/file/../evil",
            "/W/file/.xr-part-abc",
            "/W/file/sub/.xr-part-x",
        ] {
            let r = app
                .clone()
                .oneshot(write_req("PUT", bad, Some(&wtok), &[], b"x"))
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::FORBIDDEN, "must reject {bad}");
        }
        // Nothing escaped the share root.
        assert!(!dir.path().parent().unwrap().join("evil").exists());
    }

    #[tokio::test]
    async fn test_put_sha256_mismatch() {
        let key = SigningKey::from_bytes(&[24u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let (app, wtok) = writable_app(&key, dir.path(), None);

        // Wrong X-Xr-Sha256 -> 422, nothing written, no temp left behind.
        let hdr = [("x-xr-sha256", "deadbeef".to_string())];
        let r = app
            .clone()
            .oneshot(write_req("PUT", "/W/file/a.txt", Some(&wtok), &hdr, b"hello"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!dir.path().join("a.txt").exists());
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none(), "temp must be cleaned up");

        // The correct hash goes through.
        let hdr = [("x-xr-sha256", sha_hex(b"hello"))];
        let r = app
            .oneshot(write_req("PUT", "/W/file/a.txt", Some(&wtok), &hdr, b"hello"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
        assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn test_conditional_requests() {
        let key = SigningKey::from_bytes(&[25u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let (app, wtok) = writable_app(&key, dir.path(), None);
        let put = |uri: &'static str, hdrs: Vec<(&'static str, String)>, body: &'static [u8]| {
            let app = app.clone();
            let tok = wtok.clone();
            async move { app.oneshot(write_req("PUT", uri, Some(&tok), &hdrs, body)).await.unwrap().status() }
        };

        // Seed v1.
        assert_eq!(put("/W/file/a.txt", vec![], b"v1").await, StatusCode::CREATED);
        let h1 = sha_hex(b"v1");

        // If-Match on the current version replaces it (204).
        assert_eq!(
            put("/W/file/a.txt", vec![("if-match", h1.clone())], b"v2").await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"v2");

        // A now-stale If-Match -> 412, content untouched.
        assert_eq!(
            put("/W/file/a.txt", vec![("if-match", h1.clone())], b"v3").await,
            StatusCode::PRECONDITION_FAILED
        );
        assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"v2");

        // If-Match against an absent target -> 412.
        assert_eq!(
            put("/W/file/nope.txt", vec![("if-match", h1.clone())], b"x").await,
            StatusCode::PRECONDITION_FAILED
        );

        // If-None-Match:* over an existing file -> 412; over a new path -> 201.
        assert_eq!(
            put("/W/file/a.txt", vec![("if-none-match", "*".into())], b"x").await,
            StatusCode::PRECONDITION_FAILED
        );
        assert_eq!(
            put("/W/file/fresh.txt", vec![("if-none-match", "*".into())], b"n").await,
            StatusCode::CREATED
        );

        // DELETE with a mismatched If-Match -> 412, the file stays.
        let r = app
            .clone()
            .oneshot(write_req("DELETE", "/W/file/a.txt", Some(&wtok), &[("if-match", h1)], b""))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::PRECONDITION_FAILED);
        assert!(dir.path().join("a.txt").exists());
    }

    #[tokio::test]
    async fn test_put_cap_exceeded() {
        let key = SigningKey::from_bytes(&[26u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        // 1 MiB cap.
        let (app, wtok) = writable_app(&key, dir.path(), Some(1));

        // Declared Content-Length over the cap is refused up front (413).
        let hdr = [("content-length", "5000000".to_string())];
        let r = app
            .clone()
            .oneshot(write_req("PUT", "/W/file/big.bin", Some(&wtok), &hdr, b"small body"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);

        // An actual body over the cap (no declared length) is caught while
        // streaming, and leaves no temp behind.
        let big = vec![7u8; 1024 * 1024 + 1];
        let r = app
            .oneshot(write_req("PUT", "/W/file/big.bin", Some(&wtok), &[], &big))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(!dir.path().join("big.bin").exists());
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none(), "no temp junk");
    }

    #[tokio::test]
    async fn test_delete_file() {
        let key = SigningKey::from_bytes(&[27u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let (app, wtok) = writable_app(&key, dir.path(), None);

        // Put two files, one nested.
        for (uri, body) in [("/W/file/a.txt", &b"a"[..]), ("/W/file/sub/b.txt", &b"b"[..])] {
            let r = app.clone().oneshot(write_req("PUT", uri, Some(&wtok), &[], body)).await.unwrap();
            assert_eq!(r.status(), StatusCode::CREATED);
        }

        // Delete a.txt -> 204, gone from disk and the manifest.
        let r = app
            .clone()
            .oneshot(write_req("DELETE", "/W/file/a.txt", Some(&wtok), &[], b""))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert!(!dir.path().join("a.txt").exists());
        assert_eq!(manifest_paths(&app, "W", &wtok).await, vec!["sub/b.txt".to_string()]);

        // Deleting it again -> 404.
        let r = app
            .clone()
            .oneshot(write_req("DELETE", "/W/file/a.txt", Some(&wtok), &[], b""))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);

        // Deleting a directory -> 409.
        let r = app
            .oneshot(write_req("DELETE", "/W/file/sub", Some(&wtok), &[], b""))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::CONFLICT);
        assert!(dir.path().join("sub/b.txt").exists());
    }

    #[tokio::test]
    async fn test_file_share_not_writable() {
        // A file share is never writable, even if the config asked (build_shares
        // zeroes it), so a PUT is refused (LLD-28 п. 2.1).
        let key = SigningKey::from_bytes(&[28u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("report.pdf");
        std::fs::write(&file, b"hello").unwrap();
        let entries = vec![crate::config::ShareEntry {
            share_id: "F".into(),
            path: file.display().to_string(),
            name: None,
            writable: true,
            import: false,
            git: false,
            attached: false,
        }];
        let shares = build_shares(&entries);
        assert!(!shares.get("F").unwrap().writable, "a file share must not be writable");
        let app = router(state_with(shares, &key));
        let wtok = sign_share_token(&key, "F", &rw_scope(), now_unix() + 1000);

        let r = app
            .oneshot(write_req("PUT", "/F/file/report.pdf", Some(&wtok), &[], b"x"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    // -- import path (LLD-29) -------------------------------------------

    use std::time::Duration;

    use crate::config::{ImportConfig, ImportPlugin};

    /// The scope a write-binding grant carries (LLD-29 п. 2.2).
    fn rwi_scope() -> String {
        format!("{SCOPE_READ} {SCOPE_WRITE} {SCOPE_IMPORT}")
    }

    /// A public-literal URL that passes the gate without any DNS.
    const PUB_URL: &str = "http://93.184.216.34/video";

    fn one_plugin(cmd: &str, args: &[&str], patterns: &[&str], max_height: u32) -> ImportConfig {
        ImportConfig {
            timeout_min: 30,
            max_total_mb: None,
            sandbox: "none".into(),
            queue_depth: 4,
            plugins: vec![ImportPlugin {
                name: "тест".into(),
                patterns: patterns.iter().map(|s| s.to_string()).collect(),
                max_height,
                cmd: cmd.into(),
                args: args.iter().map(|s| s.to_string()).collect(),
            }],
        }
    }

    /// One import-enabled writable share "I" with a live job runner.
    fn import_app(
        key: &SigningKey,
        dir: &Path,
        cfg: Option<ImportConfig>,
        max_file_mb: Option<u64>,
    ) -> (Router, ShareToken) {
        let mut shares = SharesMap::new();
        shares.insert(
            "I".into(),
            ShareRoot { path: dir.canonicalize().unwrap(), is_file: false, writable: true, import: true, git: false },
        );
        let cache = Arc::new(HashCache::new());
        let state = Arc::new(AgentState {
            shares: RwLock::new(Arc::new(shares)),
            hub_key: key.verifying_key(),
            hash_cache: cache.clone(),
            identity: Some(SigningKey::from_bytes(&[77u8; 32])),
            max_file_mb,
            import: ImportManager::new(cfg, cache),
            git: git_manager_for_tests(),
            expose: RwLock::new(Arc::new(Vec::new())),
        });
        state.import.spawn_runner();
        let tok = sign_share_token(key, "I", &rwi_scope(), now_unix() + 1000);
        (router(state), tok)
    }

    /// A fake plugin: an executable shell script the test writes (LLD-29 п. 4).
    #[cfg(unix)]
    fn write_script(dir: &Path, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("plugin.sh");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    async fn post_import(
        app: &Router,
        tok: Option<&ShareToken>,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let mut b = HttpRequest::post(uri).header("content-type", "application/json");
        if let Some(t) = tok {
            b = b.header("authorization", format!("Bearer {}", blob(t)));
        }
        let r = app.clone().oneshot(b.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned())
        });
        (status, v)
    }

    async fn get_status(app: &Router, tok: &ShareToken, job_id: &str) -> (StatusCode, serde_json::Value) {
        let r = app
            .clone()
            .oneshot(get_with_token(&format!("/I/import/{job_id}"), Some(tok)))
            .await
            .unwrap();
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned())
        });
        (status, v)
    }

    /// Poll until the job leaves queued/running (bounded, so a hung test fails
    /// loudly instead of forever; generous, so a loaded build machine that
    /// starves the fake plugin's spawn does not flake the test).
    async fn wait_finished(app: &Router, tok: &ShareToken, job_id: &str) -> serde_json::Value {
        for _ in 0..600 {
            let (status, v) = get_status(app, tok, job_id).await;
            assert_eq!(status, StatusCode::OK, "status poll failed: {v}");
            match v.get("state").and_then(|s| s.as_str()) {
                Some("done") | Some("failed") => return v,
                _ => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        panic!("job {job_id} did not finish in time");
    }

    /// No service dirs left behind in the share root.
    fn no_job_dirs(dir: &Path) -> bool {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().starts_with(crate::import::JOB_DIR_PREFIX))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_import_progress_survives_non_utf8_output() {
        // yt-dlp on Windows prints the console codepage (cp1251): a stdout
        // line that is not valid UTF-8 must not kill the progress reader,
        // otherwise the pipe stops draining and the plugin dies with EINVAL
        // mid-download (found on the XR-141 prod check). Progress after the
        // bad line still counts.
        let key = SigningKey::from_bytes(&[39u8; 32]);
        let share = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let script = write_script(
            bin.path(),
            "printf '\\320\\240\\377\\376 codepage title\\n'\necho 'xr-progress 50'\nprintf 'x' > out.bin",
        );
        let (app, tok) = import_app(&key, share.path(), Some(one_plugin(&script, &["{url}"], &["*"], 1080)), None);

        let (status, v) = post_import(
            &app, Some(&tok), "/I/import",
            serde_json::json!({ "url": PUB_URL, "dest": "" }),
        ).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{v}");
        let job_id = v["job_id"].as_str().unwrap().to_string();

        let v = wait_finished(&app, &tok, &job_id).await;
        assert_eq!(v["state"], "done", "{v}");
        assert_eq!(v["progress"], 50.0, "прогресс после не-UTF-8 строки потерян: {v}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_import_happy_path() {
        let key = SigningKey::from_bytes(&[30u8; 32]);
        let share = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        // The fake plugin reports progress and drops a file named from
        // "metadata" (like yt-dlp names by the video title).
        let script = write_script(
            bin.path(),
            "echo 'xr-progress 25'\necho 'xr-progress  60.5%'\nprintf 'video-bytes' > 'Ролик [abc].mp4'",
        );
        let (app, tok) = import_app(&key, share.path(), Some(one_plugin(&script, &["{url}"], &["*"], 1080)), None);

        let (status, v) = post_import(
            &app, Some(&tok), "/I/import",
            serde_json::json!({ "url": PUB_URL, "dest": "видео" }),
        ).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{v}");
        let job_id = v["job_id"].as_str().unwrap().to_string();

        let v = wait_finished(&app, &tok, &job_id).await;
        assert_eq!(v["state"], "done", "{v}");
        // The last xr-progress line stuck (progress got parsed).
        assert_eq!(v["progress"], 60.5);
        assert_eq!(v["files"], serde_json::json!(["видео/Ролик [abc].mp4"]));

        // The file really lies in the dest dir and the job dir is gone.
        let published = share.path().join("видео/Ролик [abc].mp4");
        assert_eq!(std::fs::read(&published).unwrap(), b"video-bytes");
        assert!(no_job_dirs(share.path()));

        // Visible in the manifest, already hashed (the cache was seeded).
        let r = app.clone().oneshot(get_with_token("/I/manifest", Some(&tok))).await.unwrap();
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let m: ShareManifest = serde_json::from_slice(&body).unwrap();
        let entry = m.entries.iter().find(|e| e.path == "видео/Ролик [abc].mp4").expect("в манифесте");
        assert_eq!(entry.sha256, sha_hex(b"video-bytes"));
    }

    /// The listing entry for `path`, as a browsing consumer sees it.
    async fn manifest_entry(
        app: &Router,
        tok: &ShareToken,
        path: &str,
    ) -> xr_proto::share::ShareManifestEntry {
        let r = app.clone().oneshot(get_with_token("/I/manifest", Some(tok))).await.unwrap();
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let m: ShareManifest = serde_json::from_slice(&body).unwrap();
        m.entries
            .into_iter()
            .find(|e| e.path == path)
            .unwrap_or_else(|| panic!("в манифесте нет {path}"))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_import_writes_file_origin() {
        // XR-255: the plugin's origin line reaches the listing under the same
        // signature as the rest of the row, a second output file the plugin
        // said nothing about still knows the page it came from, and all of it
        // survives the agent restarting (the index is on disk, the job table
        // is not).
        let key = SigningKey::from_bytes(&[41u8; 32]);
        let share = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let script = write_script(
            bin.path(),
            "printf 'video-bytes' > 'Ролик [dQw4w9WgXcQ].mp4'\n\
             printf 'sub-bytes' > 'Ролик [dQw4w9WgXcQ].srt'\n\
             printf '%s\\n' \
             'Ролик [dQw4w9WgXcQ].mp4\thttps://www.youtube.com/watch?v=dQw4w9WgXcQ\tКанал автора\thttps://www.youtube.com/@avtor\t20260731\tНастоящий заголовок' \
             > .xr-meta.tsv",
        );
        let (app, tok) = import_app(&key, share.path(), Some(one_plugin(&script, &["{url}"], &["*"], 1080)), None);

        let (status, v) = post_import(
            &app, Some(&tok), "/I/import",
            serde_json::json!({ "url": PUB_URL, "dest": "видео" }),
        ).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{v}");
        let job_id = v["job_id"].as_str().unwrap().to_string();
        let v = wait_finished(&app, &tok, &job_id).await;
        assert_eq!(v["state"], "done", "{v}");

        let entry = manifest_entry(&app, &tok, "видео/Ролик [dQw4w9WgXcQ].mp4").await;
        let origin = entry.meta.as_ref().expect("источник у импортированного файла");
        assert_eq!(origin.url, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert_eq!(origin.source, "Канал автора");
        assert_eq!(origin.source_url, "https://www.youtube.com/@avtor");
        assert_eq!(origin.published, "2026-07-31", "дату агент приводит к одному виду");
        assert_eq!(origin.title, "Настоящий заголовок");

        // The plugin named only the video; the subtitles still carry the link
        // the job was started with, because the job knew it all along.
        let sub = manifest_entry(&app, &tok, "видео/Ролик [dQw4w9WgXcQ].srt").await;
        let sub_origin = sub.meta.as_ref().expect("источник у второго файла");
        assert_eq!(sub_origin.url, PUB_URL);
        assert_eq!(sub_origin.source, "");

        // The plugin's own line file is service data: never published, never
        // listed.
        assert!(!share.path().join("видео/.xr-meta.tsv").exists());
        assert!(manifest_paths(&app, "I", &tok)
            .await
            .iter()
            .all(|p| !p.contains(".xr-meta")));

        // A fresh agent over the same directory (a restart) still knows it.
        let (again, tok2) = import_app(&key, share.path(), None, None);
        let entry = manifest_entry(&again, &tok2, "видео/Ролик [dQw4w9WgXcQ].mp4").await;
        assert_eq!(entry.meta.expect("источник пережил рестарт").source, "Канал автора");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_write_path_drops_stale_origin() {
        // XR-255: whatever lands at a path by hand did not come from the page
        // the old file came from, so PUT and DELETE take the origin with them.
        let key = SigningKey::from_bytes(&[42u8; 32]);
        let share = tempfile::tempdir().unwrap();
        let root = share.path().canonicalize().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let script = write_script(
            bin.path(),
            "printf 'v' > 'Ролик.mp4'\n\
             printf 'Ролик.mp4\thttps://host/page\tКанал\t\t\t\\n' > .xr-meta.tsv",
        );
        let (app, tok) = import_app(&key, share.path(), Some(one_plugin(&script, &["{url}"], &["*"], 1080)), None);
        let (_, v) = post_import(
            &app, Some(&tok), "/I/import",
            serde_json::json!({ "url": PUB_URL, "dest": "" }),
        ).await;
        let job_id = v["job_id"].as_str().unwrap().to_string();
        assert_eq!(wait_finished(&app, &tok, &job_id).await["state"], "done");
        assert!(crate::meta::load(&root).contains_key("Ролик.mp4"));

        // An upload over the same name replaces the content, so the old origin
        // must not stay and claim the new bytes.
        let r = app
            .clone()
            .oneshot(write_req("PUT", "/I/file/Ролик.mp4", Some(&tok), &[], b"new-bytes"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert!(!crate::meta::load(&root).contains_key("Ролик.mp4"));
        assert!(manifest_entry(&app, &tok, "Ролик.mp4").await.meta.is_none());

        // And a delete takes the row with the file.
        crate::meta::record(
            &root,
            &[("Ролик.mp4".to_string(), xr_proto::share::FileMeta {
                url: "https://host/page".into(),
                ..Default::default()
            })],
        );
        let r = app
            .clone()
            .oneshot(write_req("DELETE", "/I/file/Ролик.mp4", Some(&tok), &[], b""))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert!(!crate::meta::load(&root).contains_key("Ролик.mp4"));
    }

    #[tokio::test]
    async fn test_import_gates() {
        let key = SigningKey::from_bytes(&[31u8; 32]);
        let share = tempfile::tempdir().unwrap();
        let cfg = one_plugin("true", &["{url}"], &["*"], 1080);
        let (app, tok) = import_app(&key, share.path(), Some(cfg.clone()), None);
        let body = serde_json::json!({ "url": PUB_URL, "dest": "" });

        // Unknown share -> 404 (before any token logic).
        let (status, _) = post_import(&app, Some(&tok), "/X/import", body.clone()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // No token -> 401; a read+write token without share:import -> 403.
        let (status, _) = post_import(&app, None, "/I/import", body.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let rw = sign_share_token(&key, "I", &rw_scope(), now_unix() + 1000);
        let (status, _) = post_import(&app, Some(&rw), "/I/import", body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // dest with traversal or a reserved component -> 403.
        for dest in ["..", ".xr-import-1", "sub/.xr-part-x"] {
            let (status, _) = post_import(
                &app, Some(&tok), "/I/import",
                serde_json::json!({ "url": PUB_URL, "dest": dest }),
            ).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "dest {dest}");
        }

        // A share without the import flag -> 403 even with a good token.
        let mut shares = SharesMap::new();
        shares.insert(
            "I".into(),
            ShareRoot { path: share.path().canonicalize().unwrap(), is_file: false, writable: true, import: false, git: false },
        );
        let cache = Arc::new(HashCache::new());
        let no_flag = router(Arc::new(AgentState {
            shares: RwLock::new(Arc::new(shares)),
            hub_key: key.verifying_key(),
            hash_cache: cache.clone(),
            identity: None,
            max_file_mb: None,
            import: ImportManager::new(Some(cfg), cache),
            git: git_manager_for_tests(),
            expose: RwLock::new(Arc::new(Vec::new())),
        }));
        let (status, _) = post_import(&no_flag, Some(&tok), "/I/import", body.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // No plugins configured -> 403 too (the double local opt-in).
        let (app_nocfg, tok2) = import_app(&key, share.path(), None, None);
        let (status, _) = post_import(&app_nocfg, Some(&tok2), "/I/import", body).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_import_url_guard_and_routing() {
        let key = SigningKey::from_bytes(&[32u8; 32]);
        let share = tempfile::tempdir().unwrap();
        // No catch-all: only youtube.com is routed.
        let (app, tok) = import_app(&key, share.path(), Some(one_plugin("true", &["{url}"], &["youtube.com"], 1080)), None);

        // Bad scheme and private addresses -> 400 up front (LLD-29 п. 2.6).
        for url in [
            "file:///etc/passwd",
            "http://192.168.1.1/router-admin",
            "http://127.0.0.1:8443/secret",
            "http://[fe80::1]/x",
            "http://localhost/x",
        ] {
            let (status, _) = post_import(
                &app, Some(&tok), "/I/import",
                serde_json::json!({ "url": url, "dest": "" }),
            ).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "url {url}");
        }

        // Public host, but no plugin takes it -> 422.
        let (status, v) = post_import(
            &app, Some(&tok), "/I/import",
            serde_json::json!({ "url": PUB_URL, "dest": "" }),
        ).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");

        // A height outside the sane range -> 400 before any enqueue.
        let (app2, tok2) = import_app(&key, share.path(), Some(one_plugin("true", &["{url}"], &["*"], 1080)), None);
        let (status, _) = post_import(
            &app2, Some(&tok2), "/I/import",
            serde_json::json!({ "url": PUB_URL, "dest": "", "height": 99999 }),
        ).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_import_failures() {
        let key = SigningKey::from_bytes(&[33u8; 32]);
        let share = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();

        // Non-zero exit: failed with the stderr tail, nothing published.
        let script = write_script(bin.path(), "echo 'сайт не отдал видео' >&2\nexit 3");
        let (app, tok) = import_app(&key, share.path(), Some(one_plugin(&script, &["{url}"], &["*"], 1080)), None);
        let (status, v) = post_import(
            &app, Some(&tok), "/I/import",
            serde_json::json!({ "url": PUB_URL, "dest": "" }),
        ).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let v = wait_finished(&app, &tok, v["job_id"].as_str().unwrap()).await;
        assert_eq!(v["state"], "failed");
        assert!(v["error"].as_str().unwrap().contains("сайт не отдал видео"), "{v}");
        assert!(no_job_dirs(share.path()));

        // A file over max_file_mb: failed, nothing published (LLD-29 п. 2.7).
        let script = write_script(bin.path(), "head -c 2097160 /dev/zero > big.bin");
        let (app, tok) = import_app(&key, share.path(), Some(one_plugin(&script, &["{url}"], &["*"], 1080)), Some(1));
        let (_, v) = post_import(
            &app, Some(&tok), "/I/import",
            serde_json::json!({ "url": PUB_URL, "dest": "" }),
        ).await;
        let v = wait_finished(&app, &tok, v["job_id"].as_str().unwrap()).await;
        assert_eq!(v["state"], "failed", "{v}");
        assert!(v["error"].as_str().unwrap().contains("max_file_mb"), "{v}");
        assert!(!share.path().join("big.bin").exists());
        assert!(no_job_dirs(share.path()));

        // A successful exit with an empty output dir is failed too, not a
        // silent done-with-nothing.
        let script = write_script(bin.path(), "exit 0");
        let (app, tok) = import_app(&key, share.path(), Some(one_plugin(&script, &["{url}"], &["*"], 1080)), None);
        let (_, v) = post_import(
            &app, Some(&tok), "/I/import",
            serde_json::json!({ "url": PUB_URL, "dest": "" }),
        ).await;
        let v = wait_finished(&app, &tok, v["job_id"].as_str().unwrap()).await;
        assert_eq!(v["state"], "failed", "{v}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_import_timeout_and_total_cap() {
        // The watchdog kills a job past its deadline or over max_total_mb;
        // driven through the manager directly to use sub-minute limits.
        use crate::import::JobSpec;
        let share = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let cache = Arc::new(HashCache::new());
        let mgr = ImportManager::new(None, cache);
        mgr.spawn_runner();
        let plugin = |cmd: &str| ImportPlugin {
            name: "тест".into(),
            patterns: vec!["*".into()],
            max_height: 1080,
            cmd: cmd.into(),
            args: vec!["{url}".into()],
        };
        let spec = |cmd: &str, timeout: Duration, cap: Option<u64>| JobSpec {
            share_id: "I".into(),
            share_root: share.path().canonicalize().unwrap(),
            dest_rel: String::new(),
            url: PUB_URL.into(),
            height: 1080,
            plugin: plugin(cmd),
            timeout,
            max_total_bytes: cap,
            max_file_bytes: None,
            sandbox: "none".into(),
        };
        let wait = |mgr: Arc<ImportManager>, id: String| async move {
            for _ in 0..600 {
                if let Some(dto) = mgr.status("I", &id) {
                    if dto.state == "done" || dto.state == "failed" {
                        return dto;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("job did not finish");
        };

        // Lifetime cap: the sleeper is killed, the job fails, the dir is gone.
        let sleeper = write_script(bin.path(), "sleep 30");
        let id = mgr.enqueue(spec(&sleeper, Duration::from_millis(200), None)).unwrap();
        let dto = wait(mgr.clone(), id).await;
        assert_eq!(dto.state, "failed");
        assert!(dto.error.unwrap().contains("предел времени"));
        assert!(no_job_dirs(share.path()));

        // Total-size cap: writes past the cap, gets killed mid-download.
        let hog = write_script(bin.path(), "head -c 100000 /dev/zero > part.bin\nsleep 30");
        let id = mgr.enqueue(spec(&hog, Duration::from_secs(60), Some(1000))).unwrap();
        let dto = wait(mgr.clone(), id).await;
        assert_eq!(dto.state, "failed");
        assert!(dto.error.unwrap().contains("max_total_mb"));
        assert!(no_job_dirs(share.path()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_import_height() {
        let key = SigningKey::from_bytes(&[34u8; 32]);
        let share = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        // The fake plugin records the substituted height: args are
        // ["<height>", "<url>"], so $1 is the effective height.
        let script = write_script(bin.path(), "printf '%s' \"$1\" > out.txt");
        let cfg = one_plugin(&script, &["{height}", "{url}"], &["*"], 1080);
        let (app, tok) = import_app(&key, share.path(), Some(cfg), None);

        let share_root = share.path().to_path_buf();
        let run = |height: Option<u32>| {
            let app = app.clone();
            let tok = tok.clone();
            let share_root = share_root.clone();
            async move {
                let mut body = serde_json::json!({ "url": PUB_URL, "dest": "" });
                if let Some(h) = height {
                    body["height"] = serde_json::json!(h);
                }
                let (status, v) = post_import(&app, Some(&tok), "/I/import", body).await;
                assert_eq!(status, StatusCode::ACCEPTED, "{v}");
                let v = wait_finished(&app, &tok, v["job_id"].as_str().unwrap()).await;
                assert_eq!(v["state"], "done", "{v}");
                std::fs::read_to_string(share_root.join("out.txt")).unwrap()
            }
        };

        // A wish over the owner's cap clamps to the cap; below passes as is;
        // no wish takes the cap (LLD-29 п. 3.9).
        assert_eq!(run(Some(4000)).await, "1080");
        assert_eq!(run(Some(720)).await, "720");
        assert_eq!(run(None).await, "1080");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_import_cancel() {
        let key = SigningKey::from_bytes(&[35u8; 32]);
        let share = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let script = write_script(bin.path(), "sleep 30");
        let (app, tok) = import_app(&key, share.path(), Some(one_plugin(&script, &["{url}"], &["*"], 1080)), None);

        let (_, v) = post_import(
            &app, Some(&tok), "/I/import",
            serde_json::json!({ "url": PUB_URL, "dest": "" }),
        ).await;
        let job_id = v["job_id"].as_str().unwrap().to_string();

        // Let it actually start (the process must be up for the kill path).
        for _ in 0..200 {
            let (_, v) = get_status(&app, &tok, &job_id).await;
            if v["state"] == "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let r = app
            .clone()
            .oneshot(write_req("DELETE", &format!("/I/import/{job_id}"), Some(&tok), &[], b""))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);

        // Polls now answer 404 (the job is forgotten, LLD-29 п. 2.5).
        let (status, _) = get_status(&app, &tok, &job_id).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // The runner reaps the killed process and removes the job dir.
        for _ in 0..300 {
            if no_job_dirs(share.path()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(no_job_dirs(share.path()));

        // Cancelling the unknown id again -> 404.
        let r = app
            .oneshot(write_req("DELETE", &format!("/I/import/{job_id}"), Some(&tok), &[], b""))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_import_job_share_scoped() {
        // The job table is agent-global, but polls and cancels are share-bound:
        // a share:import token for J must not see or kill I's job, even knowing
        // its id (review finding on XR-141).
        let key = SigningKey::from_bytes(&[37u8; 32]);
        let dir_i = tempfile::tempdir().unwrap();
        let dir_j = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let script = write_script(bin.path(), "sleep 30");
        let mut shares = SharesMap::new();
        for (id, dir) in [("I", dir_i.path()), ("J", dir_j.path())] {
            shares.insert(
                id.into(),
                ShareRoot { path: dir.canonicalize().unwrap(), is_file: false, writable: true, import: true, git: false },
            );
        }
        let cache = Arc::new(HashCache::new());
        let state = Arc::new(AgentState {
            shares: RwLock::new(Arc::new(shares)),
            hub_key: key.verifying_key(),
            hash_cache: cache.clone(),
            identity: None,
            max_file_mb: None,
            import: ImportManager::new(Some(one_plugin(&script, &["{url}"], &["*"], 1080)), cache),
            git: git_manager_for_tests(),
            expose: RwLock::new(Arc::new(Vec::new())),
        });
        state.import.spawn_runner();
        let app = router(state);
        let tok_i = sign_share_token(&key, "I", &rwi_scope(), now_unix() + 1000);
        let tok_j = sign_share_token(&key, "J", &rwi_scope(), now_unix() + 1000);

        let (_, v) = post_import(
            &app, Some(&tok_i), "/I/import",
            serde_json::json!({ "url": PUB_URL, "dest": "" }),
        ).await;
        let job_id = v["job_id"].as_str().unwrap().to_string();

        // J's own valid token against I's job id: not found, nothing leaked.
        let r = app
            .clone()
            .oneshot(get_with_token(&format!("/J/import/{job_id}"), Some(&tok_j)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r = app
            .clone()
            .oneshot(write_req("DELETE", &format!("/J/import/{job_id}"), Some(&tok_j), &[], b""))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);

        // The job is alive and still owned by I, which can cancel it.
        let (status, _) = get_status(&app, &tok_i, &job_id).await;
        assert_eq!(status, StatusCode::OK);
        let r = app
            .oneshot(write_req("DELETE", &format!("/I/import/{job_id}"), Some(&tok_i), &[], b""))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_import_output_filter() {
        let key = SigningKey::from_bytes(&[36u8; 32]);
        let share = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        // The plugin leaves a visible file, a hidden cache, a reserved-looking
        // name and a subdir; only the visible file is published (LLD-29 п. 2.4).
        let script = write_script(
            bin.path(),
            "printf 'ok' > visible.txt\nprintf 'x' > .hidden\nprintf 'y' > '.xr-хитрость'\nmkdir sub\nprintf 'z' > sub/nested.txt",
        );
        let (app, tok) = import_app(&key, share.path(), Some(one_plugin(&script, &["{url}"], &["*"], 1080)), None);

        let (_, v) = post_import(
            &app, Some(&tok), "/I/import",
            serde_json::json!({ "url": PUB_URL, "dest": "" }),
        ).await;
        let v = wait_finished(&app, &tok, v["job_id"].as_str().unwrap()).await;
        assert_eq!(v["state"], "done", "{v}");
        assert_eq!(v["files"], serde_json::json!(["visible.txt"]));

        assert_eq!(std::fs::read(share.path().join("visible.txt")).unwrap(), b"ok");
        assert!(!share.path().join(".hidden").exists());
        assert!(!share.path().join(".xr-хитрость").exists());
        assert!(!share.path().join("sub").exists());
        assert!(no_job_dirs(share.path()));
    }

    // -- git contour (LLD-33) --------------------------------------------

    /// A one-share app with the git contour on, plus its read+write token and
    /// the state (the tests drive commits by hand instead of waiting out the
    /// watcher debounce). The watcher itself is live, as after a real reload.
    fn git_app(key: &SigningKey, dir: &Path) -> (Router, ShareToken, Arc<AgentState>) {
        let mut shares = SharesMap::new();
        let mut root = dir_share(dir.canonicalize().unwrap(), true);
        root.git = true;
        shares.insert("W".into(), root);
        let state = state_with(shares, key);
        state.git.rebuild(&state.snapshot(), &git_settings());
        let tok = sign_share_token(key, "W", &rw_scope(), now_unix() + 1000);
        (router(state.clone()), tok, state)
    }

    /// Клиентский git с герметичным конфигом: ни глобального конфига машины,
    /// ни её identity, чтобы тест не зависел от окружения. Blocking-обёртка
    /// для async-тестов.
    async fn run_git(dir: &Path, args: &[&str]) -> (bool, String) {
        let dir = dir.to_path_buf();
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let out = std::process::Command::new("git")
                .args(&refs)
                .current_dir(&dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_AUTHOR_NAME", "client")
                .env("GIT_AUTHOR_EMAIL", "client@example.com")
                .env("GIT_COMMITTER_NAME", "client")
                .env("GIT_COMMITTER_EMAIL", "client@example.com")
                .output()
                .expect("git runs");
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            (out.status.success(), text)
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn test_git_transport_gates() {
        let key = SigningKey::from_bytes(&[40u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let (app, wtok, _) = git_app(&key, dir.path());

        // Чужая шара -> 404.
        let r = app
            .clone()
            .oneshot(get_with_token(
                "/N/git/info/refs?service=git-upload-pack",
                Some(&wtok),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);

        // Контур выключен -> 403, до разговора о токене (LLD-33 п. 2.3).
        let mut shares = SharesMap::new();
        shares.insert("W".into(), dir_share(dir.path().canonicalize().unwrap(), true));
        let plain = router(state_with(shares, &key));
        let r = plain
            .clone()
            .oneshot(get_with_token(
                "/W/git/info/refs?service=git-upload-pack",
                Some(&wtok),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // git=true на неписабельной шаре из ручного состояния -> тот же 403:
        // лестница не зависит от того, кто построил таблицу.
        let mut shares = SharesMap::new();
        let mut root = dir_share(dir.path().canonicalize().unwrap(), false);
        root.git = true;
        shares.insert("W".into(), root);
        let ro = router(state_with(shares, &key));
        let r = ro
            .clone()
            .oneshot(get_with_token(
                "/W/git/info/refs?service=git-upload-pack",
                Some(&wtok),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // Нет токена -> 401; токен без write -> 403: весь контур, fetch
        // включительно, живёт под share:write (LLD-33 п. 2.3).
        let r = app
            .clone()
            .oneshot(get_with_token("/W/git/info/refs?service=git-upload-pack", None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let rtok = sign_share_token(&key, "W", SCOPE_READ, now_unix() + 1000);
        let r = app
            .clone()
            .oneshot(get_with_token(
                "/W/git/info/refs?service=git-upload-pack",
                Some(&rtok),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = app
            .clone()
            .oneshot(get_with_token("/W/git/head", Some(&rtok)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // Dumb-protocol probe without service -> 400 with the parameter named.
        let r = app
            .clone()
            .oneshot(get_with_token("/W/git/info/refs", Some(&wtok)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);

        // Годный запрос -> pkt-line преамбула и advertisement.
        let r = app
            .clone()
            .oneshot(get_with_token(
                "/W/git/info/refs?service=git-upload-pack",
                Some(&wtok),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            r.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/x-git-upload-pack-advertisement"
        );
        let adv = r.into_body().collect().await.unwrap().to_bytes();
        assert!(adv.starts_with(b"001e# service=git-upload-pack\n0000"), "{adv:?}");
    }

    /// Полный круг штатным git-клиентом против живого сервера: клон, fetch
    /// автокоммита, push с материализацией в рабочую папку и канал HEAD.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_git_clone_push_roundtrip() {
        let key = SigningKey::from_bytes(&[41u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let client = tempfile::tempdir().unwrap();
        let (app, wtok, state) = git_app(&key, dir.path());

        // Первый коммит до клона, чтобы у клиента была живая ветка main.
        std::fs::write(dir.path().join("note.md"), "первая").unwrap();
        state.git.get("W").unwrap().commit_scan().await.unwrap().expect("commit");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });

        let repo = client.path().join("repo");
        let url = format!("http://{addr}/W/git");
        let header = format!("http.extraHeader=Authorization: Bearer {}", blob(&wtok));
        let (ok, text) = run_git(client.path(), &["-c", &header, "clone", &url, "repo"]).await;
        assert!(ok, "{text}");
        assert_eq!(std::fs::read(repo.join("note.md")).unwrap(), "первая".as_bytes());

        // Правка на стороне клиента -> push -> post-receive кладёт файл в
        // рабочую папку агента и двигает канал HEAD.
        std::fs::write(repo.join("from-client.md"), "толчок").unwrap();
        let (ok, text) = run_git(&repo, &["add", "-A"]).await;
        assert!(ok, "{text}");
        let (ok, text) = run_git(&repo, &["commit", "-m", "client edit"]).await;
        assert!(ok, "{text}");
        let (ok, text) = run_git(&repo, &["-c", &header, "push", "origin", "HEAD:refs/heads/main"]).await;
        assert!(ok, "{text}");
        assert_eq!(std::fs::read(dir.path().join("from-client.md")).unwrap(), "толчок".as_bytes());

        let (ok, text) = run_git(&repo, &["rev-parse", "HEAD"]).await;
        assert!(ok, "{text}");
        let expected = text.trim().to_string();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if *state.git.get("W").unwrap().head_tx.borrow() == expected {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "HEAD не догнал push: {:?}",
                state.git.get("W").unwrap().head_tx.borrow()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_head_signed_longpoll() {
        let key = SigningKey::from_bytes(&[42u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let (app, wtok, state) = git_app(&key, dir.path());

        // Нерождённая main: пустой head и проверяемая подпись (XR-046 identity
        // фиксирован в state_with).
        let r = app.clone().oneshot(get_with_token("/W/git/head", Some(&wtok))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let v: serde_json::Value =
            serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(v["head"], "");
        let signed_at = v["signed_at"].as_u64().unwrap();
        let sig = v["sig"].as_str().unwrap();
        let agent_key = SigningKey::from_bytes(&[77u8; 32]).verifying_key();
        xr_proto::share::verify_git_head(sig, &agent_key, "W", signed_at, "").unwrap();
        assert!(xr_proto::share::verify_git_head(sig, &agent_key, "N", signed_at, "").is_err());

        // Припаркованный опрос с since=<текущий> просыпается на новом коммите,
        // а не отрабатывает таймаутом.
        let app2 = app.clone();
        let tok2 = wtok.clone();
        let parked = tokio::spawn(async move {
            app2
                .oneshot(get_with_token("/W/git/head?since=&wait=30", Some(&tok2)))
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(dir.path().join("a.md"), "a").unwrap();
        state.git.get("W").unwrap().commit_scan().await.unwrap().expect("commit");

        let started = std::time::Instant::now();
        let r = parked.await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(10), "long-poll не проснулся сам");
        let v: serde_json::Value =
            serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
        let head = v["head"].as_str().unwrap().to_string();
        assert!(!head.is_empty());
        let signed_at = v["signed_at"].as_u64().unwrap();
        let sig = v["sig"].as_str().unwrap();
        xr_proto::share::verify_git_head(sig, &agent_key, "W", signed_at, &head).unwrap();
    }

    /// Первый пуш в шару, пустовавшую при `share --git`, проходит и живым
    /// транспортом: unborn main не считается грязью, материализация кладёт
    /// файл коллеги в папку владельца.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_push_into_empty_share_over_http() {
        let key = SigningKey::from_bytes(&[44u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let client = tempfile::tempdir().unwrap();
        let (app, wtok, _state) = git_app(&key, dir.path());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });

        let repo = client.path().join("repo");
        let url = format!("http://{addr}/W/git");
        let header = format!("http.extraHeader=Authorization: Bearer {}", blob(&wtok));
        let (ok, text) = run_git(client.path(), &["-c", &header, "clone", &url, "repo"]).await;
        assert!(ok, "клон пустой шары упал: {text}");
        std::fs::write(repo.join("first.md"), "издалека").unwrap();
        let (ok, text) = run_git(&repo, &["add", "-A"]).await;
        assert!(ok, "{text}");
        let (ok, text) = run_git(&repo, &["commit", "-m", "first"]).await;
        assert!(ok, "{text}");
        let (ok, text) = run_git(&repo, &["-c", &header, "push", "origin", "HEAD:refs/heads/main"]).await;
        assert!(ok, "первый пуш в пустовавшую шару отклонён: {text}");
        assert_eq!(
            std::fs::read(dir.path().join("first.md")).unwrap(),
            "издалека".as_bytes()
        );
    }

    /// Push и авто-коммит сериализуются общим op_lock: пока он занят, обе
    /// стороны ждут, а не гоняют материализацию и коммит по папке наперегонки.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_receive_pack_serialized_by_op_lock() {
        let key = SigningKey::from_bytes(&[45u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let (app, wtok, state) = git_app(&key, dir.path());
        let share = state.git.get("W").unwrap();

        std::fs::write(dir.path().join("locked.md"), "x").unwrap();
        let guard = share.op_lock.clone().lock_owned().await;
        let committing = {
            let s = share.clone();
            tokio::spawn(async move { s.commit_scan().await })
        };
        let pushing = tokio::spawn(async move {
            app.oneshot(write_req("POST", "/W/git/git-receive-pack", Some(&wtok), &[], b""))
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!committing.is_finished(), "commit_scan не ждёт op_lock");
        assert!(!pushing.is_finished(), "receive-pack не ждёт op_lock");

        drop(guard);
        committing.await.unwrap().unwrap().expect("commit");
        let _ = pushing.await.unwrap();
    }

    /// Сжатое тело запроса (git шлёт gzip на больших пушах) распаковывается и
    /// доходит до git: ручной v0-запрос upload-pack через gzip.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_git_rpc_accepts_gzip_body() {
        let key = SigningKey::from_bytes(&[43u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let (app, wtok, state) = git_app(&key, dir.path());

        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        state.git.get("W").unwrap().commit_scan().await.unwrap().expect("commit");

        // v0-объявление: oid до " HEAD" в первой строке.
        let r = app
            .clone()
            .oneshot(get_with_token(
                "/W/git/info/refs?service=git-upload-pack",
                Some(&wtok),
            ))
            .await
            .unwrap();
        let adv = r.into_body().collect().await.unwrap().to_bytes();
        let idx = adv.windows(5).position(|w| w == b" HEAD").expect("HEAD in advertisement");
        let oid = String::from_utf8_lossy(&adv[idx - 40..idx]).into_owned();

        let payload = format!("0032want {oid}\n00000009done\n");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut enc, payload.as_bytes()).unwrap();
        let gz = enc.finish().unwrap();

        let req = write_req(
            "POST",
            "/W/git/git-upload-pack",
            Some(&wtok),
            &[("content-encoding", "gzip".into())],
            &gz,
        );
        let r = app.clone().oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers().get(header::CONTENT_TYPE).unwrap(), "application/x-git-upload-pack-result");
        let body = r.into_body().collect().await.unwrap().to_bytes();
        // Ответ на done без haves начинается NAK-строкой, дальше packfile.
        assert!(body.starts_with(b"0008NAK\n"), "{}", String::from_utf8_lossy(&body));
    }

    /// Страница шары (LLD-33 п. 2.8): лестница гейтов как у чтения, сама
    /// страница вшита и не тянет ничего снаружи.
    #[tokio::test]
    async fn test_web_page_gates_and_embedded() {
        let key = SigningKey::from_bytes(&[50u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let (app, wtok, _) = git_app(&key, dir.path());
        let rtok = sign_share_token(&key, "W", SCOPE_READ, now_unix() + 1000);

        // Чужая шара -> 404, токен без шары -> 401, чужая привязка -> 403.
        let r = app.clone().oneshot(get_with_token("/N/web", Some(&wtok))).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r = app.clone().oneshot(get_with_token("/W/web", None)).await.unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let other = sign_share_token(&key, "X", &rw_scope(), now_unix() + 1000);
        let r = app.clone().oneshot(get_with_token("/W/web", Some(&other))).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // Read-токен видит страницу; контур git для неё не обязателен.
        let r = app.clone().oneshot(get_with_token("/W/web", Some(&rtok))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            r.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let page = String::from_utf8_lossy(&body).into_owned();
        assert!(page.contains("</html>"), "страница доезжает целиком");
        for needle in ["src=\"http", "href=\"http", "<script src=", "<link "] {
            assert!(!page.contains(needle), "внешний запрос {needle} в странице");
        }

        // Шара без git-контура отдаёт ту же страницу: просмотр живёт чтением.
        let mut shares = SharesMap::new();
        shares.insert("W".into(), dir_share(dir.path().canonicalize().unwrap(), true));
        let plain = router(state_with(shares, &key));
        let r = plain.oneshot(get_with_token("/W/web", Some(&rtok))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    /// Роут git/log (LLD-33 п. 2.8): та же лестница гейтов, что у транспорта,
    /// и честные строки истории после живых коммитов.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_git_log_route() {
        let key = SigningKey::from_bytes(&[51u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let (app, wtok, state) = git_app(&key, dir.path());

        // Лестница: чужая шара 404, контур выключен 403, без токена 401,
        // read-only 403.
        let r = app.clone().oneshot(get_with_token("/N/git/log", Some(&wtok))).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let mut shares = SharesMap::new();
        shares.insert("W".into(), dir_share(dir.path().canonicalize().unwrap(), true));
        let plain = router(state_with(shares, &key));
        let r = plain.clone().oneshot(get_with_token("/W/git/log", Some(&wtok))).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = app.clone().oneshot(get_with_token("/W/git/log", None)).await.unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let rtok = sign_share_token(&key, "W", SCOPE_READ, now_unix() + 1000);
        let r = app.clone().oneshot(get_with_token("/W/git/log", Some(&rtok))).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // Два коммита по разным файлам.
        std::fs::write(dir.path().join("a.md"), "один").unwrap();
        state.git.get("W").unwrap().commit_scan().await.unwrap().expect("commit 1");
        std::fs::write(dir.path().join("b.md"), "два").unwrap();
        state.git.get("W").unwrap().commit_scan().await.unwrap().expect("commit 2");

        let r = app
            .clone()
            .oneshot(get_with_token("/W/git/log", Some(&wtok)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0]["sha"].as_str().unwrap().len(), 40);
        assert_eq!(rows[0]["author"].as_str().unwrap(), "tester");
        assert!(rows[0]["subject"].as_str().unwrap().contains("b.md"));
        assert!(rows[1]["subject"].as_str().unwrap().contains("a.md"));

        // Фильтр по пути и потолок limit.
        let r = app
            .clone()
            .oneshot(get_with_token("/W/git/log?path=a.md", Some(&wtok)))
            .await
            .unwrap();
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0]["subject"].as_str().unwrap().contains("a.md"));
        let r = app
            .clone()
            .oneshot(get_with_token("/W/git/log?limit=1", Some(&wtok)))
            .await
            .unwrap();
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(rows.len(), 1);

        // Плохие пути отбиваются до git: обход и магия pathspec.
        for bad in ["../x", "a/../../x", ":/"] {
            let r = app
                .clone()
                .oneshot(get_with_token(&format!("/W/git/log?path={bad}"), Some(&wtok)))
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad}");
        }
    }

    /// Роут git/diff (LLD-33 п. 2.8): дифф между соседними коммитами, фильтр
    /// по пути, отказ на плохом sha.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_git_diff_route() {
        let key = SigningKey::from_bytes(&[52u8; 32]);
        let dir = tempfile::tempdir().unwrap();
        let (app, wtok, state) = git_app(&key, dir.path());

        std::fs::write(dir.path().join("a.md"), "первая").unwrap();
        state.git.get("W").unwrap().commit_scan().await.unwrap().expect("commit 1");
        std::fs::write(dir.path().join("a.md"), "первая\nвторая").unwrap();
        state.git.get("W").unwrap().commit_scan().await.unwrap().expect("commit 2");

        let log = app
            .clone()
            .oneshot(get_with_token("/W/git/log", Some(&wtok)))
            .await
            .unwrap();
        let body = axum::body::to_bytes(log.into_body(), 1 << 20).await.unwrap();
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        let (new, old) = (rows[0]["sha"].as_str().unwrap(), rows[1]["sha"].as_str().unwrap());

        let r = app
            .clone()
            .oneshot(get_with_token(&format!("/W/git/diff?from={old}&to={new}"), Some(&wtok)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            r.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/plain; charset=utf-8"
        );
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("+++ b/a.md"), "{text}");
        assert!(text.contains("+вторая"), "{text}");

        // Bad sha и плохой путь -> 400, а не ошибка git.
        let r = app
            .clone()
            .oneshot(get_with_token("/W/git/diff?from=zzzz&to=1234", Some(&wtok)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let r = app
            .clone()
            .oneshot(get_with_token(
                &format!("/W/git/diff?from={old}&to={new}&path=../x"),
                Some(&wtok),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);

        // Read-токен историю не видит.
        let rtok = sign_share_token(&key, "W", SCOPE_READ, now_unix() + 1000);
        let r = app
            .clone()
            .oneshot(get_with_token(&format!("/W/git/diff?from={old}&to={new}"), Some(&rtok)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }
}
