//! Экспорт локального HTTP-сервиса наружу (LLD-38, фаза 1).
//!
//! Публикация это «имя, агент, локальный адрес». Имя и агент живут в хабе, имя
//! же служит поддоменом браузерного входа, а локальный адрес не уезжает с
//! машины: он берётся строго из `[[expose]]` в конфиге агента по имени из
//! запроса. Приходящий запрос апстрим не выбирает никогда, поэтому браузерный
//! вход не становится плечом SSRF внутрь домашней сети (п. 3.4).
//!
//! Гейт стоит здесь, у агента, и одного relay-токена ему мало. Держатель
//! валидного токена на шару того же агента иначе дотянулся бы до локального
//! сервиса, подставив заголовок публикации: транзит к машине и выбор того, что
//! на ней открыто, это разные права. Поэтому у публикации свой мандат
//! [`ExposeToken`], который выписывает только хаб и проверяет офлайн только
//! агент, тем же пришпиленным ключом, которым он проверяет токены шар.
//!
//! Здесь же живёт харнесс `expose open`: локальный форвардер, который говорит
//! в тот самый обработчик, что и реверс-стрим, подставляя мандат вместо
//! будущего `xr-web`. Он позволяет проверить весь путь до сервиса без VPS,
//! обычным curl, и он же показывает отказ гейта, если мандат не подставлять.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use ed25519_dalek::VerifyingKey;
use xr_proto::share::{
    valid_publication_name, verify_expose_token, ExposeToken, EXPOSE_HEADER, FORWARDED_AUTH_HEADER,
};

use crate::config::{AgentConfig, ExposeEntry};

/// Сколько ждём соединения с апстримом. Апстрим это локальный порт, и если он
/// не отвечает за секунды, значит сервиса нет: браузеру лучше получить внятный
/// отказ, чем висеть на минутном таймауте.
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Заголовки, которые кончаются на этом соединении и дальше не едут (RFC 9110).
/// `upgrade` и `connection` в этом же списке: апгрейд до WebSocket пробрасывает
/// фаза 3 (XR-264), и до неё честнее не пустить его вовсе, чем отдать
/// полусплайс.
const HOP_BY_HOP: [header::HeaderName; 7] = [
    header::CONNECTION,
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

/// Всё, что нужно гейту публикации: пришпиленный ключ хаба, свой ключ агента и
/// публикации из конфига. Живёт и у реверс-стрима (из состояния агента), и у
/// харнесса (прямо из конфига), поэтому проверяемый путь у них общий.
#[derive(Clone)]
pub struct ExposeGate {
    pub hub_key: VerifyingKey,
    /// Base64 (standard) своего identity-ключа. `None` у конфига без
    /// identity: сверить мандат не с чем, и гейт закрыт наглухо.
    pub agent_pubkey: Option<String>,
    pub publications: Arc<Vec<ExposeEntry>>,
}

impl ExposeGate {
    pub fn publication(&self, name: &str) -> Option<&ExposeEntry> {
        self.publications.iter().find(|e| e.name == name)
    }
}

/// Имя публикации, которое назвал посредник. `None` означает обычный запрос к
/// шаре: его обслуживает роутер шары, как и до LLD-38. Заголовок приходит
/// только реверс-стримом, поэтому в сборке без relay эта развилка не нужна
/// никому, кроме тестов.
#[cfg(any(feature = "relay", test))]
pub fn requested_publication(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(EXPOSE_HEADER)?.to_str().ok()?.trim();
    // Имя из чужих рук не должно превращаться в путь или в лишнюю строку
    // журнала: всё, что не DNS-метка, отбрасывается тут же.
    valid_publication_name(raw).then(|| raw.to_string())
}

/// Обслужить запрос к публикации: мандат, поиск апстрима, проксирование.
pub async fn serve(gate: &ExposeGate, name: &str, req: Request) -> Response {
    if let Err(resp) = check_mandate(gate, name, req.headers()) {
        return resp;
    }
    let Some(entry) = gate.publication(name) else {
        tracing::warn!("публикация {name} не заведена в конфиге агента");
        return (
            StatusCode::NOT_FOUND,
            format!(
                "публикация {name} не обслуживается: в конфиге агента нет её [[expose]]-записи"
            ),
        )
            .into_response();
    };
    let upstream = entry.upstream.clone();
    proxy(&upstream, req).await
}

/// Проверить мандат публикации офлайн. Мандата нет, он чужой, протух или
/// выписан на другого агента: 403, и до апстрима запрос не доходит. Причина
/// уходит в лог, сам мандат нет.
fn check_mandate(gate: &ExposeGate, name: &str, headers: &HeaderMap) -> Result<(), Response> {
    let refused = || {
        (
            StatusCode::FORBIDDEN,
            "мандат публикации не принят".to_string(),
        )
            .into_response()
    };
    let Some(agent_pubkey) = gate.agent_pubkey.as_deref() else {
        tracing::warn!("публикация {name}: у агента нет identity-ключа, сверить мандат не с чем");
        return Err(refused());
    };
    let Some(token) = mandate_from_headers(headers) else {
        tracing::warn!("публикация {name}: запрос без мандата");
        return Err(refused());
    };
    match verify_expose_token(&token, &gate.hub_key, name, agent_pubkey, now_unix()) {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::warn!("публикация {name}: мандат отвергнут ({e})");
            Err(refused())
        }
    }
}

