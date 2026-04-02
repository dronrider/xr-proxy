//! TCP session manager: bridges smoltcp virtual sockets with real network connections.
//!
//! For each new TCP connection from the TUN:
//! 1. Look up domain from Fake DNS (by dest IP)
//! 2. Apply routing rules → Proxy or Direct
//! 3. Establish outbound connection (tunnel to xr-server or direct)
//! 4. Relay data between smoltcp socket and outbound connection

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use smoltcp::iface::SocketHandle;
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

/// Information about an active TCP session.
struct TcpSession {
    smol_handle: SocketHandle,
    /// Domain from Fake DNS lookup (if available).
    domain: Option<String>,
    /// Original destination (real IP:port, not fake).
    orig_dst: SocketAddr,
    /// Routing decision.
    action: Action,
}

/// Shared context for session management.
pub struct SessionContext {
    pub router: Router,
    pub codec: Codec,
    pub server_addr: SocketAddr,
    pub fake_dns: Arc<FakeDns>,
    pub stats: Stats,
    pub on_server_down: Action,
}

/// Manages all active TCP sessions.
pub struct SessionManager {
    sessions: HashMap<TcpSessionKey, TcpSession>,
    ctx: Arc<SessionContext>,
}

impl SessionManager {
    pub fn new(ctx: Arc<SessionContext>) -> Self {
        Self {
            sessions: HashMap::new(),
            ctx,
        }
    }

    /// Register a new TCP connection from the TUN.
    ///
    /// `smol_handle` is the smoltcp socket handle.
    /// `src_addr` / `dst_addr` are from the IP/TCP headers.
    pub fn new_session(
        &mut self,
        smol_handle: SocketHandle,
        src_addr: SocketAddr,
        dst_addr: SocketAddr,
    ) {
        let key = TcpSessionKey { src_addr, dst_addr };

        // Look up domain from fake DNS.
        let (domain, orig_dst) = if let IpAddr::V4(v4) = dst_addr.ip() {
            if FakeDns::is_fake_ip(v4) {
                if let Some(domain) = self.ctx.fake_dns.lookup(v4) {
                    // For proxy: server resolves the domain.
                    // For direct: we resolve it ourselves later.
                    (Some(domain), dst_addr)
                } else {
                    (None, dst_addr)
                }
            } else {
                (None, dst_addr)
            }
        } else {
            (None, dst_addr)
        };

        // Apply routing.
        let action = self.ctx.router.resolve(domain.as_deref(), dst_addr.ip());

        tracing::info!(
            "New session: {} -> {} [domain: {}] => {:?}",
            src_addr,
            dst_addr,
            domain.as_deref().unwrap_or("-"),
            action,
        );

        self.sessions.insert(
            key,
            TcpSession {
                smol_handle,
                domain,
                orig_dst,
                action,
            },
        );

        self.ctx.stats.connection_opened();
    }

    /// Remove a session (connection closed).
    pub fn remove_session(&mut self, key: &TcpSessionKey) -> Option<SocketHandle> {
        if let Some(session) = self.sessions.remove(key) {
            self.ctx.stats.connection_closed();
            Some(session.smol_handle)
        } else {
            None
        }
    }

    /// Get all active session keys.
    pub fn session_keys(&self) -> Vec<TcpSessionKey> {
        self.sessions.keys().cloned().collect()
    }

    /// Check if a session exists.
    pub fn has_session(&self, key: &TcpSessionKey) -> bool {
        self.sessions.contains_key(key)
    }

    /// Get the smoltcp handle for a session.
    pub fn smol_handle(&self, key: &TcpSessionKey) -> Option<SocketHandle> {
        self.sessions.get(key).map(|s| s.smol_handle)
    }

    /// Get session count.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Get the routing action for a session.
    pub fn session_action(&self, key: &TcpSessionKey) -> Option<Action> {
        self.sessions.get(key).map(|s| s.action)
    }

    /// Get the domain for a session.
    pub fn session_domain(&self, key: &TcpSessionKey) -> Option<String> {
        self.sessions
            .get(key)
            .and_then(|s| s.domain.clone())
    }

    /// Get the target address for establishing outbound connection.
    pub fn target_addr(&self, key: &TcpSessionKey) -> Option<TargetAddr> {
        let session = self.sessions.get(key)?;
        Some(if let Some(ref domain) = session.domain {
            TargetAddr::Domain(domain.clone(), session.orig_dst.port())
        } else {
            TargetAddr::Ip(session.orig_dst)
        })
    }
}

/// Spawn a relay task for a single session: smoltcp data ↔ outbound connection.
///
/// This is called from the engine's event loop when a session is ready to relay.
/// It takes ownership of the outbound connection.
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

    match action {
        Action::Proxy => {
            relay_via_proxy(ctx, target_addr, data_rx, data_tx).await
        }
        Action::Direct => {
            relay_direct(key.dst_addr, domain.as_deref(), data_rx, data_tx).await
        }
    }
}

/// Relay through the xr-server tunnel.
async fn relay_via_proxy(
    ctx: Arc<SessionContext>,
    target_addr: TargetAddr,
    mut data_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    data_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
) -> io::Result<()> {
    // Connect to xr-server.
    let mut server = tunnel::connect_with_retry(&ctx.server_addr, 3).await?;

    // Handshake.
    tunnel::handshake(&mut server, &target_addr, &ctx.codec).await?;

    let codec_up = ctx.codec.clone();
    let codec_down = ctx.codec.clone();
    let stats_up = ctx.stats.clone();
    let stats_down = ctx.stats.clone();

    let (mut sr, mut sw) = server.into_split();

    // Upload: data from smoltcp → obfuscate → server.
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

    // Download: server → deobfuscate → data to smoltcp.
    let download = async move {
        let mut buf = vec![0u8; 65536 + 256];
        let mut filled = 0;
        loop {
            let n = sr.read(&mut buf[filled..]).await?;
            if n == 0 {
                break;
            }
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
    dst: SocketAddr,
    domain: Option<&str>,
    mut data_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    data_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
) -> io::Result<()> {
    // For fake IPs, resolve domain to real IP.
    let real_dst = if let IpAddr::V4(v4) = dst.ip() {
        if FakeDns::is_fake_ip(v4) {
            if let Some(domain) = domain {
                // Resolve domain.
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

    let mut target = TcpStream::connect(real_dst).await?;
    let (mut tr, mut tw) = target.split();

    // Upload: smoltcp data → target.
    let upload = async move {
        while let Some(data) = data_rx.recv().await {
            if data.is_empty() {
                break;
            }
            tw.write_all(&data).await?;
        }
        Ok::<(), io::Error>(())
    };

    // Download: target → smoltcp.
    let download = async move {
        let mut buf = vec![0u8; 8192];
        loop {
            let n = tr.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            if data_tx.send(buf[..n].to_vec()).await.is_err() {
                break;
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
