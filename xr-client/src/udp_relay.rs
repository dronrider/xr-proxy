/// UDP Relay client: intercept UDP from LAN devices, relay through VPS.
///
/// Uses TPROXY (nftables + policy routing) to intercept UDP packets
/// while preserving original destination address.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::time::{interval, Duration};
use xr_proto::config::UdpRelayClientConfig;
use xr_proto::obfuscation::Obfuscator;
use xr_proto::udp_relay::{self, RelayPacket, RelayType};

// Linux socket constants — not always exported by libc on musl/cross targets
const SOL_IP: libc::c_int = 0;
const IP_TRANSPARENT: libc::c_int = 19;
const IP_RECVORIGDSTADDR: libc::c_int = 20;
const IP_ORIGDSTADDR: libc::c_int = 20;

// ── Flow tracking ──────────────────────────────────────────────────

struct UdpFlow {
    src_addr: SocketAddr,
    orig_dst: SocketAddr,
    last_activity: Instant,
}

struct RelayState {
    /// Map: flow_key(src_port, dst) → flow info
    flows: Mutex<HashMap<u64, UdpFlow>>,
    /// Cache of spoofed sockets: orig_dst → socket bound to that address.
    /// Used to send responses back to Switch with correct source address.
    spoof_sockets: Mutex<HashMap<SocketAddr, Arc<std::net::UdpSocket>>>,
    obfuscator: Obfuscator,
    vps_addr: SocketAddr,
    flow_timeout: Duration,
    source_ips: Vec<Ipv4Addr>,
    exclude_ports: Vec<u16>,
}

fn flow_key(src_port: u16, dst: &SocketAddr) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    src_port.hash(&mut h);
    dst.hash(&mut h);
    h.finish()
}

// ── Main entry ─────────────────────────────────────────────────────

