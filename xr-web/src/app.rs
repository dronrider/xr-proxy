//! Браузерный вход: адресация по `Host`, гейт сессии, проксирование в туннель
//! (LLD-38 п. 2.2, п. 2.3, п. 2.5).
//!
//! Публикация адресуется поддоменом, потому что проксируемое приложение видит
//! себя в корне: относительные пути, адреса WebSocket, его собственные cookie и
//! редиректы работают без переписывания. Служебные пути фронта живут под
//! префиксом `/.xr-web/` и до агента не доезжают, поэтому у приложения не
//! отбирается ни один его собственный путь.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{FromRequest, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Router;
use xr_proto::share::{
    encode_expose_mandate, valid_publication_name, WebRoute, EXPOSE_HEADER, FORWARDED_AUTH_HEADER,
};

use crate::hub::{HubApi, HubError, RouteCache};
use crate::pool::{AgentPool, LeasedBody};
use crate::session::{self, Sessions};

/// Префикс служебных путей фронта. Имя выбрано так, чтобы не пересечься с
/// путями проксируемого приложения (LLD-38 п. 2.2).
pub const SERVICE_PREFIX: &str = "/.xr-web/";

/// Метка, зарезервированная под страницы шар (`s.<домен>`, LLD-38 п. 2.6).
/// Публикацией она не считается даже сейчас, пока фазы 4 нет: иначе имя `s` в
/// реестре хаба однажды перехватило бы весь путь шар.
pub const SHARE_LABEL: &str = "s";

/// Заголовки, которые не переезжают через посредника: они про соединение, а не
/// про запрос. `Connection: close` от браузера иначе рубил бы соединение из
/// пула, а `Upgrade` без поддержки апгрейда обещал бы то, чего фаза 2 не умеет.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Заголовки адреса браузера: до агента они не едут (LLD-38 п. 2.3), адрес
/// остаётся в логе фронта. Приходящие снаружи снимаются, чтобы приложение не
/// принимало за адрес клиента то, что ему написал кто угодно.
const CLIENT_ADDR_HEADERS: [&str; 3] = ["x-forwarded-for", "x-real-ip", "forwarded"];

/// Сколько промахов пароля проходят без задержки на пару «адрес, публикация».
const FREE_ATTEMPTS: u32 = 3;
/// Потолок задержки: пять минут, как и у хаба.
const MAX_DELAY_MS: u64 = 5 * 60 * 1000;

/// Адрес браузера, положенный в запрос листенером. Через фронт он приезжает
/// заголовком, без фронта берётся у сокета.
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub std::net::SocketAddr);

pub struct WebState {
    /// Web-домен: публикация живёт на `<имя>.<домен>`. Значение приходит
    /// конфигом, в коде его нет.
    pub domain: String,
    pub hub: Arc<dyn HubApi>,
    pub routes: RouteCache,
    pub pool: Arc<AgentPool>,
    pub sessions: Sessions,
    pub attempts: LoginAttempts,
}

impl WebState {
    pub fn new(domain: String, hub: Arc<dyn HubApi>, pool: Arc<AgentPool>, session_ttl: u64) -> Self {
        Self {
            domain: domain.trim().trim_start_matches('.').to_ascii_lowercase(),
            routes: RouteCache::new(hub.clone()),
            hub,
            pool,
            sessions: Sessions::new(session_ttl),
            attempts: LoginAttempts::default(),
        }
    }
}

/// Роутер фронта. Служебные пути разобраны явно, всё остальное это запрос к
/// публикации.
pub fn router(state: Arc<WebState>) -> Router {
    Router::new()
        .route("/.xr-web/healthz", get(healthz))
        .route("/.xr-web/login", post(login))
        .route("/.xr-web/logout", post(logout))
        .fallback(any(entry))
        .with_state(state)
}

/// Состояние самого фронта. Отвечает на любом поддомене и до агента не идёт:
/// живость публикации это `GET /api/v1/web/status` на хабе.
async fn healthz() -> Response {
    (StatusCode::OK, "ok").into_response()
}

#[derive(serde::Deserialize)]
struct LoginForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

