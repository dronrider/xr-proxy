//! TCP session manager: bridges smoltcp virtual sockets with real network connections.
//!
//! For each new TCP connection from the TUN:
//! 1. Look up domain from Fake DNS (by dest IP)
//! 2. Apply routing rules → Proxy or Direct
//! 3. Establish PROTECTED outbound connection (bypasses VPN)
//! 4. Relay data between smoltcp socket and outbound connection

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::os::fd::AsRawFd;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::Duration;

use xr_proto::protocol::{Codec, Command, TargetAddr};
use xr_proto::routing::{Action, Router};
use xr_proto::tunnel;

use crate::dns::FakeDns;
use crate::stats::Stats;

/// Key for tracking a TCP connection from the TUN side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TcpSessionKey {
    pub src_addr: SocketAddr,
    pub dst_addr: SocketAddr,
}

/// Callback to protect a socket fd from being routed through VPN.
/// On Android, this calls VpnService.protect(fd).
pub type ProtectSocketFn = Arc<dyn Fn(i32) -> bool + Send + Sync>;

/// Shared context for session management.
pub struct SessionContext {
    pub router: Router,
    pub codec: Codec,
    pub server_addr: SocketAddr,
    pub fake_dns: Arc<FakeDns>,
    pub stats: Stats,
    pub on_server_down: Action,
    pub protect_socket: ProtectSocketFn,
}

/// Create a TCP connection that bypasses the VPN tunnel.
///
/// Uses tokio::net::TcpSocket with protect(fd) before connect.
async fn connect_protected(addr: SocketAddr, protect: &ProtectSocketFn) -> io::Result<TcpStream> {
    let socket = match addr {
        SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4()?,
        SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6()?,
    };

    // Protect BEFORE connecting — critical on Android.
    let fd = socket.as_raw_fd();
    let protected = protect(fd);
    if !protected {
        return Err(io::Error::new(io::ErrorKind::Other,
            format!("protect(fd={}) failed for {}", fd, addr)));
    }

    tokio::time::timeout(
        Duration::from_secs(10),
        socket.connect(addr),
    ).await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut,
        format!("connect timeout to {}", addr)))?
}

/// Connect to server, using protected socket. Single attempt — retries
/// amplify load on overloaded server and make things worse.
async fn connect_server_protected(
    addr: &SocketAddr,
    protect: &ProtectSocketFn,
) -> io::Result<TcpStream> {
    connect_protected(*addr, protect).await
}

/// Check if an IP is a private/non-routable address that should never be proxied.
fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()          // 10.x, 172.16-31.x, 192.168.x
            || v4.is_loopback()      // 127.x
            || v4.is_link_local()    // 169.254.x
        }
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// Spawn a relay task with a pre-resolved domain.
pub async fn relay_session_with_domain(
    ctx: Arc<SessionContext>,
    key: TcpSessionKey,
    domain: Option<String>,
    data_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    data_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    waker: Arc<Notify>,
) -> io::Result<()> {
    // Never proxy fake IPs without a domain — server can't connect to 198.18.x.x.
    if domain.is_none() {
        if let IpAddr::V4(v4) = key.dst_addr.ip() {
            if FakeDns::is_fake_ip(v4) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("fake IP {} without domain, dropping", v4),
                ));
            }
        }
    }

    // Never proxy private/local IPs — they're not reachable from server.
    if is_private_ip(key.dst_addr.ip()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("private IP {}, dropping", key.dst_addr.ip()),
        ));
    }

    let action = ctx.router.resolve(domain.as_deref(), key.dst_addr.ip());

    let target_addr = if let Some(ref domain) = domain {
        TargetAddr::Domain(domain.clone(), key.dst_addr.port())
    } else {
        TargetAddr::Ip(key.dst_addr)
    };

    match action {
        Action::Proxy => {
            ctx.stats.set_debug(format!(
                "proxy: {} [{}] -> srv {}",
                key.dst_addr, domain.as_deref().unwrap_or("-"), ctx.server_addr,
            ));
            relay_via_proxy(ctx, target_addr, data_rx, data_tx, waker).await
        }
        Action::Direct => {
            ctx.stats.set_debug(format!(
                "direct: {} [{}]",
                key.dst_addr, domain.as_deref().unwrap_or("-"),
            ));
            relay_direct(ctx, key.dst_addr, domain.as_deref(), data_rx, data_tx, waker).await
        }
    }
}