pub async fn run_udp_relay(
    config: &UdpRelayClientConfig,
    obfuscator: Obfuscator,
    server_address: &str,
) -> io::Result<()> {
    let vps_host = config.vps_host.as_deref().unwrap_or(server_address);
    let vps_addr: SocketAddr = format!("{}:{}", vps_host, config.vps_port)
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("bad VPS addr: {}", e)))?;

    let source_ips: Vec<Ipv4Addr> = config
        .source_ips
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();

    if source_ips.is_empty() {
        tracing::info!("UDP relay: relaying all LAN devices");
    } else {
        tracing::info!("UDP relay: relaying only {:?}", source_ips);
    }

    let state = Arc::new(RelayState {
        flows: Mutex::new(HashMap::new()),
        spoof_sockets: Mutex::new(HashMap::new()),
        obfuscator,
        vps_addr,
        flow_timeout: Duration::from_secs(config.flow_timeout_sec),
        source_ips,
        exclude_ports: config.exclude_dst_ports.clone(),
    });

    // Bind local TPROXY listener — use AsyncFd directly (not tokio UdpSocket)
    // because we need recvmsg for IP_ORIGDSTADDR, and tokio's UdpSocket
    // would double-register the fd with the reactor.
    let listen_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, config.listen_port));
    let local_std = bind_tproxy_socket(config.listen_port, config.use_tproxy)?;
    let local_async = Arc::new(tokio::io::unix::AsyncFd::new(local_std)?);
    tracing::info!(
        "UDP relay listening on {} ({} mode)",
        listen_addr,
        if config.use_tproxy { "TPROXY" } else { "REDIRECT" }
    );

    // Tunnel socket to VPS (normal tokio socket, no recvmsg needed)
    let tunnel_socket = UdpSocket::bind("0.0.0.0:0").await?;
    tracing::info!("UDP relay tunnel to {}", vps_addr);

    let tunnel = Arc::new(tunnel_socket);

    // Keepalive sender
    let ka_obfs = state.obfuscator.clone();
    let ka_tunnel = tunnel.clone();
    let ka_vps = vps_addr;
    let ka_secs = config.keepalive_interval_sec;
    tokio::spawn(async move {
        let mut timer = interval(Duration::from_secs(ka_secs));
        loop {
            timer.tick().await;
            let wire = udp_relay::encode_keepalive(&ka_obfs);
            let _ = ka_tunnel.send_to(&wire, ka_vps).await;
        }
    });

    // Flow cleanup
    let clean_state = state.clone();
    tokio::spawn(async move {
        let mut timer = interval(Duration::from_secs(30));
        loop {
            timer.tick().await;
            let timeout = clean_state.flow_timeout;

            let mut flows = clean_state.flows.lock().await;
            let before = flows.len();
            flows.retain(|_, f| f.last_activity.elapsed() < timeout);
            let removed = before - flows.len();
            if removed > 0 {
                tracing::debug!("UDP relay: cleaned {} expired flows ({} active)", removed, flows.len());
            }

            // Collect active orig_dst addresses
            let active_dsts: std::collections::HashSet<SocketAddr> =
                flows.values().map(|f| f.orig_dst).collect();
            drop(flows);

            // Clean spoof sockets for dead flows
            let mut spoof = clean_state.spoof_sockets.lock().await;
            let spoof_before = spoof.len();
            spoof.retain(|addr, _| active_dsts.contains(addr));
            let spoof_removed = spoof_before - spoof.len();
            if spoof_removed > 0 {
                tracing::debug!("UDP relay: cleaned {} spoof sockets", spoof_removed);
            }
        }
    });

    // Upstream: LAN → VPS
    let up_state = state.clone();
    let up_local = local_async.clone();
    let up_tunnel = tunnel.clone();
    let use_tproxy = config.use_tproxy;
    let upstream = async move {
        let mut buf = vec![0u8; 65536];

        loop {
            // Use recvmsg to get both src and original dst
            let (n, src_addr, orig_dst) = if use_tproxy {
                recvmsg_origdst(&up_local, &mut buf).await?
            } else {
                tracing::warn!("UDP relay REDIRECT mode not supported, use TPROXY");
                return Err(io::Error::new(io::ErrorKind::Unsupported, "REDIRECT mode not supported"));
            };

            if n == 0 {
                continue;
            }

            // Filter by source IP
            if !up_state.source_ips.is_empty() {
                if let SocketAddr::V4(v4) = src_addr {
                    if !up_state.source_ips.contains(v4.ip()) {
                        continue;
                    }
                } else {
                    continue; // skip non-v4 if source_ips are v4
                }
            }

            // Filter excluded ports
            if up_state.exclude_ports.contains(&orig_dst.port()) {
                continue;
            }

            // Filter broadcast, multicast, and LAN destinations
            if let SocketAddr::V4(v4) = orig_dst {
                let ip = v4.ip();
                if ip.is_broadcast() || ip.is_multicast()
                    || *ip == Ipv4Addr::new(255, 255, 255, 255)
                    || ip.is_loopback()
                    || is_private_ip(ip)
                {
                    continue;
                }
            }

            let src_port = src_addr.port();

            // Track flow for reverse path
            let fkey = flow_key(src_port, &orig_dst);
            {
                let mut flows = up_state.flows.lock().await;
                flows.insert(fkey, UdpFlow {
                    src_addr,
                    orig_dst,
                    last_activity: Instant::now(),
                });
            }

            // Encode and send
            let packet = RelayPacket {
                relay_type: RelayType::Data,
                dst: orig_dst,
                src_port,
                payload: buf[..n].to_vec(),
            };
            let wire = udp_relay::encode_relay_packet(&up_state.obfuscator, &packet);
            if let Err(e) = up_tunnel.send_to(&wire, up_state.vps_addr).await {
                tracing::warn!("UDP relay: send to VPS failed: {}", e);
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), io::Error>(())
    };

    // Downstream: VPS → LAN
    let down_state = state.clone();
    let down_tunnel = tunnel.clone();
    let downstream = async move {
        let mut buf = vec![0u8; 65536];
        loop {
            let (n, _peer) = down_tunnel.recv_from(&mut buf).await?;
            if n == 0 {
                continue;
            }

            let packet = match udp_relay::decode_relay_packet(&down_state.obfuscator, &buf[..n]) {
                Some(p) => p,
                None => {
                    tracing::debug!("UDP relay: invalid packet from VPS");
                    continue;
                }
            };

            match packet.relay_type {
                RelayType::Keepalive => {}
                RelayType::Data => {
                    // packet.dst = the remote server that responded (e.g. 3.71.152.160:33334)
                    // packet.src_port = Switch's source port
                    // We need to deliver payload to Switch with src = packet.dst (the server)

                    // Find Switch address from flow table
                    let fkey = flow_key(packet.src_port, &packet.dst);
                    let target_info = {
                        let mut flows = down_state.flows.lock().await;
                        if let Some(flow) = flows.get_mut(&fkey) {
                            flow.last_activity = Instant::now();
                            Some((flow.src_addr, flow.orig_dst))
                        } else {
                            // Incoming P2P: find any flow with matching src_port
                            flows.values_mut()
                                .find(|f| f.src_addr.port() == packet.src_port)
                                .map(|f| {
                                    f.last_activity = Instant::now();
                                    (f.src_addr, packet.dst)
                                })
                        }
                    };

                    if let Some((switch_addr, orig_dst)) = target_info {
                        // Send from a socket bound to orig_dst (spoofed source)
                        // so Switch sees the response coming from the real server
                        let spoof = get_or_create_spoof_socket(&down_state, orig_dst).await;
                        match spoof {
                            Ok(sock) => {
                                let fd = sock.as_raw_fd();
                                if let Err(e) = do_sendto(fd, &packet.payload, switch_addr) {
                                    tracing::warn!("UDP relay: spoof send to {} failed: {}", switch_addr, e);
                                }
                            }
                            Err(e) => {
                                tracing::warn!("UDP relay: spoof socket for {} failed: {}", orig_dst, e);
                            }
                        }
                    } else {
                        tracing::debug!(
                            "UDP relay: no flow for port={} dst={}",
                            packet.src_port, packet.dst
                        );
                    }
                }
                _ => {}
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), io::Error>(())
    };

    tokio::select! {
        r = upstream => r,
        r = downstream => r,
    }
}

// ── Socket setup ───────────────────────────────────────────────────

/// Get or create a UDP socket bound to `spoof_addr` with IP_TRANSPARENT.
/// This allows sending packets that appear to come from `spoof_addr`.
async fn get_or_create_spoof_socket(
    state: &RelayState,
    spoof_addr: SocketAddr,
) -> io::Result<Arc<std::net::UdpSocket>> {
    // Check cache first
    {
        let cache = state.spoof_sockets.lock().await;
        if let Some(sock) = cache.get(&spoof_addr) {
            return Ok(sock.clone());
        }
    }

    // Create new spoofed socket
    let sock = create_spoof_socket(spoof_addr)?;
    let sock = Arc::new(sock);

    let mut cache = state.spoof_sockets.lock().await;
    cache.insert(spoof_addr, sock.clone());

    tracing::debug!("UDP relay: created spoof socket for {}", spoof_addr);
    Ok(sock)
}

/// Create a non-blocking UDP socket bound to a non-local address via IP_TRANSPARENT.
fn create_spoof_socket(bind_addr: SocketAddr) -> io::Result<std::net::UdpSocket> {
    use std::net::UdpSocket as StdSocket;

    // Create socket
    let sock = StdSocket::bind("0.0.0.0:0")
        .map_err(|e| io::Error::new(e.kind(), format!("spoof socket create: {}", e)))?;

    let fd = sock.as_raw_fd();

    unsafe {
        let val: libc::c_int = 1;

        // IP_TRANSPARENT: allow binding to non-local addresses
        let ret = libc::setsockopt(
            fd, SOL_IP, IP_TRANSPARENT,
            &val as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
    }

    // Now re-bind to the spoofed address
    drop(sock);

    let sock = unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_NONBLOCK, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let val: libc::c_int = 1;
        libc::setsockopt(
            fd, SOL_IP, IP_TRANSPARENT,
            &val as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );

        // SO_REUSEADDR so multiple spoof sockets can coexist
        libc::setsockopt(
            fd, libc::SOL_SOCKET, libc::SO_REUSEADDR,
            &val as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );

        if let SocketAddr::V4(v4) = bind_addr {
            let mut addr: libc::sockaddr_in = std::mem::zeroed();
            addr.sin_family = libc::AF_INET as _;
            addr.sin_port = v4.port().to_be();
            addr.sin_addr.s_addr = u32::from(*v4.ip()).to_be();

            let ret = libc::bind(
                fd,
                &addr as *const libc::sockaddr_in as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            );
            if ret != 0 {
                let err = io::Error::last_os_error();
                libc::close(fd);
                return Err(err);
            }
        }

        StdSocket::from_raw_fd(fd)
    };

    Ok(sock)
}

fn bind_tproxy_socket(port: u16, use_tproxy: bool) -> io::Result<std::net::UdpSocket> {
    use std::net::UdpSocket as StdSocket;

    let socket = StdSocket::bind(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)))?;
    socket.set_nonblocking(true)?;

    if use_tproxy {
        let fd = socket.as_raw_fd();
        unsafe {
            let val: libc::c_int = 1;

            // IP_TRANSPARENT: allow binding to non-local addresses (TPROXY)
            let ret = libc::setsockopt(
                fd, SOL_IP, IP_TRANSPARENT,
                &val as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            if ret != 0 {
                return Err(io::Error::last_os_error());
            }

            // IP_RECVORIGDSTADDR: receive original destination in ancillary data
            let ret = libc::setsockopt(
                fd, SOL_IP, IP_RECVORIGDSTADDR,
                &val as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            if ret != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }

    Ok(socket)
}

// ── recvmsg with original destination ──────────────────────────────

/// Receive a UDP packet via recvmsg, extracting the original destination
/// address from IP_ORIGDSTADDR ancillary data (set by TPROXY).
async fn recvmsg_origdst(
    async_fd: &tokio::io::unix::AsyncFd<std::net::UdpSocket>,
    buf: &mut [u8],
) -> io::Result<(usize, SocketAddr, SocketAddr)> {
    loop {
        let mut guard = async_fd.readable().await?;
        let fd = async_fd.as_raw_fd();

        match do_recvmsg(fd, buf) {
            Ok(r) => {
                guard.retain_ready();
                return Ok(r);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                guard.clear_ready();
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Raw sendto syscall.
fn do_sendto(fd: i32, data: &[u8], target: SocketAddr) -> io::Result<usize> {
    unsafe {
        match target {
            SocketAddr::V4(v4) => {
                let mut addr: libc::sockaddr_in = std::mem::zeroed();
                addr.sin_family = libc::AF_INET as _;
                addr.sin_port = v4.port().to_be();
                addr.sin_addr.s_addr = u32::from(*v4.ip()).to_be();

                let n = libc::sendto(
                    fd,
                    data.as_ptr() as *const libc::c_void,
                    data.len(),
                    0,
                    &addr as *const libc::sockaddr_in as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                );
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }
            _ => Err(io::Error::new(io::ErrorKind::Unsupported, "IPv6 sendto not implemented")),
        }
    }
}

/// Raw recvmsg call extracting src addr and original dst from cmsg.
fn do_recvmsg(
    fd: i32,
    buf: &mut [u8],
) -> io::Result<(usize, SocketAddr, SocketAddr)> {
    unsafe {
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len() as _,
        };

        let mut src_addr: libc::sockaddr_in = std::mem::zeroed();
        let mut cmsg_buf = [0u8; 256]; // enough for ancillary data

        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_name = &mut src_addr as *mut _ as *mut libc::c_void;
        msg.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as _;
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cmsg_buf.len() as _;

        let n = libc::recvmsg(fd, &mut msg, 0);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }

        // Parse source address
        let src_ip = Ipv4Addr::from(u32::from_be(src_addr.sin_addr.s_addr));
        let src_port = u16::from_be(src_addr.sin_port);
        let src = SocketAddr::V4(SocketAddrV4::new(src_ip, src_port));

        // Parse original destination from cmsg (IP_ORIGDSTADDR)
        let mut orig_dst = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            let hdr = &*cmsg;
            if hdr.cmsg_level == SOL_IP && hdr.cmsg_type == IP_ORIGDSTADDR {
                let dst_addr = &*(libc::CMSG_DATA(cmsg) as *const libc::sockaddr_in);
                let dst_ip = Ipv4Addr::from(u32::from_be(dst_addr.sin_addr.s_addr));
                let dst_port = u16::from_be(dst_addr.sin_port);
                orig_dst = SocketAddr::V4(SocketAddrV4::new(dst_ip, dst_port));
                break;
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }

        if orig_dst.port() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "no IP_ORIGDSTADDR in cmsg — is TPROXY configured?",
            ));
        }

        Ok((n as usize, src, orig_dst))
    }
}

/// Check if an IPv4 address is in a private range (10/8, 172.16/12, 192.168/16).
fn is_private_ip(ip: &Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 10
        || (octets[0] == 172 && (octets[1] & 0xf0) == 16)
        || (octets[0] == 192 && octets[1] == 168)
}
