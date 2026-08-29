pub mod app;
pub mod auth;
pub mod dist;
pub mod health;
pub mod invites;
pub mod presets;
pub mod register;
pub mod share_v2;
pub mod shares;
pub mod web;

use std::sync::Arc;

use axum::http::header;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware;
use axum::routing::{delete, get, post, put};
use axum::Router;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::embed::spa_service;
use crate::state::AppState;

/// Тело ответа, когда состояние не легло на диск. Одно на все публичные ручки
/// и без единой подробности: ошибки `storage` несут путь каталога данных
/// (XR-211), а устройство каталогов хаба постороннему знать незачем.
pub(crate) const PERSIST_FAILED: &str = "failed to persist state";

/// Общий CSP ответов хаба (XR-239). Админка это SPA на том же origin, поэтому
/// всё берём с себя: `data:` нужен QR-кодам, которые фронт рисует в картинку,
/// инлайновые атрибуты стилей остаются у Vue-разметки. Страница инвайта
/// (XR-192) ставит свой, более строгий CSP с nonce, и слой её не трогает.
const DEFAULT_CSP: &str = "default-src 'self'; img-src 'self' data:; \
     style-src 'self' 'unsafe-inline'; object-src 'none'; base-uri 'self'; \
     form-action 'self'; frame-ancestors 'none'";

/// Отказ записи для публичной ручки: наружу уходит [`PERSIST_FAILED`], полная
/// ошибка с путём остаётся в логе оператора. Разбирать отказ по одному
/// «Permission denied» без пути не по чему, поэтому в лог она едет целиком
/// (`{e:#}` разворачивает цепочку контекстов).
pub(crate) fn persist_failed(what: &str, e: anyhow::Error) -> (StatusCode, String) {
    tracing::error!("не сохранилось {what}: {e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, PERSIST_FAILED.to_string())
}

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
        .route("/presets/{name}/wait", get(presets::wait_preset))
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
        .route("/invite/{token}/shares", get(share_v2::invite_shares))
        // LLD-38: реестр публикаций локальных сервисов, тот же мандат агента.
        .route("/expose/add", post(web::add))
        .route("/expose", get(web::list))
        .route("/expose/{name}", delete(web::remove))
        .route("/expose/{name}/mandate", post(web::mandate))
        // LLD-38 п. 3.5: служебные ручки браузерного фронта под общим секретом
        // [web]. Прав админки у фронта нет, ключа подписи он не видит.
        .route("/web/route", post(web::route))
        .route("/web/verify-password", post(web::verify_password))
        .route("/web/status", get(web::status));

    // Auth: login без сессии, logout только с живым Bearer (XR-194).
    let auth_routes = Router::new()
        .route("/auth/login", post(auth::login))
        .route(
            "/auth/logout",
            post(auth::logout).layer(middleware::from_fn_with_state(
                state.clone(),
                auth::require_admin,
            )),
        );

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
        .route("/exposes", get(web::admin_list))
        .route("/exposes/{name}", delete(web::admin_remove))
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
        // Живость и готовность (XR-230): верхний уровень, без аутентификации,
        // чтобы мониторинг и failover не трогали содержательные ручки.
        .route("/healthz", get(health::liveness))
        .route("/readyz", get(health::readiness))
        // Красивый путь инвайта из QR/шаринга: в браузере ведём на HTML-view
        // (сама ручка живёт под /api/v1, голый путь иначе уходит в SPA админки).
        .route("/invite/{token}", get(invites::redirect_to_view))
        .route("/invite/{token}/view", get(invites::redirect_to_view))
        .with_state(state)
        .layer(cors)
        .layer(TraceLayer::new_for_http());

    // SPA fallback for admin UI.
    api.fallback_service(spa_service())
        // Защитные заголовки (XR-239): одним слоем на всё, что отвечает хаб,
        // включая статику админки, поэтому слой стоит после fallback_service:
        // слои, поставленные раньше, на fallback уже не ложатся. Заголовок
        // вписывается только в пустое место, чтобы не задваивать второй CSP рядом
        // с nonce-CSP страницы инвайта.
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(DEFAULT_CSP),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
}

