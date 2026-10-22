/// Transparent proxy core: accept connections, extract SNI, route, tunnel.
use crate::wan;
use xr_proto::accept::accept_loop;
use xr_proto::device::{DevicePolicySet, PolicyDecision};
use xr_proto::routing::{Action, Router};
use xr_proto::server_pool::ServerPool;
use xr_proto::sni;
use xr_proto::tunnel;
use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Duration;
use xr_proto::protocol::TargetAddr;

// ── SO_ORIGINAL_DST ──────────────────────────────────────────────────

/// Get the original destination address from a redirected (NAT) connection.
/// Uses the SO_ORIGINAL_DST socket option on Linux.
fn get_original_dst(stream: &TcpStream) -> io::Result<SocketAddr> {
    use std::os::unix::io::AsRawFd;

    let fd = stream.as_raw_fd();

    unsafe {
        let mut addr: libc::sockaddr_in = std::mem::zeroed();
        let mut len: libc::socklen_t = std::mem::size_of::<libc::sockaddr_in>() as u32;

        // SOL_IP = 0, SO_ORIGINAL_DST = 80
        let ret = libc::getsockopt(
            fd,
            0,  // SOL_IP
            80, // SO_ORIGINAL_DST
            &mut addr as *mut _ as *mut libc::c_void,
            &mut len,
        );

        if ret != 0 {
            return Err(io::Error::last_os_error());
        }

        let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
        let port = u16::from_be(addr.sin_port);
        Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)))
    }
}

// ── Shared state ─────────────────────────────────────────────────────

pub struct ProxyState {
    /// `RwLock<Arc<Router>>` позволяет background preset-refresh'у
    /// подменять активные правила без рестарта клиента. Hot path —
    /// один `resolve()` на connection, read-lock держится миллисекунды;
    /// write случается на пробуждении ручки ожидания хаба (LLD-37), то есть
    /// в момент публикации новой версии пресета. `refresh_interval_secs`
    /// остался потолком паузы деградированного опроса, к записи он больше
    /// отношения не имеет.
    ///
    /// Уже установленные TCP-relay-сессии держат Action по value, так
    /// что их маршрут не меняется при swap'е — только новые подключения
    /// видят обновлённые правила. Это честная семантика "обновление
    /// применяется к новым соединениям".
    pub router: RwLock<Arc<Router>>,
    pub on_server_down: Action,
    pub listen_port: u16,
    /// Пул серверов (LLD-10): primary/backup по приоритету, failover и
    /// failback внутри. `Err` от него означает «весь пул недоступен».
    pub server_pool: Arc<ServerPool>,
    /// Политики устройств (XR-310). Пустой набор держит весь трафик на
    /// общих правилах `[routing]`, поэтому старый конфиг ведёт себя прежне.
    pub policy: DevicePolicySet,
    /// Пулы профилей выхода по ключу `device::pool_key`: интерфейс канала
    /// плюс подмножество серверов. Ключ пула по умолчанию здесь тоже,
    /// профиль без `wan` с полным списком попадает на `server_pool`.
    pub pools: HashMap<String, Arc<ServerPool>>,
    pub wan_pool: Option<Arc<wan::WanPool>>,
    pub wan_pool_bypass_ips: Vec<String>,
    pub default_pool_key: String,
}

/// Enable TCP keepalive on a stream to detect dead connections.
fn set_keepalive(stream: &TcpStream) {
    let ka = socket2::TcpKeepalive::new()
        .with_time(std::time::Duration::from_secs(60))
        .with_interval(std::time::Duration::from_secs(15));
    let sock_ref = socket2::SockRef::from(stream);
    let _ = sock_ref.set_tcp_keepalive(&ka);
}

// ── Main proxy loop ──────────────────────────────────────────────────

pub async fn run_proxy(
    listen_port: u16,
    state: Arc<ProxyState>,
) -> io::Result<()> {
    // Use SO_REUSEADDR so rapid restarts don't fail with "address already in use"
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.set_reuseaddr(true)?;
    socket.bind(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, listen_port)))?;
    let listener = socket.listen(1024)?;
    tracing::info!("Transparent proxy listening on 0.0.0.0:{}", listen_port);

    let listener = &listener;
    accept_loop(
        "proxy",
        move || async move { listener.accept().await.map(Some) },
        |client_stream, client_addr| {
            let state = state.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_connection(client_stream, client_addr, state).await {
                    let msg = e.to_string();
                    // Connection resets are normal (client closed tab, app timeout, etc.)
                    if msg.contains("reset by peer")
                        || msg.contains("Broken pipe")
                        || msg.contains("Connection refused")
                    {
                        tracing::debug!("Connection from {} closed: {}", client_addr, msg);
                    } else {
                        tracing::warn!("Connection from {} failed: {}", client_addr, e);
                    }
                }
            });
            std::future::ready(())
        },
    )
    .await
}

