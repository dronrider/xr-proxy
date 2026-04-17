//! TCP session manager: bridges smoltcp virtual sockets with real network connections.
//!
//! For each new TCP connection from the TUN:
//! 1. Look up domain from Fake DNS (by dest IP)
//! 2. Apply routing rules → Proxy or Direct
//! 3. Establish PROTECTED outbound connection (bypasses VPN)
//! 4. Relay data between smoltcp socket and outbound connection

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::AsRawFd;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::Duration;

use xr_proto::protocol::{Codec, TargetAddr};
use xr_proto::routing::{Action, Router};

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

/// Host-provided domain resolver that MUST bypass the VPN tunnel.
///
/// Used for direct-mode connections: when the tunnel is down and xr-core
/// needs to find the real IP of a fake-DNS domain, asking the host OS (e.g.
/// Android `Network.getAllByName()` on the underlying non-VPN network)
/// goes through whichever DNS channel the carrier actually allows — plain
/// UDP, DoT, DoH — whereas our own UDP:53 probes in `resolve_via_protected_dns`
/// get dropped on networks that whitelist traffic by destination port.
///
/// Returning `None` means "I can't resolve this, try the UDP fallback."
/// Implementations are called from async context but may block — wrap the
/// call site in `spawn_blocking` to avoid stalling the tokio worker.
pub type SystemResolverFn = Arc<dyn Fn(&str) -> Option<Ipv4Addr> + Send + Sync>;

/// Shared context for session management.
pub struct SessionContext {
    /// `RwLock<Arc<Router>>` — тот же паттерн, что в `xr-client::proxy::ProxyState`.
    /// Background preset-refresh внутри `VpnEngine` подменяет активные правила
    /// без рестарта движка (hot-swap). Hot path — один short-lived read-lock
    /// на `resolve()` per сессию; write — раз в `hub_refresh_interval_secs`.
    /// Живые сессии не «едут под ногами»: Action выбирается один раз по value.
    pub router: std::sync::RwLock<Arc<Router>>,
    pub codec: Codec,
    pub server_addr: SocketAddr,
    pub fake_dns: Arc<FakeDns>,
    pub stats: Stats,
    pub on_server_down: Action,
    pub protect_socket: ProtectSocketFn,
    pub mux_pool: Arc<xr_proto::mux_pool::MuxPool>,
    /// DNS resolvers for direct-connection name resolution, tried in parallel.
    /// First one to answer wins. System-provided (via Android ConnectivityManager)
    /// resolvers come first, with public resolvers as a universal fallback.
    pub dns_resolvers: Vec<SocketAddr>,
    /// Optional host-level resolver (Android `Network.getAllByName`) tried
    /// BEFORE the UDP:53 fallback. See `SystemResolverFn` docs.
    pub system_resolver: Option<SystemResolverFn>,
}

