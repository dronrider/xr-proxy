//! One-way mirror sync engine (LLD-19 §2.4, §3.4).
//!
//! Read-only share → local copy, no conflicts. The heart is a **pure** diff,
//! [`plan_sync`], that compares the agent's [`ShareManifest`] against local
//! state and emits what to fetch and what to delete. The network/filesystem
//! application ([`apply_plan`]) is a thin layer on top, reusing the
//! reqwest/SHA-256 pattern from [`crate::update`].
//!
//! Mirror semantics are **true mirror**: a file that disappeared on the server
//! is deleted locally (the UI must warn the user — §5.3). Deletion is confined
//! to the synced set under the destination root.
//!
//! Trust note: the manifest comes from the agent, so its paths are **not**
//! trusted for writing. [`safe_dest`] refuses any path that would escape the
//! destination directory — a compromised agent must not be able to plant files
//! outside the sync folder. `test_manifest_path_traversal_blocked` covers it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use xr_proto::share::{ShareGrant, ShareInfo, ShareManifest, ShareManifestEntry, ShareToken};

// ── Transfer control (progress + cancel) ─────────────────────────────
//
// One process-wide controller: the consumer (Android) drives a single transfer
// at a time, polls [`transfer_snapshot`] for a progress bar, and calls
// [`transfer_cancel`] to abort. The download loop checks the cancel flag between
// chunks and reports bytes, so a multi-GB transfer can be stopped promptly. Kept
// global to avoid threading a handle through every signature.

struct TransferControl {
    active: AtomicBool,
    cancel: AtomicBool,
    bytes_done: AtomicU64,
    bytes_total: AtomicU64,
    files_done: AtomicU64,
    files_total: AtomicU64,
    file: Mutex<String>,
}

impl TransferControl {
    const fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            bytes_done: AtomicU64::new(0),
            bytes_total: AtomicU64::new(0),
            files_done: AtomicU64::new(0),
            files_total: AtomicU64::new(0),
            file: Mutex::new(String::new()),
        }
    }
}

static TRANSFER: TransferControl = TransferControl::new();

/// RAII lock for the single active transfer. [`acquire`](TransferGuard::acquire)
/// resets the counters and marks a transfer running; dropping it releases the
/// lock. It returns `None` when a transfer is already in flight, so concurrent
/// callers (a foreground tap and the background mirror worker) never share the
/// global byte counters or write the same `.part` file at once. Without this the
/// progress bar overshoots (two streams add into one counter) and the partial
/// files corrupt each other.
#[must_use]
pub struct TransferGuard(());

impl TransferGuard {
    pub fn acquire(files_total: usize, bytes_total: u64) -> Option<Self> {
        if TRANSFER
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        TRANSFER.cancel.store(false, Ordering::Relaxed);
        TRANSFER.bytes_done.store(0, Ordering::Relaxed);
        TRANSFER.bytes_total.store(bytes_total, Ordering::Relaxed);
        TRANSFER.files_done.store(0, Ordering::Relaxed);
        TRANSFER.files_total.store(files_total as u64, Ordering::Relaxed);
        *TRANSFER.file.lock().expect("transfer lock") = String::new();
        Some(Self(()))
    }
}

impl Drop for TransferGuard {
    fn drop(&mut self) {
        // Clear the counters so a poll between this transfer and the next acquire
        // does not surface stale bytes (the "256 MB / 0 B" preparing artifact).
        // `active` is released last.
        TRANSFER.bytes_done.store(0, Ordering::Relaxed);
        TRANSFER.bytes_total.store(0, Ordering::Relaxed);
        TRANSFER.files_done.store(0, Ordering::Relaxed);
        TRANSFER.files_total.store(0, Ordering::Relaxed);
        *TRANSFER.file.lock().expect("transfer lock") = String::new();
        TRANSFER.active.store(false, Ordering::Relaxed);
    }
}

/// Mark the current file (and how many are already done).
pub fn transfer_file(path: &str, index: usize) {
    *TRANSFER.file.lock().expect("transfer lock") = path.to_string();
    TRANSFER.files_done.store(index as u64, Ordering::Relaxed);
}

/// Request cancellation of the running transfer.
pub fn transfer_cancel() {
    TRANSFER.cancel.store(true, Ordering::Relaxed);
}

fn transfer_cancelled() -> bool {
    TRANSFER.cancel.load(Ordering::Relaxed)
}

fn transfer_add_bytes(n: u64) {
    TRANSFER.bytes_done.fetch_add(n, Ordering::Relaxed);
}

