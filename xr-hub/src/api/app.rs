//! APK self-update endpoints (LLD-12 §2.1, §4).
//!
//! `GET /api/v1/app/latest`       → the signed release manifest from disk.
//! `GET /api/v1/app/download/:ver` → the APK, streamed from disk.
//!
//! The hub never signs anything here: the manifest is signed offline by the
//! owner (`xr-hub sign-release`) with the release key — the hub only serves
//! the pre-signed `manifest.json` + `manifest.sig` it finds on disk. A VPS
//! compromise can replace these files but cannot produce a valid signature
//! for the pinned client key, so the client rejects the forgery (§5.1).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use serde_json::json;
use tokio_util::io::ReaderStream;

use crate::state::AppState;

/// `GET /api/v1/app/latest` — return the signed manifest as
/// `{ "manifest": <raw manifest.json string>, "signature": <base64 sig> }`.
///
/// The manifest is embedded as an *opaque string* (not re-parsed/re-serialized)
/// so the client verifies the detached signature over the exact bytes the
/// owner signed — no canonicalization step shared between signer and verifier.
/// `404` when no release has been published.
pub async fn get_latest(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, StatusCode> {
    let dir = state.config.server.releases_path();
    let manifest = tokio::fs::read_to_string(dir.join("manifest.json"))
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let signature = tokio::fs::read_to_string(dir.join("manifest.sig"))
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let body = json!({
        "manifest": manifest,
        "signature": signature.trim(),
    });
    Ok((
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    ))
}

/// `GET /api/v1/app/download/:ver` — stream `<releases>/<ver>.apk` with the
/// Android package content-type. Integrity is the client's job (SHA-256 from
/// the signed manifest), so no signing/hashing happens here.
pub async fn download(
    State(state): State<Arc<AppState>>,
    Path(ver): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    if !is_safe_version(&ver) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let path = state.config.server.releases_path().join(format!("{ver}.apk"));
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);

    let stream = ReaderStream::new(file);
    let headers = [
        (
            header::CONTENT_TYPE,
            "application/vnd.android.package-archive".to_string(),
        ),
        (header::CONTENT_LENGTH, len.to_string()),
        (
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"xr-proxy-{ver}.apk\""),
        ),
    ];
    Ok((headers, Body::from_stream(stream)))
}

/// Allow only version strings safe as a single filename segment — alphanumeric
/// plus `. - _`. Blocks path separators / traversal (`..`, `/`).
fn is_safe_version(ver: &str) -> bool {
    !ver.is_empty()
        && ver.len() <= 64
        && !ver.contains("..")
        && ver
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::is_safe_version;

    #[test]
    fn rejects_traversal_and_separators() {
        assert!(is_safe_version("0.2.0"));
        assert!(is_safe_version("1.0.0-rc1"));
        assert!(!is_safe_version(""));
        assert!(!is_safe_version("../etc/passwd"));
        assert!(!is_safe_version("a/b"));
        assert!(!is_safe_version(".."));
        assert!(!is_safe_version(&"v".repeat(65)));
    }
}