/// Create a TCP connection that bypasses the VPN tunnel.
///
/// Uses tokio::net::TcpSocket with protect(fd) before connect.
pub(crate) async fn connect_protected_pub(addr: SocketAddr, protect: &ProtectSocketFn) -> io::Result<TcpStream> {
    connect_protected(addr, protect).await
}

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
        Duration::from_secs(5),
        socket.connect(addr),
    ).await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut,
        format!("connect timeout to {}", addr)))?
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
    // Block DNS-over-TLS (port 853). Android Private DNS bypasses FakeDNS
    // (sends queries over TLS instead of UDP:53). Blocking DoT forces Android
    // to fall back to plain UDP DNS which FakeDNS intercepts properly.
    //
    // **Kind must stay `ConnectionRefused`.** Android Private DNS interprets
    // this specific error as "port closed, try something else" and falls
    // back to UDP/53. Any other kind (InvalidInput, Other, ...) breaks the
    // fallback — DNS queries silently die, apps get "no network", and the
    // user sees "Connected but no traffic". Regression verified the hard
    // way during LLD-02 rollout.
    //
    // The engine-level classifier in `engine.rs` filters this specific
    // message out of the log entirely (it's normal per-query behaviour,
    // not a failure to report).
    if key.dst_addr.port() == 853 {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("DoT blocked ({}), forcing UDP DNS", key.dst_addr),
        ));
    }

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

    // Short-lived read-lock: resolve() возвращает Action по value.
    // После drop'а guard'а live-сессия не пересчитывает маршрут, даже если
    // фоновый hot-swap подменит Router — это осознанное поведение.
    let action = ctx.router.read().unwrap().resolve(domain.as_deref(), key.dst_addr.ip());

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
            // Open the mux stream upfront so that a failure here can fall back
            // to direct without having consumed anything from the inbound
            // channels yet (relay_via_mux_stream drains data_rx, so we can't
            // retry after that starts).
            match ctx.mux_pool.open_stream(&target_addr).await {
                Ok(mux_stream) => {
                    ctx.stats.add_log(&format!("mux relay for {:?}", target_addr));
                    tracing::debug!("mux relay for {:?}", target_addr);
                    relay_via_mux_stream(mux_stream, data_rx, data_tx, waker, &ctx.stats).await
                }
                Err(e) => {
                    if ctx.on_server_down == Action::Direct {
                        ctx.stats.add_log(&format!(
                            "mux fail for {:?}, falling back to direct: {}",
                            target_addr, e,
                        ));
                        tracing::info!("mux fail for {:?}, direct fallback: {}", target_addr, e);
                        relay_direct(ctx, key.dst_addr, domain.as_deref(), data_rx, data_tx, waker).await
                    } else {
                        // Preserve the "mux open fail:" prefix so engine.rs
                        // error classification keeps the existing behaviour.
                        Err(io::Error::new(e.kind(), format!("mux open fail: {}", e)))
                    }
                }
            }
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
    // On Android, tokio::net::lookup_host goes through the system resolver
    // which uses the VPN's DNS (FakeDNS) — returning another fake IP and
    // creating an infinite loop. We resolve via a protected UDP socket to
    // an external DNS server, bypassing the VPN tunnel entirely.
    let real_dst = if let IpAddr::V4(v4) = dst.ip() {
        if FakeDns::is_fake_ip(v4) {
            if let Some(domain) = domain {
                // Prefer the host resolver: on Android it uses the underlying
                // non-VPN Network and whatever DNS channel (plain / DoT / DoH)
                // the carrier actually allows. Our own UDP:53 probes die on
                // whitelist networks that only permit port-443 traffic.
                let ip = resolve_domain_with_fallback(
                    domain,
                    ctx.system_resolver.as_ref(),
                    &ctx.dns_resolvers,
                    &ctx.protect_socket,
                ).await?;
                SocketAddr::new(IpAddr::V4(ip), dst.port())
            } else {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "fake IP without domain"));
            }
        } else {
            dst
        }
    } else {
        dst
    };

    // Connect with PROTECTED socket.
    let target = connect_protected(real_dst, &ctx.protect_socket).await?;

    // Симметрично `mux relay for ...`: без этой записи пользователь не может
    // отличить по логу ошибок, какой путь выбрал роутер — direct или proxy.
    // Формат TargetAddr::{Domain, Ip} повторяет mux-вариант, чтобы обе строки
    // выглядели узнаваемо в ленте.
    let target_label = match domain {
        Some(d) => format!("Domain({:?}, {})", d, dst.port()),
        None => format!("Ip({})", real_dst),
    };
    ctx.stats.add_log(&format!("direct connect for {}", target_label));
    tracing::debug!("direct connect for {}", target_label);

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

/// Relay data between smoltcp channels and a MuxStream.
async fn relay_via_mux_stream(
    mut mux_stream: xr_proto::mux::MuxStream,
    mut data_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    data_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    waker: Arc<Notify>,
    stats: &Stats,
) -> io::Result<()> {
    let upload = async {
        loop {
            tokio::select! {
                data = data_rx.recv() => {
                    match data {
                        Some(d) if !d.is_empty() => {
                            stats.add_bytes_up(d.len() as u64);
                            mux_stream.send(&d).await?;
                        }
                        _ => break,
                    }
                }
                data = mux_stream.recv() => {
                    match data {
                        Some(d) if !d.is_empty() => {
                            stats.add_bytes_down(d.len() as u64);
                            if data_tx.send(d).await.is_err() { break; }
                            waker.notify_one();
                        }
                        _ => break,
                    }
                }
            }
        }
        mux_stream.close().await?;
        Ok::<(), io::Error>(())
    };

    match tokio::time::timeout(Duration::from_secs(3600), upload).await {
        Ok(r) => r,
        Err(_) => Ok(()),
    }
}