async fn login(State(state): State<Arc<WebState>>, req: Request) -> Response {
    let host = host_of(req.headers());
    let peer = peer_of(&req);
    let Some(publication) = publication_of(&host, &state.domain) else {
        return unknown_host(&host, &state.domain);
    };

    let form = match axum::Form::<LoginForm>::from_request(req, &()).await {
        Ok(axum::Form(form)) => form,
        Err(_) => {
            return html(
                StatusCode::BAD_REQUEST,
                pages_login(&publication, Some("форма входа не разобралась")),
            )
        }
    };

    let key = format!("{peer}|{publication}");
    if let Some(wait_ms) = state.attempts.blocked_for(&key, now_ms()) {
        let secs = wait_ms.div_ceil(1000);
        tracing::warn!("вход {publication}: адрес {peer} упёрся в задержку, ещё {secs} с");
        return html(
            StatusCode::TOO_MANY_REQUESTS,
            pages_login(
                &publication,
                Some(&format!("слишком много попыток, повторить через {secs} с")),
            ),
        );
    }

    match state
        .hub
        .verify_password(form.username.clone(), form.password)
        .await
    {
        Ok(true) => {
            state.attempts.succeeded(&key);
            let token = state.sessions.open(&publication, &form.username, now_unix());
            tracing::info!("вход {publication}: {} с адреса {peer}, успех", form.username);
            let cookie = session::set_cookie(&token, state.sessions.ttl_secs());
            (
                StatusCode::SEE_OTHER,
                [
                    (header::SET_COOKIE, cookie),
                    (header::LOCATION, "/".to_string()),
                ],
            )
                .into_response()
        }
        Ok(false) => {
            state.attempts.failed(&key, now_ms());
            tracing::warn!(
                "вход {publication}: {} с адреса {peer}, неверный пароль",
                form.username
            );
            html(
                StatusCode::UNAUTHORIZED,
                pages_login(&publication, Some("неверный логин или пароль")),
            )
        }
        Err(e) => {
            tracing::warn!("вход {publication}: хаб не дал вердикта ({e})");
            html(
                StatusCode::SERVICE_UNAVAILABLE,
                crate::pages::failure(
                    "Вход недоступен",
                    &format!("хаб не ответил на проверку пароля: {e}"),
                ),
            )
        }
    }
}

async fn logout(State(state): State<Arc<WebState>>, headers: HeaderMap) -> Response {
    if let Some(token) = session_token(&headers) {
        state.sessions.close(&token);
    }
    (
        StatusCode::SEE_OTHER,
        [
            (header::SET_COOKIE, session::clear_cookie()),
            (header::LOCATION, "/".to_string()),
        ],
    )
        .into_response()
}

/// Запрос к публикации: адресация, гейт сессии, маршрут, туннель.
async fn entry(State(state): State<Arc<WebState>>, req: Request) -> Response {
    let started = std::time::Instant::now();
    let method = req.method().clone();
    // Путь без query: там едут токены шар, и в лог они не попадают (п. 2.5).
    let path = req.uri().path().to_string();
    let host = host_of(req.headers());

    let Some(publication) = publication_of(&host, &state.domain) else {
        return unknown_host(&host, &state.domain);
    };
    // Всё под служебным префиксом, что не разобрали явные маршруты, остаётся у
    // фронта: до агента такой путь не доезжает.
    if path.starts_with(SERVICE_PREFIX) || path == SERVICE_PREFIX.trim_end_matches('/') {
        return html(
            StatusCode::NOT_FOUND,
            crate::pages::failure(
                "Нет такой служебной ручки",
                "пути под /.xr-web/ обслуживает сам вход и в публикацию они не идут",
            ),
        );
    }

    let response = serve_publication(&state, &publication, &host, req).await;
    tracing::info!(
        "{publication} {method} {path} -> {} за {} мс",
        response.status().as_u16(),
        started.elapsed().as_millis()
    );
    response
}