/// A poll-able snapshot of transfer progress for the UI.
#[derive(Debug, Clone, Serialize)]
pub struct TransferSnapshot {
    pub active: bool,
    pub cancelled: bool,
    pub file: String,
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

pub fn transfer_snapshot() -> TransferSnapshot {
    TransferSnapshot {
        active: TRANSFER.active.load(Ordering::Relaxed),
        cancelled: TRANSFER.cancel.load(Ordering::Relaxed),
        file: TRANSFER.file.lock().expect("transfer lock").clone(),
        files_done: TRANSFER.files_done.load(Ordering::Relaxed),
        files_total: TRANSFER.files_total.load(Ordering::Relaxed),
        bytes_done: TRANSFER.bytes_done.load(Ordering::Relaxed),
        bytes_total: TRANSFER.bytes_total.load(Ordering::Relaxed),
    }
}

/// A file we already hold locally, keyed by share-relative path + its hash.
/// (De)serializable so an Android consumer using SAF storage — where file I/O
/// happens in Kotlin, not on a Rust filesystem path — can supply local state to
/// the pure [`plan_sync`] over JNI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalFile {
    pub path: String,
    pub sha256: String,
}

/// The diff: what to download and what to remove to make local match the server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SyncPlan {
    /// New or changed files to download (full manifest entry incl. hash).
    pub fetch: Vec<ShareManifestEntry>,
    /// Share-relative paths that vanished on the server → delete locally.
    pub delete: Vec<String>,
}

impl SyncPlan {
    pub fn is_empty(&self) -> bool {
        self.fetch.is_empty() && self.delete.is_empty()
    }
}

/// Pure diff between the server manifest and local state. SHA-256 is the change
/// signal: a path present both sides with equal hash is a no-op; differing hash
/// → fetch; present only on server → fetch; present only locally → delete.
/// Output is sorted for determinism.
pub fn plan_sync(manifest: &ShareManifest, local: &[LocalFile]) -> SyncPlan {
    plan_with_selection(manifest, local, None)
}

/// True if `path` is covered by `selection`: either listed exactly, or sitting
/// under a selected **folder prefix** (so ticking a folder mirrors everything in
/// it, including files added later). `None` selection covers everything.
fn path_selected(path: &str, selection: Option<&HashSet<String>>) -> bool {
    match selection {
        None => true,
        Some(sel) => sel
            .iter()
            .any(|s| path == s.as_str() || path.starts_with(&format!("{s}/"))),
    }
}

/// Like [`plan_sync`], but restricted to a **selected subset** of the share
/// (§9.6). `selection` is a set of manifest paths and/or folder prefixes the
/// consumer chose to mirror; the desired local state is `manifest ∩ selection`. A
/// file present locally but not in the desired set is deleted, so unticking
/// something (or its removal on the server) drops the local copy. `None` selects
/// the whole tree, i.e. exactly [`plan_sync`].
pub fn plan_with_selection(
    manifest: &ShareManifest,
    local: &[LocalFile],
    selection: Option<&HashSet<String>>,
) -> SyncPlan {
    let local_by_path: HashMap<&str, &str> = local
        .iter()
        .map(|f| (f.path.as_str(), f.sha256.as_str()))
        .collect();

    // Desired = manifest entries the selection covers (everything, if no selection).
    let desired: Vec<&ShareManifestEntry> = manifest
        .entries
        .iter()
        .filter(|e| path_selected(&e.path, selection))
        .collect();
    let desired_paths: HashSet<&str> = desired.iter().map(|e| e.path.as_str()).collect();

    let mut fetch: Vec<ShareManifestEntry> = desired
        .iter()
        .filter(|e| match local_by_path.get(e.path.as_str()) {
            // Identical hash → already have it.
            Some(local_sha) => !local_sha.eq_ignore_ascii_case(&e.sha256),
            // Not present locally → new.
            None => true,
        })
        .map(|e| (*e).clone())
        .collect();

    // Delete anything local that is not in the desired set: server-deleted files
    // and files the consumer unticked both leave the desired set.
    let mut delete: Vec<String> = local
        .iter()
        .filter(|f| !desired_paths.contains(f.path.as_str()))
        .map(|f| f.path.clone())
        .collect();

    fetch.sort_by(|a, b| a.path.cmp(&b.path));
    delete.sort();
    SyncPlan { fetch, delete }
}

/// Resolve a share-relative path to a local destination under `root`, refusing
/// traversal (`..`, absolute, backslash, NUL, empty). Returns `None` to reject —
/// the manifest is not trusted to dictate where we write.
pub fn safe_dest(root: &Path, rel: &str) -> Option<PathBuf> {
    // Manifest paths are always relative; a backslash, NUL, or leading slash
    // (absolute) is malformed and refused outright.
    if rel.starts_with('/') || rel.contains('\\') || rel.contains('\0') {
        return None;
    }
    let mut out = root.to_path_buf();
    let mut components = 0usize;
    for comp in rel.split('/') {
        match comp {
            "" | "." => continue,
            ".." => return None,
            other => {
                let p = Path::new(other);
                if p.is_absolute() || p.components().count() != 1 {
                    return None;
                }
                out.push(other);
                components += 1;
            }
        }
    }
    if components == 0 {
        return None;
    }
    Some(out)
}

