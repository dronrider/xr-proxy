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
use xr_proto::share::{
    scope_contains, RelayGrant, ShareGrant, ShareInfo, ShareManifest, ShareManifestEntry,
    ShareToken, SCOPE_IMPORT, SCOPE_WRITE,
};

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
    /// Size in bytes: the fallback change signal for manifest entries whose
    /// hash the agent has not computed yet (XR-097). `default` keeps older
    /// serialized producers without the field parseable.
    #[serde(default)]
    pub size: u64,
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
/// → fetch; present only on server → fetch; present only locally → delete. An
/// entry whose hash the agent has not computed yet (empty sha256, cold cache)
/// falls back to size comparison instead of re-fetching (XR-097).
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
    let local_by_path: HashMap<&str, &LocalFile> = local
        .iter()
        .map(|f| (f.path.as_str(), f))
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
            Some(l) if !e.sha256.is_empty() => !l.sha256.eq_ignore_ascii_case(&e.sha256),
            // The agent has not hashed this file yet (a fresh agent serves the
            // listing with empty sha256 while its cache warms up, see xr-share
            // build_listing), so size is the only change signal left. Treating
            // the empty hash as "changed" re-downloaded whole shares right
            // after an agent restart (XR-097); a same-size change slips this
            // round and is caught by the next sync once the cache is warm.
            Some(l) => l.size != e.size,
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
    scan_local_dir_indexed(root, &mut HashIndex::new())
}

/// Share-relative forward-slash path of `path` under `root`: the form manifest
/// entries, plans and the hash index key on.
fn rel_slash(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

// ── Persistent hash index (XR-098, LLD-24) ──────────────────────────
//
// Re-hashing every local file on every sync holds a multi-GB share in
// "preparing" for tens of seconds and burns battery on each background cycle.
// Like the agent's HashCache (xr-share manifest.rs) a file is re-hashed only
// when its (size, mtime) changed, but the consumer's index also survives
// process restarts by persisting to a JSON file. Keys are share-relative:
// a storage migration (XR-043) renames files, which keeps mtime but changes
// the root, so absolute keys would invalidate the whole index at once.

const HASH_INDEX_VERSION: u32 = 1;

/// Per-share persistent `(relative path, size, mtime) -> sha256` index. Purely
/// an accelerator for [`scan_local_dir_indexed`]: losing or corrupting it costs
/// one full re-hash, never correctness. Like the agent's cache, it deliberately
/// misses a swapped file with identical size and mtime.
#[derive(Serialize, Deserialize)]
pub struct HashIndex {
    version: u32,
    entries: HashMap<String, HashIndexEntry>,
}

#[derive(Serialize, Deserialize)]
struct HashIndexEntry {
    size: u64,
    mtime: i64,
    sha256: String,
}

impl Default for HashIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl HashIndex {
    pub fn new() -> Self {
        Self { version: HASH_INDEX_VERSION, entries: HashMap::new() }
    }

    /// Load from `path`. A missing, unreadable, corrupt or foreign file (bad
    /// JSON, unknown version) yields an empty index: a full re-hash instead of
    /// trusting questionable entries, and no error to propagate.
    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else { return Self::new() };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(ix) if ix.version == HASH_INDEX_VERSION => ix,
            _ => Self::new(),
        }
    }

    /// Persist atomically (unique temp file + rename), so a concurrent load
    /// never sees a torn file and concurrent savers (foreground tap + background
    /// worker before the transfer lock) can only lose to each other's complete
    /// snapshot. Failure to save is not fatal to a sync; callers may ignore it.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = tmp_sibling(path, SEQ.fetch_add(1, Ordering::Relaxed));
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    fn lookup(&self, rel: &str, size: u64, mtime: Option<i64>) -> Option<&str> {
        let mtime = mtime?;
        self.entries
            .get(rel)
            .filter(|e| e.size == size && e.mtime == mtime)
            .map(|e| e.sha256.as_str())
    }

    /// An unknown mtime cannot be a trustworthy key (a made-up 0 would alias
    /// every such file), so nothing is cached and the file re-hashes each scan.
    fn insert(&mut self, rel: String, size: u64, mtime: Option<i64>, sha256: String) {
        let Some(mtime) = mtime else { return };
        self.entries.insert(rel, HashIndexEntry { size, mtime, sha256 });
    }

    fn remove(&mut self, rel: &str) {
        self.entries.remove(rel);
    }
}

