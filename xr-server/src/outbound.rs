//! Исходящие соединения сервера под правилами XR-299: цель с полем
//! `outbound` уходит через резидентский SOCKS5-прокси и никогда напрямую.
//!
//! Оба кодовых пути сервера (handler и mux_handler) открывают соединение к
//! цели функцией [`connect_target`] этой же секции, поэтому гарантия «мимо
//! прямого выхода» держится в одной точке, а не в двух похожих копиях.
//! Запасного напрямого пути у чувствительного правила нет намеренно:
//! резидент лежит, соединение рвётся, и с адреса хостинга эти домены не
//! открываются.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Duration;

use xr_proto::config::{OutboundConfig, ServerConfig};
use xr_proto::protocol::TargetAddr;
use xr_proto::routing::Router;

/// Ошибка открытия соединения к цели с классом фазы. Вызывающий не смешивает
/// отказ установки с проблемами уже идущего обмена: свой класс своя причина
/// в Close (в mux) и свой текст в журнале.
#[derive(Debug)]
pub enum TargetError {
    Resolve(io::Error),
    Connect(io::Error),
    /// Исходящий выход правила не открылся: резидент лежит, протокол
    /// отвечает отказом или конфиг ссылается на неизвестный выход. На
    /// старте такие конфиги отбраковывает `validate_server_config`, но путь
    /// остаётся закрытым и позже: лучше отказ, чем молчаливая утечка.
    Outbound(io::Error),
}

impl TargetError {
    /// Внутренняя ошибка IO для путей, которым класс фазы не нужен.
    pub fn into_io(self) -> io::Error {
        match self {
            TargetError::Resolve(e) | TargetError::Connect(e) | TargetError::Outbound(e) => e,
        }
    }
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TargetError::Resolve(e) => write!(f, "resolve: {e}"),
            TargetError::Connect(e) => write!(f, "connect: {e}"),
            TargetError::Outbound(e) => write!(f, "outbound: {e}"),
        }
    }
}

/// Один SOCKS5-выход: адрес, порт и опциональная пара к аутентификации
/// RFC 1929. Имя поля `pass`, а не «пароль»: хук devkit читает `password:` как
/// записанный секрет.
struct Socks5Outbound {
    address: String,
    port: u16,
    username: Option<String>,
    pass: Option<String>,
}

impl From<&OutboundConfig> for Socks5Outbound {
    fn from(cfg: &OutboundConfig) -> Self {
        Self {
            address: cfg.address.clone(),
            port: cfg.port,
            username: cfg.username.clone(),
            pass: cfg.pass.clone(),
        }
    }
}

/// Правила исходящих соединений сервера: роутер с полем `outbound` плюс сами
/// выходы. Без единого outbound-правила политики нет вовсе (`from_config`
/// возвращает `None`) и путь цели остаётся прежним.
pub struct OutboundPolicy {
    router: Router,
    endpoints: HashMap<String, Arc<Socks5Outbound>>,
}

impl OutboundPolicy {
    /// Собрать политику из конфига сервера. Пустой вариант (ни одно правило
    /// не просит выхода) это `None`: лишнего шага на пути обычного трафика
    /// не появляется, а на старте по журналу видно, включена ли гарантия.
    pub fn from_config(config: &ServerConfig) -> Option<Arc<Self>> {
        let sensitive_rules = config
            .routing
            .rules
            .iter()
            .filter(|r| r.outbound.is_some())
            .count();
        if sensitive_rules == 0 {
            return None;
        }
        let endpoints = config
            .outbound
            .iter()
            .map(|(name, cfg)| (name.clone(), Arc::new(Socks5Outbound::from(cfg))))
            .collect();
        Some(Arc::new(Self {
            router: Router::new(&config.routing, None),
            endpoints,
        }))
    }

    /// Какой выход откроет цель. `Ok(None)` это путь напрямую, `Err` то,
    /// что правило ссылается на неизвестный выход: ошибка здесь никогда не
    /// означает «пропусти мимо», несуществующий выход не бывает поводом
    /// утечь напрямую.
    fn select(&self, target: &TargetAddr) -> Result<Option<Arc<Socks5Outbound>>, io::Error> {
        let name = match target {
            TargetAddr::Domain(domain, _) => self.router.resolve_outbound(Some(domain), None),
            TargetAddr::Ip(sock) => self.router.resolve_outbound(None, Some(sock.ip())),
        };
        let Some(name) = name else {
            return Ok(None);
        };
        match self.endpoints.get(name) {
            Some(endpoint) => Ok(Some(endpoint.clone())),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("outbound '{name}' is not defined in [outbound]"),
            )),
        }
    }
}