/// Достать мандат из `Authorization: Bearer <blob>`. Блоб это base64url-nopad
/// JSON, та же форма, в которой едут токены шар. Чужой по форме блоб (токен
/// шары, relay-токен) сюда не разберётся, а разберись он, его отвергнет
/// подпись: домен у мандата свой.
fn mandate_from_headers(headers: &HeaderMap) -> Option<ExposeToken> {
    let blob = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?
        .trim();
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(blob).ok()?;
    serde_json::from_slice(&json).ok()
}

/// Проксировать запрос на локальный апстрим: тело идёт потоком в обе стороны,
/// служебные заголовки снимаются, `X-Xr-Forwarded-Authorization` возвращается
/// на место `Authorization` (свой заголовок приложения занят был мандатом).
/// `X-Forwarded-*` едут как их поставил посредник: агент не знает, по какой
/// схеме пришёл браузер, и выдумывать её не должен.
async fn proxy(upstream: &str, req: Request) -> Response {
    let (mut parts, body) = req.into_parts();
    parts.headers.remove(EXPOSE_HEADER);
    parts.headers.remove(header::AUTHORIZATION);
    if let Some(v) = parts.headers.remove(FORWARDED_AUTH_HEADER) {
        parts.headers.insert(header::AUTHORIZATION, v);
    }
    for name in HOP_BY_HOP {
        parts.headers.remove(name);
    }

    let stream = match tokio::time::timeout(
        UPSTREAM_CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect(upstream),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return upstream_down(upstream, &e.to_string()),
        Err(_) => return upstream_down(upstream, "таймаут соединения"),
    };
    let (mut sender, conn) = match hyper::client::conn::http1::handshake(
        hyper_util::rt::TokioIo::new(stream),
    )
    .await
    {
        Ok(pair) => pair,
        Err(e) => return upstream_down(upstream, &e.to_string()),
    };
    // Соединение живёт своим таском, пока по нему течёт тело ответа: без него
    // ответ не поедет дальше первой порции.
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!("соединение с апстримом закрылось: {e}");
        }
    });

    match sender.send_request(axum::http::Request::from_parts(parts, body)).await {
        Ok(resp) => {
            let (mut parts, body) = resp.into_parts();
            for name in HOP_BY_HOP {
                parts.headers.remove(name);
            }
            Response::from_parts(parts, Body::new(body))
        }
        Err(e) => upstream_down(upstream, &e.to_string()),
    }
}

