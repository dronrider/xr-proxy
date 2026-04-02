//! TCP session manager: bridges smoltcp virtual sockets with real network connections.
//!
//! For each new TCP connection from the TUN:
//! 1. Look up domain from Fake DNS (by dest IP)
//! 2. Apply routing rules → Proxy or Direct
//! 3. Establish PROTECTED outbound connection (bypasses VPN)
//! 4. Relay data between smoltcp socket and outbound connection

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
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
/// 1. Create raw socket via socket2
/// 2. Call protect_socket(fd) so Android VPN doesn't capture it
/// 3. Connect to target
/// 4. Convert to tokio TcpStream
async fn connect_protected(addr: SocketAddr, protect: &ProtectSocketFn) -> io::Result<TcpStream> {
    let domain = match addr {
        SocketAddr::V4(_) => socket2::Domain::IPV4,
        SocketAddr::V6(_) => socket2::Domain::IPV6,
    };

    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    socket.set_nonblocking(true)?;

    // Protect BEFORE connecting — this is critical on Android.
    let fd = socket.as_raw_fd();
    if !protect(fd) {
        tracing::warn!("Failed to protect socket fd={}", fd);
    }

    let std_stream: std::net::TcpStream = socket.into();
    let tokio_stream = TcpStream::from_std(std_stream)?;

    // Connect asynchronously.
    // Since socket is non-blocking, we need to use tokio's connect.
    // But we already have a TcpStream... we need to do the connect.
    // Actually, from_std on an unconnected socket won't work for connect.
    // Let's use a different approach.
    drop(tokio_stream);

    // Better approach: create socket, protect, then connect synchronously
    // in a blocking task.
    let protect_clone = protect.clone();
    tokio::task::spawn_blocking(move || -> io::Result<std::net::TcpStream> {
        let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;

        let raw_fd = {
            use std::os::fd::AsRawFd;
            socket.as_raw_fd()
        };
        if !protect_clone(raw_fd) {
            tracing::warn!("Failed to protect socket fd={}", raw_fd);
        }

        // Set connect timeout.
        socket.set_nonblocking(false)?;
        let sock_addr: socket2::SockAddr = addr.into();
        socket.connect_timeout(&sock_addr, std::time::Duration::from_secs(10))?;
        socket.set_nonblocking(true)?;

        Ok(socket.into())
    })
    .await
    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
    .and_then(|std_stream| TcpStream::from_std(std_stream))
}

/// Connect to server with retry, using protected sockets.
async fn connect_server_protected(
    addr: &SocketAddr,
    protect: &ProtectSocketFn,
    max_retries: u32,
) -> io::Result<TcpStream> {
    let mut delay = Duration::from_secs(1);

    for attempt in 0..=max_retries {
        match connect_protected(*addr, protect).await {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                if attempt == max_retries {
                    return Err(e);
                }
                tracing::warn!("Connect to server failed (attempt {}): {}", attempt + 1, e);
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(10));
            }
        }
    }
    unreachable!()
}

/// Spawn a relay task for a single session.
pub async fn relay_session(
    ctx: Arc<SessionContext>,
    key: TcpSessionKey,
    data_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    data_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
) -> io::Result<()> {
    let domain = ctx.fake_dns.lookup(match key.dst_addr.ip() {
        IpAddr::V4(v4) => v4,
        _ => return Err(io::Error::new(io::ErrorKind::Other, "IPv6 not supported yet")),
    });

    let action = ctx.router.resolve(domain.as_deref(), key.dst_addr.ip());

    let target_addr = if let Some(ref domain) = domain {
        TargetAddr::Domain(domain.clone(), key.dst_addr.port())
    } else {
        TargetAddr::Ip(key.dst_addr)
    };

    tracing::info!(
        "Relay: {} -> {} [{}] => {:?}",
        key.src_addr, key.dst_addr,
        domain.as_deref().unwrap_or("-"), action,
    );

    match action {
        Action::Proxy => relay_via_proxy(ctx, target_addr, data_rx, data_tx).await,
        Action::Direct => relay_direct(ctx, key.dst_addr, domain.as_deref(), data_rx, data_tx).await,
    }
}

/// Relay through the xr-server tunnel.
async fn relay_via_proxy(
    ctx: Arc<SessionContext>,
    target_addr: TargetAddr,
    mut data_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    data_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
) -> io::Result<()> {
    // Connect to xr-server with PROTECTED socket.
    let mut server = connect_server_protected(&ctx.server_addr, &ctx.protect_socket, 3).await?;

    // Handshake.
    tunnel::handshake(&mut server, &target_addr, &ctx.codec).await?;

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

// Re-export AsRawFd for use in connect_protected.
use std::os::fd::AsRawFd;
