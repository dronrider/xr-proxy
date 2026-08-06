//! Стенд для тестов фронта: хаб и агент без единого сетевого вызова.
//!
//! Агент живёт на половинке `duplex`, поэтому весь путь запроса (пул,
//! HTTP-клиент, заголовки, тело потоком) проверяется в одном процессе. Хаб
//! отвечает заранее назначенным вердиктом и считает вызовы: кеш маршрутов и
//! отсутствие ретрай-шторма судятся счётчиком, а не по времени.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use hyper::body::{Bytes, Incoming};
use xr_proto::relay_client::RELAY_ERR_AGENT_OFFLINE;
use xr_proto::share::{
    sign_expose_token, sign_relay_token, web_share_id, RelayDescriptor, RelayObf, WebRoute,
};

use crate::hub::{HubApi, HubError};
use crate::pool::{AgentIo, Dialer};

type Boxed<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Ключ «хаба» стенда: им подписан мандат, и им же тест проверяет, что до
/// агента доехал именно мандат, а не что-то похожее.
pub fn hub_key() -> SigningKey {
    SigningKey::from_bytes(&[42u8; 32])
}

/// Ключ «агента» стенда.
pub fn agent_pubkey() -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .encode(SigningKey::from_bytes(&[7u8; 32]).verifying_key().as_bytes())
}

/// Маршрут публикации, как его отдал бы хаб.
pub fn route_for(publication: &str, exp: u64) -> WebRoute {
    route_with_cap(publication, exp, 3600)
}

/// Тот же маршрут с назначенным потолком жизни сплайса: на боевом relay это
/// час, а проверке штатного закрытия нужен лимит покороче.
pub fn route_with_cap(publication: &str, exp: u64, splice_lifetime_secs: u64) -> WebRoute {
    use base64::Engine;
    let agent = agent_pubkey();
    WebRoute {
        publication: publication.to_string(),
        agent_pubkey: agent.clone(),
        relay: RelayDescriptor {
            addr: "203.0.113.7".into(),
            port: 8444,
            obf: RelayObf {
                key: base64::engine::general_purpose::STANDARD.encode(b"relay-obf-key-32-bytes-long!!!!!"),
                salt: 0xDEAD_BEEF,
                modifier: "positional_xor_rotate".into(),
                padding_min: 16,
                padding_max: 128,
            },
        },
        relay_token: sign_relay_token(&hub_key(), &web_share_id(publication), &agent, exp),
        expose_token: sign_expose_token(&hub_key(), publication, &agent, exp),
        exp,
        splice_lifetime_secs,
    }
}

/// Что стенд-хаб отвечает на запрос маршрута.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HubMode {
    /// Маршрут с этим `exp`.
    Route(u64),
    /// Публикации нет в реестре.
    Missing,
    /// Хаб не отвечает.
    Down,
}

/// Хаб стенда: считает вызовы обеих ручек и отвечает назначенным вердиктом.
pub struct FakeHub {
    pub mode: HubMode,
    pub password: String,
    pub route_calls: AtomicUsize,
    pub verify_calls: AtomicUsize,
    pub hub_down_for_password: bool,
    /// Потолок жизни сплайса в маршруте: тест штатного закрытия ставит свой,
    /// как проверка ставит его конфигом relay.
    pub splice_lifetime_secs: u64,
}

impl FakeHub {
    pub fn new(mode: HubMode) -> Self {
        Self {
            mode,
            password: "открой".into(),
            route_calls: AtomicUsize::new(0),
            verify_calls: AtomicUsize::new(0),
            hub_down_for_password: false,
            splice_lifetime_secs: 3600,
        }
    }
}

impl HubApi for FakeHub {
    fn route(&self, publication: String) -> Boxed<Result<WebRoute, HubError>> {
        self.route_calls.fetch_add(1, Ordering::SeqCst);
        let mode = self.mode;
        let cap = self.splice_lifetime_secs;
        Box::pin(async move {
            match mode {
                HubMode::Route(exp) => Ok(route_with_cap(&publication, exp, cap)),
                HubMode::Missing => Err(HubError::NotFound),
                HubMode::Down => Err(HubError::Unavailable("хаб не ответил: connect refused".into())),
            }
        })
    }

    fn verify_password(&self, _username: String, password: String) -> Boxed<Result<bool, HubError>> {
        self.verify_calls.fetch_add(1, Ordering::SeqCst);
        let expected = self.password.clone();
        let down = self.hub_down_for_password;
        Box::pin(async move {
            if down {
                return Err(HubError::Unavailable("хаб не ответил: connect refused".into()));
            }
            Ok(password == expected)
        })
    }
}