/// Modification time in whole seconds; `None` when the filesystem cannot say,
/// which the index treats as a miss rather than trusting a made-up key.
fn mtime_secs(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// Unique same-directory sibling for an atomic save. Appends to the full file
/// name instead of swapping the extension, so sibling indexes (`a.json`,
/// `a.dat`) can never meet on the same temp path.
fn tmp_sibling(path: &Path, seq: u64) -> PathBuf {
    let mut name =
        path.file_name().map(|n| n.to_os_string()).unwrap_or_else(|| "index".into());
    name.push(format!(".tmp{seq}"));
    path.with_file_name(name)
}

/// Like [`scan_local_dir`], but hashing goes through a persistent [`HashIndex`]:
/// a file whose `(size, mtime)` matches the index reuses the stored hash, so a
/// warm scan is a stat-walk with no content reads. The index is rebuilt from
/// what the walk actually saw, dropping entries for files that no longer exist.
pub fn scan_local_dir_indexed(
    root: &Path,
    index: &mut HashIndex,
) -> std::io::Result<Vec<LocalFile>> {
    let mut out = Vec::new();
    let mut fresh = HashIndex::new();
    if root.exists() {
        scan_dir(root, root, index, &mut fresh, &mut out)?;
    }
    *index = fresh;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn scan_dir(
    root: &Path,
    dir: &Path,
    index: &HashIndex,
    fresh: &mut HashIndex,
    out: &mut Vec<LocalFile>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            scan_dir(root, &path, index, fresh, out)?;
        } else if ft.is_file() {
            // An in-flight download leftover is not local state: hashing a
            // multi-GB partial re-reads it on every sync (it grows between
            // attempts, so the index never matches), and once listed it lands
            // in plan.delete, where a cancelled retry wipes the resume
            // progress (XR-107).
            if entry.file_name().to_string_lossy().ends_with(PART_SUFFIX) {
                continue;
            }
            let rel = rel_slash(root, &path);
            let meta = entry.metadata()?;
            let (size, mtime) = (meta.len(), mtime_secs(&meta));
            let sha256 = match index.lookup(&rel, size, mtime) {
                Some(hash) => hash.to_string(),
                None => sha256_file(&path)?,
            };
            fresh.insert(rel.clone(), size, mtime, sha256.clone());
            out.push(LocalFile { path: rel, sha256, size });
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
/// incomplete: the scan skips it, migration drops it instead of moving half a
/// file, and [`remove_orphan_partials`] sweeps it once its target leaves the
/// manifest.
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
            let rel = rel_slash(root, &path);
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
/// `agent_pubkey` pins the agent identity for the manifest fetch (see
/// [`fetch_manifest`]).
pub async fn sync_share(
    agent_url: &str,
    token: &ShareToken,
    agent_pubkey: &str,
    dest_root: &Path,
    dry_run: bool,
    timeout: Duration,
) -> Result<SyncResult, String> {
    sync_share_selected(agent_url, token, agent_pubkey, dest_root, None, None, dry_run, timeout)
        .await
}

/// [`sync_share`] limited to a selected subset of the share (§9.6). `selection`
/// is the set of manifest paths to mirror; `None` mirrors the whole tree.
/// `index_path` names the persistent [`HashIndex`] file (XR-098); `None` scans
/// without one, re-hashing everything.
pub async fn sync_share_selected(
    agent_url: &str,
    token: &ShareToken,
    agent_pubkey: &str,
    dest_root: &Path,
    selection: Option<&HashSet<String>>,
    index_path: Option<&Path>,
    dry_run: bool,
    timeout: Duration,
) -> Result<SyncResult, String> {
    sync_share_selected_relay(
        agent_url, token, agent_pubkey, dest_root, selection, index_path, dry_run, None, timeout,
    )
    .await
}

/// [`sync_share_selected`] that tries the direct address first and falls back to
/// the `relay` leg when the direct address is unreachable (LLD-23 §2.4). `None`
/// relay is exactly the old direct-only behaviour.
#[allow(clippy::too_many_arguments)]
pub async fn sync_share_selected_relay(
    agent_url: &str,
    token: &ShareToken,
    agent_pubkey: &str,
    dest_root: &Path,
    selection: Option<&HashSet<String>>,
    index_path: Option<&Path>,
    dry_run: bool,
    relay: Option<&RelayGrant>,
    timeout: Duration,
) -> Result<SyncResult, String> {
    direct_then_relay(agent_url, agent_pubkey, relay, &token.share_id, timeout, |client, base| async move {
        run_sync(&client, &base, token, agent_pubkey, dest_root, selection, index_path, dry_run).await
    })
    .await
}

/// Mirror a share from its [`ShareGrant`], trying the **direct** address first
/// and falling back to the **relay** only if the direct path is unreachable
/// (LLD-23 §2.4, the XR-050 order). The direct path is plain HTTP with the
/// manifest signature pinning integrity (XR-046); the relay path runs pinned
/// TLS end to end to the agent (SPKI == `agent_pubkey`), so the relay sees only
/// ciphertext. A grant with no relay leg behaves exactly as before.
pub async fn sync_share_grant(
    grant: &ShareGrant,
    dest_root: &Path,
    selection: Option<&HashSet<String>>,
    index_path: Option<&Path>,
    dry_run: bool,
    timeout: Duration,
) -> Result<SyncResult, String> {
    let token = decode_share_token(&grant.token)?;
    let direct_base = format!("http://{}:{}/{}", grant.addr, grant.port, grant.share_id);
    sync_share_selected_relay(
        &direct_base,
        &token,
        &grant.agent_pubkey,
        dest_root,
        selection,
        index_path,
        dry_run,
        grant.relay.as_ref(),
        timeout,
    )
    .await
}

// -- write path (LLD-28) --------------------------------------------

/// Returned by [`upload_file`]/[`delete_file`] when the grant's token has no
/// `share:write` scope: the invite is read-only for this share, so we refuse
/// before touching the network (LLD-28 п. 2.4). Worded for the user.
pub const ERR_NO_WRITE_SCOPE: &str = "no_write_scope: нет права записи на эту шару";

/// Upload `local_path` to `rel` inside the grant's share (LLD-28). Uses the same
/// transport as sync: direct plain HTTP first, the relay's pinned TLS as a
/// fallback (XR-050 order). Refuses before the network if the grant's token
/// lacks `share:write`. `expected_hash` is optional optimistic concurrency: the
/// hash of the version being replaced, sent as `If-Match` so a newer upload is
/// not clobbered (`412` on mismatch); `None` is last-write-wins.
pub async fn upload_file(
    grant: &ShareGrant,
    rel: &str,
    local_path: &Path,
    expected_hash: Option<&str>,
    timeout: Duration,
) -> Result<(), String> {
    let token = decode_share_token(&grant.token)?;
    if !scope_contains(&token.scope, SCOPE_WRITE) {
        return Err(ERR_NO_WRITE_SCOPE.to_string());
    }
    let direct_base = format!("http://{}:{}/{}", grant.addr, grant.port, grant.share_id);
    let token = &token; // move the reference (Copy) into the op, not the token
    direct_then_relay(
        &direct_base,
        &grant.agent_pubkey,
        grant.relay.as_ref(),
        &token.share_id,
        timeout,
        |client, base| async move { upload_with(&client, &base, token, rel, local_path, expected_hash).await },
    )
    .await
}

/// Delete `rel` from the grant's share (LLD-28). Same transport and scope check
/// as [`upload_file`]; `expected_hash` maps to `If-Match` (delete only if the
/// current content still matches).
pub async fn delete_file(
    grant: &ShareGrant,
    rel: &str,
    expected_hash: Option<&str>,
    timeout: Duration,
) -> Result<(), String> {
    let token = decode_share_token(&grant.token)?;
    if !scope_contains(&token.scope, SCOPE_WRITE) {
        return Err(ERR_NO_WRITE_SCOPE.to_string());
    }
    let direct_base = format!("http://{}:{}/{}", grant.addr, grant.port, grant.share_id);
    let token = &token; // move the reference (Copy) into the op, not the token
    direct_then_relay(
        &direct_base,
        &grant.agent_pubkey,
        grant.relay.as_ref(),
        &token.share_id,
        timeout,
        |client, base| async move { delete_with(&client, &base, token, rel, expected_hash).await },
    )
    .await
}

/// PUT a local file over an already-chosen transport (`client` + `base`),
/// streaming it so a large file is not buffered in memory. `201`/`204` are
/// success; anything else is `http_<code>`. A connect failure surfaces as
/// `network:` so [`direct_then_relay`] can fall back. Re-opening the file each
/// call keeps this safe to invoke twice (direct then relay).
async fn upload_with(
    client: &reqwest::Client,
    base_url: &str,
    token: &ShareToken,
    rel: &str,
    local_path: &Path,
    if_match: Option<&str>,
) -> Result<(), String> {
    let file = tokio::fs::File::open(local_path)
        .await
        .map_err(|e| format!("open: {e}"))?;
    let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
    let url = format!("{}/file/{}", base_url.trim_end_matches('/'), encode_path(rel));
    let mut req = client.put(&url).bearer_auth(token_blob(token)).body(body);
    if let Some(m) = if_match {
        req = req.header("If-Match", m);
    }
    let resp = req.send().await.map_err(|e| format!("network: {e}"))?;
    match resp.status().as_u16() {
        201 | 204 => Ok(()),
        code => Err(format!("http_{code}")),
    }
}

/// DELETE `rel` over an already-chosen transport. `204` is success, `404` maps to
/// a named error, otherwise `http_<code>`; a connect failure is `network:`.
async fn delete_with(
    client: &reqwest::Client,
    base_url: &str,
    token: &ShareToken,
    rel: &str,
    if_match: Option<&str>,
) -> Result<(), String> {
    let url = format!("{}/file/{}", base_url.trim_end_matches('/'), encode_path(rel));
    let mut req = client.delete(&url).bearer_auth(token_blob(token));
    if let Some(m) = if_match {
        req = req.header("If-Match", m);
    }
    let resp = req.send().await.map_err(|e| format!("network: {e}"))?;
    match resp.status().as_u16() {
        204 => Ok(()),
        404 => Err("not_found".into()),
        code => Err(format!("http_{code}")),
    }
}

// -- import path (LLD-29) -------------------------------------------

/// Returned by the import calls when the grant's token has no `share:import`
/// scope: refused before the network, worded for the user (as
/// [`ERR_NO_WRITE_SCOPE`]).
pub const ERR_NO_IMPORT_SCOPE: &str = "no_import_scope: нет права импорта на эту шару";
/// The agent answered `422`: no configured plugin takes this URL's host.
pub const ERR_NO_IMPORT_PLUGIN: &str = "no_plugin: нет плагина под эту ссылку";
/// A poll answered `404` for a job we started: the agent restarted and its
/// in-memory job table is gone (LLD-29 п. 3.7).
pub const ERR_IMPORT_JOB_LOST: &str = "job_lost: агент перезапустился, задание потерялось";

/// One import job's state as the agent reports it (LLD-29 п. 2.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportStatus {
    /// `queued` | `running` | `done` | `failed`.
    pub state: String,
    #[serde(default)]
    pub progress: Option<f64>,
    /// Published share-relative paths; present only on `done`.
    #[serde(default)]
    pub files: Vec<String>,
    /// Human-readable reason; present only on `failed`.
    #[serde(default)]
    pub error: Option<String>,
}

/// Start a URL-import job on the grant's share (LLD-29 п. 2.8): the agent
/// downloads the page's content with its configured plugin into `dest` (a
/// share-relative directory, "" = the root). `height` is the wanted frame
/// height; the agent clamps it to the owner's cap. Same transport as the write
/// path: direct first, relay fallback. Refused before the network when the
/// grant's token lacks `share:import`.
pub async fn import_url(
    grant: &ShareGrant,
    url: &str,
    dest: &str,
    height: Option<u32>,
    timeout: Duration,
) -> Result<String, String> {
    let token = decode_share_token(&grant.token)?;
    if !scope_contains(&token.scope, SCOPE_IMPORT) {
        return Err(ERR_NO_IMPORT_SCOPE.to_string());
    }
    let direct_base = format!("http://{}:{}/{}", grant.addr, grant.port, grant.share_id);
    let token = &token;
    direct_then_relay(
        &direct_base,
        &grant.agent_pubkey,
        grant.relay.as_ref(),
        &token.share_id,
        timeout,
        |client, base| async move {
            let mut body = serde_json::json!({ "url": url, "dest": dest });
            if let Some(h) = height {
                body["height"] = serde_json::json!(h);
            }
            let resp = client
                .post(format!("{}/import", base.trim_end_matches('/')))
                .bearer_auth(token_blob(token))
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("network: {e}"))?;
            match resp.status().as_u16() {
                202 => {
                    let v: serde_json::Value =
                        resp.json().await.map_err(|e| format!("parse: {e}"))?;
                    v.get("job_id")
                        .and_then(|j| j.as_str())
                        .map(str::to_string)
                        .ok_or_else(|| "parse: нет job_id в ответе".to_string())
                }
                422 => Err(ERR_NO_IMPORT_PLUGIN.to_string()),
                code => Err(format!("http_{code}")),
            }
        },
    )
    .await
}

/// Poll a job started by [`import_url`]. A `404` maps to
/// [`ERR_IMPORT_JOB_LOST`] (the UI shows it as "the job got lost").
pub async fn import_status(
    grant: &ShareGrant,
    job_id: &str,
    timeout: Duration,
) -> Result<ImportStatus, String> {
    let token = decode_share_token(&grant.token)?;
    if !scope_contains(&token.scope, SCOPE_IMPORT) {
        return Err(ERR_NO_IMPORT_SCOPE.to_string());
    }
    let direct_base = format!("http://{}:{}/{}", grant.addr, grant.port, grant.share_id);
    let token = &token;
    direct_then_relay(
        &direct_base,
        &grant.agent_pubkey,
        grant.relay.as_ref(),
        &token.share_id,
        timeout,
        |client, base| async move {
            let resp = client
                .get(format!("{}/import/{job_id}", base.trim_end_matches('/')))
                .bearer_auth(token_blob(token))
                .send()
                .await
                .map_err(|e| format!("network: {e}"))?;
            match resp.status().as_u16() {
                200 => resp.json().await.map_err(|e| format!("parse: {e}")),
                404 => Err(ERR_IMPORT_JOB_LOST.to_string()),
                code => Err(format!("http_{code}")),
            }
        },
    )
    .await
}

/// Cancel a running/queued import job; the agent kills the plugin and forgets
/// the job. Cancelling one that is already gone is not an error.
pub async fn import_cancel(grant: &ShareGrant, job_id: &str, timeout: Duration) -> Result<(), String> {
    let token = decode_share_token(&grant.token)?;
    if !scope_contains(&token.scope, SCOPE_IMPORT) {
        return Err(ERR_NO_IMPORT_SCOPE.to_string());
    }
    let direct_base = format!("http://{}:{}/{}", grant.addr, grant.port, grant.share_id);
    let token = &token;
    direct_then_relay(
        &direct_base,
        &grant.agent_pubkey,
        grant.relay.as_ref(),
        &token.share_id,
        timeout,
        |client, base| async move {
            let resp = client
                .delete(format!("{}/import/{job_id}", base.trim_end_matches('/')))
                .bearer_auth(token_blob(token))
                .send()
                .await
                .map_err(|e| format!("network: {e}"))?;
            match resp.status().as_u16() {
                204 | 404 => Ok(()),
                code => Err(format!("http_{code}")),
            }
        },
    )
    .await
}

/// A connect/timeout failure (as opposed to an authoritative HTTP/signature
/// answer): only this warrants falling back to the relay.
fn is_unreachable(err: &str) -> bool {
    err.starts_with("network:")
}

/// How long a direct-path liveness probe may take before we give up on it and
/// fall back to the relay. Kept well under the op timeout: a NAT that accepts the
/// TCP connection but never answers (CGNAT, stale port-forward) must not cost the
/// full manifest timeout (60s) or transfer timeout (up to an hour) before the
/// relay takes over.
const DIRECT_PROBE_TIMEOUT: Duration = Duration::from_secs(6);

/// A quick liveness check of the direct agent address. Any HTTP answer (even an
/// error status like 401) proves the agent is up and answering; only a transport
/// error or the short deadline counts as unreachable. Unauthenticated on purpose:
/// we only need the response line, not the body, and the probe carries no token.
async fn direct_reachable(agent_url: &str, probe_timeout: Duration) -> bool {
    let Ok(client) = http_client(probe_timeout) else { return false };
    let url = format!("{}/manifest", agent_url.trim_end_matches('/'));
    client.get(&url).send().await.is_ok()
}

/// Run `op` over the direct transport first (plain HTTP at `agent_url`); if that
/// fails *unreachable* and a `relay` leg is present, bring up the relay (pinned
/// TLS over a loopback forwarder, base `https://<loopback>/<share_id>`) and run
/// `op` there instead (LLD-23 п. 2.4). The relay leg is held for the whole `op`
/// and dropped after. `op` gets an owned client and base URL; it must be safe to
/// invoke twice (a failed direct manifest fetch writes nothing before the retry).
///
/// When a relay is available, the direct path is first probed with a short
/// deadline ([`direct_reachable`]): a dead-but-TCP-accepting address is skipped in
/// seconds instead of stalling the whole op timeout before the relay takes over.
/// With no relay to fall back to, the probe is pointless, so `op` runs directly
/// under its own timeout as before.
async fn direct_then_relay<T, F, Fut>(
    agent_url: &str,
    agent_pubkey: &str,
    relay: Option<&RelayGrant>,
    share_id: &str,
    timeout: Duration,
    op: F,
) -> Result<T, String>
where
    F: Fn(reqwest::Client, String) -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    // The probe must never cost more than the op itself would (a caller may pass a
    // timeout below the probe budget).
    let probe = DIRECT_PROBE_TIMEOUT.min(timeout);
    let try_direct = relay.is_none() || direct_reachable(agent_url, probe).await;
    if try_direct {
        match op(http_client(timeout)?, agent_url.to_string()).await {
            Ok(v) => return Ok(v),
            // Only an unreachable failure with a relay to fall back to is retried;
            // an authoritative HTTP/signature error is the answer, direct or not.
            Err(e) if !(is_unreachable(&e) && relay.is_some()) => return Err(e),
            Err(_) => {}
        }
    }

    let relay = relay.ok_or_else(|| "network: прямой путь недоступен, relay не задан".to_string())?;
    let leg = RelayLeg::open(relay).await?;
    let client = pinned_client(agent_pubkey, timeout)?;
    let base = format!("https://{}/{}", leg.local_addr(), share_id);
    let out = map_relay_outcome(op(client, base).await, leg.agent_offline());
    drop(leg); // stop the loopback forwarder once the op is done
    out
}