async fn serve_publication(
    state: &Arc<WebState>,
    publication: &str,
    host: &str,
    req: Request,
) -> Response {
    let session = session_token(req.headers())
        .and_then(|token| state.sessions.touch(&token, publication, now_unix()));
    if session.is_none() {
        // XHR дашборда получает код, а не страницу: HTML в разборе JSON
        // выглядит как поломка сервиса, хотя это всего лишь протухшая сессия.
        return if wants_json(req.headers()) {
            (
                StatusCode::UNAUTHORIZED,
                [(header::CONTENT_TYPE, "application/json")],
                r#"{"error":"unauthenticated"}"#,
            )
                .into_response()
        } else {
            html(StatusCode::OK, pages_login(publication, None))
        };
    }

    let route = match state.routes.get(publication, now_unix()).await {
        Ok(route) => route,
        Err(HubError::NotFound) => {
            return html(
                StatusCode::NOT_FOUND,
                crate::pages::failure(
                    "Публикация не заведена",
                    &format!("хаб не знает публикации {publication}"),
                ),
            )
        }
        Err(e) => {
            tracing::warn!("маршрут {publication} не получен: {e}");
            return html(
                StatusCode::BAD_GATEWAY,
                crate::pages::failure("Маршрут не получен", &format!("{e}")),
            );
        }
    };

    let mut lease = match state.pool.checkout(route.clone()).await {
        Ok(lease) => lease,
        Err(e) => {
            // Агента нет в реестре relay: это штатное «машина выключена», и
            // ретраить его незачем, transit ответил вердиктом за один RTT.
            let offline = e.to_string() == xr_proto::relay_client::RELAY_ERR_AGENT_OFFLINE;
            let (title, detail) = if offline {
                ("Машина не на связи", format!(
                    "агент публикации {publication} сейчас не подключён к relay: включите машину и обновите страницу"
                ))
            } else {
                ("Канал до машины не поднялся", format!("{e}"))
            };
            tracing::warn!("туннель {publication}: {e}");
            return html(StatusCode::BAD_GATEWAY, crate::pages::failure(title, &detail));
        }
    };

    let outgoing = upstream_request(req, &route, host, publication);
    match lease.sender().send_request(outgoing).await {
        Ok(resp) => {
            let (mut parts, body) = resp.into_parts();
            for name in HOP_BY_HOP {
                parts.headers.remove(name);
            }
            if parts.status == StatusCode::FORBIDDEN {
                // Мандат публикации отвергнут агентом: скорее всего он протух
                // или публикация сменила агента. Маршрут забываем, чтобы
                // следующий заход взял свежий мандат, но тут же не повторяем.
                tracing::warn!("публикация {publication}: агент отверг мандат");
                state.routes.forget(publication);
            }
            Response::from_parts(parts, Body::new(LeasedBody::new(body, lease)))
        }
        Err(e) => {
            tracing::warn!("обрыв туннеля {publication}: {e}");
            html(
                StatusCode::BAD_GATEWAY,
                crate::pages::failure(
                    "Соединение с машиной оборвалось",
                    &format!("запрос до агента не доехал: {e}"),
                ),
            )
        }
    }
}

/// Собрать запрос к агенту: пришедший как есть плюс служебные заголовки
/// публикации (LLD-38 п. 2.3).
fn upstream_request(req: Request, route: &WebRoute, host: &str, publication: &str) -> Request {
    let (mut parts, body) = req.into_parts();
    let headers = &mut parts.headers;

    for name in HOP_BY_HOP {
        headers.remove(name);
    }
    for name in CLIENT_ADDR_HEADERS {
        headers.remove(name);
    }
    // Свой Authorization браузера уступает место мандату, но не пропадает:
    // приложению за апстримом он может быть нужен, и агент вернёт его назад.
    if let Some(theirs) = headers.remove(header::AUTHORIZATION) {
        headers.insert(HeaderName::from_static(FORWARDED_AUTH_HEADER), theirs);
    }
    if let Ok(value) = HeaderValue::from_str(&format!(
        "Bearer {}",
        encode_expose_mandate(&route.expose_token)
    )) {
        headers.insert(header::AUTHORIZATION, value);
    }
    if let Ok(value) = HeaderValue::from_str(publication) {
        headers.insert(HeaderName::from_static(EXPOSE_HEADER), value);
    }
    headers.insert(
        HeaderName::from_static("x-forwarded-proto"),
        HeaderValue::from_static("https"),
    );
    if let Ok(value) = HeaderValue::from_str(host) {
        headers.insert(HeaderName::from_static("x-forwarded-host"), value);
    }
    // Cookie приложения едут дальше, своя сессия фронта нет.
    if let Some(raw) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        match session::cookie_header_without_session(raw) {
            Some(rest) => {
                if let Ok(value) = HeaderValue::from_str(&rest) {
                    headers.insert(header::COOKIE, value);
                }
            }
            None => {
                headers.remove(header::COOKIE);
            }
        }
    }
    Request::from_parts(parts, body)
}