/// Апстрим не отозвался. Причину называем и в ответе, и в логе: молчаливая
/// пятисотка неотличима от поломки самого канала.
fn upstream_down(upstream: &str, why: &str) -> Response {
    tracing::warn!("апстрим {upstream} недоступен: {why}");
    (
        StatusCode::BAD_GATEWAY,
        format!("локальный сервис {upstream} не отвечает: {why}"),
    )
        .into_response()
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// Харнесс `expose open`.

/// Локальный форвардер: слушает на `listener` и гонит каждый запрос через тот
/// же гейт и то же проксирование, что и реверс-стрим, подставляя заголовки
/// вместо будущего `xr-web`. `mandate` это блоб мандата от хаба; `None`
/// проверяет отказ гейта (запрос доходит до агента, но права на публикацию не
/// предъявляет, как держатель одного лишь relay-токена).
pub async fn serve_harness(
    listener: tokio::net::TcpListener,
    gate: ExposeGate,
    name: String,
    mandate: Option<String>,
) -> Result<()> {
    let app = axum::Router::new().fallback(axum::routing::any(
        move |mut req: Request| {
            let gate = gate.clone();
            let name = name.clone();
            let mandate = mandate.clone();
            async move {
                let headers = req.headers_mut();
                headers.insert(
                    EXPOSE_HEADER,
                    HeaderValue::from_str(&name).expect("имя публикации это DNS-метка"),
                );
                match &mandate {
                    Some(blob) => {
                        let value = HeaderValue::from_str(&format!("Bearer {blob}"))
                            .unwrap_or(HeaderValue::from_static("Bearer -"));
                        headers.insert(header::AUTHORIZATION, value);
                    }
                    None => {
                        headers.remove(header::AUTHORIZATION);
                    }
                }
                // Схему и внешнее имя ставит посредник, а на харнессе это он и
                // есть: приложению за апстримом видно, что запрос пришёл не
                // напрямую.
                headers.insert("x-forwarded-proto", HeaderValue::from_static("http"));
                serve(&gate, &name, req).await
            }
        },
    ));
    axum::serve(listener, app).await.context("харнесс expose open")
}

/// Собрать гейт из конфига: пришпиленный ключ хаба, свой identity и список
/// публикаций.
pub fn gate_from_config(cfg: &AgentConfig, config_path: &Path) -> Result<ExposeGate> {
    let hub_key = cfg.hub_verifying_key()?;
    let agent_pubkey = cfg.identity_signing_key(config_path)?.map(|k| {
        base64::engine::general_purpose::STANDARD.encode(k.verifying_key().as_bytes())
    });
    Ok(ExposeGate {
        hub_key,
        agent_pubkey,
        publications: Arc::new(cfg.exposes.clone()),
    })
}

// Команды `xr-share expose ...`.

#[derive(clap::Subcommand)]
pub enum ExposeCommand {
    /// Завести публикацию: записать её в хаб и в конфиг агента.
    Add {
        /// Имя публикации, оно же поддомен браузерного входа.
        #[arg(long)]
        name: String,
        /// Локальный адрес сервиса, например 127.0.0.1:8765. Можно не
        /// указывать, если запись в конфиге уже есть.
        #[arg(long)]
        upstream: Option<String>,
    },
    /// Снять публикацию с хаба и из конфига.
    Rm {
        #[arg(long)]
        name: String,
    },
    /// Показать публикации этого агента: что помнит хаб и что обслуживается.
    Ls,
    /// Поднять локальный форвардер к публикации и напечатать его адрес.
    /// Проверка всего пути до сервиса без VPS: `curl` по напечатанному адресу.
    Open {
        #[arg(long)]
        name: String,
        /// Порт форвардера; 0 (по умолчанию) это любой свободный.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Не подставлять мандат публикации: так проверяется отказ гейта.
        /// Запрос доходит до обработчика и получает 403.
        #[arg(long)]
        without_mandate: bool,
    },
}

/// Точка входа команды `expose`.
pub fn run(config_path: &Path, cmd: ExposeCommand) -> Result<()> {
    match cmd {
        ExposeCommand::Add { name, upstream } => add(config_path, &name, upstream.as_deref()),
        ExposeCommand::Rm { name } => rm(config_path, &name),
        ExposeCommand::Ls => ls(config_path),
        ExposeCommand::Open { name, port, without_mandate } => {
            open(config_path, &name, port, !without_mandate)
        }
    }
}

/// Хаб и мандат агента из конфига: без них публикацию не завести.
fn hub_and_credential(cfg: &AgentConfig) -> Result<(String, String)> {
    let hub = cfg
        .hub_url
        .clone()
        .context("в конфиге нет hub_url, переустанови через `xr-share install`")?;
    let cred = cfg
        .agent_credential
        .clone()
        .context("в конфиге нет agent_credential - сначала `xr-share install --token <reg-токен>`")?;
    Ok((hub.trim_end_matches('/').to_string(), cred))
}

fn add(config_path: &Path, name: &str, upstream: Option<&str>) -> Result<()> {
    let mut cfg = crate::cli::read_config(config_path)?;
    let (hub, cred) = hub_and_credential(&cfg)?;
    if !valid_publication_name(name) {
        bail!(
            "имя публикации это DNS-метка (строчные буквы, цифры, дефис, 1..63 символа, \
             дефис не с краю): оно же поддомен"
        );
    }
    let upstream = match (upstream, cfg.exposes.iter().find(|e| e.name == name)) {
        (Some(u), _) => u.to_string(),
        (None, Some(existing)) => existing.upstream.clone(),
        (None, None) => bail!("для новой публикации нужен --upstream (например 127.0.0.1:8765)"),
    };

    let body = serde_json::json!({ "credential": cred, "name": name });
    let resp = crate::cli::hub_post(&format!("{hub}/api/v1/expose/add"), &body)
        .context("регистрация публикации в хабе")?;
    let agent = crate::cli::str_field(&resp, "agent_pubkey")?;

    match cfg.exposes.iter_mut().find(|e| e.name == name) {
        Some(e) => e.upstream = upstream.clone(),
        None => cfg.exposes.push(ExposeEntry { name: name.to_string(), upstream: upstream.clone() }),
    }
    cfg.validate_expose()?;
    crate::cli::write_config(config_path, &cfg)?;

    println!("Публикация {name} заведена: {upstream} на агенте {}", short(&agent));
    println!("Проверить без VPS: xr-share expose open --name {name}");
    Ok(())
}

fn rm(config_path: &Path, name: &str) -> Result<()> {
    let mut cfg = crate::cli::read_config(config_path)?;
    let (hub, cred) = hub_and_credential(&cfg)?;
    hub_delete(&format!("{hub}/api/v1/expose/{name}"), &cred)?;
    let before = cfg.exposes.len();
    cfg.exposes.retain(|e| e.name != name);
    if cfg.exposes.len() != before {
        crate::cli::write_config(config_path, &cfg)?;
    }
    println!("Публикация {name} снята");
    Ok(())
}

fn ls(config_path: &Path) -> Result<()> {
    let cfg = crate::cli::read_config(config_path)?;
    let (hub, cred) = hub_and_credential(&cfg)?;
    let list = hub_get(&format!("{hub}/api/v1/expose"), &cred)?;
    let rows = list.as_array().cloned().unwrap_or_default();
    if rows.is_empty() && cfg.exposes.is_empty() {
        println!("Публикаций нет. Завести: xr-share expose add --name dash --upstream 127.0.0.1:8765");
        return Ok(());
    }
    println!("{:<20} {:<24} {}", "ИМЯ", "АПСТРИМ", "СОСТОЯНИЕ");
    for row in &rows {
        let name = row.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        match cfg.exposes.iter().find(|e| e.name == name) {
            Some(e) => println!("{name:<20} {:<24} обслуживается", e.upstream),
            // Запись в хабе без записи в конфиге не обслуживается вовсе, и это
            // видно тут, а не по 404 из браузера.
            None => println!("{name:<20} {:<24} нет записи в конфиге агента", "-"),
        }
    }
    for e in &cfg.exposes {
        if !rows.iter().any(|r| r.get("name").and_then(|v| v.as_str()) == Some(e.name.as_str())) {
            println!("{:<20} {:<24} нет записи в хабе (expose add)", e.name, e.upstream);
        }
    }
    Ok(())
}

fn open(config_path: &Path, name: &str, port: u16, with_mandate: bool) -> Result<()> {
    let cfg = crate::cli::read_config(config_path)?;
    cfg.validate_expose()?;
    let entry = cfg
        .exposes
        .iter()
        .find(|e| e.name == name)
        .with_context(|| format!("в конфиге нет публикации {name}: заведи её `expose add`"))?
        .clone();
    let gate = gate_from_config(&cfg, config_path)?;
    if gate.agent_pubkey.is_none() {
        bail!("у агента нет identity-ключа, мандат сверять нечем - переустанови `xr-share install`");
    }

    let mandate = if with_mandate {
        let (hub, cred) = hub_and_credential(&cfg)?;
        let body = serde_json::json!({ "credential": cred });
        let resp = crate::cli::hub_post(&format!("{hub}/api/v1/expose/{name}/mandate"), &body)
            .context("мандат публикации у хаба")?;
        Some(crate::cli::str_field(&resp, "mandate")?)
    } else {
        None
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("рантайм для форвардера")?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .with_context(|| format!("не занять порт {port} на 127.0.0.1"))?;
        let local = listener.local_addr()?;
        println!("http://{local}");
        println!("Публикация {name} -> {}", entry.upstream);
        if mandate.is_none() {
            println!("Мандат не подставляется: гейт обязан ответить 403");
        }
        println!("Остановить: Ctrl-C");
        serve_harness(listener, gate, name.to_string(), mandate).await
    })
}