/// Error category of a share whose agent is gone from the relay: the relay leg
/// is up, but there is no one behind it. Distinct from `network:` (this is an
/// authoritative verdict, not the phone being offline) and worded for the user,
/// who otherwise saw the raw reqwest error against the loopback address (XR-134).
pub const ERR_AGENT_OFFLINE: &str = "agent_offline: агент шары не на связи";

/// The relay leg fails a request by closing the loopback socket, so `op` itself
/// reports a bare transport error; when the forwarder recorded that the agent is
/// off the relay, that recorded verdict replaces the noise. Authoritative
/// answers (HTTP status, signature) pass through untouched.
fn map_relay_outcome<T>(out: Result<T, String>, agent_offline: bool) -> Result<T, String> {
    match out {
        Err(e) if is_unreachable(&e) && agent_offline => Err(ERR_AGENT_OFFLINE.to_string()),
        other => other,
    }
}

/// The core mirror over an already-chosen transport (`client` + `base_url`):
/// fetch the manifest, scan, diff, and (unless `dry_run`) apply.
#[allow(clippy::too_many_arguments)]
async fn run_sync(
    client: &reqwest::Client,
    base_url: &str,
    token: &ShareToken,
    agent_pubkey: &str,
    dest_root: &Path,
    selection: Option<&HashSet<String>>,
    index_path: Option<&Path>,
    dry_run: bool,
) -> Result<SyncResult, String> {
    let manifest = fetch_manifest_with(client, base_url, token, agent_pubkey).await?;
    let mut index = index_path.map(HashIndex::load).unwrap_or_default();
    let local = scan_local_dir_indexed(dest_root, &mut index).map_err(|e| format!("scan: {e}"))?;
    // Persist right after the scan: the expensive hashing just happened, and a
    // failed or cancelled transfer below must not lose the warmed cache. This
    // also lets a dry run warm the index for the next real sync.
    if let Some(p) = index_path {
        let _ = index.save(p);
    }
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
    let report = apply_plan_with(client, base_url, token, &plan, dest_root).await;
    remove_orphan_partials(dest_root, &manifest, selection);
    if let Some(p) = index_path {
        index_apply_report(&mut index, &plan, &report, dest_root);
        let _ = index.save(p);
    }
    Ok(SyncResult { plan, report: Some(report) })
}