/// Имя публикации из `Host`. Порт отбрасывается, регистр не важен, всё, что не
/// поддомен web-домена, публикацией не считается.
pub fn publication_of(host: &str, domain: &str) -> Option<String> {
    let host = host.trim().to_ascii_lowercase();
    let host = host.split(':').next().unwrap_or_default().trim_end_matches('.');
    let name = host.strip_suffix(&format!(".{domain}"))?;
    (valid_publication_name(name) && name != SHARE_LABEL).then(|| name.to_string())
}

fn unknown_host(host: &str, domain: &str) -> Response {
    // Путь шар (`s.<домен>`) это фаза 4 (XR-265): пока его нет, отказ называет
    // причину, а не притворяется поломкой.
    let detail = if host.split(':').next().unwrap_or_default() == format!("{SHARE_LABEL}.{domain}") {
        "страница шары наружу ещё не включена".to_string()
    } else {
        format!("имя {host} не адресует публикацию на домене {domain}")
    };
    html(
        StatusCode::NOT_FOUND,
        crate::pages::failure("Нет такой публикации", &detail),
    )
}

fn host_of(headers: &HeaderMap) -> String {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// Адрес браузера для лога и лимита попыток: за фронтом он приезжает
/// заголовком, без фронта берётся у сокета.
fn peer_of(req: &Request) -> String {
    if let Some(forwarded) = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return forwarded.to_string();
    }
    req.extensions()
        .get::<PeerAddr>()
        .map(|p| p.0.ip().to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(session::token_from_cookie_header)
}

/// Ждёт ли запрос машинного ответа: XHR дашборда получает 401 и JSON, а
/// человек за браузером страницу входа.
fn wants_json(headers: &HeaderMap) -> bool {
    if headers.contains_key("x-requested-with") {
        return true;
    }
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("application/json"))
}