/// Сколько ждать хвост ClientHello, который не влез в первый сегмент.
const SNI_FRAGMENT_WAIT: Duration = Duration::from_millis(500);

/// Пауза между подглядываниями. Сокет остаётся читаемым, пока в нём лежат
/// первые байты, поэтому `peek` возвращается сразу и тем же куском: новых байт
/// приходится ждать по часам, иначе цикл сожрёт ядро.
const SNI_PEEK_POLL: Duration = Duration::from_millis(5);

/// Досмотреть первые байты клиента до целого рекорда с ClientHello и вернуть,
/// сколько байт лежит в `buf`. Имя снималось с одной порции, а с постквантовым
/// key_share рекорд перестал влезать в сегмент: обрезанное начало не давало
/// имени, и проксируемый сайт уходил действием по умолчанию. `peek` байты не
/// съедает, поэтому релей потом прочитает их сам.
async fn peek_client_hello(
    client: &TcpStream,
    buf: &mut Vec<u8>,
    first: usize,
) -> io::Result<usize> {
    let mut n = first;
    let deadline = tokio::time::Instant::now() + SNI_FRAGMENT_WAIT;

    while let Some(want) = sni::client_hello_record_len(&buf[..n]) {
        if n >= want {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                "SNI: ClientHello не собрался, {} байт из {}, маршрут по IP",
                n,
                want
            );
            break;
        }
        if buf.len() < want {
            buf.resize(want, 0);
        }
        tokio::time::sleep(SNI_PEEK_POLL).await;
        n = client.peek(&mut buf[..want]).await?;
    }

    Ok(n)
}

/// Порты, которым маршрутизация доверяет подсмотренному SNI.
fn is_web_port(port: u16) -> bool {
    matches!(port, 80 | 443)
}

/// SNI, который политика устройств вправе читать как имя ресурса (XR-310).
/// Вне web-портов имя недостоверно: маскарад под TLS шлёт ClientHello с
/// чужими доменами (разбор у `verdict_action`), и доменное правило на
/// таком имени отправило бы маскирующийся поток не в его профиль, реальный
/// IP утёк бы там, где профиль обещает прокси. Вне 80/443 политика
/// получает `None`, у правил устройств остаются совпадения `ip_ranges`;
/// общий путь `[routing]` поступает так же через `verdict_action`.
fn policy_sni(web_port: bool, sni: Option<&str>) -> Option<&str> {
    if web_port { sni } else { None }
}

/// Сколько peek ждёт первые байты клиента. На web-портах SNI решает маршрут,
/// и ClientHello с постквантовым key_share приезжает медленно и кусками,
/// поэтому ожидание длинное. Вне web-портов подсмотренные байты нужны ровно
/// на первый байт 0x16: обфускация под TLS шлёт ClientHello сразу после
/// connect и укладывается в короткое окно, а молчащему клиенту (server-first
/// протокол: VNC, SMTP, MySQL, IRC) дальше нужен баннер сервера, а не наше
/// ожидание.
const PEEK_WAIT_WEB: Duration = Duration::from_secs(10);
const PEEK_WAIT_OTHER: Duration = Duration::from_millis(300);

/// Решение по первым байтам клиента: маршрут или разрыв соединения.
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
enum PeekVerdict {
    /// Клиент на web-порту так и не заговорил: соединение мёртвое.
    Dead,
    /// Действие маршрутизации и подсмотренное SNI-имя для туннеля.
    Route(Action, Option<String>),
}

/// Вердикт общих правил `[routing]` по фактам подсмотра (XR-292).
///
/// SNI-роутинг доверяем только на стандартных web-портах (80/443). На любом
/// нестандартном порту SNI скорее всего fake (Telegram MTProto маскирует
/// обфусцированный поток под TLS-handshake с self.events.data.microsoft.com,
/// ssl.gstatic.com и подобными доменами для обхода DPI). Решение по такому
/// SNI = заведомо неправильный routing -> direct -> провайдерский RST.
///
/// Для non-80/443 портов смотрим на сам первый байт: 0x16 = TLS handshake
/// ContentType. Если это TLS, почти наверняка обфусцированный/маскированный
/// протокол (Telegram MTProto на 5277/5993, DoT на 853 и т.п.) -> Proxy.
/// Если нет, это сырой TCP-протокол (BitTorrent peer handshake начинается
/// с 0x13 + "BitTorrent protocol", IRC и т.д.). Проксировать его бессмысленно
/// (IP клиента всё равно засветится в peer-listing), а вред огромный:
/// BitTorrent открывает десятки одновременных Connect'ов к мёртвым/firewalled
/// пирам, забивает mux writer-канал и target-семафор xr-server'а
/// (max_connections=256), из-за чего ConnectAck для легитимного TLS-трафика
/// (YouTube, шортсы) timeout'ит и видео фризит.
fn verdict_action(web_port: bool, tls_first_byte: bool, resolved: Action) -> Action {
    if web_port {
        resolved
    } else if tls_first_byte {
        Action::Proxy
    } else {
        Action::Direct
    }
}