/// Scan a local directory into [`LocalFile`]s (relative forward-slash paths +
/// SHA-256). Symlinks are skipped. Used to compute local state before a diff.
pub fn scan_local_dir(root: &Path) -> std::io::Result<Vec<LocalFile>> {
    let mut out = Vec::new();
    if root.exists() {
        scan_dir(root, root, &mut out)?;
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn scan_dir(root: &Path, dir: &Path, out: &mut Vec<LocalFile>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            scan_dir(root, &path, out)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            out.push(LocalFile {
                path: rel,
                sha256: sha256_file(&path)?,
            });
        }
    }
    Ok(())
}

// ── Storage migration (XR-043) ───────────────────────────────────────
//
// Changing a share's storage directory moves the already-downloaded data, it
// does not re-download it: shares run to tens of GB and the move is offline. On
// the same volume every file is a rename (instant, no extra space); across
// volumes it is a copy+remove, pre-checked against free space so a full target
// fails before the first byte moves. The diff is hash-based, so a moved file is
// seen as already-present on the next sync and never re-fetched.

/// Suffix [`download_entry`] gives an in-flight download. Such a leftover is
/// incomplete, so migration drops it instead of moving half a file.
const PART_SUFFIX: &str = ".xrsync-part";

/// Outcome of [`migrate_dir`]. `moved`/`bytes` count relocated files; `conflicts`
/// are paths left in place because the destination already held a **different**
/// file there (the source copy is kept, the user decides); `failed` are per-file
/// errors; `cancelled` is true when the user aborted (already-moved files stay).
#[derive(Debug, Default, Clone, Serialize)]
pub struct MigrateReport {
    pub moved: usize,
    pub bytes: u64,
    pub conflicts: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub cancelled: bool,
}

struct MigFile {
    abs: PathBuf,
    rel: String,
    size: u64,
}

/// Count the files (and total bytes) [`migrate_dir`] would move, ignoring
/// `.xrsync-part` leftovers. Cheap (metadata only, no hashing): the caller sizes
/// the progress bar with it before taking the transfer lock.
pub fn dir_totals(root: &Path) -> (usize, u64) {
    let files = collect_files(root);
    let bytes = files.iter().map(|f| f.size).sum();
    (files.len(), bytes)
}

/// Enumerate real files under `root` (relative forward-slash path + size),
/// skipping symlinks and `.xrsync-part` leftovers. Sorted for determinism.
fn collect_files(root: &Path) -> Vec<MigFile> {
    let mut out = Vec::new();
    if root.exists() {
        let _ = collect_dir(root, root, &mut out);
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

fn collect_dir(root: &Path, dir: &Path, out: &mut Vec<MigFile>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            collect_dir(root, &path, out)?;
        } else if ft.is_file() {
            if entry.file_name().to_string_lossy().ends_with(PART_SUFFIX) {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push(MigFile { abs: path, rel, size });
        }
    }
    Ok(())
}

/// Move every file under `src_root` into `dst_root`, preserving relative paths,
/// so a storage-directory change keeps the already-downloaded data (XR-043 §3).
/// A destination file that is byte-identical is treated as already-migrated (the
/// source duplicate is removed); a **differing** destination file is a conflict
/// and left untouched. Reports per-file outcomes; only an upfront failure
/// (unreadable source, full target volume) returns `Err`. Honours the global
/// transfer cancel flag and feeds the progress controller the UI polls.
pub fn migrate_dir(src_root: &Path, dst_root: &Path) -> Result<MigrateReport, String> {
    let mut report = MigrateReport::default();
    if src_root == dst_root || !src_root.exists() {
        return Ok(report); // same place, or nothing downloaded yet
    }
    // A nested pair would chase its own output (move into a sub/parent of the
    // source). The picker prevents this; belt and braces.
    if dst_root.starts_with(src_root) || src_root.starts_with(dst_root) {
        return Err("nested directories".into());
    }
    std::fs::create_dir_all(dst_root).map_err(|e| format!("mkdir dst: {e}"))?;

    let files = collect_files(src_root);

    // Free-space precheck only matters across volumes (a same-volume rename needs
    // no extra space). Failing here, before the first file moves, keeps a full
    // target from leaving a half-migrated share.
    let cross = match (device_of(src_root), device_of(dst_root)) {
        (Some(a), Some(b)) => a != b,
        _ => true, // unknown -> assume cross and check space
    };
    if cross {
        let total: u64 = files.iter().map(|f| f.size).sum();
        if let Some(free) = free_space(dst_root) {
            if free < total {
                return Err(format!("no_space: need {total}, free {free}"));
            }
        }
    }

    for f in &files {
        if transfer_cancelled() {
            report.cancelled = true;
            break;
        }
        transfer_file(&f.rel, report.moved);
        let Some(dst) = safe_dest(dst_root, &f.rel) else {
            report.failed.push((f.rel.clone(), "unsafe path".into()));
            continue;
        };
        if dst.exists() {
            match files_identical(&f.abs, &dst) {
                Ok(true) => {
                    let _ = std::fs::remove_file(&f.abs);
                    prune_empty_dirs(src_root, &f.abs);
                    report.moved += 1;
                    report.bytes += f.size;
                    transfer_add_bytes(f.size);
                }
                Ok(false) => report.conflicts.push(f.rel.clone()),
                Err(e) => report.failed.push((f.rel.clone(), format!("compare: {e}"))),
            }
            continue;
        }
        if let Some(parent) = dst.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                report.failed.push((f.rel.clone(), format!("mkdir: {e}")));
                continue;
            }
        }
        match move_file(&f.abs, &dst) {
            Ok(()) => {
                prune_empty_dirs(src_root, &f.abs);
                report.moved += 1;
                report.bytes += f.size;
                transfer_add_bytes(f.size);
            }
            Err(e) => report.failed.push((f.rel.clone(), e)),
        }
    }

    // Tidy the old location: drop incomplete leftovers and now-empty dirs.
    // Skipped on cancel so a half migration stays resumable from a clean tree.
    if !report.cancelled {
        remove_part_leftovers(src_root);
        remove_empty_dirs(src_root);
    }
    Ok(report)
}