/// Fold an apply outcome into the index: a fetched file's hash was already
/// verified against the manifest entry during download, so a stat is enough to
/// record it and the next scan will not re-read multi-GB downloads. An entry
/// the agent had not hashed yet (empty sha256, the XR-097 cold cache) is
/// skipped and picked up by the next scan instead. Deleted paths leave the
/// index.
fn index_apply_report(index: &mut HashIndex, plan: &SyncPlan, report: &ApplyReport, dest_root: &Path) {
    let by_path: HashMap<&str, &ShareManifestEntry> =
        plan.fetch.iter().map(|e| (e.path.as_str(), e)).collect();
    for rel in &report.fetched {
        let Some(entry) = by_path.get(rel.as_str()) else { continue };
        if entry.sha256.is_empty() {
            continue;
        }
        let Some(dest) = safe_dest(dest_root, rel) else { continue };
        if let Ok(meta) = std::fs::metadata(&dest) {
            index.insert(
                rel.clone(),
                meta.len(),
                mtime_secs(&meta),
                entry.sha256.to_ascii_lowercase(),
            );
        }
    }
    for rel in &report.deleted {
        index.remove(rel);
    }
}

/// GET the agent's manifest, presenting `token` (verified by the agent offline).
///
/// `agent_pubkey` is the base64 identity key pinned from the grant (XR-046).
/// When non-empty the agent's signature headers are **required** and verified
/// over the exact body bytes before parsing: without this a MITM on the plain
/// HTTP data-path could rewrite a file and its hash together, and the SHA-256
/// download check would happily confirm the substitution. Fail-closed, so a
/// stripped signature is also a rejection. An empty `agent_pubkey` skips the
/// check (no pin to verify against).
pub async fn fetch_manifest(
    agent_url: &str,
    token: &ShareToken,
    agent_pubkey: &str,
    timeout: Duration,
) -> Result<ShareManifest, String> {
    fetch_manifest_relay(agent_url, token, agent_pubkey, None, timeout).await
}

/// [`fetch_manifest`] that falls back to the `relay` leg when the direct address
/// is unreachable (LLD-23 §2.4). `None` relay is the old direct-only behaviour.
pub async fn fetch_manifest_relay(
    agent_url: &str,
    token: &ShareToken,
    agent_pubkey: &str,
    relay: Option<&RelayGrant>,
    timeout: Duration,
) -> Result<ShareManifest, String> {
    direct_then_relay(agent_url, agent_pubkey, relay, &token.share_id, timeout, |client, base| async move {
        fetch_manifest_with(&client, &base, token, agent_pubkey).await
    })
    .await
}