/// Агент стенда: половинка `duplex`, на которой крутится HTTP/1.1-сервер.
/// Отвечает эхом того, что до него доехало, поэтому заголовки публикации
/// судятся по телу ответа.
pub struct DuplexDialer {
    pub dials: AtomicUsize,
    /// Сколько апгрейдов агент увидел закрытыми со стороны фронта: так тест
    /// судит, доехало ли до агента закрытие браузера.
    pub ws_closed: Arc<AtomicUsize>,
    offline: bool,
    /// Отвечать `403`, как агент с отвергнутым мандатом.
    refuse_mandate: bool,
    /// Таски агентских половинок: тест гасит их, когда ему нужно оборвать
    /// транзит под уже открытым соединением.
    agents: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl DuplexDialer {
    pub fn serving() -> Self {
        Self {
            dials: AtomicUsize::new(0),
            ws_closed: Arc::default(),
            offline: false,
            refuse_mandate: false,
            agents: Arc::default(),
        }
    }

    /// Оборвать транзит всех выданных соединений: агентская половинка `duplex`
    /// исчезает, как при обрыве сплайса на relay или уходе агента в
    /// переподключение. Соединение при этом уже открыто и лежит в пуле, чем
    /// этот случай и отличается от отказа на дозвоне.
    pub fn kill_agents(&self) {
        for task in self.agents.lock().expect("agents lock").drain(..) {
            task.abort();
        }
    }

    /// Агента нет в реестре relay: тот же отказ, что даёт настоящий транзит.
    pub fn offline() -> Self {
        Self {
            dials: AtomicUsize::new(0),
            ws_closed: Arc::default(),
            offline: true,
            refuse_mandate: false,
            agents: Arc::default(),
        }
    }

    pub fn refusing_mandate() -> Self {
        Self {
            dials: AtomicUsize::new(0),
            ws_closed: Arc::default(),
            offline: false,
            refuse_mandate: true,
            agents: Arc::default(),
        }
    }
}

impl Dialer for DuplexDialer {
    fn connect(&self, _route: Arc<WebRoute>) -> Boxed<io::Result<Box<dyn AgentIo>>> {
        self.dials.fetch_add(1, Ordering::SeqCst);
        if self.offline {
            return Box::pin(async {
                Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    RELAY_ERR_AGENT_OFFLINE,
                ))
            });
        }
        let refuse = self.refuse_mandate;
        let agents = self.agents.clone();
        let ws_closed = self.ws_closed.clone();
        Box::pin(async move {
            let (ours, theirs) = tokio::io::duplex(64 * 1024);
            let task = tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                    let ws_closed = ws_closed.clone();
                    async move {
                        Ok::<_, std::convert::Infallible>(handle(req, refuse, ws_closed).await)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(theirs), service)
                    .with_upgrades()
                    .await;
            });
            agents.lock().expect("agents lock").push(task);
            Ok(Box::new(ours) as Box<dyn AgentIo>)
        })
    }
}

/// Что агент стенда делает с запросом: апгрейд он принимает и эхом гоняет
/// байты, обычный запрос отдаёт эхом заголовков.
async fn handle(
    req: hyper::Request<Incoming>,
    refuse_mandate: bool,
    ws_closed: Arc<AtomicUsize>,
) -> hyper::Response<http_body_util::Full<Bytes>> {
    if !refuse_mandate && crate::upgrade::requested(req.headers()).is_some() {
        return ws_echo(req, ws_closed).await;
    }
    echo(req, refuse_mandate).await
}

/// Агент, принявший апгрейд: отвечает `101` и дальше гоняет байты обратно как
/// есть. Кадры он не разбирает, потому что эхо-сервису это и не нужно: тест
/// сверяет байты, а закрытие со стороны фронта считает `ws_closed`.
async fn ws_echo(
    mut req: hyper::Request<Incoming>,
    ws_closed: Arc<AtomicUsize>,
) -> hyper::Response<http_body_util::Full<Bytes>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let upgraded = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        let Ok(io) = upgraded.await else { return };
        let mut io = hyper_util::rt::TokioIo::new(io);
        let mut buf = vec![0u8; 4096];
        loop {
            match io.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if io.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        ws_closed.fetch_add(1, Ordering::SeqCst);
    });
    hyper::Response::builder()
        .status(101)
        .header("upgrade", "websocket")
        .header("connection", "Upgrade")
        .header("sec-websocket-accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        .body(http_body_util::Full::default())
        .expect("ответ апгрейда")
}

/// Ответ агента стенда: строка-маркер, метод с полным URI, отсортированные
/// заголовки и тело запроса. Всё, что фронт обязан или не обязан передавать,
/// видно прямо в теле ответа.
async fn echo(
    req: hyper::Request<Incoming>,
    refuse_mandate: bool,
) -> hyper::Response<http_body_util::Full<Bytes>> {
    use http_body_util::BodyExt;
    let (parts, body) = req.into_parts();
    let bytes = body.collect().await.map(|b| b.to_bytes()).unwrap_or_default();
    if refuse_mandate {
        return hyper::Response::builder()
            .status(403)
            .body(http_body_util::Full::new(Bytes::from_static(
                b"mandate refused",
            )))
            .expect("ответ отказа");
    }
    let mut lines = vec![
        "agent ok".to_string(),
        format!("{} {}", parts.method, parts.uri),
    ];
    let mut headers: Vec<String> = parts
        .headers
        .iter()
        .map(|(k, v)| format!("{k}={}", v.to_str().unwrap_or("?")))
        .collect();
    headers.sort();
    lines.extend(headers);
    if !bytes.is_empty() {
        lines.push(format!("body={}", String::from_utf8_lossy(&bytes)));
    }
    hyper::Response::new(http_body_util::Full::new(Bytes::from(lines.join("\n"))))
}