// ── Protected DNS resolver ─────────────────────────────────────────

/// Fallback public DNS resolvers, used when no system-provided resolvers
/// are available or they all fail. Covers cases where the carrier blocks
/// Google (8.8.8.8) but allows Cloudflare (1.1.1.1) or Yandex (77.88.8.8).
pub const FALLBACK_DNS_RESOLVERS: &[SocketAddr] = &[
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 53),
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 53),
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(77, 88, 8, 8)), 53),
];

/// Resolve a domain to an IPv4 via the host OS first, falling back to our
/// own UDP:53 client on protected sockets.
///
/// The host-level path matters on whitelist-style carrier networks where
/// plain UDP DNS to public resolvers is dropped but the OS resolver (via
/// DoT/DoH set up by the carrier) still works. Without it, a dead xr-server
/// combined with a blocked UDP:53 means even "direct" traffic can't resolve
/// fake IPs — so the user loses whitelist sites too, not just proxied ones.
pub(crate) async fn resolve_domain_with_fallback(
    domain: &str,
    system_resolver: Option<&SystemResolverFn>,
    resolvers: &[SocketAddr],
    protect: &ProtectSocketFn,
) -> io::Result<Ipv4Addr> {
    if let Some(resolver) = system_resolver {
        let resolver = resolver.clone();
        let domain_owned = domain.to_string();
        // System resolvers are synchronous JNI/syscall chains — run on a
        // blocking pool so we don't stall the tokio worker.
        match tokio::task::spawn_blocking(move || resolver(&domain_owned)).await {
            Ok(Some(ip)) => {
                tracing::debug!("system resolver returned {} for {}", ip, domain);
                return Ok(ip);
            }
            Ok(None) => {
                tracing::debug!("system resolver has no answer for {}, using UDP fallback", domain);
            }
            Err(e) => {
                tracing::warn!("system resolver task panicked for {}: {}", domain, e);
            }
        }
    }
    resolve_via_protected_dns(domain, resolvers, protect).await
}

/// Resolve a domain to an IPv4 address via protected UDP sockets.
///
/// On Android, the system DNS resolver routes through the VPN's FakeDNS,
/// returning fake IPs and creating a loop. This function sends raw DNS
/// queries through protected (VPN-bypassing) UDP sockets.
///
/// Queries **all resolvers in parallel** and returns the first successful
/// answer. This matters in restricted carrier networks where one resolver
/// may be blocked while another works (e.g. mobile operator whitelist).
async fn resolve_via_protected_dns(
    domain: &str,
    resolvers: &[SocketAddr],
    protect: &ProtectSocketFn,
) -> io::Result<Ipv4Addr> {
    // Always query at least the fallback resolvers — if the caller passed an
    // empty list we'd otherwise silently return "no DNS" here.
    let effective: Vec<SocketAddr> = if resolvers.is_empty() {
        FALLBACK_DNS_RESOLVERS.to_vec()
    } else {
        // Deduplicate while preserving caller-provided order so system
        // resolvers still win in the happy path.
        let mut seen: std::collections::HashSet<SocketAddr> = std::collections::HashSet::new();
        let mut out: Vec<SocketAddr> = Vec::new();
        for r in resolvers.iter().chain(FALLBACK_DNS_RESOLVERS.iter()) {
            if seen.insert(*r) {
                out.push(*r);
            }
        }
        out
    };

    // Race all resolvers concurrently. First success wins; collect errors
    // until the last task to finish.
    let n = effective.len();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<io::Result<Ipv4Addr>>(n);
    for &resolver in &effective {
        let tx = tx.clone();
        let domain = domain.to_string();
        let protect = protect.clone();
        tokio::spawn(async move {
            let result = query_single_resolver(&domain, resolver, &protect).await;
            let _ = tx.send(result).await;
        });
    }
    drop(tx); // close sender so the channel ends after all tasks finish

    let mut last_err: Option<io::Error> = None;
    while let Some(result) = rx.recv().await {
        match result {
            Ok(ip) => return Ok(ip),
            Err(e) => last_err = Some(e),
        }
    }

    Err(last_err.unwrap_or_else(|| io::Error::new(
        io::ErrorKind::TimedOut,
        format!("DNS resolve timeout for {}", domain),
    )))
}

