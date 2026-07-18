//! Build the [`ShareManifest`] the agent serves: a listing of every regular
//! file under the share root with size, mtime and streaming SHA-256 (LLD-19
//! §2.3). The hashes are the integrity anchor for downloads and the change
//! signal for sync (`xr-core`).
//!
//! Symlinks are **not** followed and not listed — only regular files are
//! exposed, which keeps the listing aligned with what [`crate::safepath`] will
//! actually serve and avoids leaking anything a symlink might point to.
//!
//! Hashing every file on every `/manifest` request is O(total bytes), which is
//! far too slow for a large share (a consumer times out before it finishes). So
//! the agent keeps a [`HashCache`]: a file is hashed once and re-hashed only
//! when its size or mtime changes. A background warmer (see `main`) primes it so
//! even the first request is cheap.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;
use xr_proto::share::{ShareManifest, ShareManifestEntry};

/// Reserved name prefix for everything service-owned inside a share (LLD-29
/// п. 3.8): upload temps (`.xr-part-*`), import job dirs (`.xr-import-*`) and
/// any future private names. The walk skips such components (files *and*
/// directories), and [`crate::safepath`] refuses request paths naming them, so
/// nothing half-done is ever listed, read, overwritten or deleted via a route.
pub const RESERVED_PREFIX: &str = ".xr-";

/// Prefix of an in-flight upload's temp file (LLD-28), one name inside the
/// reserved [`RESERVED_PREFIX`] namespace. A `PUT` streams into
/// `.xr-part-<rand>` next to its target and renames on completion.
pub const UPLOAD_TEMP_PREFIX: &str = ".xr-part-";

/// True if `name` is service-owned (see [`RESERVED_PREFIX`]).
fn is_reserved(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with(RESERVED_PREFIX)
}

/// Per-file SHA-256 cache keyed by absolute path. Shared across all shares and
/// requests. A path is re-hashed only when its `(size, mtime)` changes, so
/// building a large share's manifest costs a directory walk plus a stat per file
/// once the cache is warm (no re-reading file contents).
#[derive(Default)]
pub struct HashCache {
    inner: Mutex<HashMap<PathBuf, CacheEntry>>,
}

struct CacheEntry {
    size: u64,
    mtime: i64,
    sha256: String,
}

impl HashCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The file's hash, reusing the cached value if `(size, mtime)` still match,
    /// otherwise computing and storing it. The lock is never held while hashing.
    fn hashed(&self, path: &Path, size: u64, mtime: i64) -> Result<String> {
        if let Some(e) = self.inner.lock().expect("hash cache poisoned").get(path) {
            if e.size == size && e.mtime == mtime {
                return Ok(e.sha256.clone());
            }
        }
        let sha = sha256_file(path)?;
        self.inner
            .lock()
            .expect("hash cache poisoned")
            .insert(path.to_path_buf(), CacheEntry { size, mtime, sha256: sha.clone() });
        Ok(sha)
    }

    /// The cached hash if `(size, mtime)` still match, **without ever hashing**.
    /// Used by the listing path so a request never blocks on a cold file.
    fn cached(&self, path: &Path, size: u64, mtime: i64) -> Option<String> {
        self.inner
            .lock()
            .expect("hash cache poisoned")
            .get(path)
            .filter(|e| e.size == size && e.mtime == mtime)
            .map(|e| e.sha256.clone())
    }

    /// The current SHA-256 of `path`, stating it and hashing through the cache
    /// (a warm `(size, mtime)` hits without re-reading). Used by the write path's
    /// `If-Match` precondition, which needs the target's real current content
    /// hash (LLD-28). Errors if the file cannot be stat'd or read.
    pub fn hash_of(&self, path: &Path) -> Result<String> {
        let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        self.hashed(path, meta.len(), mtime_secs(&meta))
    }

    /// Record a known hash for `path` at the given `(size, mtime)`, so a manifest
    /// build right after an upload returns the file already hashed (no lazy warm,
    /// LLD-28 п. 2.3). The caller passes the hash it computed while streaming and
    /// the freshly-renamed file's metadata.
    pub fn seed(&self, path: &Path, size: u64, mtime: i64, sha256: String) {
        self.inner
            .lock()
            .expect("hash cache poisoned")
            .insert(path.to_path_buf(), CacheEntry { size, mtime, sha256 });
    }
}

