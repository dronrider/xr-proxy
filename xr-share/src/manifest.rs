//! Build the [`ShareManifest`] the agent serves: a listing of every regular
//! file under the share root with size, mtime and streaming SHA-256 (LLD-19
//! §2.3). The hashes are the integrity anchor for downloads and the change
//! signal for sync (`xr-core`).
//!
//! Symlinks are **not** followed and not listed — only regular files are
//! exposed, which keeps the listing aligned with what [`crate::safepath`] will
//! actually serve and avoids leaking anything a symlink might point to.

use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;
use xr_proto::share::{ShareManifest, ShareManifestEntry};

/// Walk `root` and produce a manifest. Entries are sorted by path so the output
/// is deterministic (stable diffs, reproducible tests).
pub fn build_manifest(root: &Path) -> Result<ShareManifest> {
    let mut entries = Vec::new();

    for entry in WalkDir::new(root).follow_links(false).into_iter() {
        let entry = entry.context("walking share directory")?;
        // Only regular files — skip dirs and symlinks.
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
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        entries.push(ShareManifestEntry {
            path: rel,
            size: meta.len(),
            mtime,
            sha256: sha256_file(path)?,
        });
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(ShareManifest { entries })
}

/// Build a one-entry manifest for a **single-file** share (§9.1): the entry path
/// is the file's own name, so the consumer fetches `/file/{name}`. The file is
/// the share root; there is no directory to walk.
pub fn build_manifest_for_file(path: &Path) -> Result<ShareManifest> {
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if !meta.is_file() {
        anyhow::bail!("share path is not a regular file: {}", path.display());
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .context("file share has no file name")?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(ShareManifest {
        entries: vec![ShareManifestEntry {
            path: name,
            size: meta.len(),
            mtime,
            sha256: sha256_file(path)?,
        }],
    })
}

/// Streaming SHA-256 of a file → lowercase hex. Reads in 64 KiB chunks so a
/// large file is never held in memory at once (mirrors `xr-core::update`).
fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
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
    use std::fs;

    /// SHA-256 of "hello" — the well-known vector.
    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn manifest_lists_files_with_hashes() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        fs::write(dir.path().join("sub/b.bin"), b"world").unwrap();

        let m = build_manifest(dir.path()).unwrap();
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

        let m = build_manifest_for_file(&file).unwrap();
        assert_eq!(m.entries.len(), 1);
        // Path is the file's own name, not an absolute path.
        assert_eq!(m.entries[0].path, "report.pdf");
        assert_eq!(m.entries[0].size, 5);
        assert_eq!(m.entries[0].sha256, HELLO_SHA);

        // A directory is rejected by the file-share builder.
        assert!(build_manifest_for_file(dir.path()).is_err());
    }

    #[test]
    fn manifest_skips_symlinks_and_dirs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("real.txt"), b"hello").unwrap();
        fs::create_dir(dir.path().join("emptydir")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("link.txt")).unwrap();

        let m = build_manifest(dir.path()).unwrap();
        // Only the one regular file — no dir entry, no symlink entry.
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.entries[0].path, "real.txt");
    }
}
