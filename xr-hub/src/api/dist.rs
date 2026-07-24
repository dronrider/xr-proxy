//! Serve the `xr-share` distribution (XR-028): install scripts, agent binaries,
//! and `SHA256SUMS` from `<data_dir>/share-dist/`. Public, read-only, with a
//! strict single-segment filename check (no traversal). This is what the
//! one-line installer reaches:
//!
//! `curl -fsSL https://<hub>/share/install.sh | sh`
//!
//! Like the APK endpoint, the hub just streams pre-staged files from disk; the
//! installer verifies the binary's SHA-256 against the served `SHA256SUMS`.
//!
//! Той же механикой раздаётся setup-dist (XR-015, LLD-13): бинари
//! xr-setup/xr-server/xr-hub для автоустановки, `GET /api/v1/setup/{file}`
//! из `<data_dir>/setup-dist/`.

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use tokio_util::io::ReaderStream;

use crate::state::AppState;

/// `GET /share/:file`: stream a distribution file from `<data_dir>/share-dist`.
pub async fn serve(
    state: State<Arc<AppState>>,
    file: AxPath<String>,
) -> Result<impl IntoResponse, StatusCode> {
    serve_from(state, file, "share-dist").await
}

/// `GET /api/v1/setup/:file`: stream from `<data_dir>/setup-dist` (XR-015).
pub async fn serve_setup(
    state: State<Arc<AppState>>,
    file: AxPath<String>,
) -> Result<impl IntoResponse, StatusCode> {
    serve_from(state, file, "setup-dist").await
}

async fn serve_from(
    State(state): State<Arc<AppState>>,
    AxPath(file): AxPath<String>,
    dist_dir: &'static str,
) -> Result<impl IntoResponse, StatusCode> {
    if !is_safe_name(&file) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let path = Path::new(&state.config.server.data_dir)
        .join(dist_dir)
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
    use super::*;
    use crate::state::AppState;
    use axum::extract::{Path as AxPath, State};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn state_with_data_dir(data_dir: &std::path::Path) -> Arc<AppState> {
        let config: crate::config::HubConfig = toml::from_str(&format!(
            "[server]\ndata_dir = \"{}\"\n[admin]\nusers = []\n",
            data_dir.display()
        ))
        .unwrap();
        Arc::new(AppState {
            presets: RwLock::new(HashMap::new()),
            invites: RwLock::new(HashMap::new()),
            shares: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            config,
            signing: None,
        })
    }

    #[tokio::test]
    async fn setup_dist_serves_staged_file_and_404s_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("setup-dist")).unwrap();
        std::fs::write(dir.path().join("setup-dist/SHA256SUMS"), "abc  bin\n").unwrap();
        let state = state_with_data_dir(dir.path());

        assert!(
            serve_setup(State(state.clone()), AxPath("SHA256SUMS".into()))
                .await
                .is_ok()
        );
        assert_eq!(
            serve_setup(State(state.clone()), AxPath("нет-такого".into()))
                .await
                .err(),
            Some(StatusCode::BAD_REQUEST),
            "не ASCII-имя режется валидацией"
        );
        assert_eq!(
            serve_setup(State(state), AxPath("missing".into())).await.err(),
            Some(StatusCode::NOT_FOUND)
        );
    }

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
