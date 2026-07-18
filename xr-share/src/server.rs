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
use std::time::{SystemTime, UNIX_EPOCH};

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
use tokio::io::AsyncWriteExt;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use xr_proto::share::{
    sign_share_manifest, verify_share_token, ShareManifest, MANIFEST_SIGNED_AT_HEADER,
    MANIFEST_SIG_HEADER, SCOPE_IMPORT, SCOPE_READ, SCOPE_WRITE,
};

use crate::auth::extract_token;
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
                map.insert(e.share_id.clone(), ShareRoot { path: canon, is_file, writable, import });
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
}

impl AgentState {
    /// Cheap snapshot of the current share table (clones the `Arc`, not the map).
    fn snapshot(&self) -> Arc<SharesMap> {
        self.shares.read().expect("shares lock poisoned").clone()
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
    let target = {
        let shares = state.snapshot();
        let share = shares
            .get(share_id)
            .ok_or((StatusCode::NOT_FOUND, "no such share"))?;
        if !share.writable {
            return Err((StatusCode::FORBIDDEN, "share is read-only"));
        }
        check_token(state, share_id, SCOPE_WRITE, &req)?;
        resolve_within(&share.path, rel).map_err(|_| (StatusCode::FORBIDDEN, "path rejected"))?
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
    let target = {
        let shares = state.snapshot();
        let share = shares
            .get(share_id)
            .ok_or((StatusCode::NOT_FOUND, "no such share"))?;
        if !share.writable {
            return Err((StatusCode::FORBIDDEN, "share is read-only"));
        }
        check_token(state, share_id, SCOPE_WRITE, &req)?;
        resolve_within(&share.path, rel).map_err(|_| (StatusCode::FORBIDDEN, "path rejected"))?
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
/// Returns the share's canonical root for the POST handler's later steps.
fn import_gates(
    state: &AgentState,
    share_id: &str,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> Result<PathBuf, (StatusCode, &'static str)> {
    let shares = state.snapshot();
    let share = shares
        .get(share_id)
        .ok_or((StatusCode::NOT_FOUND, "no such share"))?;
    if !share.writable || !share.import || !state.import.has_plugins() {
        return Err((StatusCode::FORBIDDEN, "import is off for this share"));
    }
    check_token_parts(state, share_id, SCOPE_IMPORT, headers, uri)?;
    Ok(share.path.clone())
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
    let root = match import_gates(&state, &share_id, &headers, &uri) {
        Ok(root) => root,
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
    // Config snapshot: routing, limits and sandbox come from one version.
    let Some(cfg) = state.import.config() else {
        return (StatusCode::FORBIDDEN, "import is off for this share").into_response();
    };
    let Some(plugin) = import::route_plugin(&cfg.plugins, &host) else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "нет плагина под этот URL").into_response();
    };
    let height = match import::effective_height(req.height, plugin) {
        Ok(h) => h,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };

    let spec = JobSpec {
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
    match state.import.status(&job_id) {
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
    if state.import.cancel(&job_id) {
        tracing::info!("import share={share_id} job={job_id} cancelled");
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "no such job").into_response()
    }
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
        })
    }

    /// A directory share; `writable` opts into the write path (LLD-28).
    fn dir_share(path: PathBuf, writable: bool) -> ShareRoot {
        ShareRoot { path, is_file: false, writable, import: false }
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
        shares.insert("F".into(), ShareRoot { path: file.canonicalize().unwrap(), is_file: true, writable: false, import: false });
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
            ShareRoot { path: dir.canonicalize().unwrap(), is_file: false, writable: true, import: true },
        );
        let cache = Arc::new(HashCache::new());
        let state = Arc::new(AgentState {
            shares: RwLock::new(Arc::new(shares)),
            hub_key: key.verifying_key(),
            hash_cache: cache.clone(),
            identity: Some(SigningKey::from_bytes(&[77u8; 32])),
            max_file_mb,
            import: ImportManager::new(cfg, cache),
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
    /// loudly instead of forever).
    async fn wait_finished(app: &Router, tok: &ShareToken, job_id: &str) -> serde_json::Value {
        for _ in 0..300 {
            let (status, v) = get_status(app, tok, job_id).await;
            assert_eq!(status, StatusCode::OK, "status poll failed: {v}");
            match v.get("state").and_then(|s| s.as_str()) {
                Some("done") | Some("failed") => return v,
                _ => tokio::time::sleep(Duration::from_millis(30)).await,
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
            ShareRoot { path: share.path().canonicalize().unwrap(), is_file: false, writable: true, import: false },
        );
        let cache = Arc::new(HashCache::new());
        let no_flag = router(Arc::new(AgentState {
            shares: RwLock::new(Arc::new(shares)),
            hub_key: key.verifying_key(),
            hash_cache: cache.clone(),
            identity: None,
            max_file_mb: None,
            import: ImportManager::new(Some(cfg), cache),
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
            for _ in 0..300 {
                if let Some(dto) = mgr.status(&id) {
                    if dto.state == "done" || dto.state == "failed" {
                        return dto;
                    }
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
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
}