fn hub_get(url: &str, credential: &str) -> Result<serde_json::Value> {
    hub_call(ureq::get(url), credential)
}

fn hub_delete(url: &str, credential: &str) -> Result<serde_json::Value> {
    hub_call(ureq::delete(url), credential)
}

fn hub_call(req: ureq::Request, credential: &str) -> Result<serde_json::Value> {
    match req
        .timeout(Duration::from_secs(15))
        .set("authorization", &format!("Bearer {credential}"))
        .call()
    {
        Ok(r) => {
            let s = r.into_string().unwrap_or_default();
            if s.trim().is_empty() {
                Ok(serde_json::Value::Null)
            } else {
                serde_json::from_str(&s).context("разбор ответа хаба")
            }
        }
        Err(ureq::Error::Status(code, r)) => bail!(
            "хаб отклонил запрос (HTTP {code}): {}",
            r.into_string().unwrap_or_default()
        ),
        Err(e) => bail!("сеть при запросе к хабу: {e}"),
    }
}

fn short(s: &str) -> String {
    s.chars().take(12).collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use xr_proto::share::{sign_expose_token, sign_relay_token, sign_share_token};

    fn hub() -> SigningKey {
        SigningKey::from_bytes(&[42u8; 32])
    }

    fn identity() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn agent_pk() -> String {
        base64::engine::general_purpose::STANDARD
            .encode(identity().verifying_key().as_bytes())
    }

    fn blob<T: serde::Serialize>(v: &T) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
    }

    fn gate(upstream: &str) -> ExposeGate {
        ExposeGate {
            hub_key: hub().verifying_key(),
            agent_pubkey: Some(agent_pk()),
            publications: Arc::new(vec![ExposeEntry {
                name: "dash".into(),
                upstream: upstream.into(),
            }]),
        }
    }

    fn mandate(name: &str) -> String {
        blob(&sign_expose_token(&hub(), name, &agent_pk(), now_unix() + 3600))
    }

    fn request(auth: Option<&str>) -> Request {
        let mut b = axum::http::Request::builder().uri("/").header(EXPOSE_HEADER, "dash");
        if let Some(a) = auth {
            b = b.header(header::AUTHORIZATION, format!("Bearer {a}"));
        }
        b.body(Body::empty()).unwrap()
    }

    /// Локальный сервис: отвечает 200 и эхом того, что до него доехало.
    async fn upstream() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().fallback(axum::routing::any(|req: Request| async move {
            let mut seen: Vec<String> = req
                .headers()
                .iter()
                .map(|(k, v)| format!("{k}={}", v.to_str().unwrap_or("?")))
                .collect();
            seen.sort();
            format!("upstream ok\n{}", seen.join("\n"))
        }));
        let h = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr.to_string(), h)
    }

    async fn body_text(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn test_expose_requires_mandate() {
        // LLD-38 п. 2.3: без мандата и с мандатом на другую публикацию запрос
        // получает 403 и до апстрима не доходит. Апстрим тут закрытый порт:
        // дойди запрос до него, ответ был бы 502, а не 403.
        let g = gate("127.0.0.1:1");

        let resp = serve(&g, "dash", request(None)).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let other = serve(&g, "dash", request(Some(&mandate("notes")))).await;
        assert_eq!(other.status(), StatusCode::FORBIDDEN);

        // Мандат на нужную публикацию, но выписанный не хабом.
        let foreign = SigningKey::from_bytes(&[3u8; 32]);
        let forged = blob(&sign_expose_token(&foreign, "dash", &agent_pk(), now_unix() + 3600));
        let resp = serve(&g, "dash", request(Some(&forged))).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Протухший мандат.
        let stale = blob(&sign_expose_token(&hub(), "dash", &agent_pk(), now_unix() - 1));
        let resp = serve(&g, "dash", request(Some(&stale))).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Мандат на другого агента: перехваченный у соседа он не работает.
        let alien = blob(&sign_expose_token(&hub(), "dash", "WFla", now_unix() + 3600));
        let resp = serve(&g, "dash", request(Some(&alien))).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_relay_token_alone_cannot_reach_expose() {
        // Дыра, ради которой мандат и заведён (LLD-38 п. 3.4): держатель
        // валидного relay-токена на шару того же агента подставляет заголовок
        // публикации и надеется попасть в локальный сервис. Не попадает: у
        // мандата свой домен подписи, и токен транзита в нём не считается.
        let g = gate("127.0.0.1:1");
        let relay = blob(&sign_relay_token(&hub(), "dash", &agent_pk(), now_unix() + 3600));
        let resp = serve(&g, "dash", request(Some(&relay))).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Токен шары в той же роли тоже не проходит.
        let share = blob(&sign_share_token(&hub(), "dash", "share:read", now_unix() + 3600));
        let resp = serve(&g, "dash", request(Some(&share))).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_expose_proxies_upstream() {
        // Запрос доезжает до локального сервиса, служебные заголовки сняты, а
        // Authorization приложения возвращён на место из служебного.
        let (addr, _h) = upstream().await;
        let g = gate(&addr);
        let req = axum::http::Request::builder()
            .uri("/page?q=1")
            .header(EXPOSE_HEADER, "dash")
            .header(header::AUTHORIZATION, format!("Bearer {}", mandate("dash")))
            .header(FORWARDED_AUTH_HEADER, "Bearer app-token")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-host", "dash.web.example.com")
            .body(Body::empty())
            .unwrap();
        let resp = serve(&g, "dash", req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let text = body_text(resp).await;
        assert!(text.contains("upstream ok"), "{text}");
        assert!(!text.contains(EXPOSE_HEADER), "служебный заголовок обязан быть снят: {text}");
        assert!(
            !text.contains(FORWARDED_AUTH_HEADER),
            "перенесённый заголовок не едет дважды: {text}"
        );
        assert!(
            text.contains("authorization=Bearer app-token"),
            "приложению возвращают его собственный Authorization: {text}"
        );
        assert!(text.contains("x-forwarded-proto=https"), "{text}");
        assert!(text.contains("x-forwarded-host=dash.web.example.com"), "{text}");
    }

    #[tokio::test]
    async fn test_expose_unknown_name() {
        // Имя есть в хабе (мандат на него валиден), но записи в конфиге нет:
        // 404 с внятной причиной, а не пятисотка и не тихий 403.
        let g = gate("127.0.0.1:1");
        let req = axum::http::Request::builder()
            .uri("/")
            .header(EXPOSE_HEADER, "ghost")
            .header(header::AUTHORIZATION, format!("Bearer {}", mandate("ghost")))
            .body(Body::empty())
            .unwrap();
        let resp = serve(&g, "ghost", req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(body_text(resp).await.contains("[[expose]]"));
    }

    #[tokio::test]
    async fn upstream_down_names_the_reason() {
        // Сервис на машине не поднят: 502 с адресом и причиной. Иначе отказ
        // канала неотличим от отказа приложения.
        let g = gate("127.0.0.1:1");
        let resp = serve(&g, "dash", request(Some(&mandate("dash")))).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert!(body_text(resp).await.contains("127.0.0.1:1"));
    }

    #[test]
    fn requested_publication_takes_only_labels() {
        let mut h = HeaderMap::new();
        assert_eq!(requested_publication(&h), None);
        h.insert(EXPOSE_HEADER, HeaderValue::from_static("dash"));
        assert_eq!(requested_publication(&h).as_deref(), Some("dash"));
        // Имя из чужих рук: путь в заголовке не должен стать именем.
        h.insert(EXPOSE_HEADER, HeaderValue::from_static("../../etc/passwd"));
        assert_eq!(requested_publication(&h), None);
    }

    /// Харнесс целиком, как его гоняет проверка задачи: сервис, форвардер,
    /// запрос по напечатанному адресу. С мандатом отдаётся тело сервиса, без
    /// мандата тот же путь отвечает 403.
    #[tokio::test]
    async fn harness_serves_with_mandate_and_refuses_without() {
        let (addr, _h) = upstream().await;
        let g = gate(&addr);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = listener.local_addr().unwrap();
        let with = tokio::spawn(serve_harness(
            listener,
            g.clone(),
            "dash".into(),
            Some(mandate("dash")),
        ));
        let body = reqwest::get(format!("http://{local}/page")).await.unwrap();
        assert_eq!(body.status(), 200);
        let text = body.text().await.unwrap();
        assert!(text.contains("upstream ok"), "{text}");
        assert!(text.contains("x-forwarded-proto=http"), "{text}");
        with.abort();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = listener.local_addr().unwrap();
        let without = tokio::spawn(serve_harness(listener, g, "dash".into(), None));
        let resp = reqwest::get(format!("http://{local}/page")).await.unwrap();
        assert_eq!(resp.status(), 403);
        without.abort();
    }
}