/// Факты подсмотра первых байт клиента, без решения о маршруте. Политика
/// устройств (XR-310) решает по ним раньше общих правил, поэтому подсмотр
/// отделён от вердикта.
#[derive(Debug, Default)]
struct PeekFacts {
    sni: Option<String>,
    /// Первый байт 0x16, начало TLS-записи.
    tls_first_byte: bool,
    /// Клиент на web-порту промолчал всё окно: соединение мёртвое.
    dead: bool,
}

/// Подсмотреть первые байты клиента. Молчание вне web-портов это штатный
/// случай server-first протокола: баннер обещает сервер, поэтому по таймауту
/// короткого ожидания факты остаются пустыми и соединение уйдёт в Direct,
/// а не рвётся после долгого висения (XR-292). На web-порту клиент обязан
/// говорить первым, тишина означает мёртвое соединение.
async fn peek_facts(client: &TcpStream, orig_dst: SocketAddr) -> io::Result<PeekFacts> {
    let mut peek_buf = vec![0u8; 4096];
    let wait = if is_web_port(orig_dst.port()) {
        PEEK_WAIT_WEB
    } else {
        PEEK_WAIT_OTHER
    };
    let n = match tokio::time::timeout(wait, client.peek(&mut peek_buf)).await {
        Ok(result) => result?,
        Err(_) if !is_web_port(orig_dst.port()) => {
            tracing::debug!("Peek timeout -> {}: server-first, routing direct", orig_dst);
            0
        }
        Err(_) => {
            tracing::debug!("Peek timeout -> {}: dropping", orig_dst);
            return Ok(PeekFacts { dead: true, ..Default::default() });
        }
    };
    let n = peek_client_hello(client, &mut peek_buf, n).await?;
    let sni = sni::extract_sni(&peek_buf[..n]);
    Ok(PeekFacts {
        sni,
        tls_first_byte: n > 0 && peek_buf[0] == 0x16,
        dead: false,
    })
}

/// Подсмотреть первые байты клиента и решить маршрут по общим правилам
/// `[routing]` (XR-292). Политика устройств сюда не входит: у неё свой
/// судья над теми же фактами в `handle_connection`. Живёт для тестов
/// подсмотра: продакшн-путь собирает те же части по ходу `handle_connection`.
#[cfg(test)]
async fn decide_route<F>(
    client: &TcpStream,
    orig_dst: SocketAddr,
    resolve: F,
) -> io::Result<PeekVerdict>
where
    F: FnOnce(Option<&str>) -> Action,
{
    let facts = peek_facts(client, orig_dst).await?;
    if facts.dead {
        return Ok(PeekVerdict::Dead);
    }
    let resolved = resolve(facts.sni.as_deref());
    let action = verdict_action(is_web_port(orig_dst.port()), facts.tls_first_byte, resolved);
    Ok(PeekVerdict::Route(action, facts.sni))
}