/// Move one file, preferring an instant rename and falling back to copy+remove
/// when the destination is on another filesystem (`EXDEV`).
fn move_file(src: &Path, dst: &Path) -> Result<(), String> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if is_exdev(&e) => {
            std::fs::copy(src, dst).map_err(|e| format!("copy: {e}"))?;
            std::fs::remove_file(src).map_err(|e| format!("remove src: {e}"))?;
            Ok(())
        }
        Err(e) => Err(format!("rename: {e}")),
    }
}

/// Same bytes? Compares size first (cheap), then SHA-256 over both files.
fn files_identical(a: &Path, b: &Path) -> std::io::Result<bool> {
    let (ma, mb) = (std::fs::metadata(a)?, std::fs::metadata(b)?);
    if ma.len() != mb.len() {
        return Ok(false);
    }
    Ok(sha256_file(a)? == sha256_file(b)?)
}

fn remove_part_leftovers(root: &Path) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            remove_part_leftovers(&path);
        } else if entry.file_name().to_string_lossy().ends_with(PART_SUFFIX) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Remove empty directories under `root` bottom-up, including `root` itself once
/// empty (the share's old folder is gone after a full migration). A directory
/// still holding a conflicted file is non-empty, so `remove_dir` no-ops and it
/// survives.
fn remove_empty_dirs(root: &Path) {
    if let Ok(rd) = std::fs::read_dir(root) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                remove_empty_dirs(&path);
            }
        }
    }
    let _ = std::fs::remove_dir(root);
}

#[cfg(unix)]
fn device_of(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.dev())
}

#[cfg(not(unix))]
fn device_of(_path: &Path) -> Option<u64> {
    None
}

#[cfg(unix)]
fn free_space(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs fills a zeroed struct from a valid C path; on success we
    // read two scalar fields only.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(not(unix))]
fn free_space(_path: &Path) -> Option<u64> {
    None
}

fn is_exdev(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::EXDEV)
}

// ── Network application ──────────────────────────────────────────────

/// Outcome of [`apply_plan`]: which paths were fetched/deleted, and per-path
/// failures (path, reason) — a failure on one file never aborts the rest.
#[derive(Debug, Default, Clone, Serialize)]
pub struct ApplyReport {
    pub fetched: Vec<String>,
    pub deleted: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// Result of [`sync_share`]: the computed plan, plus the apply outcome (absent
/// on a `dry_run`, so the UI can preview deletions before committing — §5.3).
#[derive(Debug, Clone, Serialize)]
pub struct SyncResult {
    pub plan: SyncPlan,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<ApplyReport>,
}

/// GET the hub's public share index (`GET /api/v1/shares`): for each share, the
/// consumer-facing name + `addr:port` + pinned agent key. No token needed — the
/// hub is just a directory; access is gated at the agent.
pub async fn list_shares(hub_url: &str, timeout: Duration) -> Result<Vec<ShareInfo>, String> {
    let client = http_client(timeout)?;
    let url = format!("{}/api/v1/shares", hub_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("http_{}", resp.status().as_u16()));
    }
    resp.json::<Vec<ShareInfo>>()
        .await
        .map_err(|e| format!("parse: {e}"))
}