async fn query_single_resolver(
    domain: &str,
    resolver: SocketAddr,
    protect: &ProtectSocketFn,
) -> io::Result<Ipv4Addr> {
    let std_sock = std::net::UdpSocket::bind("0.0.0.0:0")?;
    std_sock.set_nonblocking(true)?;
    if !protect(std_sock.as_raw_fd()) {
        return Err(io::Error::new(io::ErrorKind::Other, "protect(dns socket) failed"));
    }
    let sock = tokio::net::UdpSocket::from_std(std_sock)?;

    let query = build_dns_query(domain);
    sock.send_to(&query, resolver).await?;

    let mut buf = [0u8; 512];
    let n = tokio::time::timeout(Duration::from_secs(3), sock.recv(&mut buf))
        .await
        .map_err(|_| io::Error::new(
            io::ErrorKind::TimedOut,
            format!("DNS resolve timeout for {} via {}", domain, resolver),
        ))??;

    parse_dns_a_record(&buf[..n], domain)
}

/// Build a minimal DNS A-record query for the given domain.
fn build_dns_query(domain: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    // Header: ID=0xABCD, Flags=0x0100 (RD=1), QDCOUNT=1.
    buf.extend_from_slice(&[0xAB, 0xCD, 0x01, 0x00]);
    buf.extend_from_slice(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    // Question: encoded domain name.
    for label in domain.split('.') {
        buf.push(label.len() as u8);
        buf.extend_from_slice(label.as_bytes());
    }
    buf.push(0); // Root terminator.
    buf.extend_from_slice(&[0x00, 0x01]); // QTYPE = A
    buf.extend_from_slice(&[0x00, 0x01]); // QCLASS = IN
    buf
}

/// Parse the first A record (IPv4) from a DNS response.
fn parse_dns_a_record(data: &[u8], domain: &str) -> io::Result<Ipv4Addr> {
    if data.len() < 12 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "DNS response too short"));
    }
    let ancount = u16::from_be_bytes([data[6], data[7]]);
    if ancount == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("DNS: no answers for {}", domain),
        ));
    }

    // Skip header (12 bytes) + question section.
    let mut pos = 12;
    pos = skip_dns_name(data, pos);
    pos += 4; // QTYPE + QCLASS

    // Scan answer records for the first A record.
    for _ in 0..ancount {
        if pos >= data.len() { break; }
        pos = skip_dns_name(data, pos);
        if pos + 10 > data.len() { break; }
        let rtype = u16::from_be_bytes([data[pos], data[pos + 1]]);
        let rdlength = u16::from_be_bytes([data[pos + 8], data[pos + 9]]) as usize;
        pos += 10;
        if rtype == 1 && rdlength == 4 && pos + 4 <= data.len() {
            return Ok(Ipv4Addr::new(data[pos], data[pos + 1], data[pos + 2], data[pos + 3]));
        }
        pos += rdlength;
    }

    Err(io::Error::new(
        io::ErrorKind::Other,
        format!("DNS: no A record for {}", domain),
    ))
}

