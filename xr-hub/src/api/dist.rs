//! Serve the `xr-share` distribution (XR-028): install scripts, agent binaries,
//! and `SHA256SUMS` from `<data_dir>/share-dist/`. Public, read-only, with a
//! strict single-segment filename check (no traversal). This is what the
//! one-line installer reaches:
//!
//! `curl -fsSL https://<hub>/share/install.sh | sh`
//!
//! Like the APK endpoint, the hub just streams pre-staged files from disk; the
//! installer verifies the binary's SHA-256 against the served `SHA256SUMS`.

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use tokio_util::io::ReaderStream;

use crate::state::AppState;

/// `GET /share/:file` — stream a distribution file from `<data_dir>/share-dist`.
pub async fn serve(
    State(state): State<Arc<AppState>>,
    AxPath(file): AxPath<String>,
) -> Result<impl IntoResponse, StatusCode> {
    if !is_safe_name(&file) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let path = Path::new(&state.config.server.data_dir)
        .join("share-dist")
        .join(&file);
    let f = tokio::fs::File::open(&path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);

    let headers = [
        (header::CONTENT_TYPE, content_type(&file).to_string()),
        (header::CONTENT_LENGTH, len.to_string()),
    ];
    Ok((headers, Body::from_stream(ReaderStream::new(f))))
}

/// One filename segment, no traversal: `[A-Za-z0-9._-]`, no `..`.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

fn content_type(name: &str) -> &'static str {
    if name.ends_with(".sh") || name.ends_with(".ps1") || name == "SHA256SUMS" {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::is_safe_name;

    #[test]
    fn rejects_traversal_and_separators() {
        assert!(is_safe_name("install.sh"));
        assert!(is_safe_name("install.ps1"));
        assert!(is_safe_name("xr-share-linux-x86_64"));
        assert!(is_safe_name("xr-share-windows-x86_64.exe"));
        assert!(is_safe_name("SHA256SUMS"));
        assert!(!is_safe_name("../etc/passwd"));
        assert!(!is_safe_name("a/b"));
        assert!(!is_safe_name(".."));
        assert!(!is_safe_name(""));
    }
}