/// [`fetch_manifest`] over an already-built client and base URL, so the caller
/// picks the transport (plain direct or pinned-TLS relay, LLD-23).
async fn fetch_manifest_with(
    client: &reqwest::Client,
    agent_url: &str,
    token: &ShareToken,
    agent_pubkey: &str,
) -> Result<ShareManifest, String> {
    use xr_proto::share::{MANIFEST_SIGNED_AT_HEADER, MANIFEST_SIG_HEADER};

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
    let sig = resp
        .headers()
        .get(MANIFEST_SIG_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let signed_at = resp
        .headers()
        .get(MANIFEST_SIGNED_AT_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let body = resp.bytes().await.map_err(|e| format!("read: {e}"))?;

    if !agent_pubkey.is_empty() {
        let key = xr_proto::share::parse_agent_pubkey(agent_pubkey)
            .map_err(|e| format!("agent_pubkey: {e}"))?;
        let (Some(sig), Some(signed_at)) = (sig, signed_at) else {
            // Either a pre-XR-046 agent or a stripped signature; the two are
            // indistinguishable here, so both are refused.
            return Err("manifest_unsigned".into());
        };
        xr_proto::share::verify_share_manifest(&sig, &key, &token.share_id, signed_at, &body)
            .map_err(|e| format!("manifest_signature: {e}"))?;
    }

    serde_json::from_slice::<ShareManifest>(&body).map_err(|e| format!("parse: {e}"))
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
    // Direct-only path: no relay leg, so the agent key (only needed to pin the
    // relay's TLS) is unused here.
    download_entry_relay(agent_url, token, entry, dest_root, "", None, timeout).await
}

/// [`download_entry`] that falls back to the `relay` leg when the direct address
/// is unreachable (LLD-23 §2.4). `agent_pubkey` pins the relay's end-to-end TLS
/// (ignored on the direct path). `None` relay is the old direct-only behaviour.
#[allow(clippy::too_many_arguments)]
pub async fn download_entry_relay(
    agent_url: &str,
    token: &ShareToken,
    entry: &ShareManifestEntry,
    dest_root: &Path,
    agent_pubkey: &str,
    relay: Option<&RelayGrant>,
    timeout: Duration,
) -> Result<(), String> {
    direct_then_relay(agent_url, agent_pubkey, relay, &token.share_id, timeout, |client, base| async move {
        download_entry_with(&client, &base, token, entry, dest_root).await
    })
    .await
}

/// [`download_entry`] over an already-built client and base URL (transport
/// chosen by the caller, LLD-23).
async fn download_entry_with(
    client: &reqwest::Client,
    agent_url: &str,
    token: &ShareToken,
    entry: &ShareManifestEntry,
    dest_root: &Path,
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
    let client = match http_client(timeout) {
        Ok(c) => c,
        Err(e) => {
            let mut report = ApplyReport::default();
            for entry in &plan.fetch {
                report.failed.push((entry.path.clone(), e.clone()));
            }
            return report;
        }
    };
    apply_plan_with(&client, agent_url, token, plan, dest_root).await
}

/// [`apply_plan`] over an already-built client and base URL (transport chosen by
/// the caller, LLD-23).
async fn apply_plan_with(
    client: &reqwest::Client,
    agent_url: &str,
    token: &ShareToken,
    plan: &SyncPlan,
    dest_root: &Path,
) -> ApplyReport {
    let mut report = ApplyReport::default();

    for (i, entry) in plan.fetch.iter().enumerate() {
        if transfer_cancelled() {
            report.failed.push((entry.path.clone(), "cancelled".into()));
            break;
        }
        transfer_file(&entry.path, i);
        match download_entry_with(client, agent_url, token, entry, dest_root).await {
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

/// Sweep `.xrsync-part` leftovers whose target the manifest (within the
/// selection) no longer covers. The scan does not list partials, so an orphan
/// would otherwise sit on disk forever; one whose target is still covered
/// stays, it is the Range-resume progress of the next fetch. A share file
/// that itself ends in the suffix is covered by its own path and untouched.
fn remove_orphan_partials(
    root: &Path,
    manifest: &ShareManifest,
    selection: Option<&HashSet<String>>,
) {
    let covered: HashSet<&str> = manifest
        .entries
        .iter()
        .filter(|e| path_selected(&e.path, selection))
        .map(|e| e.path.as_str())
        .collect();
    let mut orphans = Vec::new();
    collect_orphan_partials(root, root, &covered, &mut orphans);
    for path in orphans {
        if std::fs::remove_file(&path).is_ok() {
            prune_empty_dirs(root, &path);
        }
    }
}

fn collect_orphan_partials(
    root: &Path,
    dir: &Path,
    covered: &HashSet<&str>,
    out: &mut Vec<PathBuf>,
) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            collect_orphan_partials(root, &path, covered, out);
        } else if ft.is_file() {
            let rel = rel_slash(root, &path);
            let Some(target) = rel.strip_suffix(PART_SUFFIX) else { continue };
            if !covered.contains(rel.as_str()) && !covered.contains(target) {
                out.push(path);
            }
        }
    }
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

/// A reqwest client that trusts exactly the agent behind `agent_pubkey` (base64
/// Ed25519), for the relay path where TLS is end-to-end and pinned to the key
/// (LLD-23 §2.3). The hostname is irrelevant (we dial a loopback forwarder), the
/// pin is on the key.
fn pinned_client(agent_pubkey: &str, timeout: Duration) -> Result<reqwest::Client, String> {
    let tls = xr_proto::relay_tls::pinned_client_config(agent_pubkey)?;
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .build()
        .map_err(|e| format!("pinned http client: {e}"))
}

/// A live relay leg for the consumer: the loopback forwarder that turns local
/// TCP connections into relay streams to the agent (LLD-23 §2.2). Held for the
/// duration of a transfer; dropping it stops the forwarder.
struct RelayLeg {
    fwd: xr_proto::relay_client::LoopbackForwarder,
    local_addr: std::net::SocketAddr,
}

impl RelayLeg {
    async fn open(grant: &xr_proto::share::RelayGrant) -> Result<Self, String> {
        let endpoint = std::sync::Arc::new(
            xr_proto::relay_client::RelayEndpoint::from_grant(grant).map_err(|e| format!("relay grant: {e}"))?,
        );
        let fwd = xr_proto::relay_client::LoopbackForwarder::spawn(endpoint)
            .await
            .map_err(|e| format!("relay forwarder: {e}"))?;
        let local_addr = fwd.local_addr();
        Ok(Self { fwd, local_addr })
    }

    fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }

    /// The relay refused the latest stream because the share's agent isn't
    /// registered there: the share is dead, not merely unreachable (XR-134).
    fn agent_offline(&self) -> bool {
        self.fwd.agent_offline()
    }
}

/// Decode a grant's access-token blob (base64url-nopad JSON) into a [`ShareToken`].
fn decode_share_token(blob: &str) -> Result<ShareToken, String> {
    use base64::Engine as _;
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(blob.trim())
        .map_err(|e| format!("token base64url: {e}"))?;
    serde_json::from_slice(&json).map_err(|e| format!("token json: {e}"))
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

    // ── relay transport (LLD-23 consumer side) ──────────────────────────

    fn token_blob_of(share_id: &str) -> String {
        use base64::Engine as _;
        let t = ShareToken { share_id: share_id.into(), scope: "share:read".into(), exp: 9_999_999_999, signature: "sig".into() };
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&t).unwrap())
    }

    #[test]
    fn decode_share_token_roundtrips() {
        let t = decode_share_token(&token_blob_of("S")).unwrap();
        assert_eq!(t.share_id, "S");
        assert!(decode_share_token("@@@").is_err());
    }

    #[test]
    fn pinned_client_needs_a_valid_key() {
        use base64::Engine as _;
        let good = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        assert!(pinned_client(&good, Duration::from_secs(5)).is_ok());
        // Not 32 bytes / not base64 -> a clear error, not a panic.
        assert!(pinned_client("QQ==", Duration::from_secs(5)).is_err());
        assert!(pinned_client("@@@", Duration::from_secs(5)).is_err());
    }

    #[test]
    fn is_unreachable_only_matches_network_errors() {
        assert!(is_unreachable("network: connection refused"));
        assert!(!is_unreachable("http_403"));
        assert!(!is_unreachable("manifest_unsigned"));
        assert!(!is_unreachable("manifest_signature: bad"));
    }

    /// The relay leg fails requests by closing the loopback socket, so `op`
    /// reports a bare transport error; the forwarder's "agent offline" verdict
    /// must replace that noise with the named category, while authoritative
    /// answers pass through untouched (XR-134).
    #[test]
    fn relay_outcome_maps_agent_offline() {
        let dead: Result<(), String> =
            Err("network: error sending request for url (https://127.0.0.1:9/s/manifest)".into());
        assert_eq!(map_relay_outcome(dead, true).unwrap_err(), ERR_AGENT_OFFLINE);
        // No verdict from the forwarder: the network error stays as is.
        let plain: Result<(), String> = Err("network: x".into());
        assert_eq!(map_relay_outcome(plain, false).unwrap_err(), "network: x");
        // An authoritative agent answer wins over a stale offline verdict.
        let http: Result<(), String> = Err("http_403".into());
        assert_eq!(map_relay_outcome(http, true).unwrap_err(), "http_403");
        let ok: Result<i32, String> = Ok(7);
        assert_eq!(map_relay_outcome(ok, true).unwrap(), 7);
    }

    /// A grant with no relay leg and an unreachable direct address surfaces the
    /// direct network error unchanged: the relay path is not invented when it
    /// isn't offered, and the direct path is otherwise untouched (LLD-23 §2.4).
    #[tokio::test]
    async fn grant_without_relay_reports_direct_failure() {
        let dir = tempfile::tempdir().unwrap();
        let grant = ShareGrant {
            share_id: "S".into(),
            name: "S".into(),
            // 127.0.0.1:1 refuses fast; no relay leg.
            addr: "127.0.0.1".into(),
            port: 1,
            agent_pubkey: String::new(),
            token: token_blob_of("S"),
            exp: 9_999_999_999,
            relay: None,
        };
        let err = sync_share_grant(&grant, dir.path(), None, None, false, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(is_unreachable(&err), "expected a direct network error, got {err}");
    }

    /// When the direct address is unreachable and a relay leg IS present, the
    /// fallback branch runs. A malformed relay obf makes that branch fail
    /// distinctly (`relay grant: ...`), proving direct-first then relay-last
    /// executed rather than stopping at the direct error (LLD-23 §2.4).
    #[tokio::test]
    async fn fetch_manifest_falls_back_to_relay_when_direct_unreachable() {
        use xr_proto::share::{RelayObf, RelayToken};
        let token = ShareToken { share_id: "S".into(), scope: "share:read".into(), exp: 9_999_999_999, signature: "s".into() };
        let relay = RelayGrant {
            addr: "127.0.0.1".into(),
            port: 2,
            // Malformed key: RelayEndpoint::from_grant fails, so the relay branch
            // returns "relay grant: ..." rather than a network error.
            obf: RelayObf {
                key: "@@@".into(),
                salt: 0,
                modifier: "positional_xor_rotate".into(),
                padding_min: 0,
                padding_max: 0,
            },
            relay_token: RelayToken {
                share_id: "S".into(),
                agent_pubkey: "QQ==".into(),
                exp: 9_999_999_999,
                signature: "s".into(),
            },
        };
        let err = fetch_manifest_relay(
            "http://127.0.0.1:1/S", // refuses fast
            &token,
            "",
            Some(&relay),
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        assert!(err.contains("relay grant"), "fallback must reach the relay leg, got {err}");
    }

    // -- write path: upload_file / delete_file (LLD-28) ------------------

    /// A canned one-shot HTTP server that captures the request line + headers and
    /// answers `status_line`. Returns its address and a channel with the request
    /// text (enough to assert method/path/headers; the streamed body follows).
    async fn serve_capture(
        status_line: &'static str,
    ) -> (std::net::SocketAddr, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let resp = format!("{status_line}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
            let _ = tx.send(req);
        });
        (addr, rx)
    }

    /// A direct-only grant to `addr` whose token carries `scope`.
    fn write_grant(addr: std::net::SocketAddr, scope: &str) -> ShareGrant {
        use base64::Engine as _;
        let token = ShareToken {
            share_id: "S".into(),
            scope: scope.into(),
            exp: 9_999_999_999,
            signature: "sig".into(),
        };
        let blob = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&token).unwrap());
        ShareGrant {
            share_id: "S".into(),
            name: "S".into(),
            addr: addr.ip().to_string(),
            port: addr.port(),
            agent_pubkey: String::new(),
            token: blob,
            exp: 9_999_999_999,
            relay: None,
        }
    }

    #[tokio::test]
    async fn test_upload_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, b"hello upload").unwrap();
        let (addr, rx) = serve_capture("HTTP/1.1 201 Created").await;
        let grant = write_grant(addr, "share:read share:write");

        upload_file(&grant, "docs/a.txt", &f, Some("abc123"), Duration::from_secs(5))
            .await
            .unwrap();

        let req = rx.await.unwrap();
        assert!(req.starts_with("PUT /S/file/docs/a.txt "), "wrong request line: {req}");
        // The expected hash rides along as If-Match (optimistic concurrency).
        assert!(req.to_lowercase().contains("if-match: abc123"), "If-Match missing: {req}");
        assert!(req.contains("authorization: Bearer") || req.contains("Authorization: Bearer"));
    }

    #[tokio::test]
    async fn test_delete_via_grant() {
        let (addr, rx) = serve_capture("HTTP/1.1 204 No Content").await;
        let grant = write_grant(addr, "share:read share:write");

        delete_file(&grant, "old.txt", None, Duration::from_secs(5))
            .await
            .unwrap();

        let req = rx.await.unwrap();
        assert!(req.starts_with("DELETE /S/file/old.txt "), "wrong request line: {req}");
    }

    // -- import path (LLD-29) -------------------------------------------

    /// [`serve_capture`] with a JSON body in the answer (the import routes
    /// return payloads, not just status lines).
    async fn serve_capture_json(
        status_line: &'static str,
        body: &'static str,
    ) -> (std::net::SocketAddr, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let resp = format!(
                "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
            let _ = tx.send(req);
        });
        (addr, rx)
    }

    const IMPORT_SCOPE: &str = "share:read share:write share:import";

    #[tokio::test]
    async fn test_import_roundtrip() {
        // Start: POST /S/import with the url/dest/height body -> job_id.
        let (addr, rx) = serve_capture_json("HTTP/1.1 202 Accepted", r#"{"job_id":"ab12"}"#).await;
        let grant = write_grant(addr, IMPORT_SCOPE);
        let job = import_url(&grant, "https://youtu.be/x", "видео", Some(720), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(job, "ab12");
        let req = rx.await.unwrap();
        assert!(req.starts_with("POST /S/import "), "wrong request line: {req}");
        assert!(req.contains(r#""height":720"#), "height missing: {req}");
        assert!(req.contains(r#""dest":"видео""#), "dest missing: {req}");

        // Poll: GET /S/import/{id} -> the parsed status.
        let (addr, rx) = serve_capture_json(
            "HTTP/1.1 200 OK",
            r#"{"state":"done","progress":100.0,"files":["видео/Ролик.mp4"]}"#,
        )
        .await;
        let grant = write_grant(addr, IMPORT_SCOPE);
        let st = import_status(&grant, "ab12", Duration::from_secs(5)).await.unwrap();
        assert_eq!(st.state, "done");
        assert_eq!(st.files, vec!["видео/Ролик.mp4".to_string()]);
        assert!(rx.await.unwrap().starts_with("GET /S/import/ab12 "));

        // Cancel: DELETE -> 204 is Ok.
        let (addr, rx) = serve_capture_json("HTTP/1.1 204 No Content", "").await;
        let grant = write_grant(addr, IMPORT_SCOPE);
        import_cancel(&grant, "ab12", Duration::from_secs(5)).await.unwrap();
        assert!(rx.await.unwrap().starts_with("DELETE /S/import/ab12 "));
    }

    #[tokio::test]
    async fn test_import_error_mapping() {
        // 422 on start means no plugin takes the URL; 404 on poll means the
        // agent restarted and lost the job (LLD-29 п. 3.7). Both map to their
        // named errors, single-sourced here for the UI.
        let (addr, _rx) = serve_capture_json("HTTP/1.1 422 Unprocessable Entity", "").await;
        let grant = write_grant(addr, IMPORT_SCOPE);
        let err = import_url(&grant, "https://example.org/x", "", None, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(err, ERR_NO_IMPORT_PLUGIN);

        let (addr, _rx) = serve_capture_json("HTTP/1.1 404 Not Found", "").await;
        let grant = write_grant(addr, IMPORT_SCOPE);
        let err = import_status(&grant, "gone", Duration::from_secs(5)).await.unwrap_err();
        assert_eq!(err, ERR_IMPORT_JOB_LOST);
    }

    #[tokio::test]
    async fn import_refused_without_scope_before_network() {
        // A grant without share:import is refused locally: the unreachable
        // address is never dialed.
        let grant = write_grant("127.0.0.1:1".parse().unwrap(), "share:read share:write");
        let err = import_url(&grant, "https://x/y", "", None, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert_eq!(err, ERR_NO_IMPORT_SCOPE);
        let err = import_status(&grant, "j", Duration::from_secs(2)).await.unwrap_err();
        assert_eq!(err, ERR_NO_IMPORT_SCOPE);
        let err = import_cancel(&grant, "j", Duration::from_secs(2)).await.unwrap_err();
        assert_eq!(err, ERR_NO_IMPORT_SCOPE);
    }

    #[tokio::test]
    async fn write_refused_without_scope_before_network() {
        // A read-only grant is refused locally, so an unreachable address is never
        // dialed (the error is the scope error, not a network one).
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, b"x").unwrap();
        let grant = write_grant("127.0.0.1:1".parse().unwrap(), "share:read");

        let err = upload_file(&grant, "a.txt", &f, None, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert_eq!(err, ERR_NO_WRITE_SCOPE);
        let err = delete_file(&grant, "a.txt", None, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert_eq!(err, ERR_NO_WRITE_SCOPE);
    }

    fn entry(path: &str, sha: &str) -> ShareManifestEntry {
        ShareManifestEntry {
            path: path.into(),
            size: 10,
            mtime: 1,
            sha256: sha.into(),
        }
    }
    fn local(path: &str, sha: &str) -> LocalFile {
        local_sized(path, sha, 10)
    }
    fn local_sized(path: &str, sha: &str, size: u64) -> LocalFile {
        LocalFile { path: path.into(), sha256: sha.into(), size }
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
    fn test_plan_sync_unhashed_manifest_keeps_local() {
        // A freshly (re)started agent serves the listing before its hash cache
        // warms up: sha256 comes empty ("not known yet", see xr-share
        // build_listing). An intact local file must not re-fetch on that
        // (XR-097: sync right after an agent restart re-downloaded the whole
        // selection).
        let m = manifest(vec![entry("a.txt", "")]);
        let plan = plan_sync(&m, &[local("a.txt", "realhash")]);
        assert!(plan.is_empty(), "unhashed manifest entry must not refetch an intact local file");
    }

    #[test]
    fn test_plan_sync_unhashed_manifest_size_change_refetches() {
        // With the hash unknown, size is the only change signal left.
        let m = manifest(vec![entry("a.txt", "")]); // entry size is 10
        let plan = plan_sync(&m, &[local_sized("a.txt", "realhash", 11)]);
        assert_eq!(plan.fetch.len(), 1);
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
    fn test_plan_empty_selection_is_delete_only() {
        // XR-135: an empty (but present) selection is not "the whole share".
        // Nothing is desired, so every local file is deleted and nothing is
        // fetched. This is the plan a manual "sync with no ticks" drives to wipe
        // the download, and it must differ from None (the whole share).
        let m = manifest(vec![entry("a.txt", "a"), entry("b.txt", "b")]);
        let have = vec![local("a.txt", "a"), local("b.txt", "b")];
        let empty: HashSet<String> = HashSet::new();

        let plan = plan_with_selection(&m, &have, Some(&empty));
        assert!(plan.fetch.is_empty());
        assert_eq!(plan.delete, vec!["a.txt".to_string(), "b.txt".to_string()]);

        // None keeps both (they match the manifest), Some(empty) drops both.
        let whole = plan_with_selection(&m, &have, None);
        assert!(whole.fetch.is_empty() && whole.delete.is_empty());
        assert_ne!(plan, whole);
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

    #[test]
    fn scan_skips_download_partials() {
        // A cancelled download leaves a multi-GB `.xrsync-part` behind. Listing
        // it would re-hash it on every sync and, worse, put it in plan.delete,
        // where the delete pass of a re-cancelled retry wipes the resume
        // progress (XR-107).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), b"done").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.bin.xrsync-part"), b"half").unwrap();

        let local = scan_local_dir(dir.path()).unwrap();
        assert_eq!(
            local.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            vec!["a.bin"]
        );

        // Through the plan: the pending fetch stays, no mirror-delete appears.
        let m = manifest(vec![entry("a.bin", &local[0].sha256), entry("sub/b.bin", "x")]);
        let plan = plan_sync(&m, &local);
        assert!(plan.delete.is_empty(), "partial must not be mirror-deleted");
        assert_eq!(
            plan.fetch.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            vec!["sub/b.bin"]
        );
    }

    #[test]
    fn orphan_partials_swept_when_target_leaves_manifest() {
        // With partials invisible to the scan, one whose file left the share
        // needs its own sweep or it lives forever.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/gone.bin.xrsync-part"), b"orphan").unwrap();
        std::fs::write(dir.path().join("keep.bin.xrsync-part"), b"resume").unwrap();
        std::fs::write(dir.path().join("named.xrsync-part"), b"real").unwrap();

        let m = manifest(vec![entry("keep.bin", "k"), entry("named.xrsync-part", "n")]);
        remove_orphan_partials(dir.path(), &m, None);

        // The orphan is gone together with its now-empty directory ...
        assert!(!dir.path().join("sub").exists());
        // ... the resumable partial of a still-covered target stays ...
        assert!(dir.path().join("keep.bin.xrsync-part").exists());
        // ... and a share file that itself ends in the suffix is untouched.
        assert!(dir.path().join("named.xrsync-part").exists());

        // Unticking the target orphans its partial the same way.
        let sel: HashSet<String> = ["named.xrsync-part".to_string()].into_iter().collect();
        remove_orphan_partials(dir.path(), &m, Some(&sel));
        assert!(!dir.path().join("keep.bin.xrsync-part").exists());
        assert!(dir.path().join("named.xrsync-part").exists());
    }

    // ── HashIndex (XR-098) ────────────────────────────────────────────

    /// Rewrite `path` with same-length `content`, restoring the original mtime.
    /// A `(size, mtime)` index then treats the file as unchanged, which is how
    /// tests prove a scan served the hash from the index instead of re-reading.
    fn rewrite_keeping_mtime(path: &Path, content: &[u8]) {
        let mtime = std::fs::metadata(path).unwrap().modified().unwrap();
        assert_eq!(std::fs::metadata(path).unwrap().len(), content.len() as u64);
        std::fs::write(path, content).unwrap();
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(mtime).unwrap();
    }

    #[test]
    fn indexed_scan_reuses_hash_without_rereading() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("a.bin");
        std::fs::write(&file, b"hello").unwrap();

        let mut index = HashIndex::new();
        let cold = scan_local_dir_indexed(root.path(), &mut index).unwrap();

        // Swap the content keeping (size, mtime): the warm scan must return the
        // cached hash, proving the content was not re-read. The pre-index scan
        // re-hashed every file on every pass (the 40-second "preparing").
        rewrite_keeping_mtime(&file, b"HELLO");
        let warm = scan_local_dir_indexed(root.path(), &mut index).unwrap();
        assert_eq!(cold, warm, "warm scan must serve the hash from the index");
    }

    #[test]
    fn hash_index_persists_between_instances() {
        let root = tempfile::tempdir().unwrap();
        let aux = tempfile::tempdir().unwrap();
        let ix_path = aux.path().join("share.json");
        std::fs::write(root.path().join("a.txt"), b"hello").unwrap();

        let mut index = HashIndex::load(&ix_path); // missing file -> empty index
        let cold = scan_local_dir_indexed(root.path(), &mut index).unwrap();
        index.save(&ix_path).unwrap();

        // A fresh instance (new process in real life) still avoids re-reading.
        rewrite_keeping_mtime(&root.path().join("a.txt"), b"WORLD");
        let mut reloaded = HashIndex::load(&ix_path);
        let warm = scan_local_dir_indexed(root.path(), &mut reloaded).unwrap();
        assert_eq!(cold, warm, "reloaded index must serve cached hashes");
    }

    #[test]
    fn hash_index_rejects_corrupt_or_foreign_file() {
        let root = tempfile::tempdir().unwrap();
        let aux = tempfile::tempdir().unwrap();
        let ix_path = aux.path().join("share.json");
        std::fs::write(root.path().join("a.txt"), b"hello").unwrap();

        let mut index = HashIndex::new();
        let cold = scan_local_dir_indexed(root.path(), &mut index).unwrap();
        rewrite_keeping_mtime(&root.path().join("a.txt"), b"WORLD");

        // Corrupt JSON and an unknown version must both fall back to an empty
        // index, i.e. an honest full re-hash that sees the new content.
        for junk in [&b"{ not json"[..], &br#"{"version":999,"entries":{}}"#[..]] {
            std::fs::write(&ix_path, junk).unwrap();
            let mut loaded = HashIndex::load(&ix_path);
            assert!(loaded.entries.is_empty(), "junk index must load empty");
            let rescanned = scan_local_dir_indexed(root.path(), &mut loaded).unwrap();
            assert_ne!(rescanned[0].sha256, cold[0].sha256, "must re-hash for real");
        }
    }

    #[test]
    fn unknown_mtime_is_a_miss_not_a_key() {
        let mut ix = HashIndex::new();
        ix.insert("a".into(), 5, Some(7), "h".into());
        assert_eq!(ix.lookup("a", 5, Some(7)), Some("h"));
        assert_eq!(ix.lookup("a", 5, None), None, "unknown mtime must never hit");
        ix.insert("b".into(), 5, None, "h2".into());
        assert!(!ix.entries.contains_key("b"), "unknown mtime must not be cached");
    }

    #[test]
    fn save_tmp_sibling_keeps_the_full_file_name() {
        // `with_extension` would collapse `a.json` and `a.dat` into the same
        // `a.tmpN` space; appending keeps sibling indexes apart by construction.
        let a = tmp_sibling(Path::new("/x/a.json"), 7);
        assert_eq!(a, Path::new("/x/a.json.tmp7"));
        assert_ne!(a, tmp_sibling(Path::new("/x/a.dat"), 7));
    }

    #[test]
    fn hash_index_prunes_deleted_files() {
        let root = tempfile::tempdir().unwrap();
        let aux = tempfile::tempdir().unwrap();
        let ix_path = aux.path().join("share.json");
        std::fs::write(root.path().join("a.txt"), b"stay").unwrap();
        std::fs::write(root.path().join("b.txt"), b"gone").unwrap();

        let mut index = HashIndex::new();
        scan_local_dir_indexed(root.path(), &mut index).unwrap();
        std::fs::remove_file(root.path().join("b.txt")).unwrap();
        scan_local_dir_indexed(root.path(), &mut index).unwrap();
        index.save(&ix_path).unwrap();

        let reloaded = HashIndex::load(&ix_path);
        assert!(reloaded.entries.contains_key("a.txt"));
        assert!(!reloaded.entries.contains_key("b.txt"), "deleted file must leave the index");
    }

    #[test]
    fn hash_index_relative_keys_survive_root_move() {
        // A storage migration (XR-043) renames files: mtime survives, the root
        // changes. Relative keys must keep the cache warm across the move.
        let parent = tempfile::tempdir().unwrap();
        let old_root = parent.path().join("A");
        std::fs::create_dir_all(old_root.join("sub")).unwrap();
        std::fs::write(old_root.join("sub/f.bin"), b"hello").unwrap();

        let mut index = HashIndex::new();
        let before = scan_local_dir_indexed(&old_root, &mut index).unwrap();

        let new_root = parent.path().join("B");
        std::fs::rename(&old_root, &new_root).unwrap();
        rewrite_keeping_mtime(&new_root.join("sub/f.bin"), b"HELLO");
        let after = scan_local_dir_indexed(&new_root, &mut index).unwrap();
        assert_eq!(before, after, "moved root must still hit the index");
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

    // ── fetch_manifest signature pinning (XR-046) ────────────────────

    /// One-shot canned HTTP server: accepts a single connection, ignores the
    /// request, answers with `response`. Returns the base URL.
    async fn serve_once(response: String) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        format!("http://{addr}")
    }

    fn http_response(body: &str, extra_headers: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{extra_headers}connection: close\r\n\r\n{body}",
            body.len(),
        )
    }

    fn test_token(share_id: &str) -> ShareToken {
        ShareToken { share_id: share_id.into(), scope: "share:read".into(), exp: u64::MAX, signature: "sig".into() }
    }

    fn signed_headers(key: &ed25519_dalek::SigningKey, share_id: &str, signed_at: u64, body: &str) -> String {
        use xr_proto::share::{sign_share_manifest, MANIFEST_SIGNED_AT_HEADER, MANIFEST_SIG_HEADER};
        let sig = sign_share_manifest(key, share_id, signed_at, body.as_bytes());
        format!("{MANIFEST_SIG_HEADER}: {sig}\r\n{MANIFEST_SIGNED_AT_HEADER}: {signed_at}\r\n")
    }

    fn agent_key() -> (ed25519_dalek::SigningKey, String) {
        use base64::Engine;
        let key = ed25519_dalek::SigningKey::from_bytes(&[21u8; 32]);
        let pub_b64 =
            base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes());
        (key, pub_b64)
    }

    const MANIFEST_BODY: &str =
        r#"{"entries":[{"path":"a.txt","size":5,"mtime":1,"sha256":"aa"}]}"#;

    #[tokio::test]
    async fn fetch_manifest_verifies_pinned_signature() {
        let (key, pub_b64) = agent_key();
        let url = serve_once(http_response(
            MANIFEST_BODY,
            &signed_headers(&key, "s1", 1234, MANIFEST_BODY),
        ))
        .await;
        let m = fetch_manifest(&url, &test_token("s1"), &pub_b64, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.entries[0].path, "a.txt");
    }

    #[tokio::test]
    async fn fetch_manifest_rejects_tampered_body() {
        // The MITM scenario XR-046 closes: the body (a hash inside it) was
        // rewritten in flight, the signature no longer matches. Passes only
        // with verification in place; the pre-XR-046 code accepted this.
        let (key, pub_b64) = agent_key();
        let forged = MANIFEST_BODY.replace("\"aa\"", "\"bb\"");
        let url =
            serve_once(http_response(&forged, &signed_headers(&key, "s1", 1234, MANIFEST_BODY)))
                .await;
        let err = fetch_manifest(&url, &test_token("s1"), &pub_b64, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.starts_with("manifest_signature"), "{err}");
    }

    #[tokio::test]
    async fn fetch_manifest_rejects_wrong_share_replay() {
        // A valid manifest of share s2, signed by the same agent, replayed for
        // a request about s1: the share_id binding must reject it.
        let (key, pub_b64) = agent_key();
        let url = serve_once(http_response(
            MANIFEST_BODY,
            &signed_headers(&key, "s2", 1234, MANIFEST_BODY),
        ))
        .await;
        let err = fetch_manifest(&url, &test_token("s1"), &pub_b64, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.starts_with("manifest_signature"), "{err}");
    }

    #[tokio::test]
    async fn fetch_manifest_rejects_stripped_signature() {
        // No signature headers while a key is pinned: either an old agent or a
        // MITM stripping headers; both are refused (fail closed).
        let (_key, pub_b64) = agent_key();
        let url = serve_once(http_response(MANIFEST_BODY, "")).await;
        let err = fetch_manifest(&url, &test_token("s1"), &pub_b64, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(err, "manifest_unsigned");
    }

    #[tokio::test]
    async fn fetch_manifest_without_pin_skips_verification() {
        // An empty agent_pubkey means there is nothing to verify against; the
        // legacy unverified path must keep working.
        let url = serve_once(http_response(MANIFEST_BODY, "")).await;
        let m = fetch_manifest(&url, &test_token("s1"), "", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(m.entries.len(), 1);
    }

    // direct reachability probe: fast relay fallback (XR-128)

    #[tokio::test]
    async fn direct_reachable_true_on_any_http_answer() {
        // Any HTTP response, even an unauthenticated 401, proves the agent is up.
        let url = serve_once(
            "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into(),
        )
        .await;
        assert!(direct_reachable(&url, Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn direct_reachable_false_and_fast_when_accepted_but_silent() {
        use tokio::io::AsyncReadExt;
        // A NAT that accepts the TCP connection but never answers: the probe must
        // honour its own short deadline (the input here) rather than the caller's
        // op timeout, which for a real download can be up to an hour. This checks
        // direct_reachable in isolation; the caller passes DIRECT_PROBE_TIMEOUT.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 64];
                let _ = sock.read(&mut buf).await;
                std::future::pending::<()>().await; // hold open, never reply
            }
        });
        let start = std::time::Instant::now();
        let ok = direct_reachable(&format!("http://{addr}"), Duration::from_millis(400)).await;
        let elapsed = start.elapsed();
        assert!(!ok);
        assert!(elapsed < Duration::from_secs(5), "probe stalled {elapsed:?}");
    }

    // ── sync_share_selected with a persistent index (XR-098) ─────────

    /// Multi-request sibling of [`serve_once`]: answers `/manifest` with
    /// `manifest_body` and any other GET (a `/file/...` download) with
    /// `file_body`, for as many connections as the test makes.
    async fn serve_share(manifest_body: String, file_body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let body = if buf[..n].starts_with(b"GET /manifest") {
                    manifest_body.clone()
                } else {
                    file_body.to_string()
                };
                let _ = sock.write_all(http_response(&body, "").as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[tokio::test]
    async fn sync_with_index_records_download_and_skips_rehash() {
        let dest = tempfile::tempdir().unwrap();
        let aux = tempfile::tempdir().unwrap();
        let ix_path = aux.path().join("share.json");
        let manifest_body = format!(
            r#"{{"entries":[{{"path":"a.txt","size":5,"mtime":1,"sha256":"{HELLO_SHA}"}}]}}"#
        );

        let url = serve_share(manifest_body.clone(), "hello").await;
        let res = sync_share_selected(
            &url, &test_token("s1"), "", dest.path(), None, Some(&ix_path), false,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(res.report.unwrap().fetched, vec!["a.txt".to_string()]);

        // The verified download landed in the persisted index right away.
        let index = HashIndex::load(&ix_path);
        let meta = std::fs::metadata(dest.path().join("a.txt")).unwrap();
        assert_eq!(index.lookup("a.txt", meta.len(), mtime_secs(&meta)), Some(HELLO_SHA));

        // Second sync must be a no-op via the index: swap the content keeping
        // (size, mtime). An honest re-hash would see a changed file and
        // re-fetch; the indexed scan matches the manifest and plans nothing.
        rewrite_keeping_mtime(&dest.path().join("a.txt"), b"XXXXX");
        let url2 = serve_share(manifest_body, "hello").await;
        let res2 = sync_share_selected(
            &url2, &test_token("s1"), "", dest.path(), None, Some(&ix_path), false,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(res2.plan.is_empty(), "warm sync must plan nothing: {:?}", res2.plan);
    }
}