/// Skip a DNS name (handles both labels and compression pointers).
fn skip_dns_name(data: &[u8], mut pos: usize) -> usize {
    while pos < data.len() {
        let b = data[pos];
        if b == 0 { return pos + 1; }
        if b & 0xC0 == 0xC0 { return pos + 2; } // Compression pointer.
        pos += (b as usize) + 1;
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_query_format() {
        let q = build_dns_query("example.com");
        // Header: 12 bytes.
        assert_eq!(&q[..2], &[0xAB, 0xCD]); // ID
        assert_eq!(&q[2..4], &[0x01, 0x00]); // Flags: RD=1
        assert_eq!(&q[4..6], &[0x00, 0x01]); // QDCOUNT=1
        // Question: 7(example) + 3(com) + 1(root) + 4(type+class) = 17
        assert_eq!(q[12], 7); // "example" label length
        assert_eq!(&q[13..20], b"example");
        assert_eq!(q[20], 3); // "com" label length
        assert_eq!(&q[21..24], b"com");
        assert_eq!(q[24], 0); // Root
        assert_eq!(&q[25..27], &[0x00, 0x01]); // QTYPE=A
        assert_eq!(&q[27..29], &[0x00, 0x01]); // QCLASS=IN
    }

    #[test]
    fn dns_response_parse_a_record() {
        // Minimal DNS response: header + question + 1 A record answer.
        let mut resp = build_dns_query("t.co");
        // Patch header: QR=1, AA=1, ANCOUNT=1.
        resp[2] = 0x85; resp[3] = 0x00;
        resp[6] = 0x00; resp[7] = 0x01;
        // Answer: compression pointer to name at offset 12, TYPE=A,
        // CLASS=IN, TTL=300, RDLENGTH=4, RDATA=93.184.216.34.
        resp.extend_from_slice(&[0xC0, 12]); // Name pointer
        resp.extend_from_slice(&[0x00, 0x01]); // TYPE=A
        resp.extend_from_slice(&[0x00, 0x01]); // CLASS=IN
        resp.extend_from_slice(&[0x00, 0x00, 0x01, 0x2C]); // TTL=300
        resp.extend_from_slice(&[0x00, 0x04]); // RDLENGTH=4
        resp.extend_from_slice(&[93, 184, 216, 34]); // IP

        let ip = parse_dns_a_record(&resp, "t.co").unwrap();
        assert_eq!(ip, Ipv4Addr::new(93, 184, 216, 34));
    }

    #[test]
    fn dns_response_no_answers() {
        let mut resp = build_dns_query("nxdomain.test");
        resp[2] = 0x81; resp[3] = 0x03; // QR=1, RCODE=NXDOMAIN
        // ANCOUNT stays 0.
        let err = parse_dns_a_record(&resp, "nxdomain.test").unwrap_err();
        assert!(err.to_string().contains("no answers"));
    }

    #[tokio::test]
    async fn system_resolver_short_circuits_udp_fallback() {
        // When the host resolver returns an IP we must NOT fall through to
        // the UDP path — otherwise we'd still time out on whitelist networks
        // that block UDP:53. Using an unroutable resolver as a canary:
        // if it ever gets contacted the test would hang on the 3s timeout.
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen_clone = seen.clone();
        let resolver: SystemResolverFn = Arc::new(move |host: &str| {
            seen_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(host, "example.com");
            Some(Ipv4Addr::new(93, 184, 216, 34))
        });
        let protect: ProtectSocketFn = Arc::new(|_| true);
        let ip = resolve_domain_with_fallback(
            "example.com",
            Some(&resolver),
            &[SocketAddr::new(IpAddr::V4(Ipv4Addr::new(240, 0, 0, 1)), 53)],
            &protect,
        ).await.expect("system resolver should succeed");
        assert_eq!(ip, Ipv4Addr::new(93, 184, 216, 34));
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn system_resolver_none_falls_back_to_udp() {
        // Resolver returns None → must hit UDP path. We supply no resolvers
        // and a protect that always fails, so the UDP branch errors out
        // quickly; the assertion is that the error surfaces (i.e. fallback
        // ran), not that DNS worked.
        let resolver: SystemResolverFn = Arc::new(|_| None);
        let protect: ProtectSocketFn = Arc::new(|_| false);
        let err = resolve_domain_with_fallback(
            "example.com",
            Some(&resolver),
            &[SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 53)],
            &protect,
        ).await.expect_err("UDP fallback must fail with protect returning false");
        assert!(
            err.to_string().contains("protect") || err.to_string().contains("DNS"),
            "unexpected error: {}", err,
        );
    }
}