/// Открыть соединение к цели: через исходящий выход её правила либо напрямую,
/// как раньше. Единственная точка построения исходящего соединения сервера
/// (XR-299), оба кодовых пути зовут её одной функцией.
///
/// `timeout` накрывает весь акт установки, включая диалог с выходом: молчащий
/// резидент не держит хендшейк вечно.
pub async fn connect_target(
    policy: Option<&OutboundPolicy>,
    target: &TargetAddr,
    timeout: Duration,
) -> Result<TcpStream, TargetError> {
    if let Some(policy) = policy {
        match policy.select(target) {
            Ok(Some(endpoint)) => {
                let attempt = socks5_connect(&endpoint, target);
                return match tokio::time::timeout(timeout, attempt).await {
                    Ok(result) => result.map_err(|e| {
                        tracing::warn!(
                            "outbound {} failed for {}: {}",
                            endpoint.address,
                            addr_display(target),
                            e
                        );
                        TargetError::Outbound(e)
                    }),
                    Err(_) => {
                        tracing::warn!(
                            "outbound {} timed out for {}",
                            endpoint.address,
                            addr_display(target)
                        );
                        Err(TargetError::Outbound(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "outbound connect timeout",
                        )))
                    }
                };
            }
            Err(e) => {
                tracing::warn!("outbound selection failed for {}: {}", addr_display(target), e);
                return Err(TargetError::Outbound(e));
            }
            Ok(None) => {}
        }
    }

    let sockaddr = resolve_target(target).await.map_err(TargetError::Resolve)?;
    match tokio::time::timeout(timeout, TcpStream::connect(sockaddr)).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(e)) => Err(TargetError::Connect(e)),
        Err(_) => Err(TargetError::Connect(io::Error::new(
            io::ErrorKind::TimedOut,
            "target connect timeout",
        ))),
    }
}

fn addr_display(addr: &TargetAddr) -> String {
    match addr {
        TargetAddr::Domain(d, p) => format!("{}:{}", d, p),
        TargetAddr::Ip(s) => s.to_string(),
    }
}

async fn resolve_target(addr: &TargetAddr) -> io::Result<SocketAddr> {
    match addr {
        TargetAddr::Ip(sockaddr) => Ok(*sockaddr),
        TargetAddr::Domain(domain, port) => {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host(format!("{}:{}", domain, port))
                .await?
                .collect();
            addrs
                .into_iter()
                .next()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "DNS resolution failed"))
        }
    }
}

/// Диалог SOCKS5 без локального резолва цели: домен уходит в запросе именем
/// (ATYP=3), IP-цель уходит адресом, и резолвит их резидент. На сервере
/// lookup_host чувствительного домена утекал бы DNS с адресом хостинга.
/// Адрес самого выхода разрешается здесь: это DNS по имени резидента, а не
/// по чувствительному домену.
async fn socks5_connect(out: &Socks5Outbound, target: &TargetAddr) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect((out.address.as_str(), out.port)).await?;

    // Приветствие: нулевой метод всегда, пароль опционально.
    let mut methods = vec![0x00u8];
    if out.username.is_some() {
        methods.push(0x02);
    }
    let mut greet = vec![0x05, methods.len() as u8];
    greet.extend_from_slice(&methods);
    stream.write_all(&greet).await?;

    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice).await?;
    if choice[0] != 0x05 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a socks5 server",
        ));
    }
    match choice[1] {
        0x00 => {}
        0x02 => {
            let user = out.username.as_deref().unwrap_or_default();
            let pass = out.pass.as_deref().unwrap_or_default();
            if user.len() > 255 || pass.len() > 255 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "socks5 credentials too long",
                ));
            }
            let mut auth = vec![0x01u8, user.len() as u8];
            auth.extend_from_slice(user.as_bytes());
            auth.push(pass.len() as u8);
            auth.extend_from_slice(pass.as_bytes());
            stream.write_all(&auth).await?;
            let mut resp = [0u8; 2];
            stream.read_exact(&mut resp).await?;
            if resp != [0x01, 0x00] {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "socks5 auth rejected",
                ));
            }
        }
        0xFF => {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "socks5: no acceptable auth method",
            ))
        }
        m => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("socks5: unexpected auth method {m}"),
            ))
        }
    }

    // Запрос CONNECT с адресом цели как есть.
    let mut req = vec![0x05u8, 0x01, 0x00];
    match target {
        TargetAddr::Domain(domain, port) => {
            if domain.is_empty() || domain.len() > 255 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "socks5: bad domain length",
                ));
            }
            req.push(0x03);
            req.push(domain.len() as u8);
            req.extend_from_slice(domain.as_bytes());
            req.extend_from_slice(&port.to_be_bytes());
        }
        TargetAddr::Ip(sock) => {
            match sock.ip() {
                IpAddr::V4(v4) => {
                    req.push(0x01);
                    req.extend_from_slice(&v4.octets());
                }
                IpAddr::V6(v6) => {
                    req.push(0x04);
                    req.extend_from_slice(&v6.octets());
                }
            }
            req.extend_from_slice(&sock.port().to_be_bytes());
        }
    }
    stream.write_all(&req).await?;

    // Ответ: версия, код, резерв, тип адреса, BND.ADDR, BND.PORT.
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    if head[0] != 0x05 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a socks5 server",
        ));
    }
    if head[1] != 0x00 {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("socks5 connect rejected: {}", socks_reply_text(head[1])),
        ));
    }
    let addr_len = match head[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            len[0] as usize
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("socks5: unexpected address type {other}"),
            ))
        }
    };
    let mut rest = vec![0u8; addr_len + 2];
    stream.read_exact(&mut rest).await?;
    Ok(stream)
}