/// Shared by the import publish path too, so a seeded cache entry keys on the
/// same rounding the manifest walk uses.
pub(crate) fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The walk both manifest builders share: no symlink following, and every
/// reserved `.xr-` component pruned. Pruning (not per-entry filtering) matters
/// for directories: an import job dir's contents must never surface, however
/// the files inside are named (LLD-29 п. 3.8). The root itself is exempt so an
/// oddly-named share still serves.
fn walk_share(root: &Path) -> impl Iterator<Item = walkdir::Result<walkdir::DirEntry>> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !is_reserved(e.file_name()))
}

/// Walk `root` and produce a manifest, hashing through `cache`. Entries are
/// sorted by path so the output is deterministic (stable diffs, reproducible
/// tests).
pub fn build_manifest(root: &Path, cache: &HashCache) -> Result<ShareManifest> {
    let mut entries = Vec::new();

    for entry in walk_share(root) {
        let entry = entry.context("walking share directory")?;
        // Only regular files (dirs and symlinks are skipped).
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .context("path not under root")?
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");

        let meta = entry.metadata().context("reading file metadata")?;
        let mtime = mtime_secs(&meta);

        entries.push(ShareManifestEntry {
            path: rel,
            size: meta.len(),
            mtime,
            sha256: cache.hashed(path, meta.len(), mtime)?,
        });
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(ShareManifest { entries })
}

/// Like [`build_manifest`] but **never hashes**: each entry carries the cached
/// hash if present, else an empty string. Browsing must be instant even on a
/// cold cache and a huge share (XR-039), so the SHA-256 is filled lazily by the
/// warmer ([`build_manifest`]); a consumer treats an empty hash as "not known
/// yet" (skip verify / fall back to size+mtime).
pub fn build_listing(root: &Path, cache: &HashCache) -> Result<ShareManifest> {
    let mut entries = Vec::new();

    for entry in walk_share(root) {
        let entry = entry.context("walking share directory")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .context("path not under root")?
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");

        let meta = entry.metadata().context("reading file metadata")?;
        let mtime = mtime_secs(&meta);

        entries.push(ShareManifestEntry {
            path: rel,
            size: meta.len(),
            mtime,
            sha256: cache.cached(path, meta.len(), mtime).unwrap_or_default(),
        });
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(ShareManifest { entries })
}

/// Listing for a single-file share, without hashing (see [`build_listing`]).
pub fn build_listing_for_file(path: &Path, cache: &HashCache) -> Result<ShareManifest> {
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if !meta.is_file() {
        anyhow::bail!("share path is not a regular file: {}", path.display());
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .context("file share has no file name")?;
    let mtime = mtime_secs(&meta);
    Ok(ShareManifest {
        entries: vec![ShareManifestEntry {
            path: name,
            size: meta.len(),
            mtime,
            sha256: cache.cached(path, meta.len(), mtime).unwrap_or_default(),
        }],
    })
}

/// Build a one-entry manifest for a **single-file** share (§9.1): the entry path
/// is the file's own name, so the consumer fetches `/file/{name}`. The file is
/// the share root; there is no directory to walk.
pub fn build_manifest_for_file(path: &Path, cache: &HashCache) -> Result<ShareManifest> {
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if !meta.is_file() {
        anyhow::bail!("share path is not a regular file: {}", path.display());
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .context("file share has no file name")?;
    let mtime = mtime_secs(&meta);
    Ok(ShareManifest {
        entries: vec![ShareManifestEntry {
            path: name,
            size: meta.len(),
            mtime,
            sha256: cache.hashed(path, meta.len(), mtime)?,
        }],
    })
}

/// Streaming SHA-256 of a file as lowercase hex. Reads in 64 KiB chunks so a
/// large file is never held in memory at once (mirrors `xr-core::update`).
fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    Ok(sha256_stream(&mut file)?)
}

/// The hashing loop behind [`sha256_file`], on any reader. The import publish
/// path hashes from an already-open handle so it can fsync the same descriptor.
pub(crate) fn sha256_stream(r: &mut impl std::io::Read) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf)?;
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
    use std::fs;

    /// SHA-256 of "hello" — the well-known vector.
    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn manifest_lists_files_with_hashes() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        fs::write(dir.path().join("sub/b.bin"), b"world").unwrap();

        let m = build_manifest(dir.path(), &HashCache::new()).unwrap();
        assert_eq!(m.entries.len(), 2);

        // Sorted by path: "a.txt" before "sub/b.bin".
        assert_eq!(m.entries[0].path, "a.txt");
        assert_eq!(m.entries[0].size, 5);
        assert_eq!(m.entries[0].sha256, HELLO_SHA);
        assert_eq!(m.entries[1].path, "sub/b.bin");
    }

    #[test]
    fn file_share_manifest_has_one_entry() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("report.pdf");
        fs::write(&file, b"hello").unwrap();

        let m = build_manifest_for_file(&file, &HashCache::new()).unwrap();
        assert_eq!(m.entries.len(), 1);
        // Path is the file's own name, not an absolute path.
        assert_eq!(m.entries[0].path, "report.pdf");
        assert_eq!(m.entries[0].size, 5);
        assert_eq!(m.entries[0].sha256, HELLO_SHA);

        // A directory is rejected by the file-share builder.
        assert!(build_manifest_for_file(dir.path(), &HashCache::new()).is_err());
    }

    #[test]
    fn manifest_skips_symlinks_and_dirs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("real.txt"), b"hello").unwrap();
        fs::create_dir(dir.path().join("emptydir")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("link.txt")).unwrap();

        let m = build_manifest(dir.path(), &HashCache::new()).unwrap();
        // Only the one regular file — no dir entry, no symlink entry.
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.entries[0].path, "real.txt");
    }

    #[test]
    fn test_manifest_skips_xr_namespace() {
        // Nothing service-owned may appear in the listing or the hashed manifest:
        // an in-flight upload's `.xr-part-*` (LLD-28) and a whole `.xr-import-*`
        // job directory with its contents (LLD-29), whatever the files inside
        // are called.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("real.txt"), b"hello").unwrap();
        fs::write(dir.path().join(".xr-part-abc123"), b"half").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/.xr-part-def"), b"partial").unwrap();
        fs::create_dir(dir.path().join(".xr-import-77")).unwrap();
        fs::write(dir.path().join(".xr-import-77/Ролик.mp4"), b"downloading").unwrap();

        let cache = HashCache::new();
        let m = build_manifest(dir.path(), &cache).unwrap();
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.entries[0].path, "real.txt");

        let l = build_listing(dir.path(), &cache).unwrap();
        assert_eq!(l.entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), vec!["real.txt"]);
    }

    #[test]
    fn hash_of_and_seed_roundtrip() {
        // `hash_of` returns the file's real content hash; `seed` records a known
        // hash so a later manifest build serves it warm (LLD-28 write path).
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        fs::write(&f, b"hello").unwrap();
        let cache = HashCache::new();
        assert_eq!(cache.hash_of(&f).unwrap(), HELLO_SHA);

        // Seed a fresh cache with a size/mtime the file actually has, so the
        // listing (which never hashes) still returns the hash.
        let fresh = HashCache::new();
        let meta = std::fs::metadata(&f).unwrap();
        let mtime = mtime_secs(&meta);
        fresh.seed(&f, meta.len(), mtime, HELLO_SHA.to_string());
        let l = build_listing(dir.path(), &fresh).unwrap();
        assert_eq!(l.entries[0].sha256, HELLO_SHA);
    }

    #[test]
    fn cache_reuses_until_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        fs::write(&f, b"hello").unwrap();
        let cache = HashCache::new();

        // First build hashes and caches; a second build serves the same hash warm.
        assert_eq!(build_manifest(dir.path(), &cache).unwrap().entries[0].sha256, HELLO_SHA);
        assert_eq!(build_manifest(dir.path(), &cache).unwrap().entries[0].sha256, HELLO_SHA);

        // A different-size write invalidates the (size, mtime) key → recompute.
        fs::write(&f, b"different length content").unwrap();
        let m = build_manifest(dir.path(), &cache).unwrap();
        assert_eq!(m.entries[0].size, 24);
        assert_ne!(m.entries[0].sha256, HELLO_SHA);
    }
}
