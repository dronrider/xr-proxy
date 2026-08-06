//! Соединения до агента и пул на публикацию (LLD-38 п. 2.3, п. 3.6).
//!
//! Каждое новое соединение это mux-стрим до relay плюс TLS-хендшейк через VPS,
//! то есть лишние RTT на каждый запрос страницы. Поэтому соединение живёт в
//! пуле публикации и переиспользуется: страница с десятком запросов едет по уже
//! готовым.
//!
//! Аренда (`Lease`) кончается не с ответом, а с концом его тела: пока тело
//! течёт, соединение занято, и вернуть его в пул раньше значило бы отдать
//! другому запросу середину чужого ответа. Апгрейд забирает аренду насовсем
//! через [`Lease::detach`]: после `101` соединение перестаёт быть HTTP, и в пул
//! ему возвращаться некуда. Возраст соединения ([`Lease::age`]) переживает
//! лежание в пуле, потому что потолок жизни сплайса relay отсчитывает от
//! открытия стрима, а не от начала апгрейда.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use hyper::body::{Body as HttpBody, Bytes, Frame, Incoming, SizeHint};
use hyper::client::conn::http1::SendRequest;
use tokio::io::{AsyncRead, AsyncWrite};
use xr_proto::relay_client::{relay_tls_connect, RelayEndpoint};
use xr_proto::share::{RelayGrant, WebRoute};

/// Сколько простаивающее соединение считается годным. Relay рубит сплайс по
/// своему потолку, а агент может уйти в переподключение: соединение, лежавшее
/// дольше, дешевле открыть заново, чем отдать браузеру обрыв.
const IDLE_TTL: Duration = Duration::from_secs(90);
/// Сколько простаивающих соединений держим на публикацию.
const MAX_IDLE_PER_PUBLICATION: usize = 8;
/// Сколько ждём соединения до агента: relay отвечает «агента нет» за один RTT,
/// а вот повисший транзит браузеру ждать незачем.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

type Boxed<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Транспорт до агента: всё, что умеет читать и писать байты.
pub trait AgentIo: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> AgentIo for T {}

/// Как открывается соединение до агента. Настоящий набирает relay, тестовый
/// отдаёт половинку duplex, поэтому весь путь запроса проверяется без сети.
pub trait Dialer: Send + Sync + 'static {
    fn connect(&self, route: Arc<WebRoute>) -> Boxed<io::Result<Box<dyn AgentIo>>>;
}

/// Настоящий набор: обфусцированный mux к relay по токену маршрута, поверх
/// сплайса pinned-TLS с проверкой `SPKI == agent_pubkey`.
#[derive(Default)]
pub struct RelayDialer {
    /// Эндпоинт на публикацию: он держит живой mux к relay и переоткрывает его
    /// сам, поэтому на каждое соединение уходит один стрим, а не новый TCP.
    /// Ключ маршрута (`exp`) в значении: обновился маршрут, обновился и токен,
    /// и старый эндпоинт больше не годится.
    endpoints: Mutex<HashMap<String, (u64, Arc<RelayEndpoint>)>>,
}

impl RelayDialer {
    pub fn new() -> Self {
        Self::default()
    }

    fn endpoint(&self, route: &WebRoute) -> Result<Arc<RelayEndpoint>, String> {
        let mut guard = self.endpoints.lock().expect("relay endpoints lock");
        if let Some((exp, endpoint)) = guard.get(&route.publication) {
            if *exp == route.exp {
                return Ok(endpoint.clone());
            }
        }
        let grant = RelayGrant {
            addr: route.relay.addr.clone(),
            port: route.relay.port,
            obf: route.relay.obf.clone(),
            relay_token: route.relay_token.clone(),
        };
        let endpoint = Arc::new(RelayEndpoint::from_grant(&grant)?);
        guard.insert(route.publication.clone(), (route.exp, endpoint.clone()));
        Ok(endpoint)
    }
}

impl Dialer for RelayDialer {
    fn connect(&self, route: Arc<WebRoute>) -> Boxed<io::Result<Box<dyn AgentIo>>> {
        let endpoint = self
            .endpoint(&route)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e));
        Box::pin(async move {
            let endpoint = endpoint?;
            match tokio::time::timeout(CONNECT_TIMEOUT, relay_tls_connect(&endpoint)).await {
                Ok(Ok(tls)) => Ok(Box::new(tls) as Box<dyn AgentIo>),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "транзит до агента не поднялся вовремя",
                )),
            }
        })
    }
}

struct Idle {
    sender: SendRequest<axum::body::Body>,
    since: Instant,
    /// Когда это соединение открылось. Потолок жизни сплайса relay считает от
    /// открытия стрима, а не от начала апгрейда, поэтому взятое из пула
    /// соединение приносит апгрейду свой прожитый срок.
    opened_at: Instant,
}

/// Пул соединений: своя очередь на публикацию.
pub struct AgentPool {
    dialer: Arc<dyn Dialer>,
    idle: Mutex<HashMap<String, Vec<Idle>>>,
}

impl AgentPool {
    pub fn new(dialer: Arc<dyn Dialer>) -> Arc<Self> {
        Arc::new(Self {
            dialer,
            idle: Mutex::new(HashMap::new()),
        })
    }