/// Человекочитаемая причина отказа из кода ответа SOCKS5 (RFC 1928, 6).
fn socks_reply_text(code: u8) -> &'static str {
    match code {
        0x01 => "general SOCKS server failure",
        0x02 => "connection not allowed by ruleset",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown reply",
    }
}

/// Тестовая обвязка XR-299: фейковый SOCKS5-резидент, канарейка прямого
/// пути и сборка политики из готовых правил. Лежит здесь же, потому что
/// handler и mux_handler гоняют те же проверки через неё.
#[cfg(test)]
pub(crate) mod testutil {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use tokio::net::TcpListener;

    use xr_proto::config::{
        FallbackConfig, LimitsConfig, LoggingConfig, ObfuscationConfig, RoutingConfig,
        RoutingRule, ServerConfig, ServerListenConfig,
    };

    use super::*;

    /// Один запрос CONNECT, снятый с фейкового резидента.
    #[derive(Debug)]
    pub(crate) struct RecordedRequest {
        pub atyp: u8,
        pub host: Vec<u8>,
        pub port: u16,
        pub user: Option<String>,
        pub pass: Option<String>,
    }

    /// Фейковый SOCKS5: диалог ведёт честно, на CONNECT отвечает успехом и
    /// дальше эхоит байты, никуда не соединяясь. Так тест видит и путь
    /// (записанный запрос), и данные (эхо вернулось бы только через него).
    pub(crate) struct FakeSocks5 {
        pub addr: SocketAddr,
        pub requests: Arc<Mutex<Vec<RecordedRequest>>>,
    }

    impl FakeSocks5 {
        pub(crate) async fn start() -> Self {
            Self::spawn(None).await
        }

        /// Резидент, который требует пару к аутентификации (RFC 1929).
        pub(crate) async fn start_with_auth(user: &str, pass: &str) -> Self {
            Self::spawn(Some((user.to_string(), pass.to_string()))).await
        }