async fn handle_connection(
    mut client: TcpStream,
    client_addr: SocketAddr,
    state: Arc<ProxyState>,
) -> io::Result<()> {
    // Get original destination
    let orig_dst = get_original_dst(&client)?;
    let dest_ip = orig_dst.ip();

    // Loop detection: if the original destination is our own listen port,
    // someone is connecting directly to the proxy (e.g. from WAN).
    // Drop to prevent infinite loops.
    if orig_dst.port() == state.listen_port {
        tracing::debug!("Loop detected: {} -> {} (own listen port), dropping", client_addr, orig_dst);
        return Ok(());
    }

    // Enable TCP keepalive to detect dead connections
    set_keepalive(&client);

    // Read-lock роутера живёт ровно длину вызова resolve(): guard не Send,
    // и держать его через await-точки подсмотра не выйдет. Сначала решает
    // политика устройств (XR-310): известное устройство получает профиль
    // выхода без эвристики первого байта, неизвестное живёт прежним путём
    // `[routing]` с теми же фактами подсмотра.
    let facts = peek_facts(&client, orig_dst).await?;
    if facts.dead {
        return Ok(());
    }
    let decision = state.policy.decide(
        client_addr.ip(),
        policy_sni(is_web_port(orig_dst.port()), facts.sni.as_deref()),
        dest_ip,
    );
    let (action, sni_name, exit) = match decision {
        PolicyDecision::Exit { exit, device, rule } => {
            tracing::info!(
                "{} -> {} [SNI: {}] device '{}' rule {} exit '{}' wan {} => {:?}",
                client_addr,
                orig_dst,
                facts.sni.as_deref().unwrap_or("-"),
                device,
                rule.as_deref().unwrap_or("default"),
                exit.name,
                exit.interface.as_deref().unwrap_or("main"),
                exit.action
            );
            (exit.action, facts.sni.clone(), Some(exit))
        }
        PolicyDecision::Global => {
            let resolved = state.router.read().unwrap().resolve(facts.sni.as_deref(), dest_ip);
            let action =
                verdict_action(is_web_port(orig_dst.port()), facts.tls_first_byte, resolved);
            (action, facts.sni.clone(), None)
        }
    };

    if exit.is_none() {
        tracing::info!(
            "{} -> {} [SNI: {}] => {:?}",
            client_addr,
            orig_dst,
            sni_name.as_deref().unwrap_or("-"),
            action
        );
    }

    let idle_timeout = Duration::from_secs(300);
    let max_lifetime = Duration::from_secs(3600);

    // Hard cap на прямой TCP connect. Без него default Linux SYN retry
    // выкручивает на ~130 секунд, и при BitTorrent-нагрузке (десятки
    // dial/сек к мёртвым/firewalled пирам) сокеты накапливаются: видели
    // 845 open fd при единственном активном LAN-стриме, что топит tokio
    // runtime и тормозит легитимный YouTube-трафик.
    let direct_connect_timeout = Duration::from_secs(5);

    let balanced = if exit.as_ref().is_none_or(|e| e.use_wan_pool)
        && !xr_proto::config::wan_pool_bypasses(&state.wan_pool_bypass_ips, client_addr.ip())
    { state.wan_pool.as_deref() } else { None };

    match action {
        Action::Block => {
            // Правило маршрутизации явно блокирует: рвём соединение, наружу не
            // выпускаем. Для проксируемого трафика это fail-closed, IP не течёт.
            tracing::debug!("Blocked {} -> {} (action=block)", client_addr, orig_dst);
            Ok(())
        }
        Action::Direct => {
            // Напрямую к цели через канал профиля: у устройства без профиля
            // это основной маршрут ОС, как и до XR-310.
            let iface = exit.as_ref().and_then(|e| e.interface.as_deref());
            direct_relay(&mut client, orig_dst, iface, balanced, direct_connect_timeout, max_lifetime).await
        }
        Action::Proxy => {
            // Туннель через пул профиля. Для устройства без профиля это пул
            // по умолчанию с прежним поведением on_server_down (XR-292).
            let (primary, fallback, on_down, iface) = match exit.as_ref() {
                Some(e) => (
                    state.pools.get(&e.pool_key).cloned(),
                    e.fallback_pool_key
                        .as_ref()
                        .and_then(|k| state.pools.get(k).cloned()),
                    e.on_server_down,
                    e.interface.as_deref(),
                ),
                None => (
                    Some(state.server_pool.clone()),
                    None,
                    state.on_server_down,
                    None,
                ),
            };
            let primary = match primary {
                Some(pool) => pool,
                None => {
                    // Профилю нужен пул, которого нет в реестре. Это ошибка
                    // сборки, но соединение обязано закрыться, а не уйти
                    // незаданным путём (XR-310, fail-closed).
                    tracing::error!(
                        "exit pool missing, dropping {} -> {}",
                        client_addr,
                        orig_dst
                    );
                    return Ok(());
                }
            };
            let target_addr = target_addr_for(orig_dst, sni_name.as_deref());

            // Резерв канала пробуется только на отказе открытия стрима: как
            // только релей тронул байты клиента, повторная отправка через
            // другой пул дублировала бы поток.
            let opened = if let Some(wan_pool) = balanced {
                let key = exit.as_ref().map(|e| e.pool_key.as_str())
                    .unwrap_or(&state.default_pool_key);
                wan_pool.open_stream(key, &target_addr).await
            } else {
                primary.open_stream(&target_addr).await
            };
            let mux_stream = match opened {
                Ok(stream) => stream,
                Err(e) => {
                    let mut retry = None;
                    if let Some(fb_pool) = &fallback {
                        tracing::warn!(
                            "pool exhausted for {} ({}), retrying via fallback wan",
                            orig_dst,
                            e
                        );
                        retry = fb_pool.open_stream(&target_addr).await.ok();
                    }
                    match retry {
                        Some(stream) => stream,
                        None => {
                            tracing::warn!(
                                "Tunnel to {} failed: {}, fallback={:?}",
                                orig_dst,
                                e,
                                on_down
                            );
                            if on_down == Action::Direct {
                                return direct_relay(
                                    &mut client,
                                    orig_dst,
                                    iface,
                                    balanced,
                                    direct_connect_timeout,
                                    max_lifetime,
                                )
                                .await;
                            }
                            return Err(e);
                        }
                    }
                }
            };
            match relay_mux(&mut client, mux_stream, idle_timeout, max_lifetime).await {
                Ok(()) => Ok(()),
                Err(RelayError::LocalClient(e)) => {
                    tracing::debug!(
                        "LAN client closed early ({} -> {}): {}",
                        client_addr,
                        orig_dst,
                        e
                    );
                    Ok(())
                }
                Err(RelayError::Tunnel(e)) => {
                    tracing::warn!(
                        "Tunnel to {} failed: {}, fallback={:?}",
                        orig_dst,
                        e,
                        on_down
                    );
                    if on_down == Action::Direct {
                        direct_relay(&mut client, orig_dst, iface, balanced, direct_connect_timeout, max_lifetime).await
                    } else {
                        Err(e)
                    }
                }
            }
        }
    }
}