/// Relay through the xr-server tunnel.
async fn relay_via_proxy(
    ctx: Arc<SessionContext>,
    target_addr: TargetAddr,
    mut data_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    data_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    waker: Arc<Notify>,
) -> io::Result<()> {
    // Connect to xr-server with PROTECTED socket (single attempt).
    let mut server = connect_server_protected(&ctx.server_addr, &ctx.protect_socket).await
        .map_err(|e| {
            ctx.stats.add_log(&format!("srv connect fail: {}", e));
            e
        })?;

    ctx.stats.add_log(&format!("srv connected, handshaking for {:?}", target_addr));

    // Handshake.
    tunnel::handshake(&mut server, &target_addr, &ctx.codec).await
        .map_err(|e| {
            ctx.stats.add_log(&format!("handshake fail: {}", e));
            e
        })?;

    ctx.stats.add_log(&format!("relay started for {:?}", target_addr));

    let codec_up = ctx.codec.clone();
    let codec_down = ctx.codec.clone();
    let stats_up = ctx.stats.clone();
    let stats_down = ctx.stats.clone();

    let (mut sr, mut sw) = server.into_split();

    let upload = async move {
        while let Some(data) = data_rx.recv().await {
            if data.is_empty() {
                let close = codec_up.encode_frame(Command::Close, &[])?;
                sw.write_all(&close).await?;
                break;
            }
            stats_up.add_bytes_up(data.len() as u64);
            let frame = codec_up.encode_frame(Command::Data, &data)?;
            sw.write_all(&frame).await?;
        }
        Ok::<(), io::Error>(())
    };

    let download = async move {
        let mut buf = vec![0u8; 65536 + 256];
        let mut filled = 0;
        loop {
            let n = sr.read(&mut buf[filled..]).await?;
            if n == 0 { break; }
            filled += n;

            loop {
                match codec_down.decode_frame(&buf[..filled])? {
                    Some((frame, consumed)) => {
                        match frame.command {
                            Command::Data => {
                                stats_down.add_bytes_down(frame.payload.len() as u64);
                                if data_tx.send(frame.payload).await.is_err() {
                                    return Ok(());
                                }
                                // Wake event loop to deliver data to smoltcp immediately.
                                waker.notify_one();
                            }
                            Command::Close => return Ok(()),
                            _ => {}
                        }
                        buf.copy_within(consumed..filled, 0);
                        filled -= consumed;
                    }
                    None => break,
                }
            }
        }
        Ok::<(), io::Error>(())
    };

    let result = tokio::time::timeout(Duration::from_secs(3600), async {
        tokio::select! {
            r = upload => r,
            r = download => r,
        }
    });

    match result.await {
        Ok(r) => r,
        Err(_) => Ok(()),
    }
}

/// Relay directly to the target.
async fn relay_direct(
    ctx: Arc<SessionContext>,
    dst: SocketAddr,
    domain: Option<&str>,
    mut data_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    data_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    waker: Arc<Notify>,
) -> io::Result<()> {
    // Resolve real destination for fake IPs.
    let real_dst = if let IpAddr::V4(v4) = dst.ip() {
        if FakeDns::is_fake_ip(v4) {
            if let Some(domain) = domain {
                let addr_str = format!("{}:{}", domain, dst.port());
                let resolved = tokio::net::lookup_host(&addr_str)
                    .await?
                    .next()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "DNS resolution failed"))?;
                resolved
            } else {
                return Err(io::Error::new(io::ErrorKind::Other, "fake IP without domain"));
            }
        } else {
            dst
        }
    } else {
        dst
    };

    // Connect with PROTECTED socket.
    let target = connect_protected(real_dst, &ctx.protect_socket).await?;
    let (mut tr, mut tw) = target.into_split();

    let upload = async move {
        while let Some(data) = data_rx.recv().await {
            if data.is_empty() { break; }
            tw.write_all(&data).await?;
        }
        Ok::<(), io::Error>(())
    };

    let download = async move {
        let mut buf = vec![0u8; 8192];
        loop {
            let n = tr.read(&mut buf).await?;
            if n == 0 { break; }
            if data_tx.send(buf[..n].to_vec()).await.is_err() { break; }
            // Wake event loop to deliver data to smoltcp immediately.
            waker.notify_one();
        }
        Ok::<(), io::Error>(())
    };

    let result = tokio::time::timeout(Duration::from_secs(3600), async {
        tokio::select! {
            r = upload => r,
            r = download => r,
        }
    });

    match result.await {
        Ok(r) => r,
        Err(_) => Ok(()),
    }
}