/// GET the shares attached to an invite (`GET /api/v1/invite/{token}/shares`,
/// §9.5). The invite is the access anchor: each grant carries the agent address,
/// the key to pin, and a hub-minted access token. A `410` means the invite is
/// expired or revoked.
pub async fn list_invite_shares(
    hub_url: &str,
    invite_token: &str,
    timeout: Duration,
) -> Result<Vec<ShareGrant>, String> {
    let client = http_client(timeout)?;
    let url = format!(
        "{}/api/v1/invite/{}/shares",
        hub_url.trim_end_matches('/'),
        invite_token
    );
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("http_{}", resp.status().as_u16()));
    }
    resp.json::<Vec<ShareGrant>>()
        .await
        .map_err(|e| format!("parse: {e}"))
}

/// One-shot mirror: fetch the manifest, scan `dest_root`, diff, and (unless
/// `dry_run`) apply. This is what a background sync job calls; `dry_run` returns
/// the plan only, so the UI can warn about deletions before committing.
pub async fn sync_share(
    agent_url: &str,
    token: &ShareToken,
    dest_root: &Path,
    dry_run: bool,
    timeout: Duration,
) -> Result<SyncResult, String> {
    sync_share_selected(agent_url, token, dest_root, None, dry_run, timeout).await
}

/// [`sync_share`] limited to a selected subset of the share (§9.6). `selection`
/// is the set of manifest paths to mirror; `None` mirrors the whole tree.
pub async fn sync_share_selected(
    agent_url: &str,
    token: &ShareToken,
    dest_root: &Path,
    selection: Option<&HashSet<String>>,
    dry_run: bool,
    timeout: Duration,
) -> Result<SyncResult, String> {
    let manifest = fetch_manifest(agent_url, token, timeout).await?;
    let local = scan_local_dir(dest_root).map_err(|e| format!("scan: {e}"))?;
    let plan = plan_with_selection(&manifest, &local, selection);
    if dry_run {
        return Ok(SyncResult { plan, report: None });
    }
    let bytes_total: u64 = plan.fetch.iter().map(|e| e.size).sum();
    // Refuse to run a second transfer in parallel with another one (e.g. the
    // background worker while the user taps a file): they would corrupt the
    // shared progress and the same `.part`. The skipped sync runs next cycle.
    let _guard = match TransferGuard::acquire(plan.fetch.len(), bytes_total) {
        Some(g) => g,
        None => return Err("busy".into()),
    };
    let report = apply_plan(agent_url, token, &plan, dest_root, timeout).await;
    Ok(SyncResult { plan, report: Some(report) })
}

/// GET the agent's manifest, presenting `token` (verified by the agent offline).
pub async fn fetch_manifest(
    agent_url: &str,
    token: &ShareToken,
    timeout: Duration,
) -> Result<ShareManifest, String> {
    let client = http_client(timeout)?;
    let url = format!("{}/manifest", agent_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .bearer_auth(token_blob(token))
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("http_{}", resp.status().as_u16()));
    }
    resp.json::<ShareManifest>()
        .await
        .map_err(|e| format!("parse: {e}"))
}

