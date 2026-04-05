/// Transparent proxy core: accept connections, extract SNI, route, tunnel.
use xr_proto::mux_pool::MuxPool;
use xr_proto::routing::{Action, Router};
use xr_proto::sni;
use xr_proto::tunnel;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Duration;
use xr_proto::protocol::{Codec, TargetAddr};

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
    pub router: Router,
    pub codec: Codec,
    pub server_addr: SocketAddr,
    pub on_server_down: Action,
    pub listen_port: u16,
    pub mux_pool: Arc<MuxPool>,
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
    let action = state.router.resolve(sni_name.as_deref(), dest_ip);

    tracing::info!(
        "{} -> {} [SNI: {}] => {:?}",
        client_addr, orig_dst, sni_display, action
    );

    let idle_timeout = Duration::from_secs(300);
    let max_lifetime = Duration::from_secs(3600);

    match action {
        Action::Direct => {
            // Connect directly to the original destination
            let mut target = TcpStream::connect(orig_dst).await?;
            set_keepalive(&target);
            tunnel::relay_bidirectional(&mut client, &mut target, max_lifetime).await
        }
        Action::Proxy => {
            // Connect through the obfuscated tunnel
            match tunnel_connection(&mut client, orig_dst, sni_name.as_deref(), &state, idle_timeout, max_lifetime).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    tracing::warn!("Tunnel to {} failed: {}, fallback={:?}",
                        orig_dst, e, state.on_server_down);
                    if state.on_server_down == Action::Direct {
                        // Fallback: try direct connection
                        let mut target = TcpStream::connect(orig_dst).await?;
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

// ── Tunnel through server ────────────────────────────────────────────

async fn tunnel_connection(
    client: &mut TcpStream,
    orig_dst: SocketAddr,
    sni_name: Option<&str>,
    state: &ProxyState,
    idle_timeout: Duration,
    max_lifetime: Duration,
) -> io::Result<()> {
    let target_addr = if let Some(domain) = sni_name {
        TargetAddr::Domain(domain.to_string(), orig_dst.port())
    } else {
        TargetAddr::Ip(orig_dst)
    };

    // Try multiplexed connection first (single persistent TCP, no new handshake).
    if !state.mux_pool.is_legacy() {
        match state.mux_pool.open_stream(&target_addr).await {
            Ok(mux_stream) => {
                return relay_mux(client, mux_stream, idle_timeout, max_lifetime).await;
            }
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                tracing::info!("Server doesn't support mux, using legacy");
            }
            Err(e) => {
                tracing::debug!("Mux open_stream failed: {}, trying legacy", e);
            }
        }
    }

    // Legacy: per-request TCP with retry.
    let mut server = {
        let mut last_err = None;
        let mut connected = None;
        for _ in 0..3 {
            match tunnel::connect_to_server(&state.server_addr).await {
                Ok(mut s) => {
                    set_keepalive(&s);
                    match tunnel::handshake(&mut s, &target_addr, &state.codec).await {
                        Ok(()) => { connected = Some(s); break; }
                        Err(e) => { last_err = Some(e); }
                    }
                }
                Err(e) => { last_err = Some(e); }
            }
        }
        connected.ok_or_else(|| last_err.unwrap())?
    };

    tunnel::relay_obfuscated(client, &mut server, &state.codec, idle_timeout, max_lifetime).await
}

/// Relay data between a local client and a MuxStream.
async fn relay_mux(
    client: &mut TcpStream,
    mut mux_stream: xr_proto::mux::MuxStream,
    idle_timeout: Duration,
    max_lifetime: Duration,
) -> io::Result<()> {
    let (mut cr, mut cw) = client.split();

    let upload = async {
        let mut buf = vec![0u8; 8192];
        loop {
            tokio::select! {
                result = tokio::time::timeout(idle_timeout, cr.read(&mut buf)) => {
                    match result {
                        Ok(Ok(0)) | Err(_) => break,
                        Ok(Ok(n)) => mux_stream.send(&buf[..n]).await?,
                        Ok(Err(e)) => return Err(e),
                    }
                }
                data = mux_stream.recv() => {
                    match data {
                        Some(d) if !d.is_empty() => cw.write_all(&d).await?,
                        _ => break,
                    }
                }
            }
        }
        mux_stream.close().await?;
        Ok::<(), io::Error>(())
    };

    match tokio::time::timeout(max_lifetime, upload).await {
        Ok(r) => r,
        Err(_) => Ok(()),
    }
}