    /// Готовое к запросу соединение: из пула, если там есть живое, иначе новое.
    pub async fn checkout(self: &Arc<Self>, route: Arc<WebRoute>) -> io::Result<Lease> {
        let publication = route.publication.clone();
        if let Some((sender, opened_at)) = self.take_idle(&publication) {
            return Ok(self.lease(publication, sender, opened_at));
        }
        let opened_at = Instant::now();
        let io = self.dialer.connect(route).await?;
        let (sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(io))
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("HTTP до агента: {e}")))?;
        // Соединение живёт своим таском, пока по нему течёт тело: без него
        // ответ не поедет дальше первой порции. `with_upgrades` отдаёт таск
        // апгрейду, когда агент отвечает `101`.
        tokio::spawn(async move {
            if let Err(e) = conn.with_upgrades().await {
                tracing::debug!("соединение до агента закрылось: {e}");
            }
        });
        Ok(self.lease(publication, sender, opened_at))
    }

    /// Сколько простаивающих соединений держит публикация: диагностика и тесты.
    pub fn idle_count(&self, publication: &str) -> usize {
        self.idle
            .lock()
            .expect("pool lock")
            .get(publication)
            .map(Vec::len)
            .unwrap_or(0)
    }

    fn lease(
        self: &Arc<Self>,
        publication: String,
        sender: SendRequest<axum::body::Body>,
        opened_at: Instant,
    ) -> Lease {
        Lease {
            pool: self.clone(),
            publication,
            sender: Some(sender),
            opened_at,
        }
    }

    fn take_idle(&self, publication: &str) -> Option<(SendRequest<axum::body::Body>, Instant)> {
        let mut guard = self.idle.lock().expect("pool lock");
        let queue = guard.get_mut(publication)?;
        // С конца: свежее лежит последним, а протухшее по дороге отбрасывается.
        while let Some(idle) = queue.pop() {
            if idle.since.elapsed() < IDLE_TTL && idle.sender.is_ready() {
                return Some((idle.sender, idle.opened_at));
            }
        }
        None
    }

    fn put_idle(&self, publication: &str, sender: SendRequest<axum::body::Body>, opened_at: Instant) {
        if !sender.is_ready() {
            return;
        }
        let mut guard = self.idle.lock().expect("pool lock");
        let queue = guard.entry(publication.to_string()).or_default();
        queue.retain(|idle| idle.since.elapsed() < IDLE_TTL);
        if queue.len() < MAX_IDLE_PER_PUBLICATION {
            queue.push(Idle {
                sender,
                since: Instant::now(),
                opened_at,
            });
        }
    }
}

/// Занятое соединение. Пока аренда жива, соединение никому не отдадут; на
/// сбросе оно возвращается в пул, если после ответа осталось пригодным.
pub struct Lease {
    pool: Arc<AgentPool>,
    publication: String,
    sender: Option<SendRequest<axum::body::Body>>,
    opened_at: Instant,
}

impl Lease {
    pub fn sender(&mut self) -> &mut SendRequest<axum::body::Body> {
        self.sender.as_mut().expect("аренда без соединения")
    }

    /// Сколько это соединение живёт. Апгрейд считает от него свой срок штатного
    /// закрытия: потолок сплайса relay отмеряет от открытия стрима, а
    /// соединение могло полежать в пуле.
    pub fn age(&self) -> Duration {
        self.opened_at.elapsed()
    }

    /// Забрать соединение насовсем: после апгрейда оно перестаёт быть HTTP, и
    /// место в пуле ему уже не полагается (п. 3.6).
    pub fn detach(mut self) -> SendRequest<axum::body::Body> {
        self.sender.take().expect("аренда без соединения")
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            self.pool.put_idle(&self.publication, sender, self.opened_at);
        }
    }
}

/// Тело ответа, которое держит аренду до последнего кадра. Ответ едет
/// потоком, целиком в память VPS он не собирается ни в какой момент.
pub struct LeasedBody {
    inner: Incoming,
    lease: Option<Lease>,
}

impl LeasedBody {
    pub fn new(inner: Incoming, lease: Lease) -> Self {
        Self {
            inner,
            lease: Some(lease),
        }
    }
}

impl HttpBody for LeasedBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        match &polled {
            // Тело кончилось (или оборвалось): аренда отпускается тут же, а не
            // ждёт, пока браузер закроет вкладку.
            Poll::Ready(None) | Poll::Ready(Some(Err(_))) => {
                self.lease = None;
            }
            _ => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{route_for, DuplexDialer};
    use std::sync::atomic::Ordering;