/// Прямое соединение с целью через выбранный канал и релей без туннеля.
/// Таймаут соединения здесь не ошибка: молчащее соединение закрывается
/// тихо, как и до XR-292.
async fn direct_relay(
    client: &mut TcpStream,
    orig_dst: SocketAddr,
    iface: Option<&str>,
    balanced: Option<&wan::WanPool>,
    connect_timeout: Duration,
    max_lifetime: Duration,
) -> io::Result<()> {
    let connection = match balanced {
        Some(pool) => pool.connect(orig_dst, connect_timeout).await,
        None => wan::connect_via(iface, orig_dst, connect_timeout).await,
    };
    let mut target = match connection {
        Ok(stream) => stream,
        Err(e) if e.kind() == io::ErrorKind::TimedOut => {
            tracing::debug!("direct connect to {} timed out", orig_dst);
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    set_keepalive(&target);
    tunnel::relay_bidirectional(client, &mut target, max_lifetime).await
}

/// Адрес цели для туннеля: подсмотренное доменное имя экономит серверу
/// резолв, без имени уходит оригинальный адрес.
fn target_addr_for(orig_dst: SocketAddr, sni_name: Option<&str>) -> TargetAddr {
    if let Some(domain) = sni_name {
        TargetAddr::Domain(domain.to_string(), orig_dst.port())
    } else {
        TargetAddr::Ip(orig_dst)
    }
}

/// Side that produced a relay error. We need to distinguish them because a
/// LAN-side reset is normal (browser tab closed, app backgrounded) and must
/// not trigger a direct fallback or a noisy warn — but a tunnel-side error
/// is a real signal that the obfuscated path is unhealthy.
#[derive(Debug)]
enum RelayError {
    LocalClient(io::Error),
    Tunnel(io::Error),
}

impl From<RelayError> for io::Error {
    fn from(e: RelayError) -> Self {
        match e {
            RelayError::LocalClient(e) | RelayError::Tunnel(e) => e,
        }
    }
}

// Открытие стрима живёт в handle_connection: выбор пула, резерв канала и
// прямой запасной ход переплетены с решением политики (XR-310), а сам релей
// ниже устроен одинаково для любого пула.

/// Relay data between a local client and a MuxStream.
///
/// Upload (LAN→mux) and download (mux→LAN) run as independent tasks. They
/// must NOT share a `tokio::select!` loop: a slow LAN writer would otherwise
/// stall mux recv polling, the per-stream channel would overflow on a CDN
/// burst, and the mux reader task would kill the stream with
/// "channel full, closing".
async fn relay_mux(
    client: &mut TcpStream,
    mux_stream: xr_proto::mux::MuxStream,
    idle_timeout: Duration,
    max_lifetime: Duration,
) -> Result<(), RelayError> {
    let (mut cr, mut cw) = client.split();
    let (mut mux_r, mut mux_w) = mux_stream.split();

    // Each direction tags its own errors so the caller can tell whether the
    // LAN side or the tunnel side died. Without this distinction a perfectly
    // normal browser-tab close shows up as "Tunnel to X failed: Connection
    // reset by peer" and triggers a pointless direct fallback on a dead
    // local socket.
    let upload = async {
        let mut buf = vec![0u8; 8192];
        loop {
            match tokio::time::timeout(idle_timeout, cr.read(&mut buf)).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) => mux_w.send(&buf[..n]).await.map_err(RelayError::Tunnel)?,
                Ok(Err(e)) => return Err(RelayError::LocalClient(e)),
            }
        }
        mux_w.close().await.map_err(RelayError::Tunnel)?;
        Ok::<(), RelayError>(())
    };

    let download = async {
        loop {
            match mux_r.recv().await {
                Some(d) if !d.is_empty() => {
                    cw.write_all(&d).await.map_err(RelayError::LocalClient)?
                }
                _ => break,
            }
        }
        Ok::<(), RelayError>(())
    };

    let combined = async {
        tokio::select! {
            r = upload => r,
            r = download => r,
        }
    };

    match tokio::time::timeout(max_lifetime, combined).await {
        Ok(r) => r,
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use xr_proto::config::{RoutingConfig, RoutingRule};

    #[test]
    fn web_port_trusts_routing_rules() {
        assert_eq!(verdict_action(true, false, Action::Proxy), Action::Proxy);
        assert_eq!(verdict_action(true, false, Action::Direct), Action::Direct);
        assert_eq!(verdict_action(true, false, Action::Block), Action::Block);
    }

    #[test]
    fn off_web_port_tls_first_byte_wins_over_rules() {
        // Эвристика XR-292: вне web-портов первый байт 0x16 это Proxy, даже
        // если правила по SNI сказали Direct.
        assert_eq!(verdict_action(false, true, Action::Direct), Action::Proxy);
    }

    #[test]
    fn off_web_port_silence_and_plain_bytes_go_direct() {
        assert_eq!(verdict_action(false, false, Action::Proxy), Action::Direct);
        assert_eq!(verdict_action(false, false, Action::Block), Action::Direct);
    }

    #[test]
    fn device_policy_sees_sni_only_on_web_ports() {
        // XR-310: вне web-портов SNI это чужое имя из маскарада под TLS,
        // доменное правило устройства на нём совпадать не обязано. Решение
        // остаётся за default_exit и ip_ranges.
        assert_eq!(policy_sni(true, Some("ssl.gstatic.com")), Some("ssl.gstatic.com"));
        assert_eq!(policy_sni(false, Some("ssl.gstatic.com")), None);
        assert_eq!(policy_sni(true, None), None);
    }

    /// ClientHello с SNI и расширением перед ним: имя лежит дальше первого
    /// сегмента, ровно как у браузера с постквантовым key_share.
    fn client_hello(hostname: &str, key_share_len: usize) -> Vec<u8> {
        let host = hostname.as_bytes();

        let mut sni_ext = Vec::new();
        sni_ext.extend_from_slice(&((host.len() + 3) as u16).to_be_bytes()); // list len
        sni_ext.push(0x00); // host_name
        sni_ext.extend_from_slice(&(host.len() as u16).to_be_bytes());
        sni_ext.extend_from_slice(host);

        let mut extensions = Vec::new();
        extensions.extend_from_slice(&0x0033u16.to_be_bytes()); // key_share
        extensions.extend_from_slice(&(key_share_len as u16).to_be_bytes());
        extensions.extend(std::iter::repeat(0xa5).take(key_share_len));
        extensions.extend_from_slice(&0u16.to_be_bytes()); // server_name
        extensions.extend_from_slice(&(sni_ext.len() as u16).to_be_bytes());
        extensions.extend_from_slice(&sni_ext);

        let mut body = Vec::new();
        body.extend_from_slice(&[0x03, 0x03]); // TLS 1.2
        body.extend_from_slice(&[0u8; 32]); // random
        body.push(0); // session id len
        body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // cipher suites
        body.extend_from_slice(&[0x01, 0x00]); // compression
        body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
        body.extend_from_slice(&extensions);

        let mut hs = vec![0x01]; // ClientHello
        let len = body.len();
        hs.extend_from_slice(&[(len >> 16) as u8, (len >> 8) as u8, len as u8]);
        hs.extend_from_slice(&body);

        let mut record = vec![0x16, 0x03, 0x01];
        record.extend_from_slice(&(hs.len() as u16).to_be_bytes());
        record.extend_from_slice(&hs);
        record
    }

    /// Поднимает пару сокетов и отдаёт принятое соединение вместе с концом,
    /// в который пишет «браузер».
    async fn socket_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let writer = TcpStream::connect(addr).await.unwrap();
        let (accepted, _) = listener.accept().await.unwrap();
        (accepted, writer)
    }

    /// ClientHello приехал двумя сегментами: на первом имени нет, и без
    /// дочитывания соединение ушло бы маршрутом по умолчанию. Рекорд взят
    /// длиннее стартового буфера, чтобы под него пришлось расти.
    #[tokio::test]
    async fn peek_gathers_fragmented_client_hello() {
        let hello = client_hello("news.example.org", 6000);
        let (client, writer) = socket_pair().await;

        let head = hello[..1400].to_vec();
        let tail = hello[1400..].to_vec();
        // Хвост уходит только после первого подглядывания, иначе тест зависел
        // бы от того, кто из задач успел раньше.
        let (send_tail, tail_wanted) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let mut writer = writer;
            writer.write_all(&head).await.unwrap();
            tail_wanted.await.unwrap();
            writer.write_all(&tail).await.unwrap();
            // Держим сокет открытым, чтобы дочитывание работало на живом
            // соединении, а не на закрытом.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let mut buf = vec![0u8; 4096];
        let first = client.peek(&mut buf).await.unwrap();
        assert!(sni::extract_sni(&buf[..first]).is_none(), "первый сегмент имени не содержит");
        send_tail.send(()).unwrap();

        let n = peek_client_hello(&client, &mut buf, first).await.unwrap();
        assert_eq!(n, hello.len());
        assert_eq!(
            sni::extract_sni(&buf[..n]).as_deref(),
            Some("news.example.org"),
        );
    }

    /// Байты остаются в сокете: релей после подглядывания читает их сам.
    #[tokio::test]
    async fn peek_does_not_consume_bytes() {
        let hello = client_hello("news.example.org", 6000);
        let (mut client, writer) = socket_pair().await;

        let payload = hello.clone();
        tokio::spawn(async move {
            let mut writer = writer;
            writer.write_all(&payload).await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let mut buf = vec![0u8; 4096];
        let first = client.peek(&mut buf).await.unwrap();
        let n = peek_client_hello(&client, &mut buf, first).await.unwrap();
        assert_eq!(n, hello.len());

        let mut got = vec![0u8; hello.len()];
        client.read_exact(&mut got).await.unwrap();
        assert_eq!(got, hello);
    }

    /// Сырой TCP-протокол за ожидание платить не должен: не-handshake первый
    /// байт выводит из цикла сразу.
    #[tokio::test]
    async fn peek_does_not_wait_for_non_tls() {
        let (client, writer) = socket_pair().await;
        tokio::spawn(async move {
            let mut writer = writer;
            writer.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let mut buf = vec![0u8; 4096];
        let first = client.peek(&mut buf).await.unwrap();
        let started = std::time::Instant::now();
        let n = peek_client_hello(&client, &mut buf, first).await.unwrap();
        assert_eq!(n, first);
        assert!(started.elapsed() < SNI_FRAGMENT_WAIT, "ждали {:?}", started.elapsed());
    }

    /// Хвост так и не пришёл: ждём не дольше своего предела и отдаём то, что
    /// собралось, вместо бесконечного ожидания.
    #[tokio::test]
    async fn peek_gives_up_on_missing_tail() {
        let hello = client_hello("news.example.org", 6000);
        let (client, writer) = socket_pair().await;

        let head = hello[..1400].to_vec();
        tokio::spawn(async move {
            let mut writer = writer;
            writer.write_all(&head).await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let mut buf = vec![0u8; 4096];
        let first = client.peek(&mut buf).await.unwrap();
        let started = std::time::Instant::now();
        // Свой предел досмотр обязан соблюдать сам, поэтому снаружи стоит
        // заведомо больший: без него потерянный предел вешал бы прогон.
        let n = tokio::time::timeout(
            SNI_FRAGMENT_WAIT * 4,
            peek_client_hello(&client, &mut buf, first),
        )
        .await
        .expect("досмотр не уложился в свой предел")
        .unwrap();
        assert_eq!(n, 1400);
        assert!(sni::extract_sni(&buf[..n]).is_none());
        assert!(started.elapsed() < SNI_FRAGMENT_WAIT * 3, "ждали {:?}", started.elapsed());
    }

    /// Держит сокет открытым и молчит: peek упирается в таймаут, а не в EOF.
    /// Возвращённая задача доживает до конца теста на своей паузе.
    fn hold_silent(writer: TcpStream) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let _silent = writer;
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
    }

    /// Server-first протокол (VNC, SMTP, MySQL, IRC): клиент молчит и ждёт
    /// баннер сервера. На порту вне 80/443 маршрут обязан стать Direct за
    /// короткое время, а не висеть до дропа. Внешний потолок меньше
    /// web-портового ожидания: десяти секунд прежнего поведения тест не
    /// переживает.
    #[tokio::test]
    async fn silent_client_off_web_port_routes_direct() {
        let (client, writer) = socket_pair().await;
        let _hold = hold_silent(writer);

        let orig_dst: SocketAddr = "203.0.113.7:5900".parse().unwrap();
        let router = router_proxying(&["youtube.com"]);
        let ip = orig_dst.ip();
        let verdict = tokio::time::timeout(
            Duration::from_secs(2),
            decide_route(&client, orig_dst, |sni| router.resolve(sni, ip)),
        )
        .await
        .expect("короткий peek не уложился в две секунды")
        .unwrap();

        assert_eq!(verdict, PeekVerdict::Route(Action::Direct, None));
    }

    /// Первый байт 0x16 на нестандартном порту уезжает в Proxy: обфускация
    /// под TLS (MTProto, DoT) обязана попадать в короткое окно ожидания.
    /// Решение роутера при этом не спрашивается: SNI вне web-портов фейковый.
    #[tokio::test]
    async fn tls_first_byte_off_web_port_routes_proxy() {
        let (client, writer) = socket_pair().await;
        let hello = client_hello("tube.example.org", 100);
        tokio::spawn(async move {
            let mut writer = writer;
            writer.write_all(&hello).await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let orig_dst: SocketAddr = "203.0.113.7:5277".parse().unwrap();
        let router = router_proxying(&["youtube.com"]);
        let ip = orig_dst.ip();
        let verdict = decide_route(&client, orig_dst, |sni| router.resolve(sni, ip))
            .await
            .unwrap();

        assert_eq!(
            verdict,
            PeekVerdict::Route(Action::Proxy, Some("tube.example.org".into()))
        );
    }

    /// На web-портах маршрут по-прежнему решает SNI из ClientHello.
    #[tokio::test]
    async fn web_port_routes_by_sni() {
        let (client, writer) = socket_pair().await;
        let hello = client_hello("youtube.com", 100);
        tokio::spawn(async move {
            let mut writer = writer;
            writer.write_all(&hello).await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let orig_dst: SocketAddr = "203.0.113.7:443".parse().unwrap();
        let router = router_proxying(&["youtube.com"]);
        let ip = orig_dst.ip();
        let verdict = decide_route(&client, orig_dst, |sni| router.resolve(sni, ip))
            .await
            .unwrap();

        assert_eq!(
            verdict,
            PeekVerdict::Route(Action::Proxy, Some("youtube.com".into()))
        );
    }

    /// Молчащий клиент на web-порту рвётся: HTTP обязан говорить первым, и
    /// тихое соединение там мёртвое. Время паущено, десятисекундное ожидание
    /// срабатывает мгновенно; писатель держится дольше него, чтобы peek
    /// упёрся в таймаут, а не в EOF.
    #[tokio::test(start_paused = true)]
    async fn silent_client_on_web_port_is_dropped() {
        let (client, writer) = socket_pair().await;
        tokio::spawn(async move {
            let _silent = writer;
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        let orig_dst: SocketAddr = "203.0.113.7:443".parse().unwrap();
        let router = router_proxying(&[]);
        let ip = orig_dst.ip();
        let verdict = decide_route(&client, orig_dst, |sni| router.resolve(sni, ip))
            .await
            .unwrap();

        assert_eq!(verdict, PeekVerdict::Dead);
    }

    fn router_proxying(domains: &[&str]) -> Router {
        let cfg = RoutingConfig {
            default_action: "direct".into(),
            rules: vec![RoutingRule {
                name: None,
                action: "proxy".into(),
                domains: domains.iter().map(|s| s.to_string()).collect(),
                ip_ranges: vec![],
                geoip: vec![],
                outbound: None,
            }],
        };
        Router::new(&cfg, None)
    }

    /// Hot-swap должен менять решение `resolve()` для новых запросов.
    /// Без этого теста можно случайно сломать RwLock<Arc<Router>> семантику
    /// (напр. забыть `*guard = ...` и получить тихий no-op).
    #[test]
    fn hot_swap_changes_router_decision() {
        let initial = router_proxying(&["youtube.com"]);
        let slot: RwLock<Arc<Router>> = RwLock::new(Arc::new(initial));

        // До swap'а: youtube → Proxy, ya.ru → Direct.
        let ip: std::net::IpAddr = "1.2.3.4".parse().unwrap();
        assert_eq!(slot.read().unwrap().resolve(Some("youtube.com"), ip), Action::Proxy);
        assert_eq!(slot.read().unwrap().resolve(Some("ya.ru"), ip), Action::Direct);

        // Swap: теперь в списке только ya.ru.
        let replacement = router_proxying(&["ya.ru"]);
        *slot.write().unwrap() = Arc::new(replacement);

        // После swap'а: youtube → Direct (выпал из правил), ya.ru → Proxy.
        assert_eq!(slot.read().unwrap().resolve(Some("youtube.com"), ip), Action::Direct);
        assert_eq!(slot.read().unwrap().resolve(Some("ya.ru"), ip), Action::Proxy);
    }

    /// Active Arc<Router>, полученный ДО swap'а, должен продолжать видеть
    /// старые правила — это гарантирует, что уже установленные TCP-сессии
    /// не "меняют маршрут под ногами".
    #[test]
    fn hot_swap_leaves_snapshot_readers_untouched() {
        let slot: RwLock<Arc<Router>> = RwLock::new(Arc::new(router_proxying(&["youtube.com"])));

        // Читатель взял снимок Router'а до swap'а.
        let snapshot: Arc<Router> = slot.read().unwrap().clone();

        let ip: std::net::IpAddr = "1.2.3.4".parse().unwrap();
        assert_eq!(snapshot.resolve(Some("youtube.com"), ip), Action::Proxy);

        // Swap на полностью другой набор.
        *slot.write().unwrap() = Arc::new(router_proxying(&["ya.ru"]));

        // Старый snapshot остался с прежним решением.
        assert_eq!(snapshot.resolve(Some("youtube.com"), ip), Action::Proxy);
        assert_eq!(snapshot.resolve(Some("ya.ru"), ip), Action::Direct);
    }
}
