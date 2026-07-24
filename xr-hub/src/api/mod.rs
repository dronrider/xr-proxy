pub mod app;
pub mod auth;
pub mod dist;
pub mod invites;
pub mod presets;
pub mod register;
pub mod share_v2;
pub mod shares;

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
        .route("/invite/{token}/view", get(invites::view_invite))
        .route("/invite/{token}/claim", post(invites::claim_invite))
        .route("/public-key", get(presets::get_public_key))
        .route("/app/latest", get(app::get_latest))
        .route("/app/download/{ver}", get(app::download))
        // Автоустановка (XR-015): бинари xr-setup/xr-server/xr-hub и install.sh.
        .route("/setup/{file}", get(dist::serve_setup))
        .route("/shares", get(shares::list_shares))
        .route("/share/register", post(register::register))
        // v2 self-service multishare (agent-authenticated by reg-token/credential).
        .route("/relay", get(share_v2::get_relay))
        .route("/share/exchange", post(share_v2::exchange))
        .route("/share/add", post(share_v2::add))
        .route("/share/mint", post(share_v2::mint))
        .route("/share/unshare", post(share_v2::unshare))
        // XR-031: shares attach to invites; consumer lists its shares by invite.
        .route("/share/attach", post(share_v2::attach))
        .route("/share/detach", post(share_v2::detach))
        .route("/invite/{token}/shares", get(share_v2::invite_shares));

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
        .route("/shares", get(shares::admin_list_shares))
        .route("/shares", post(shares::create_share))
        .route("/shares/{id}", delete(shares::delete_share))
        .route("/shares/{id}/token", post(shares::mint_token))
        .route("/shares/reg-token", post(register::create_reg_token))
        .route("/shares/setup-token", post(register::create_setup_token))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_admin,
        ));

    let api = Router::new()
        .nest("/api/v1", public)
        .nest("/api/v1", auth_routes)
        .nest("/api/v1/admin", admin)
        // Top-level so the install one-liner is a clean URL (xr-share dist).
        .route("/share/{file}", get(dist::serve))
        // Красивый путь инвайта из QR/шаринга: в браузере ведём на HTML-view
        // (сама ручка живёт под /api/v1, голый путь иначе уходит в SPA админки).
        .route("/invite/{token}", get(invites::redirect_to_view))
        .route("/invite/{token}/view", get(invites::redirect_to_view))
        .with_state(state)
        .layer(cors)
        .layer(TraceLayer::new_for_http());

    // SPA fallback for admin UI.
    api.fallback_service(spa_service())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    use super::*;

    fn empty_state() -> Arc<AppState> {
        let config: crate::config::HubConfig =
            toml::from_str("[server]\n[admin]\nusers = []").unwrap();
        Arc::new(AppState {
            presets: RwLock::new(HashMap::new()),
            invites: RwLock::new(HashMap::new()),
            shares: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
            config,
            signing: None,
        })
    }

    async fn get(uri: &str) -> axum::response::Response {
        router(empty_state())
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    // Регрессия XR-130: голый путь /invite/<token> проваливался в SPA-заглушку
    // админки (HTTP 200, <title>xr-hub Admin</title>), и ссылка из QR/шаринга не
    // открывалась у получателя. Теперь верхнеуровневый маршрут редиректит на
    // HTML-view под /api/v1. Токен в маршрут не заглядывает, инвайт не нужен.
    #[tokio::test]
    async fn pretty_invite_path_redirects_to_view() {
        let resp = get("/invite/SOMETOKEN").await;

        assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/api/v1/invite/SOMETOKEN/view"
        );
    }

    #[tokio::test]
    async fn pretty_invite_view_path_redirects_to_view() {
        let resp = get("/invite/SOMETOKEN/view").await;

        assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/api/v1/invite/SOMETOKEN/view"
        );
    }

    // Токен из пути не должен утекать в Location сырьём: подставленные CR/LF
    // percent-кодируются, инъекции заголовка через голый путь нет.
    #[tokio::test]
    async fn pretty_invite_path_escapes_crlf_in_token() {
        let resp = get("/invite/tok%0d%0aSet-Cookie:x").await;

        assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
        let loc = resp.headers().get(header::LOCATION).unwrap().to_str().unwrap();
        assert!(!loc.contains('\r') && !loc.contains('\n'), "CR/LF в Location: {loc}");
    }

    // Голый путь без нашего маршрута отдавал бы страницу админки. Фиксируем, что
    // раньше туда и проваливалось: SPA-заглушка узнаётся по заголовку страницы.
    #[tokio::test]
    async fn unknown_path_still_serves_spa() {
        let resp = get("/no-such-route").await;

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert!(
            String::from_utf8_lossy(&body).contains("<title>xr-hub Admin</title>"),
            "неизвестный путь должен отдавать SPA админки"
        );
    }
}
