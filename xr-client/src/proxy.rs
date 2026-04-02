/// Transparent proxy core: accept connections, extract SNI, route, tunnel.
use xr_proto::routing::{Action, Router};
use xr_proto::sni;
use xr_proto::tunnel;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
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
    // Connect to xr-proxy-server
    let mut server = tunnel::connect_with_retry(&state.server_addr, 3).await?;
    set_keepalive(&server);

    // Build connect payload with target address
    let target_addr = if let Some(domain) = sni_name {
        TargetAddr::Domain(domain.to_string(), orig_dst.port())
    } else {
        TargetAddr::Ip(orig_dst)
    };

    // Handshake: Connect → ConnectAck
    tunnel::handshake(&mut server, &target_addr, &state.codec).await?;

    // Relay data: client <-> obfuscated tunnel <-> server
    tunnel::relay_obfuscated(client, &mut server, &state.codec, idle_timeout, max_lifetime).await
}
