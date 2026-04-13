pub mod auth;
pub mod invites;
pub mod presets;

use std::sync::Arc;

use axum::middleware;
use axum::routing::{delete, get, post, put};
use axum::Router;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::embed::spa_service;
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    let cors = if state.config.admin.allowed_origins.is_empty() {
        CorsLayer::permissive()
    } else {
        let origins: Vec<_> = state
            .config
            .admin
            .allowed_origins
            .iter()
            .filter_map(|o| o.parse().ok())
            .collect();
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any)
    };

    // Public API routes.
    let public = Router::new()
        .route("/presets", get(presets::list_presets))
        .route("/presets/{name}", get(presets::get_preset))
        .route("/invite/{token}", get(invites::get_invite_info))
        .route("/invite/{token}/claim", post(invites::claim_invite))
        .route("/public-key", get(presets::get_public_key));

    // Auth (no session required).
    let auth_routes = Router::new()
        .route("/auth/login", post(auth::login));

    // Admin API routes (require session token).
    let admin = Router::new()
        .route("/presets", post(presets::create_preset))
        .route("/presets/{name}", put(presets::update_preset))
        .route("/presets/{name}", delete(presets::delete_preset))
        .route("/invites", get(invites::list_invites))
        .route("/invites", post(invites::create_invite))
        .route("/invites/{token}", delete(invites::revoke_invite))
        .route("/invite-defaults", get(invites::get_invite_defaults))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_admin,
        ));

    let api = Router::new()
        .nest("/api/v1", public)
        .nest("/api/v1", auth_routes)
        .nest("/api/v1/admin", admin)
        .with_state(state)
        .layer(cors)
        .layer(TraceLayer::new_for_http());

    // SPA fallback for admin UI.
    api.fallback_service(spa_service())
}