/// Download one entry to `dest_root`, streaming + verifying SHA-256, and only
/// publishing the file (atomic rename from a `.part`) once the hash matches —
/// a truncated/corrupt transfer never appears as a real file (§5.5).
pub async fn download_entry(
    agent_url: &str,
    token: &ShareToken,
    entry: &ShareManifestEntry,
    dest_root: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let dest = safe_dest(dest_root, &entry.path).ok_or_else(|| format!("unsafe path: {}", entry.path))?;
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("mkdir: {e}"))?;
    }
    let part = dest.with_file_name(format!(
        "{}{PART_SUFFIX}",
        dest.file_name().and_then(|n| n.to_str()).unwrap_or("download")
    ));

    use tokio::io::AsyncWriteExt;

    // Resume: if a shorter-than-expected partial is on disk, ask for the rest
    // with a Range request rather than re-downloading from byte zero (a 20 GB
    // file should not restart after a blip). A complete/oversized stale partial
    // is discarded. Integrity is still the final SHA-256 over the whole file.
    let mut resume_from: u64 = 0;
    if let Ok(meta) = tokio::fs::metadata(&part).await {
        let n = meta.len();
        if n > 0 && n < entry.size {
            resume_from = n;
        } else {
            let _ = tokio::fs::remove_file(&part).await;
        }
    }

    let client = http_client(timeout)?;
    let url = format!("{}/file/{}", agent_url.trim_end_matches('/'), encode_path(&entry.path));
    let mut req = client.get(&url).bearer_auth(token_blob(token));
    if resume_from > 0 {
        req = req.header("Range", format!("bytes={resume_from}-"));
    }
    let mut resp = req.send().await.map_err(|e| format!("network: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("http_{}", resp.status().as_u16()));
    }

    // The agent honoured the range only if it answered 206; on a plain 200 the
    // body is the whole file, so we must restart to avoid corrupting the partial.
    let appending = resume_from > 0 && resp.status().as_u16() == 206;

    let mut hasher = Sha256::new();
    let mut file = if appending {
        // Re-hash the bytes already on disk so the final check covers them.
        read_into_hasher(&part, &mut hasher).await.map_err(|e| format!("read part: {e}"))?;
        transfer_add_bytes(resume_from);
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(&part)
            .await
            .map_err(|e| format!("open part: {e}"))?
    } else {
        tokio::fs::File::create(&part).await.map_err(|e| format!("create: {e}"))?
    };

    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("read: {e}"))? {
        if transfer_cancelled() {
            // Keep the partial so the next attempt resumes from here.
            let _ = file.flush().await;
            return Err("cancelled".into());
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(|e| format!("write: {e}"))?;
        transfer_add_bytes(chunk.len() as u64);
    }
    file.flush().await.map_err(|e| format!("flush: {e}"))?;
    drop(file);

    let got = hex_lower(&hasher.finalize());
    // An empty expected hash means the listing hasn't hashed this file yet
    // (XR-039 cold cache): skip verification rather than fail, since there is
    // nothing to compare against. A non-empty mismatch is still a hard error.
    if !entry.sha256.is_empty() && !got.eq_ignore_ascii_case(&entry.sha256) {
        // A corrupt result is discarded so the next attempt starts clean.
        let _ = tokio::fs::remove_file(&part).await;
        return Err(format!("sha256 mismatch (want {}, got {got})", entry.sha256));
    }
    tokio::fs::rename(&part, &dest)
        .await
        .map_err(|e| format!("rename: {e}"))
}

/// Stream a file's existing bytes through a hasher (for resuming a partial).
async fn read_into_hasher(path: &Path, hasher: &mut Sha256) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(path).await?;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(())
}

/// Apply a plan against a live agent: download fetches (verified), then delete
/// removed files. Per-file errors are collected, not fatal.
pub async fn apply_plan(
    agent_url: &str,
    token: &ShareToken,
    plan: &SyncPlan,
    dest_root: &Path,
    timeout: Duration,
) -> ApplyReport {
    let mut report = ApplyReport::default();

    for (i, entry) in plan.fetch.iter().enumerate() {
        if transfer_cancelled() {
            report.failed.push((entry.path.clone(), "cancelled".into()));
            break;
        }
        transfer_file(&entry.path, i);
        match download_entry(agent_url, token, entry, dest_root, timeout).await {
            Ok(()) => report.fetched.push(entry.path.clone()),
            Err(e) => report.failed.push((entry.path.clone(), e)),
        }
    }

    for rel in &plan.delete {
        let Some(path) = safe_dest(dest_root, rel) else {
            report.failed.push((rel.clone(), "unsafe path".into()));
            continue;
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {
                report.deleted.push(rel.clone());
                prune_empty_dirs(dest_root, &path);
            }
            // Already gone is success for a mirror; anything else is a failure.
            Err(_) if !path.exists() => report.deleted.push(rel.clone()),
            Err(e) => report.failed.push((rel.clone(), format!("delete: {e}"))),
        }
    }

    report
}

/// Remove now-empty parent directories up to (not including) `root`.
fn prune_empty_dirs(root: &Path, file: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) {
            break;
        }
        if std::fs::remove_dir(d).is_err() {
            break; // non-empty or error → stop climbing
        }
        dir = d.parent();
    }
}

fn http_client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        // Fail fast if the agent is unreachable, but let the overall timeout be
        // generous so a large file download is not cut off mid-transfer (callers
        // pass a long total for sync, a short one for the manifest/listing).
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .build()
        .map_err(|e| format!("http client: {e}"))
}