    async fn get(pool: &Arc<AgentPool>, route: Arc<WebRoute>) -> String {
        let mut lease = pool.checkout(route).await.expect("соединение до агента");
        let req = hyper::Request::builder()
            .uri("/")
            .header("host", "dash.web.example.com")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = lease.sender().send_request(req).await.expect("ответ агента");
        let body = LeasedBody::new(resp.into_body(), lease);
        let bytes = axum::body::to_bytes(axum::body::Body::new(body), 1 << 20)
            .await
            .expect("тело ответа");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn connection_is_reused_between_requests() {
        // Ради этого пул и заведён (п. 3.6): страница с десятком запросов не
        // должна стоить десяти хендшейков через VPS.
        let dialer = Arc::new(DuplexDialer::serving());
        let pool = AgentPool::new(dialer.clone());
        let route = Arc::new(route_for("dash", 10_000));

        assert!(get(&pool, route.clone()).await.contains("agent ok"));
        assert_eq!(pool.idle_count("dash"), 1, "после ответа соединение вернулось в пул");
        assert!(get(&pool, route.clone()).await.contains("agent ok"));
        assert_eq!(dialer.dials.load(Ordering::SeqCst), 1, "второй запрос взял готовое");
    }

    #[tokio::test]
    async fn body_holds_the_lease_until_the_last_frame() {
        // Соединение, возвращённое в пул до конца тела, отдало бы следующему
        // запросу середину чужого ответа.
        let dialer = Arc::new(DuplexDialer::serving());
        let pool = AgentPool::new(dialer.clone());
        let route = Arc::new(route_for("dash", 10_000));

        let mut lease = pool.checkout(route).await.unwrap();
        let req = hyper::Request::builder()
            .uri("/")
            .header("host", "dash.web.example.com")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = lease.sender().send_request(req).await.unwrap();
        let body = LeasedBody::new(resp.into_body(), lease);
        assert_eq!(pool.idle_count("dash"), 0, "тело ещё течёт, соединение занято");
        let _ = axum::body::to_bytes(axum::body::Body::new(body), 1 << 20).await.unwrap();
        assert_eq!(pool.idle_count("dash"), 1);
    }

    #[tokio::test]
    async fn broken_connection_is_not_returned_to_the_pool() {
        // Транзит оборвался под уже открытым соединением (relay срубил сплайс,
        // агент ушёл в переподключение): дозвон при этом состоялся, а запрос
        // упал. Мёртвому соединению в пуле не место, иначе следующий запрос
        // получил бы отказ на ровном месте. Отказ на дозвоне и ответ агента
        // `403` это другие пути, их держат свои тесты.
        let dialer = Arc::new(DuplexDialer::serving());
        let pool = AgentPool::new(dialer.clone());
        let route = Arc::new(route_for("dash", 10_000));

        assert!(get(&pool, route.clone()).await.contains("agent ok"));
        assert_eq!(pool.idle_count("dash"), 1);

        let mut lease = pool.checkout(route.clone()).await.expect("соединение из пула");
        assert_eq!(dialer.dials.load(Ordering::SeqCst), 1, "взяли готовое, не дозванивались");
        assert_eq!(pool.idle_count("dash"), 0, "аренда забрала соединение из пула");

        dialer.kill_agents();
        let req = hyper::Request::builder()
            .uri("/")
            .header("host", "dash.web.example.com")
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(
            lease.sender().send_request(req).await.is_err(),
            "оборванный транзит обязан провалить запрос, а не подвесить его"
        );
        drop(lease);
        assert_eq!(pool.idle_count("dash"), 0, "мёртвое соединение в пул не возвращается");

        // Следующий запрос дозванивается заново и работает.
        assert!(get(&pool, route).await.contains("agent ok"));
        assert_eq!(dialer.dials.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn detached_lease_never_returns_to_the_pool() {
        // Апгрейд забирает соединение насовсем: HTTP на нём больше не живёт.
        let dialer = Arc::new(DuplexDialer::serving());
        let pool = AgentPool::new(dialer.clone());
        let route = Arc::new(route_for("dash", 10_000));
        let lease = pool.checkout(route).await.unwrap();
        let sender = lease.detach();
        assert_eq!(pool.idle_count("dash"), 0);
        drop(sender);
    }

    #[tokio::test]
    async fn pooled_connection_remembers_when_it_was_opened() {
        // Потолок жизни сплайса relay отмеряет от открытия стрима. Считай
        // апгрейд свой срок от момента, когда его взяли из пула, соединение,
        // полежавшее там минуту, закрывалось бы штатно уже после обрыва.
        let dialer = Arc::new(DuplexDialer::serving());
        let pool = AgentPool::new(dialer.clone());
        let route = Arc::new(route_for("dash", 10_000));

        assert!(get(&pool, route.clone()).await.contains("agent ok"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let lease = pool.checkout(route).await.expect("соединение из пула");
        assert_eq!(dialer.dials.load(Ordering::SeqCst), 1, "взяли готовое");
        assert!(
            lease.age() >= Duration::from_millis(50),
            "возраст соединения обязан пережить лежание в пуле: {:?}",
            lease.age()
        );
    }

    #[tokio::test]
    async fn refused_dial_is_reported_as_is() {
        let dialer = Arc::new(DuplexDialer::offline());
        let pool = AgentPool::new(dialer.clone());
        let err = match pool.checkout(Arc::new(route_for("dash", 10_000))).await {
            Ok(_) => panic!("агента нет, соединения быть не должно"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(dialer.dials.load(Ordering::SeqCst), 1, "второй попытки не делаем");
    }
}