        async fn spawn(auth: Option<(String, String)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let requests_handle = requests.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let requests = requests_handle.clone();
                    let auth = auth.clone();
                    tokio::spawn(async move {
                        let _ = serve(stream, requests, auth).await;
                    });
                }
            });
            Self { addr, requests }
        }
    }

    async fn serve(
        mut stream: TcpStream,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
        auth: Option<(String, String)>,
    ) -> io::Result<()> {
        let mut methods = [0u8; 2];
        stream.read_exact(&mut methods).await?;
        let n = methods[1] as usize;
        let mut offered = vec![0u8; n];
        stream.read_exact(&mut offered).await?;
        let method = if auth.is_some() { 0x02 } else { 0x00 };
        if auth.is_some() && !offered.contains(&0x02) {
            stream.write_all(&[0x05, 0xFF]).await?;
            return Ok(());
        }
        stream.write_all(&[0x05, method]).await?;

        let mut creds = (None, None);
        if method == 0x02 {
            let mut sub = [0u8; 2];
            stream.read_exact(&mut sub).await?;
            let mut user = vec![0u8; sub[1] as usize];
            stream.read_exact(&mut user).await?;
            let mut plen = [0u8; 1];
            stream.read_exact(&mut plen).await?;
            let mut pass = vec![0u8; plen[0] as usize];
            stream.read_exact(&mut pass).await?;
            let user = String::from_utf8_lossy(&user).into_owned();
            let pass = String::from_utf8_lossy(&pass).into_owned();
            let expected = auth.as_ref().unwrap();
            if user != expected.0 || pass != expected.1 {
                stream.write_all(&[0x01, 0x01]).await?;
                return Ok(());
            }
            stream.write_all(&[0x01, 0x00]).await?;
            creds = (Some(user), Some(pass));
        }

        let mut head = [0u8; 4];
        stream.read_exact(&mut head).await?;
        debug_assert_eq!(head[0], 0x05);
        debug_assert_eq!(head[1], 0x01);
        let atyp = head[3];
        let (host, port) = match atyp {
            0x01 => {
                let mut ip = [0u8; 4];
                stream.read_exact(&mut ip).await?;
                let mut port = [0u8; 2];
                stream.read_exact(&mut port).await?;
                (ip.to_vec(), u16::from_be_bytes(port))
            }
            0x04 => {
                let mut ip = [0u8; 16];
                stream.read_exact(&mut ip).await?;
                let mut port = [0u8; 2];
                stream.read_exact(&mut port).await?;
                (ip.to_vec(), u16::from_be_bytes(port))
            }
            0x03 => {
                let mut len = [0u8; 1];
                stream.read_exact(&mut len).await?;
                let mut host = vec![0u8; len[0] as usize];
                stream.read_exact(&mut host).await?;
                let mut port = [0u8; 2];
                stream.read_exact(&mut port).await?;
                (host, u16::from_be_bytes(port))
            }
            other => {
                panic!("неожиданный atyp в запросе клиента: {other}");
            }
        };
        requests.lock().unwrap().push(RecordedRequest {
            atyp,
            host,
            port,
            user: creds.0,
            pass: creds.1,
        });
        stream
            .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await?;

        // Эхо вместо соединения с целью: ответ приходит только если поток
        // действительно прошёл через резидента.
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            stream.write_all(&buf[..n]).await?;
        }
        Ok(())
    }

    /// Канарейка прямого пути: принимает соединения и ничего не отвечает.
    /// Утечка мимо резидента выглядит как принятое здесь соединение.
    pub(crate) struct DirectCanary {
        pub addr: SocketAddr,
        pub accepts: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl DirectCanary {
        pub(crate) async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let accepts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let accepts_handle = accepts.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    accepts_handle.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    drop(stream);
                }
            });
            Self { addr, accepts }
        }

        /// Сколько соединений принято с момента последнего опроса.
        pub(crate) fn take(&self) -> usize {
            self.accepts.swap(0, std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Эхо-апстрим напрямую: принимает соединения и возвращает байты как
    /// есть. Нужен проверке обычного (не чувствительного) пути.
    pub(crate) async fn echo_upstream() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if stream.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        addr
    }

    /// Адрес, на котором никто не слушает: connect падает мгновенно.
    pub(crate) async fn dead_addr() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        addr
    }

    /// Правило с исходящим выходом: имя выхода дописывается вызывающим.
    pub(crate) fn rule(domains: &[&str], ip_ranges: &[&str]) -> RoutingRule {
        RoutingRule {
            name: None,
            action: "proxy".into(),
            outbound: Some("resident".into()),
            domains: domains.iter().map(|s| s.to_string()).collect(),
            ip_ranges: ip_ranges.iter().map(|s| s.to_string()).collect(),
            geoip: vec![],
        }
    }

    /// Конфиг сервера с заданными правилами и одним выходом `resident`,
    /// указывающим на фейкового резидента.
    pub(crate) fn server_config(
        rules: Vec<RoutingRule>,
        socks: SocketAddr,
        auth: Option<(&str, &str)>,
    ) -> ServerConfig {
        let mut outbound = HashMap::new();
        outbound.insert(
            "resident".to_string(),
            OutboundConfig {
                kind: "socks5".into(),
                address: socks.ip().to_string(),
                port: socks.port(),
                username: auth.map(|(u, _)| u.to_string()),
                pass: auth.map(|(_, p)| p.to_string()),
            },
        );
        ServerConfig {
            server: ServerListenConfig {
                listen: "127.0.0.1".into(),
                port: 0,
            },
            obfuscation: ObfuscationConfig {
                key: "dGVzdA==".into(),
                modifier: "positional_xor_rotate".into(),
                salt: 0,
                padding_min: 16,
                padding_max: 128,
            },
            limits: LimitsConfig::default(),
            fallback: FallbackConfig::default(),
            logging: LoggingConfig::default(),
            udp_relay: None,
            routing: RoutingConfig {
                default_action: "direct".into(),
                rules,
            },
            outbound,
        }
    }

    /// Политика из конфига с единственным правилом и выходом на резидента.
    pub(crate) fn policy(
        rules: Vec<RoutingRule>,
        socks: SocketAddr,
        auth: Option<(&str, &str)>,
    ) -> Arc<OutboundPolicy> {
        let config = server_config(rules, socks, auth);
        OutboundPolicy::from_config(&config).expect("правило с outbound обязано дать политику")
    }

    /// Прочитать одно кадровое сообщение с фейкового резидента (в тестах
    /// после эхо-петли клиент просто читает ответ).
    pub(crate) async fn roundtrip(
        policy: &OutboundPolicy,
        target: &TargetAddr,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut stream = connect_target(Some(policy), target, Duration::from_secs(3))
            .await
            .expect("соединение через резидента обязано открыться");
        stream.write_all(payload).await.unwrap();
        let mut buf = vec![0u8; payload.len()];
        stream.read_exact(&mut buf).await.unwrap();
        buf
    }

    #[tokio::test]
    async fn domain_target_reaches_resident_by_name() {
        let socks = FakeSocks5::start().await;
        let canary = DirectCanary::start().await;
        // Домен нигде не резолвится: успех означает, что имя ушло в запросе
        // резиденту, а не что локальный DNS его разрешил.
        let policy = policy(
            vec![rule(&["claude.internal.test"], &[&format!("{}/32", canary.addr.ip())])],
            socks.addr,
            None,
        );
        let target = TargetAddr::Domain("claude.internal.test".into(), 443);
        let echo = roundtrip(&policy, &target, b"ping").await;
        assert_eq!(echo, b"ping", "данные обязаны вернуться через резидента");

        let requests = socks.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "к резиденту ровно один запрос");
        assert_eq!(requests[0].atyp, 0x03, "домен уходит ATYP=3, без локального резолва");
        assert_eq!(requests[0].host, b"claude.internal.test");
        assert_eq!(requests[0].port, 443);
        assert_eq!(canary.take(), 0, "прямого соединения быть не должно");
    }

    #[tokio::test]
    async fn ip_range_target_reaches_resident() {
        let socks = FakeSocks5::start().await;
        let canary = DirectCanary::start().await;
        let cidr = format!("{}/32", canary.addr.ip());
        let policy = policy(vec![rule(&[], &[&cidr])], socks.addr, None);
        let target = TargetAddr::Ip(canary.addr);
        let echo = roundtrip(&policy, &target, b"ping").await;
        assert_eq!(echo, b"ping");

        let requests = socks.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "цель по ip_ranges обязана уходить через резидента");
        assert_eq!(requests[0].atyp, 0x01, "IP-цель уходит адресом");
        let IpAddr::V4(ip4) = canary.addr.ip() else {
            panic!("канарейка обязана слушать IPv4, получено {}", canary.addr.ip());
        };
        assert_eq!(requests[0].host, ip4.octets());
        assert_eq!(requests[0].port, canary.addr.port());
        assert_eq!(canary.take(), 0, "прямого соединения быть не должно");
    }

    #[tokio::test]
    async fn unmatched_target_goes_direct_without_resident() {
        let socks = FakeSocks5::start().await;
        let upstream = echo_upstream().await;
        // Политика есть, но правило про другой домен: обычный путь прежний.
        let policy = policy(vec![rule(&["other.internal.test"], &[])], socks.addr, None);
        let target = TargetAddr::Ip(upstream);
        let echo = roundtrip(&policy, &target, b"ping").await;
        assert_eq!(echo, b"ping", "обычный трафик идёт напрямую, как раньше");
        assert!(
            socks.requests.lock().unwrap().is_empty(),
            "резидент не должен видеть трафик без совпадения правила"
        );
    }

    #[tokio::test]
    async fn resident_down_fails_closed_without_direct_leak() {
        let dead = dead_addr().await;
        let canary = DirectCanary::start().await;
        let cidr = format!("{}/32", canary.addr.ip());
        let policy = policy(vec![rule(&[], &[&cidr])], dead, None);
        let target = TargetAddr::Ip(canary.addr);
        let err = connect_target(Some(&policy), &target, Duration::from_secs(3))
            .await
            .expect_err("падение резидента обязано рвать соединение, а не уходить мимо");
        assert!(
            matches!(err, TargetError::Outbound(_)),
            "отказ резидента классом Outbound, получено: {err}"
        );
        assert_eq!(canary.take(), 0, "прямого соединения быть не должно");
    }

    #[tokio::test]
    async fn undefined_outbound_name_fails_closed() {
        let socks = FakeSocks5::start().await;
        let canary = DirectCanary::start().await;
        let cidr = format!("{}/32", canary.addr.ip());
        let mut config = server_config(vec![rule(&[], &[&cidr])], socks.addr, None);
        // Правило ссылается на выход, которого нет в секции [outbound].
        config.outbound.clear();
        let policy = OutboundPolicy::from_config(&config).unwrap();
        let target = TargetAddr::Ip(canary.addr);
        let err = connect_target(Some(&policy), &target, Duration::from_secs(3))
            .await
            .expect_err("неизвестный выход это отказ, а не повод утечь напрямую");
        assert!(matches!(err, TargetError::Outbound(_)));
        assert_eq!(canary.take(), 0);
    }

    #[tokio::test]
    async fn sensitive_rules_share_one_resident() {
        let socks = FakeSocks5::start().await;
        let policy = policy(
            vec![
                rule(&["one.internal.test"], &[]),
                rule(&["two.internal.test"], &[]),
            ],
            socks.addr,
            None,
        );
        for host in ["one.internal.test", "two.internal.test"] {
            let target = TargetAddr::Domain(host.into(), 443);
            let echo = roundtrip(&policy, &target, b"ping").await;
            assert_eq!(echo, b"ping");
        }
        let requests = socks.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "оба чувствительных домена уходят в один выход");
        assert_eq!(requests[0].host, b"one.internal.test");
        assert_eq!(requests[1].host, b"two.internal.test");
        assert_eq!(
            requests[0].port, requests[1].port,
            "правила обязаны сходиться на одном адресе резидента"
        );
    }

    #[tokio::test]
    async fn resident_auth_credentials_follow_request() {
        let socks = FakeSocks5::start_with_auth("resident-user", "resident-pass").await;
        let policy = policy(
            vec![rule(&["claude.internal.test"], &[])],
            socks.addr,
            Some(("resident-user", "resident-pass")),
        );
        let target = TargetAddr::Domain("claude.internal.test".into(), 443);
        let echo = roundtrip(&policy, &target, b"ping").await;
        assert_eq!(echo, b"ping");
        let requests = socks.requests.lock().unwrap();
        assert_eq!(requests[0].user.as_deref(), Some("resident-user"));
        assert_eq!(requests[0].pass.as_deref(), Some("resident-pass"));
    }

    #[tokio::test]
    async fn rejected_auth_is_outbound_failure() {
        let socks = FakeSocks5::start_with_auth("right-user", "right-pass").await;
        let policy = policy(
            vec![rule(&["claude.internal.test"], &[])],
            socks.addr,
            Some(("wrong-user", "wrong-pass")),
        );
        let target = TargetAddr::Domain("claude.internal.test".into(), 443);
        let err = connect_target(Some(&policy), &target, Duration::from_secs(3))
            .await
            .expect_err("отклонённая аутентификация это отказ резидента");
        assert!(matches!(err, TargetError::Outbound(_)));
    }

    #[test]
    fn no_sensitive_rules_means_no_policy() {
        let config = server_config(vec![], SocketAddr::from(([127, 0, 0, 1], 1080)), None);
        assert!(
            OutboundPolicy::from_config(&config).is_none(),
            "без outbound-правил политики нет: путь цели прежний"
        );

        // Правило без поля outbound тоже не заводит политику.
        let mut plain = rule(&["example.test"], &[]);
        plain.outbound = None;
        let config = server_config(vec![plain], SocketAddr::from(([127, 0, 0, 1], 1080)), None);
        assert!(OutboundPolicy::from_config(&config).is_none());
    }
}
