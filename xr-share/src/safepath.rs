//! Anti-traversal path resolution — the security core of the agent (LLD-19 §5.2).
//!
//! A consumer asks for a relative path inside a share; the agent must resolve it
//! to a real file **strictly within** the served directory and refuse anything
//! that escapes — `..`, absolute paths, or a symlink that points outside the
//! root. Getting this wrong exposes the whole disk, so it is defended twice:
//!
//! 1. **Lexical** — reject `..`, absolute paths, backslashes and NUL *before*
//!    touching the filesystem, so traversal can't depend on FS state.
//! 2. **Canonical** — resolve the deepest existing ancestor with
//!    `canonicalize()` (which follows symlinks) and require it to stay under the
//!    canonical root, catching a symlink that escapes.
//!
//! `test_path_traversal_blocked` exercises both layers.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafePathError {
    /// Path contained `..`, was absolute, or used a forbidden character.
    InvalidComponent,
    /// Path resolved (via a symlink) to a location outside the share root.
    Escapes,
    /// The share root itself could not be canonicalized (mis-configured agent).
    BadRoot,
}

impl std::fmt::Display for SafePathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::InvalidComponent => "path contains an invalid or traversing component",
            Self::Escapes => "path escapes the share root",
            Self::BadRoot => "share root cannot be resolved",
        };
        f.write_str(s)
    }
}

impl std::error::Error for SafePathError {}

/// Resolve `requested` (a forward-slash relative path, as received in the URL)
/// against `root`, guaranteeing the result is a path inside `root`. Returns the
/// joined path on success — which may not exist yet (the caller opens it and a
/// missing file becomes a 404); existence is not this function's concern, only
/// containment.
pub fn resolve_within(root: &Path, requested: &str) -> Result<PathBuf, SafePathError> {
    // ── Layer 1: lexical ─────────────────────────────────────────────
    // Backslash and NUL are never legitimate in a share path and would be
    // separators / terminators on some platforms.
    if requested.contains('\\') || requested.contains('\0') {
        return Err(SafePathError::InvalidComponent);
    }

    let mut safe = PathBuf::new();
    for comp in requested.split('/') {
        match comp {
            // Empty (leading/trailing/double slash) and "." are no-ops.
            "" | "." => continue,
            // The one component that can escape — refuse outright.
            ".." => return Err(SafePathError::InvalidComponent),
            other => {
                // The whole `.xr-` namespace is reserved (LLD-28 upload temps,
                // LLD-29 import job dirs): no request path may name such a
                // component, so nothing service-owned can be read, overwritten,
                // or deleted through a route.
                if other.starts_with(crate::manifest::RESERVED_PREFIX) {
                    return Err(SafePathError::InvalidComponent);
                }
                // A component that is itself absolute (e.g. "C:" or starts with
                // a separator after our split shouldn't happen, but be strict).
                let p = Path::new(other);
                if p.is_absolute() || p.components().count() != 1 {
                    return Err(SafePathError::InvalidComponent);
                }
                safe.push(other);
            }
        }
    }

    let candidate = root.join(&safe);

    // ── Layer 2: canonical containment ───────────────────────────────
    let canon_root = root.canonicalize().map_err(|_| SafePathError::BadRoot)?;

    // Canonicalize the deepest ancestor that actually exists (the candidate
    // itself may be a not-yet-existing file). `root` always exists, so the walk
    // terminates. A symlink anywhere in the existing chain resolves here and is
    // caught by the prefix check.
    let mut probe = candidate.as_path();
    loop {
        match probe.canonicalize() {
            Ok(real) => {
                if real.starts_with(&canon_root) {
                    return Ok(candidate);
                }
                return Err(SafePathError::Escapes);
            }
            Err(_) => match probe.parent() {
                Some(parent) => probe = parent,
                None => return Err(SafePathError::Escapes),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/file.txt"), b"hello").unwrap();
        fs::write(dir.path().join("top.txt"), b"top").unwrap();
        dir
    }

    #[test]
    fn test_path_traversal_blocked() {
        let root = tmp_root();
        let r = root.path();

        // Legitimate paths resolve inside root.
        assert_eq!(resolve_within(r, "top.txt").unwrap(), r.join("top.txt"));
        assert_eq!(resolve_within(r, "sub/file.txt").unwrap(), r.join("sub/file.txt"));
        // Leading slash, "./", and double slashes are harmless and normalized.
        assert_eq!(resolve_within(r, "/top.txt").unwrap(), r.join("top.txt"));
        assert_eq!(resolve_within(r, "./sub//file.txt").unwrap(), r.join("sub/file.txt"));
        // A not-yet-existing file inside root is allowed (caller 404s on open).
        assert!(resolve_within(r, "sub/missing.txt").is_ok());

        // ── Traversal must be refused ──
        for bad in [
            "../etc/passwd",
            "..",
            "sub/../../etc/passwd",
            "a/../../b",
            "../../../../../../etc/shadow",
        ] {
            assert_eq!(
                resolve_within(r, bad),
                Err(SafePathError::InvalidComponent),
                "should reject lexically: {bad}"
            );
        }

        // Absolute path component and backslash.
        assert_eq!(resolve_within(r, "\\windows\\system32"), Err(SafePathError::InvalidComponent));
        assert_eq!(resolve_within(r, "foo\0bar"), Err(SafePathError::InvalidComponent));

        // The reserved `.xr-` namespace is refused in any position (LLD-28
        // upload temps, LLD-29 import job dirs), so no one can reach anything
        // service-owned through a route.
        assert_eq!(resolve_within(r, ".xr-part-abc"), Err(SafePathError::InvalidComponent));
        assert_eq!(resolve_within(r, "sub/.xr-part-abc"), Err(SafePathError::InvalidComponent));
        assert_eq!(resolve_within(r, ".xr-import-1/a.mp4"), Err(SafePathError::InvalidComponent));
        assert_eq!(resolve_within(r, ".xr-хитрость"), Err(SafePathError::InvalidComponent));
        // A benign name that merely resembles it is fine.
        assert!(resolve_within(r, "xr-part.txt").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_escape_blocked() {
        use std::os::unix::fs::symlink;
        let root = tmp_root();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"secret").unwrap();

        // A symlink *inside* the share that points outside it.
        symlink(outside.path(), root.path().join("escape")).unwrap();

        // Requesting through the symlink must be caught by canonicalization,
        // even though it is lexically clean (no `..`).
        assert_eq!(
            resolve_within(root.path(), "escape/secret"),
            Err(SafePathError::Escapes),
            "symlink pointing outside root must be refused"
        );

        // A symlink that stays *inside* the root is fine.
        symlink(root.path().join("sub"), root.path().join("innerlink")).unwrap();
        assert!(resolve_within(root.path(), "innerlink/file.txt").is_ok());
    }
}