/// Encode a ShareToken into the URL-safe base64 blob the agent expects
/// (`Authorization: Bearer <blob>`; base64url-no-pad of the JSON).
fn token_blob(token: &ShareToken) -> String {
    use base64::Engine;
    let json = serde_json::to_vec(token).unwrap_or_default();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

/// Percent-encode a share path for the URL, preserving `/` separators.
fn encode_path(p: &str) -> String {
    p.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{:02x}", b);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, sha: &str) -> ShareManifestEntry {
        ShareManifestEntry {
            path: path.into(),
            size: 10,
            mtime: 1,
            sha256: sha.into(),
        }
    }
    fn local(path: &str, sha: &str) -> LocalFile {
        LocalFile { path: path.into(), sha256: sha.into() }
    }
    fn manifest(entries: Vec<ShareManifestEntry>) -> ShareManifest {
        ShareManifest { entries }
    }

    #[test]
    fn test_plan_sync_new_file() {
        let m = manifest(vec![entry("a.txt", "aaa")]);
        let plan = plan_sync(&m, &[]);
        assert_eq!(plan.fetch.len(), 1);
        assert_eq!(plan.fetch[0].path, "a.txt");
        assert!(plan.delete.is_empty());
    }

    #[test]
    fn test_plan_sync_changed_hash_refetches() {
        let m = manifest(vec![entry("a.txt", "NEWHASH")]);
        let plan = plan_sync(&m, &[local("a.txt", "oldhash")]);
        assert_eq!(plan.fetch.len(), 1);
        assert_eq!(plan.fetch[0].sha256, "NEWHASH");
        assert!(plan.delete.is_empty());
    }

    #[test]
    fn test_plan_sync_identical_is_noop() {
        let m = manifest(vec![entry("a.txt", "SAME")]);
        // case-insensitive hash compare
        let plan = plan_sync(&m, &[local("a.txt", "same")]);
        assert!(plan.is_empty(), "identical file must be a no-op");
    }

    #[test]
    fn test_plan_sync_server_deleted_removes_local() {
        let m = manifest(vec![entry("keep.txt", "k")]);
        let plan = plan_sync(&m, &[local("keep.txt", "k"), local("gone.txt", "g")]);
        assert!(plan.fetch.is_empty());
        assert_eq!(plan.delete, vec!["gone.txt".to_string()]);
    }

    #[test]
    fn test_plan_sync_empty_manifest_deletes_all() {
        let plan = plan_sync(&manifest(vec![]), &[local("a", "1"), local("b", "2")]);
        assert!(plan.fetch.is_empty());
        assert_eq!(plan.delete, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn test_plan_sync_mixed_and_sorted() {
        let m = manifest(vec![
            entry("z.txt", "z"),     // new
            entry("a.txt", "same"),  // unchanged
            entry("m.txt", "new"),   // changed
        ]);
        let local = vec![
            local("a.txt", "same"),
            local("m.txt", "old"),
            local("old.txt", "o"), // server-deleted
        ];
        let plan = plan_sync(&m, &local);
        // fetch = changed + new, sorted by path
        assert_eq!(
            plan.fetch.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            vec!["m.txt", "z.txt"]
        );
        assert_eq!(plan.delete, vec!["old.txt".to_string()]);
    }

    #[test]
    fn test_plan_selection_subset() {
        let m = manifest(vec![entry("a.txt", "a"), entry("b.txt", "b"), entry("c.txt", "c")]);
        let sel: HashSet<String> = ["a.txt".to_string(), "c.txt".to_string()].into_iter().collect();
        // have a.txt (current) and b.txt (unselected → must be removed).
        let have = vec![local("a.txt", "a"), local("b.txt", "b")];

        let plan = plan_with_selection(&m, &have, Some(&sel));
        // fetch: only c.txt (new + selected); a.txt is identical, b.txt unselected.
        assert_eq!(
            plan.fetch.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            vec!["c.txt"]
        );
        // delete: b.txt (present locally but outside the selection).
        assert_eq!(plan.delete, vec!["b.txt".to_string()]);

        // No selection is identical to plan_sync (whole tree).
        assert_eq!(plan_with_selection(&m, &have, None), plan_sync(&m, &have));

        // A changed hash inside the selection is re-fetched.
        let m2 = manifest(vec![entry("a.txt", "NEW"), entry("c.txt", "c")]);
        let plan2 = plan_with_selection(&m2, &[local("a.txt", "old")], Some(&sel));
        assert_eq!(plan2.fetch.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), vec!["a.txt", "c.txt"]);
    }

    #[test]
    fn test_plan_folder_prefix_selection() {
        let m = manifest(vec![
            entry("docs/a.txt", "a"),
            entry("docs/sub/b.txt", "b"),
            entry("other/c.txt", "c"),
        ]);
        // Ticking the "docs" folder mirrors everything under it, not "other".
        let sel: HashSet<String> = ["docs".to_string()].into_iter().collect();
        let plan = plan_with_selection(&m, &[], Some(&sel));
        assert_eq!(
            plan.fetch.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            vec!["docs/a.txt", "docs/sub/b.txt"]
        );
        // A "docs"-prefixed name that is not actually under the folder is excluded.
        let m2 = manifest(vec![entry("docs2/x", "x"), entry("docs/y", "y")]);
        let plan2 = plan_with_selection(&m2, &[], Some(&sel));
        assert_eq!(plan2.fetch.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), vec!["docs/y"]);
    }

    #[test]
    fn test_manifest_path_traversal_blocked() {
        let root = Path::new("/srv/sync");
        // Legitimate.
        assert_eq!(safe_dest(root, "a.txt"), Some(root.join("a.txt")));
        assert_eq!(safe_dest(root, "sub/b.bin"), Some(root.join("sub/b.bin")));
        // A malicious manifest path must never resolve outside root.
        for bad in ["../evil", "..", "a/../../b", "/etc/passwd", "sub/../../x", "", ".", "x\\y", "x\0y"] {
            assert_eq!(safe_dest(root, bad), None, "must reject: {bad:?}");
        }
    }

    #[test]
    fn scan_then_plan_roundtrips_to_empty() {
        // Scanning a dir and diffing it against a manifest that mirrors it
        // exactly should produce an empty plan.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.bin"), b"world").unwrap();

        let local = scan_local_dir(dir.path()).unwrap();
        assert_eq!(local.len(), 2);

        let m = manifest(
            local
                .iter()
                .map(|f| entry(&f.path, &f.sha256))
                .collect(),
        );
        assert!(plan_sync(&m, &local).is_empty());

        // Now drop a local file → it should be re-fetched.
        let partial: Vec<_> = local.iter().filter(|f| f.path == "a.txt").cloned().collect();
        let plan = plan_sync(&m, &partial);
        assert_eq!(plan.fetch.iter().map(|e| e.path.clone()).collect::<Vec<_>>(), vec!["sub/b.bin"]);
    }

    // ── migrate_dir (XR-043) ─────────────────────────────────────────

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    #[test]
    fn migrate_moves_tree_and_clears_source() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("sub/b.bin"), b"world").unwrap();
        // An incomplete download must be dropped, not migrated.
        std::fs::write(src.path().join("c.bin.xrsync-part"), b"partial").unwrap();

        let rep = migrate_dir(src.path(), dst.path()).unwrap();
        assert_eq!(rep.moved, 2);
        assert!(rep.conflicts.is_empty() && rep.failed.is_empty());

        assert_eq!(read(&dst.path().join("a.txt")), "hello");
        assert_eq!(read(&dst.path().join("sub/b.bin")), "world");
        // The diff would now see the moved files as already present.
        let m = manifest(
            scan_local_dir(dst.path()).unwrap().iter().map(|f| entry(&f.path, &f.sha256)).collect(),
        );
        assert!(plan_sync(&m, &scan_local_dir(dst.path()).unwrap()).is_empty());
        // Source folder is gone (the leftover .part was dropped, dirs pruned).
        assert!(!src.path().exists());
    }

    #[test]
    fn migrate_skips_identical_keeps_conflict() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("same.txt"), b"DATA").unwrap();
        std::fs::write(src.path().join("diff.txt"), b"src-version").unwrap();
        // Destination already holds these paths: one identical, one different.
        std::fs::write(dst.path().join("same.txt"), b"DATA").unwrap();
        std::fs::write(dst.path().join("diff.txt"), b"dst-version").unwrap();

        let rep = migrate_dir(src.path(), dst.path()).unwrap();
        // Identical file counts as migrated; its source duplicate is removed.
        assert_eq!(rep.moved, 1);
        assert_eq!(rep.conflicts, vec!["diff.txt".to_string()]);
        assert!(!src.path().join("same.txt").exists());
        // Conflict: destination untouched, source copy left for the user.
        assert_eq!(read(&dst.path().join("diff.txt")), "dst-version");
        assert_eq!(read(&src.path().join("diff.txt")), "src-version");
    }

    #[test]
    fn migrate_noops_when_same_or_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x"), b"1").unwrap();
        // Same source and destination: nothing to do, files untouched.
        let rep = migrate_dir(dir.path(), dir.path()).unwrap();
        assert_eq!(rep.moved, 0);
        assert!(dir.path().join("x").exists());

        // Nested destination is refused outright.
        let nested = dir.path().join("inside");
        assert!(migrate_dir(dir.path(), &nested).is_err());

        // A never-downloaded share (missing source) is a clean no-op.
        let missing = dir.path().join("nope");
        let rep = migrate_dir(&missing, dir.path()).unwrap();
        assert_eq!(rep.moved, 0);
    }
}
