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
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use xr_proto::share::{ShareGrant, ShareInfo, ShareManifest, ShareManifestEntry, ShareToken};

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

/// Like [`plan_sync`], but restricted to a **selected subset** of the share
/// (§9.6). `selection` is the set of manifest paths the consumer chose to mirror;
/// the desired local state is `manifest ∩ selection`. A file present locally but
/// not in the desired set is deleted, so unticking a file (or its removal on the
/// server) drops the local copy. `None` selects the whole tree, i.e. exactly
/// [`plan_sync`].
pub fn plan_with_selection(
    manifest: &ShareManifest,
    local: &[LocalFile],
    selection: Option<&HashSet<String>>,
) -> SyncPlan {
    let local_by_path: HashMap<&str, &str> = local
        .iter()
        .map(|f| (f.path.as_str(), f.sha256.as_str()))
        .collect();

    // Desired = manifest entries that are selected (everything, if no selection).
    let desired: Vec<&ShareManifestEntry> = manifest
        .entries
        .iter()
        .filter(|e| selection.map_or(true, |sel| sel.contains(e.path.as_str())))
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
        "{}.xrsync-part",
        dest.file_name().and_then(|n| n.to_str()).unwrap_or("download")
    ));

    let client = http_client(timeout)?;
    let url = format!("{}/file/{}", agent_url.trim_end_matches('/'), encode_path(&entry.path));
    let mut resp = client
        .get(&url)
        .bearer_auth(token_blob(token))
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("http_{}", resp.status().as_u16()));
    }

    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::File::create(&part)
        .await
        .map_err(|e| format!("create: {e}"))?;
    let mut hasher = Sha256::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("read: {e}"))? {
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(|e| format!("write: {e}"))?;
    }
    file.flush().await.map_err(|e| format!("flush: {e}"))?;
    drop(file);

    let got = hex_lower(&hasher.finalize());
    if !got.eq_ignore_ascii_case(&entry.sha256) {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(format!("sha256 mismatch (want {}, got {got})", entry.sha256));
    }
    tokio::fs::rename(&part, &dest)
        .await
        .map_err(|e| format!("rename: {e}"))
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

    for entry in &plan.fetch {
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
}