// regcheck:test-begin
/// Перехват лога для тестов: обещание «путь остаётся в логе оператора»
/// проверяется по настоящему выводу tracing, а не по чтению исходников.
#[cfg(test)]
pub(crate) mod testlog {
    use std::io;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub(crate) struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Buffer {
        pub(crate) fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buffer {
        type Writer = Buffer;

        fn make_writer(&'a self) -> Buffer {
            self.clone()
        }
    }

    /// Гвард держать до конца теста: подписчик ставится на текущий поток, а
    /// тесты гоняются на однопоточном рантайме, поэтому его хватает и на await.
    pub(crate) fn capture() -> (Buffer, tracing::subscriber::DefaultGuard) {
        let buf = Buffer::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::ERROR)
            .finish();
        (buf.clone(), tracing::subscriber::set_default(sub))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    use super::*;

    fn empty_config() -> crate::config::HubConfig {
        toml::from_str("[server]\n[admin]\nusers = []").unwrap()
    }

    fn state_from(config: crate::config::HubConfig) -> Arc<AppState> {
        let sessions = crate::sessions::SessionStore::new(
            std::time::Duration::from_secs(config.admin.session_ttl_secs),
            config.admin.max_sessions_per_user,
        );
        state_with_sessions(config, sessions)
    }

    fn state_with_sessions(
        config: crate::config::HubConfig,
        sessions: crate::sessions::SessionStore,
    ) -> Arc<AppState> {
        state_full(
            config,
            sessions,
            Default::default(),
            Arc::new(tokio::sync::Semaphore::new(4)),
        )
    }

    fn state_full(
        config: crate::config::HubConfig,
        sessions: crate::sessions::SessionStore,
        login_attempts: auth::LoginAttempts,
        argon2_gate: Arc<tokio::sync::Semaphore>,
    ) -> Arc<AppState> {
        Arc::new(AppState {
            presets: RwLock::new(HashMap::new()),
            invites: RwLock::new(HashMap::new()),
            shares: RwLock::new(HashMap::new()),
            exposes: RwLock::new(HashMap::new()),
            sessions,
            config,
            signing: None,
            preset_gen: tokio::sync::watch::Sender::new(0),
            web_attempts: Default::default(),
            login_attempts,
            argon2_gate,
            ready: std::sync::atomic::AtomicBool::new(true),
        })
    }

    fn empty_state() -> Arc<AppState> {
        state_from(empty_config())
    }

    async fn get(uri: &str) -> axum::response::Response {
        router(empty_state())
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn body_text(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Состояние с живым инвайтом: собирает его сам `build_invite`, поэтому
    /// токен и payload такие же, как у настоящей ручки создания.
    async fn state_with_invite() -> (Arc<AppState>, String) {
        let dir = tempfile::tempdir().expect("временный каталог для инвайта");
        let mut config = empty_config();
        config.server.data_dir = dir.path().to_str().unwrap().into();
        let state = state_from(config);

        let invite = invites::build_invite(&state, None, true, String::new(), None, None)
            .await
            .expect("инвайт для страницы view");
        (state, invite.token)
    }

    // XR-239: защитные заголовки ставит один слой на всё, что отвечает хаб:
    // JSON API, верхнеуровневые ручки и статику админки из fallback. До него
    // заголовки несла только страница инвайта (XR-192), остальное уходило
    // голым.
    #[tokio::test]
    async fn security_headers_cover_all_responses() {
        for (uri, what) in [
            ("/healthz", "живость"),
            ("/api/v1/presets", "JSON API"),
            ("/invite/SOMETOKEN", "редирект инвайта"),
            ("/no-such-route", "статика админки"),
        ] {
            let resp = get(uri).await;
            let headers = resp.headers();
            assert_eq!(
                headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap_or_else(|| panic!("нет X-Content-Type-Options на {what}")),
                "nosniff"
            );
            assert_eq!(
                headers.get(header::REFERRER_POLICY).unwrap_or_else(|| panic!("нет Referrer-Policy на {what}")),
                "no-referrer"
            );
            let csp = headers
                .get(header::CONTENT_SECURITY_POLICY)
                .unwrap_or_else(|| panic!("нет CSP на {what}"))
                .to_str()
                .unwrap();
            assert!(
                csp.starts_with("default-src 'self'"),
                "на {what} стоит чужой CSP: {csp}"
            );
        }
    }

    // XR-239: страница инвайта несёт свой строгий CSP с nonce (XR-192). Общий
    // слой обязан дополнить ответ недостающими заголовками, а не задвоить CSP:
    // браузер выполнил бы обе политики сразу и строгая перестала бы быть
    // единственной.
    #[tokio::test]
    async fn invite_view_keeps_its_own_csp() {
        let (state, token) = state_with_invite().await;
        let view = format!("/api/v1/invite/{token}/view");
        let resp = router(state)
            .oneshot(Request::builder().uri(view).body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let csps: Vec<_> = resp
            .headers()
            .get_all(header::CONTENT_SECURITY_POLICY)
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        assert_eq!(csps.len(), 1, "CSP обязан быть один: {csps:?}");
        assert!(
            csps[0].starts_with("default-src 'none'") && csps[0].contains("nonce-"),
            "строгий nonce-CSP страницы потерялся: {csps:?}"
        );
        assert_eq!(
            resp.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        assert_eq!(
            resp.headers().get(header::REFERRER_POLICY).unwrap(),
            "no-referrer"
        );
    }

    // XR-230: живость это ответ «ok» без аутентификации и без взгляда на
    // состояние, поэтому аптайм-чек и failover не дёргают содержательные
    // ручки. Запрос идёт без единого заголовка.
    #[tokio::test]
    async fn healthz_answers_ok_without_auth() {
        let resp = get("/healthz").await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_text(resp).await, "ok");
    }

    // XR-230: готовность держит 503, пока hydrate не загрузил инвайты, шары
    // и ключ подписи, и отпускает после его конца.
    #[tokio::test]
    async fn readyz_reflects_hydrate() {
        let state = empty_state();
        state.ready.store(false, std::sync::atomic::Ordering::Release);
        let not_ready = router(state.clone())
            .oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(not_ready.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_text(not_ready).await, "not ready");

        state.ready.store(true, std::sync::atomic::Ordering::Release);
        let ready = router(state)
            .oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(ready.status(), StatusCode::OK);
        assert_eq!(body_text(ready).await, "ready");
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
    // Собран admin UI или вшита заглушка (XR-238), решает build.rs, поэтому
    // ответ сверяем с тем, что вшито.
    #[tokio::test]
    async fn unknown_path_still_serves_spa() {
        let resp = get("/no-such-route").await;

        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8_lossy(&body).to_string();

        if crate::embed::UI_PLACEHOLDER {
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert!(body.contains("npm run build"), "заглушка молчит о сборке: {body}");
        } else {
            assert_eq!(status, StatusCode::OK);
            assert!(
                body.contains("<title>xr-hub Admin</title>"),
                "неизвестный путь должен отдавать SPA админки"
            );
        }
    }

    // ===== Admin-сессии (XR-194) =====

    /// Конфиг с одним оператором и известным паролем. Хэш считается на месте:
    /// вбитый в тест аргон2-блоб устарел бы при смене параметров по умолчанию.
    fn user_config(ttl_secs: u64, max_sessions: usize) -> crate::config::HubConfig {
        let hash = auth::hash_password("secret").expect("хэш пароля для теста");
        toml::from_str(&format!(
            "[server]\n[admin]\nsession_ttl_secs = {ttl_secs}\nmax_sessions_per_user = {max_sessions}\n\
             [[admin.users]]\nusername = \"root\"\npassword_hash = \"{hash}\"\n"
        ))
        .unwrap()
    }

    /// Состояние с ручными часами: тест двигает время сдвигом, а не сном на
    /// длину TTL.
    fn manual_state(
        ttl_secs: u64,
        max_sessions: usize,
    ) -> (
        Arc<AppState>,
        Arc<std::sync::atomic::AtomicU64>,
    ) {
        let offset = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let clock = {
            let offset = offset.clone();
            Arc::new(move || {
                std::time::Instant::now()
                    + std::time::Duration::from_secs(offset.load(std::sync::atomic::Ordering::SeqCst))
            }) as crate::sessions::Clock
        };
        let sessions = crate::sessions::SessionStore::with_clock(
            std::time::Duration::from_secs(ttl_secs),
            max_sessions,
            clock,
        );
        let state = state_with_sessions(user_config(ttl_secs, max_sessions), sessions);
        (state, offset)
    }

    async fn login(state: &Arc<AppState>, username: &str, password: &str) -> String {
        let body = format!("{{\"username\":\"{username}\",\"password\":\"{password}\"}}");
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let text = body_text(resp).await;
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        value["token"].as_str().expect("token в ответе входа").into()
    }

    /// Ответ админской ручки по Bearer-токену: живой ли он ещё.
    async fn admin_get(state: &Arc<AppState>, bearer: &str) -> StatusCode {
        let req = Request::builder()
            .uri("/api/v1/admin/invites")
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            .body(Body::empty())
            .unwrap();
        router(state.clone()).oneshot(req).await.unwrap().status()
    }

    async fn logout(state: &Arc<AppState>, bearer: &str) -> StatusCode {
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/logout")
            .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
            .body(Body::empty())
            .unwrap();
        router(state.clone()).oneshot(req).await.unwrap().status()
    }

    // XR-194: сессия истекает по TTL, и после этого Bearer отвергается,
    // а не живёт до рестарта процесса.
    #[tokio::test]
    async fn expired_session_is_rejected() {
        let (state, offset) = manual_state(60, 5);
        let bearer = login(&state, "root", "secret").await;

        assert_eq!(admin_get(&state, &bearer).await, StatusCode::OK);
        offset.store(61, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            admin_get(&state, &bearer).await,
            StatusCode::UNAUTHORIZED,
            "истёкший по TTL Bearer обязан отвергаться"
        );
    }

    // XR-194: logout гасит токен, повторный logout тем же Bearer уже не
    // проходит, потому что токен мёртв.
    #[tokio::test]
    async fn logout_revokes_bearer() {
        let (state, _offset) = manual_state(3600, 5);
        let bearer = login(&state, "root", "secret").await;

        assert_eq!(logout(&state, &bearer).await, StatusCode::NO_CONTENT);
        assert_eq!(admin_get(&state, &bearer).await, StatusCode::UNAUTHORIZED);
        assert_eq!(logout(&state, &bearer).await, StatusCode::UNAUTHORIZED);
    }

    // XR-194: число сессий на оператора ограничено, новая вытесняет старейшую.
    #[tokio::test]
    async fn session_limit_evicts_oldest() {
        let (state, _offset) = manual_state(3600, 2);
        let first = login(&state, "root", "secret").await;
        let second = login(&state, "root", "secret").await;
        let third = login(&state, "root", "secret").await;

        assert_eq!(admin_get(&state, &first).await, StatusCode::UNAUTHORIZED);
        assert_eq!(admin_get(&state, &second).await, StatusCode::OK);
        assert_eq!(admin_get(&state, &third).await, StatusCode::OK);
    }

    // ===== Лимит попыток входа (XR-195) =====

    /// Состояние с тремя попытками входа в минуту на источник и известным
    /// паролем `secret` (как у `manual_state`, но часы настоящие).
    fn login_limit_state(max_attempts: u32) -> Arc<AppState> {
        let config = user_config(3600, 5);
        let sessions = crate::sessions::SessionStore::new(
            std::time::Duration::from_secs(3600),
            config.admin.max_sessions_per_user,
        );
        state_full(
            config,
            sessions,
            auth::LoginAttempts::new(max_attempts, 60_000),
            Arc::new(tokio::sync::Semaphore::new(auth::argon2_parallelism())),
        )
    }

    /// Логин с явным адресом источника: слушатель кладёт адрес в соединение,
    /// одншот-вызов кладёт его в запрос расширением. Источники различаются
    /// последним октетом.
    async fn login_from(state: &Arc<AppState>, source: u8, password: &str) -> StatusCode {
        let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::new(192, 0, 2, source), 9999));
        let body = format!("{{\"username\":\"root\",\"password\":\"{password}\"}}");
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .extension(axum::extract::ConnectInfo(addr))
            .body(Body::from(body))
            .unwrap();
        router(state.clone()).oneshot(req).await.unwrap().status()
    }

    // XR-195: серия неудачных логинов упирается в лимит, дальнейшие попытки
    // отбиваются без проверки пароля, а содержательные ручки хаба под штормом
    // отвечают.
    #[tokio::test]
    async fn login_storm_hits_limit_and_hub_stays_alive() {
        let state = login_limit_state(3);

        for _ in 0..3 {
            assert_eq!(
                login_from(&state, 1, "wrong").await,
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            login_from(&state, 1, "wrong").await,
            StatusCode::TOO_MANY_REQUESTS,
            "исчерпавший лимит источник обязан получить отказ"
        );
        assert_eq!(
            login_from(&state, 1, "secret").await,
            StatusCode::TOO_MANY_REQUESTS,
            "отказ сидит до конца окна даже с верным паролем, argon2 не зовётся"
        );

        let req = Request::builder()
            .uri("/api/v1/presets")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            router(state.clone()).oneshot(req).await.unwrap().status(),
            StatusCode::OK,
            "под штормом логина пресеты обязаны отвечать"
        );

        assert_eq!(
            login_from(&state, 2, "secret").await,
            StatusCode::OK,
            "лимит одного источника не запирает другого"
        );
    }

    // XR-195: верный вход снимает счётчик, соседние промахи не копятся
    // через успешный логин.
    #[tokio::test]
    async fn successful_login_resets_attempt_counter() {
        let state = login_limit_state(2);

        assert_eq!(login_from(&state, 1, "wrong").await, StatusCode::UNAUTHORIZED);
        assert_eq!(login_from(&state, 1, "secret").await, StatusCode::OK);
        assert_eq!(
            login_from(&state, 1, "wrong").await,
            StatusCode::UNAUTHORIZED,
            "после успеха тот же источник всё ещё в лимите, а не в отказе"
        );
    }
}
// regcheck:test-end
