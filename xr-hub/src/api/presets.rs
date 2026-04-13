use std::path::Path;
use std::sync::Arc;

use axum::extract::{self, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use xr_proto::config::RoutingConfig;
use xr_proto::preset::{Preset, PresetSummary};

use crate::state::AppState;
use crate::storage;

// ── Public ──────────────────────────────────────────────────────────

pub async fn list_presets(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let presets = state.presets.read().await;
    let mut summaries: Vec<PresetSummary> = presets.values().map(|p| p.summary()).collect();
    summaries.sort_by(|a, b| a.name.cmp(&b.name));

    let etag = format!("\"{}\"", summaries.iter().map(|s| s.version).sum::<u64>());
    let mut headers = HeaderMap::new();
    headers.insert("etag", etag.parse().unwrap());
    headers.insert(
        "x-hub-version",
        env!("CARGO_PKG_VERSION").parse().unwrap(),
    );
    (headers, Json(summaries))
}

pub async fn get_preset(
    State(state): State<Arc<AppState>>,
    extract::Path(name): extract::Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    let presets = state.presets.read().await;
    let preset = presets.get(&name).ok_or(StatusCode::NOT_FOUND)?;

    let etag = format!("\"{}\"", preset.version);

    // ETag / If-None-Match
    if let Some(inm) = headers.get("if-none-match").and_then(|v| v.to_str().ok()) {
        if inm == etag {
            return Err(StatusCode::NOT_MODIFIED);
        }
    }

    let mut resp_headers = HeaderMap::new();
    resp_headers.insert("etag", etag.parse().unwrap());
    Ok((resp_headers, Json(preset.clone())))
}

pub async fn get_public_key(
    State(state): State<Arc<AppState>>,
) -> Result<String, StatusCode> {
    let ctx = state.signing.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let pubkey = ctx.verifying_key();
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(pubkey.as_bytes()))
}

// ── Admin ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreatePresetRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub rules: RoutingConfig,
}

pub async fn create_preset(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreatePresetRequest>,
) -> Result<(StatusCode, Json<Preset>), (StatusCode, String)> {
    validate_slug(&req.name)?;
    validate_rules_size(&req.rules)?;

    let mut presets = state.presets.write().await;
    if presets.contains_key(&req.name) {
        return Err((StatusCode::CONFLICT, format!("preset '{}' already exists", req.name)));
    }

    let now = chrono::Utc::now().to_rfc3339();
    let mut preset = Preset {
        name: req.name,
        version: 1,
        updated_at: now,
        description: req.description,
        rules: req.rules,
        signature: None,
    };

    if let Some(ctx) = &state.signing {
        preset.signature = Some(ctx.sign_preset(&preset));
    }

    let data_dir = Path::new(&state.config.server.data_dir);
    storage::save_preset(data_dir, &preset)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    presets.insert(preset.name.clone(), preset.clone());
    Ok((StatusCode::CREATED, Json(preset)))
}

pub async fn update_preset(
    State(state): State<Arc<AppState>>,
    extract::Path(name): extract::Path<String>,
    Json(req): Json<CreatePresetRequest>,
) -> Result<Json<Preset>, (StatusCode, String)> {
    validate_rules_size(&req.rules)?;

    let mut presets = state.presets.write().await;
    let existing = presets
        .get(&name)
        .ok_or((StatusCode::NOT_FOUND, format!("preset '{name}' not found")))?;

    let now = chrono::Utc::now().to_rfc3339();
    let mut preset = Preset {
        name: name.clone(),
        version: existing.version + 1,
        updated_at: now,
        description: req.description,
        rules: req.rules,
        signature: None,
    };

    if let Some(ctx) = &state.signing {
        preset.signature = Some(ctx.sign_preset(&preset));
    }

    let data_dir = Path::new(&state.config.server.data_dir);
    storage::save_preset(data_dir, &preset)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    presets.insert(name, preset.clone());
    Ok(Json(preset))
}

pub async fn delete_preset(
    State(state): State<Arc<AppState>>,
    extract::Path(name): extract::Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let mut presets = state.presets.write().await;
    if presets.remove(&name).is_none() {
        return Err((StatusCode::NOT_FOUND, format!("preset '{name}' not found")));
    }

    let data_dir = Path::new(&state.config.server.data_dir);
    storage::delete_preset_file(data_dir, &name)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

fn validate_slug(name: &str) -> Result<(), (StatusCode, String)> {
    if name.is_empty() || name.len() > 64 {
        return Err((StatusCode::BAD_REQUEST, "name must be 1-64 characters".into()));
    }
    if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
        return Err((StatusCode::BAD_REQUEST, "name must match [a-z0-9_-]+".into()));
    }
    Ok(())
}

fn validate_rules_size(rules: &RoutingConfig) -> Result<(), (StatusCode, String)> {
    if rules.rules.len() > 10_000 {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "max 10000 rules".into()));
    }
    Ok(())
}
