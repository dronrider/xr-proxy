/// Transparent proxy core: accept connections, extract SNI, route, tunnel.
use xr_proto::routing::{Action, Router};
use xr_proto::server_pool::ServerPool;
use xr_proto::sni;
use xr_proto::tunnel;
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
    /// write случается раз в `refresh_interval_secs` (5 мин default).
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

    loop {
        let (client_stream, client_addr) = listener.accept().await?;
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
    }
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

    // Peek at first bytes for SNI extraction (with timeout — don't hang on dead connections)
    let mut peek_buf = vec![0u8; 4096];
    let n = match tokio::time::timeout(Duration::from_secs(10), client.peek(&mut peek_buf)).await {
        Ok(result) => result?,
        Err(_) => {
            tracing::debug!("Peek timeout from {}, dropping", client_addr);
            return Ok(());
        }
    };
    let sni_name = sni::extract_sni(&peek_buf[..n]);

    let sni_display = sni_name.as_deref().unwrap_or("-");
    // Один short-lived read-lock: resolve() возвращает Action по value,
    // поэтому guard живёт ровно длину этого statement.
    let resolved_action = state.router.read().unwrap().resolve(sni_name.as_deref(), dest_ip);

    // SNI-роутинг доверяем только на стандартных web-портах (80/443). На любом
    // нестандартном порту SNI скорее всего fake (Telegram MTProto маскирует
    // обфусцированный поток под TLS-handshake с self.events.data.microsoft.com,
    // ssl.gstatic.com и подобными доменами для обхода DPI). Решение по такому
    // SNI = заведомо неправильный routing → direct → провайдерский RST.
    //
    // Для non-80/443 портов смотрим на сам первый байт: 0x16 = TLS handshake
    // ContentType. Если это TLS — почти наверняка обфусцированный/маскированный
    // протокол (Telegram MTProto на 5277/5993, DoT на 853 и т.п.) → Proxy.
    // Если нет — это сырой TCP-протокол (BitTorrent peer handshake начинается с
    // 0x13 + "BitTorrent protocol", IRC и т.д.). Проксировать его бессмысленно
    // (IP клиента всё равно засветится в peer-listing), а вред огромный:
    // BitTorrent открывает десятки одновременных Connect'ов к мёртвым/firewalled
    // пирам, забивает mux writer-канал и target-семафор xr-server'а
    // (max_connections=256), из-за чего ConnectAck для легитимного TLS-трафика
    // (YouTube, шортсы) timeout'ит и видео фризит.
    let looks_like_tls = peek_buf.get(0) == Some(&0x16);
    let action = match orig_dst.port() {
        80 | 443 => resolved_action,
        _ if looks_like_tls => Action::Proxy,
        _ => Action::Direct,
    };

    tracing::info!(
        "{} -> {} [SNI: {}] => {:?}",
        client_addr, orig_dst, sni_display, action
    );

    let idle_timeout = Duration::from_secs(300);
    let max_lifetime = Duration::from_secs(3600);

    // Hard cap on Direct TCP connect — без него default Linux SYN retry
    // выкручивает на ~130 секунд, и при BitTorrent-нагрузке (десятки
    // dial/сек к мёртвым/firewalled пирам) сокеты накапливаются: видели
    // 845 open fd при единственном активном LAN-стриме, что топит tokio
    // runtime и тормозит легитимный YouTube-трафик.
    let direct_connect_timeout = Duration::from_secs(5);

    match action {
        Action::Direct => {
            // Connect directly to the original destination
            let mut target = match tokio::time::timeout(
                direct_connect_timeout,
                TcpStream::connect(orig_dst),
            ).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    tracing::debug!("direct connect to {} timed out", orig_dst);
                    return Ok(());
                }
            };
            set_keepalive(&target);
            tunnel::relay_bidirectional(&mut client, &mut target, max_lifetime).await
        }
        Action::Proxy => {
            // Connect through the obfuscated tunnel.
            //
            // We distinguish errors by side: if the LAN client closed first
            // (RST/EPIPE on read/write), there is nothing to fall back to —
            // the local socket is dead. Only tunnel-side failures justify the
            // direct fallback.
            match tunnel_connection(&mut client, orig_dst, sni_name.as_deref(), &state, idle_timeout, max_lifetime).await {
                Ok(()) => Ok(()),
                Err(RelayError::LocalClient(e)) => {
                    tracing::debug!("LAN client closed early ({} -> {}): {}", client_addr, orig_dst, e);
                    Ok(())
                }
                Err(RelayError::Tunnel(e)) => {
                    tracing::warn!("Tunnel to {} failed: {}, fallback={:?}",
                        orig_dst, e, state.on_server_down);
                    if state.on_server_down == Action::Direct {
                        // Fallback: try direct connection.
                        let mut target = match tokio::time::timeout(
                            direct_connect_timeout,
                            TcpStream::connect(orig_dst),
                        ).await {
                            Ok(Ok(s)) => s,
                            Ok(Err(e)) => return Err(e),
                            Err(_) => {
                                tracing::debug!("direct fallback to {} timed out", orig_dst);
                                return Ok(());
                            }
                        };
                        set_keepalive(&target);
                        tunnel::relay_bidirectional(&mut client, &mut target, max_lifetime).await
                    } else {
                        Err(e)
                    }
                }
            }
        }
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

// ── Tunnel through server ────────────────────────────────────────────

async fn tunnel_connection(
    client: &mut TcpStream,
    orig_dst: SocketAddr,
    sni_name: Option<&str>,
    state: &ProxyState,
    idle_timeout: Duration,
    max_lifetime: Duration,
) -> Result<(), RelayError> {
    let target_addr = if let Some(domain) = sni_name {
        TargetAddr::Domain(domain.to_string(), orig_dst.port())
    } else {
        TargetAddr::Ip(orig_dst)
    };

    // Failure to open a mux stream is a tunnel-side problem (mux dead,
    // ConnectAck timeout, etc.), so the direct fallback is appropriate. Err
    // от server_pool означает, что исчерпан весь пул серверов, не один VPS.
    let mux_stream = state
        .server_pool
        .open_stream(&target_addr)
        .await
        .map_err(RelayError::Tunnel)?;
    relay_mux(client, mux_stream, idle_timeout, max_lifetime).await
}

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
    use xr_proto::config::{RoutingConfig, RoutingRule};

    fn router_proxying(domains: &[&str]) -> Router {
        let cfg = RoutingConfig {
            default_action: "direct".into(),
            rules: vec![RoutingRule {
                action: "proxy".into(),
                domains: domains.iter().map(|s| s.to_string()).collect(),
                ip_ranges: vec![],
                geoip: vec![],
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