fn html(status: StatusCode, body: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

fn pages_login(publication: &str, error: Option<&str>) -> String {
    crate::pages::login(publication, error)
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Счётчик промахов пароля на пару «адрес, публикация» с растущей задержкой
/// (LLD-38 п. 3.2). Второй рубеж, свой лимит есть и у хаба: пароль владельца
/// стоит за запуском задач на его машине.
#[derive(Default)]
pub struct LoginAttempts {
    inner: Mutex<HashMap<String, Attempt>>,
}

#[derive(Default)]
struct Attempt {
    failures: u32,
    blocked_until_ms: u64,
}

impl LoginAttempts {
    /// Сколько ещё миллисекунд ждать; `None` значит можно пробовать сейчас.
    pub fn blocked_for(&self, key: &str, now_ms: u64) -> Option<u64> {
        let map = self.inner.lock().expect("attempts lock");
        let a = map.get(key)?;
        (a.blocked_until_ms > now_ms).then(|| a.blocked_until_ms - now_ms)
    }

    pub fn failed(&self, key: &str, now_ms: u64) {
        let mut map = self.inner.lock().expect("attempts lock");
        let a = map.entry(key.to_string()).or_default();
        a.failures = a.failures.saturating_add(1);
        let over = a.failures.saturating_sub(FREE_ATTEMPTS);
        let delay = if over == 0 {
            0
        } else {
            (1000u64 << (over - 1).min(20)).min(MAX_DELAY_MS)
        };
        a.blocked_until_ms = now_ms.saturating_add(delay);
    }

    pub fn succeeded(&self, key: &str) {
        self.inner.lock().expect("attempts lock").remove(key);
    }
}

/// Стенд фронта для тестов: состояние плюс ручки на поддельные хаб и агента,
/// по которым судятся кеш и отсутствие лишних походов.
#[cfg(test)]
pub struct Stand {
    pub state: Arc<WebState>,
    pub hub: Arc<crate::test_support::FakeHub>,
    pub dialer: Arc<crate::test_support::DuplexDialer>,
}

#[cfg(test)]
pub fn stand(mode: crate::test_support::HubMode, dialer: crate::test_support::DuplexDialer) -> Stand {
    let hub = Arc::new(crate::test_support::FakeHub::new(mode));
    let dialer = Arc::new(dialer);
    let state = Arc::new(WebState::new(
        "web.example.com".into(),
        hub.clone(),
        AgentPool::new(dialer.clone()),
        7 * 24 * 3600,
    ));
    Stand { state, hub, dialer }
}

/// Состояние со здоровыми хабом и агентом: нужно тестам листенера, которым
/// интересна только склейка.
#[cfg(test)]
pub fn tests_support_state() -> Arc<WebState> {
    stand(
        crate::test_support::HubMode::Route(now_unix() + 3600),
        crate::test_support::DuplexDialer::serving(),
    )
    .state
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{agent_pubkey, hub_key, DuplexDialer, HubMode};
    use base64::Engine as _;
    use std::sync::atomic::Ordering;
    use tower::ServiceExt;
    use xr_proto::share::verify_expose_token;

    const HOST: &str = "dash.web.example.com";

    fn far_future() -> u64 {
        now_unix() + 3600
    }

    fn get(host: &str, path: &str) -> axum::http::request::Builder {
        Request::builder().method("GET").uri(path).header(header::HOST, host)
    }

    async fn call(state: &Arc<WebState>, req: Request) -> Response {
        router(state.clone()).oneshot(req).await.expect("роутер не падает")
    }

    async fn text(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22).await.expect("тело");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Пройти вход и вернуть значение cookie сессии.
    async fn sign_in(state: &Arc<WebState>, host: &str) -> String {
        let req = Request::builder()
            .method("POST")
            .uri("/.xr-web/login")
            .header(header::HOST, host)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("username=owner&password=открой"))
            .unwrap();
        let resp = call(state, req).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER, "верный пароль пускает");
        let cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .expect("вход обязан поставить cookie")
            .to_str()
            .unwrap()
            .to_string();
        cookie
    }

    fn cookie_value(set_cookie: &str) -> String {
        set_cookie.split(';').next().unwrap().to_string()
    }

    #[tokio::test]
    async fn test_web_requires_session() {
        let s = stand(HubMode::Route(far_future()), DuplexDialer::serving());

        // Обычный запрос без сессии: форма входа, а не отказ. И до агента он
        // не идёт: гейт стоит раньше туннеля.
        let resp = call(&s.state, get(HOST, "/").body(Body::empty()).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let page = text(resp).await;
        assert!(page.contains("/.xr-web/login"), "{page}");
        assert_eq!(s.dialer.dials.load(Ordering::SeqCst), 0, "туннель не поднимался");
        assert_eq!(s.hub.route_calls.load(Ordering::SeqCst), 0, "маршрут не спрашивали");

        // XHR дашборда: код и JSON, а не HTML.
        for req in [
            get(HOST, "/api/state")
                .header(header::ACCEPT, "application/json")
                .body(Body::empty())
                .unwrap(),
            get(HOST, "/api/state")
                .header("x-requested-with", "XMLHttpRequest")
                .body(Body::empty())
                .unwrap(),
        ] {
            let resp = call(&s.state, req).await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(text(resp).await, r#"{"error":"unauthenticated"}"#);
        }

        // После входа тот же запрос проксируется до агента.
        let cookie = cookie_value(&sign_in(&s.state, HOST).await);
        let resp = call(
            &s.state,
            get(HOST, "/").header(header::COOKIE, &cookie).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(text(resp).await.contains("agent ok"));
    }

    #[tokio::test]
    async fn wrong_password_and_hub_silence_are_told_apart() {
        let s = stand(HubMode::Route(far_future()), DuplexDialer::serving());
        let req = Request::builder()
            .method("POST")
            .uri("/.xr-web/login")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("username=owner&password=мимо"))
            .unwrap();
        let resp = call(&s.state, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(resp.headers().get(header::SET_COOKIE).is_none(), "сессии нет");
        assert!(text(resp).await.contains("неверный логин или пароль"));

        // Хаб не отвечает: это 503 с причиной, а не «неверный пароль».
        let hub = Arc::new(crate::test_support::FakeHub {
            hub_down_for_password: true,
            ..crate::test_support::FakeHub::new(HubMode::Route(far_future()))
        });
        let state = Arc::new(WebState::new(
            "web.example.com".into(),
            hub,
            AgentPool::new(Arc::new(DuplexDialer::serving())),
            3600,
        ));
        let req = Request::builder()
            .method("POST")
            .uri("/.xr-web/login")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("username=owner&password=открой"))
            .unwrap();
        let resp = call(&state, req).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(text(resp).await.contains("хаб не ответил"));
    }

    #[tokio::test]
    async fn test_web_session_cookie_is_host_only() {
        let s = stand(HubMode::Route(far_future()), DuplexDialer::serving());
        let set_cookie = sign_in(&s.state, HOST).await;

        for flag in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/"] {
            assert!(set_cookie.contains(flag), "в cookie нет {flag}: {set_cookie}");
        }
        assert!(
            !set_cookie.to_ascii_lowercase().contains("domain"),
            "cookie обязана быть host-only: {set_cookie}"
        );

        // Та же cookie на соседней публикации: сессия не считается, и до
        // агента запрос не идёт. Браузер её туда и не отдаст, но токен могут
        // принести руками.
        let dials_before = s.dialer.dials.load(Ordering::SeqCst);
        let resp = call(
            &s.state,
            get("notes.web.example.com", "/")
                .header(header::COOKIE, cookie_value(&set_cookie))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(text(resp).await.contains("/.xr-web/login"), "чужая публикация просит вход");
        assert_eq!(s.dialer.dials.load(Ordering::SeqCst), dials_before);
    }

    #[tokio::test]
    async fn test_web_route_cache_and_refresh() {
        // Свежий маршрут кешируется: страница с десятком запросов не стоит
        // десяти походов в хаб.
        let s = stand(HubMode::Route(far_future()), DuplexDialer::serving());
        let cookie = cookie_value(&sign_in(&s.state, HOST).await);
        for _ in 0..3 {
            let resp = call(
                &s.state,
                get(HOST, "/").header(header::COOKIE, &cookie).body(Body::empty()).unwrap(),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::OK);
        }
        assert_eq!(s.hub.route_calls.load(Ordering::SeqCst), 1);

        // Маршрут, которому осталось меньше запаса: каждый запрос берёт его
        // заново, иначе мандат умрёт на полпути.
        let stale = stand(HubMode::Route(now_unix() + 5), DuplexDialer::serving());
        let cookie = cookie_value(&sign_in(&stale.state, HOST).await);
        for _ in 0..2 {
            let resp = call(
                &stale.state,
                get(HOST, "/").header(header::COOKIE, &cookie).body(Body::empty()).unwrap(),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::OK);
        }
        assert_eq!(stale.hub.route_calls.load(Ordering::SeqCst), 2);

        // Хаб молчит: 502 с названной причиной, а не пятисотка без слов.
        let down = stand(HubMode::Down, DuplexDialer::serving());
        let cookie = cookie_value(&sign_in(&down.state, HOST).await);
        let resp = call(
            &down.state,
            get(HOST, "/").header(header::COOKIE, &cookie).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let page = text(resp).await;
        assert!(page.contains("хаб не ответил"), "{page}");
        assert_eq!(down.dialer.dials.load(Ordering::SeqCst), 0, "без маршрута туннель не поднимаем");

        // Публикации нет в реестре: 404 с этой самой причиной.
        let missing = stand(HubMode::Missing, DuplexDialer::serving());
        let cookie = cookie_value(&sign_in(&missing.state, HOST).await);
        let resp = call(
            &missing.state,
            get(HOST, "/").header(header::COOKIE, &cookie).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(text(resp).await.contains("dash"));
    }

    #[tokio::test]
    async fn test_web_agent_offline_page() {
        let s = stand(HubMode::Route(far_future()), DuplexDialer::offline());
        let cookie = cookie_value(&sign_in(&s.state, HOST).await);
        let resp = call(
            &s.state,
            get(HOST, "/").header(header::COOKIE, &cookie).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let page = text(resp).await;
        assert!(page.contains("не на связи"), "причина обязана быть названа: {page}");
        // Ретрай-шторма нет: вердикт relay получен за один заход, и второго
        // соединения на тот же запрос фронт не открывает.
        assert_eq!(s.dialer.dials.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn request_reaches_the_agent_as_it_came() {
        let s = stand(HubMode::Route(far_future()), DuplexDialer::serving());
        let cookie = cookie_value(&sign_in(&s.state, HOST).await);
        let req = Request::builder()
            .method("POST")
            .uri("/api/run?job=build&n=2")
            .header(header::HOST, HOST)
            .header(header::COOKIE, format!("{cookie}; app_sid=42"))
            .header(header::AUTHORIZATION, "Bearer app-token")
            .header("x-forwarded-for", "198.51.100.9")
            .header("connection", "close")
            .body(Body::from("полезная нагрузка"))
            .unwrap();
        let seen = text(call(&s.state, req).await).await;

        assert!(seen.contains("POST /api/run?job=build&n=2"), "метод, путь и query как пришли: {seen}");
        assert!(seen.contains("body=полезная нагрузка"), "тело доехало: {seen}");
        assert!(seen.contains("x-xr-expose=dash"), "{seen}");
        assert!(seen.contains("x-forwarded-proto=https"), "{seen}");
        assert!(seen.contains(&format!("x-forwarded-host={HOST}")), "{seen}");
        assert!(
            seen.contains("x-xr-forwarded-authorization=Bearer app-token"),
            "свой Authorization браузера переезжает, а не пропадает: {seen}"
        );
        // Cookie приложения доехала, сессия фронта нет.
        assert!(seen.contains("cookie=app_sid=42"), "{seen}");
        assert!(!seen.contains("xrweb="), "сессия фронта до агента не едет: {seen}");
        // Адрес браузера остаётся на VPS, hop-by-hop не переезжает.
        assert!(!seen.contains("x-forwarded-for"), "{seen}");
        assert!(!seen.contains("connection=close"), "{seen}");

        // В Authorization едет мандат публикации, и он настоящий: проверяется
        // ключом хаба на то же имя и того же агента.
        let blob = seen
            .lines()
            .find_map(|l| l.strip_prefix("authorization=Bearer "))
            .expect("мандат обязан быть предъявлен");
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(blob)
            .expect("мандат это base64url-блоб");
        let token: xr_proto::share::ExposeToken = serde_json::from_slice(&json).unwrap();
        assert_eq!(
            verify_expose_token(&token, &hub_key().verifying_key(), "dash", &agent_pubkey(), now_unix()),
            Ok(())
        );
    }

    #[tokio::test]
    async fn service_paths_never_reach_the_agent() {
        let s = stand(HubMode::Route(far_future()), DuplexDialer::serving());

        // Здоровье фронта отвечает и без сессии, и без агента.
        let resp = call(&s.state, get(HOST, "/.xr-web/healthz").body(Body::empty()).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(text(resp).await, "ok");

        let cookie = cookie_value(&sign_in(&s.state, HOST).await);
        let dials = s.dialer.dials.load(Ordering::SeqCst);
        let resp = call(
            &s.state,
            get(HOST, "/.xr-web/whatever")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(s.dialer.dials.load(Ordering::SeqCst), dials, "служебный путь до агента не идёт");

        // Логаут снимает сессию, и следующий запрос снова просит вход.
        let resp = call(
            &s.state,
            Request::builder()
                .method("POST")
                .uri("/.xr-web/logout")
                .header(header::HOST, HOST)
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert!(s.state.sessions.is_empty());
        let resp = call(
            &s.state,
            get(HOST, "/").header(header::COOKIE, &cookie).body(Body::empty()).unwrap(),
        )
        .await;
        assert!(text(resp).await.contains("/.xr-web/login"));
    }

    #[tokio::test]
    async fn foreign_host_is_refused_by_name() {
        let s = stand(HubMode::Route(far_future()), DuplexDialer::serving());
        for (host, expect) in [
            ("web.example.com", "не адресует публикацию"),
            ("dash.web.other.com", "не адресует публикацию"),
            // Путь шар это фаза 4: отказ называет причину, а не молчит.
            ("s.web.example.com", "шары"),
        ] {
            let resp = call(&s.state, get(host, "/").body(Body::empty()).unwrap()).await;
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{host}");
            assert!(text(resp).await.contains(expect), "{host}");
        }
        assert_eq!(s.hub.route_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn publication_is_taken_only_from_our_domain() {
        let d = "web.example.com";
        assert_eq!(publication_of("dash.web.example.com", d).as_deref(), Some("dash"));
        assert_eq!(publication_of("DASH.Web.Example.Com:443", d).as_deref(), Some("dash"));
        assert_eq!(publication_of("web.example.com", d), None);
        assert_eq!(publication_of("dash.web.example.com.evil.net", d), None);
        assert_eq!(publication_of("a.b.web.example.com", d), None, "метка одна, поддомены глубже не наши");
        assert_eq!(publication_of("", d), None);
        // Метка `s` отдана страницам шар (фаза 4) и публикацией не считается.
        assert_eq!(publication_of("s.web.example.com", d), None);
    }

    #[tokio::test]
    async fn mandate_refused_by_the_agent_drops_the_cached_route() {
        // Агент отверг мандат: маршрут забываем, чтобы следующий заход взял
        // свежий, но тут же не повторяем (ретрай-шторма быть не должно).
        let s = stand(HubMode::Route(far_future()), DuplexDialer::refusing_mandate());
        let cookie = cookie_value(&sign_in(&s.state, HOST).await);
        for _ in 0..2 {
            let resp = call(
                &s.state,
                get(HOST, "/").header(header::COOKIE, &cookie).body(Body::empty()).unwrap(),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        }
        assert_eq!(s.hub.route_calls.load(Ordering::SeqCst), 2, "маршрут взят заново");
        assert_eq!(
            s.dialer.dials.load(Ordering::SeqCst),
            1,
            "отвергнут мандат, а не транспорт: соединение из пула переиспользуется"
        );
    }

    #[tokio::test]
    async fn test_web_login_rate_limit() {
        let s = stand(HubMode::Route(far_future()), DuplexDialer::serving());
        let wrong = || {
            Request::builder()
                .method("POST")
                .uri("/.xr-web/login")
                .header(header::HOST, HOST)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("username=owner&password=мимо"))
                .unwrap()
        };
        for _ in 0..FREE_ATTEMPTS {
            assert_eq!(call(&s.state, wrong()).await.status(), StatusCode::UNAUTHORIZED);
        }
        // Четвёртый промах включает задержку, и следующая попытка упирается в
        // неё, не доходя до хаба.
        assert_eq!(call(&s.state, wrong()).await.status(), StatusCode::UNAUTHORIZED);
        let calls = s.hub.verify_calls.load(Ordering::SeqCst);
        let resp = call(&s.state, wrong()).await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(text(resp).await.contains("слишком много попыток"));
        assert_eq!(s.hub.verify_calls.load(Ordering::SeqCst), calls, "хаб не дёргается под задержкой");
    }

    #[test]
    fn attempts_delay_grows_and_success_clears_it() {
        let a = LoginAttempts::default();
        for i in 0..FREE_ATTEMPTS {
            a.failed("ip|dash", 1_000);
            assert_eq!(a.blocked_for("ip|dash", 1_000), None, "промах {i} ещё свободный");
        }
        a.failed("ip|dash", 1_000);
        assert_eq!(a.blocked_for("ip|dash", 1_000), Some(1_000));
        a.failed("ip|dash", 1_000);
        assert_eq!(a.blocked_for("ip|dash", 1_000), Some(2_000), "задержка удваивается");
        assert_eq!(a.blocked_for("ip|dash", 3_100), None, "задержка проходит");
        // Соседняя публикация с того же адреса своей задержки не наследует.
        assert_eq!(a.blocked_for("ip|notes", 1_000), None);
        a.succeeded("ip|dash");
        assert_eq!(a.blocked_for("ip|dash", 1_000), None);
    }

    #[test]
    fn json_is_wanted_only_when_asked() {
        let mut h = HeaderMap::new();
        assert!(!wants_json(&h));
        h.insert(header::ACCEPT, HeaderValue::from_static("text/html,*/*"));
        assert!(!wants_json(&h));
        h.insert(header::ACCEPT, HeaderValue::from_static("Application/JSON"));
        assert!(wants_json(&h));
        let mut h = HeaderMap::new();
        h.insert("x-requested-with", HeaderValue::from_static("XMLHttpRequest"));
        assert!(wants_json(&h));
    }

    #[test]
    fn peer_prefers_the_front_header() {
        let req = Request::builder()
            .uri("/")
            .header("x-forwarded-for", "198.51.100.9, 10.0.0.1")
            .body(Body::empty())
            .unwrap();
        assert_eq!(peer_of(&req), "198.51.100.9");

        let mut req = Request::builder().uri("/").body(Body::empty()).unwrap();
        req.extensions_mut().insert(PeerAddr("203.0.113.4:5000".parse().unwrap()));
        assert_eq!(peer_of(&req), "203.0.113.4");

        let req = Request::builder().uri("/").body(Body::empty()).unwrap();
        assert_eq!(peer_of(&req), "-", "без адреса лог не врёт чужим");
    }
}
