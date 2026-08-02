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

// На боевых целях значение берём у libc: оно разное по архитектурам
// (на mips свой набор флагов). Заглушка нужна только для сборки на маке,
// где крейт собирают ради юнит-тестов, а не для запуска.
#[cfg(target_os = "linux")]
use libc::SOCK_NONBLOCK;
#[cfg(not(target_os = "linux"))]
const SOCK_NONBLOCK: libc::c_int = 0o4000;

// ── Flow tracking ──────────────────────────────────────────────────

/// Из туннеля обратно приходит только `src_port` и `dst`, адреса устройства в
/// `RelayPacket` нет. Поэтому туннельный порт и есть весь наш NAT: он уникален
/// на устройство, и ответ по нему находит ровно одно устройство.
const TUNNEL_PORT_POOL: std::ops::RangeInclusive<u16> = 40000..=65000;

struct UdpFlow {
    src_addr: SocketAddr,
    last_activity: Instant,
}

/// Таблица флоу с NAT по туннельному порту. Логика сокетов не касается, чтобы
/// гоняться юнитами там, где TPROXY не поднять.
///
/// Маппинг endpoint-independent: номер выдаётся на `(адрес, порт)` устройства и
/// держится на всех его адресатов сразу. Иначе NAT на VPS становится symmetric,
/// а от типа NAT у Switch и Xbox зависит мультиплеер, ради которого весь relay
/// и заведён.
struct FlowTable {
    /// туннельный порт -> флоу
    flows: HashMap<u16, UdpFlow>,
    /// адрес устройства -> его туннельный порт
    ports: HashMap<SocketAddr, u16>,
    pool: std::ops::RangeInclusive<u16>,
}

impl FlowTable {
    fn new(pool: std::ops::RangeInclusive<u16>) -> Self {
        FlowTable {
            flows: HashMap::new(),
            ports: HashMap::new(),
            pool,
        }
    }

    /// Туннельный порт устройства, заводя флоу на первом пакете. `None` значит,
    /// что свободных портов не осталось и пакет придётся отбросить.
    fn touch(&mut self, src_addr: SocketAddr, now: Instant) -> Option<u16> {
        if let Some(&port) = self.ports.get(&src_addr) {
            if let Some(flow) = self.flows.get_mut(&port) {
                flow.last_activity = now;
            }
            return Some(port);
        }

        let port = match self.allocate(src_addr.port()) {
            Some(p) => p,
            None => {
                tracing::warn!(
                    "UDP relay: tunnel port pool exhausted ({} flows), dropping {}",
                    self.flows.len(), src_addr
                );
                return None;
            }
        };
        if port != src_addr.port() {
            // Вторая приставка той же модели сидит на том же порту, что первая
            // (Xbox 3074, PS 3478), и настоящий номер уже занят.
            tracing::debug!("UDP relay: {} goes through tunnel port {}", src_addr, port);
        }

        self.ports.insert(src_addr, port);
        self.flows.insert(port, UdpFlow { src_addr, last_activity: now });
        Some(port)
    }

    /// Настоящий порт устройства достаётся тому, кто пришёл первым: NAT
    /// приставок и P2P держатся на нём. Остальным идёт первый свободный номер
    /// из пула, выдача детерминированная.
    fn allocate(&self, preferred: u16) -> Option<u16> {
        if preferred != 0 && !self.flows.contains_key(&preferred) {
            return Some(preferred);
        }
        self.pool.clone().find(|p| !self.flows.contains_key(p))
    }

    /// Пакет от устройства, готовый к отправке в туннель.
    fn upstream_packet(
        &mut self,
        src_addr: SocketAddr,
        orig_dst: SocketAddr,
        payload: Vec<u8>,
        now: Instant,
    ) -> Option<RelayPacket> {
        let tunnel_port = self.touch(src_addr, now)?;
        Some(RelayPacket {
            relay_type: RelayType::Data,
            dst: orig_dst,
            src_port: tunnel_port,
            payload,
        })
    }

    /// Кому отдать ответ из туннеля и с какого адреса спуфить источник.
    /// `packet.src_port` это туннельный порт, он же ключ таблицы, поэтому
    /// поиска «любой флоу с таким портом» больше нет. Спуфим отправителя
    /// пакета, а не `orig_dst` флоу: у входящего P2P это разные адреса.
    fn downstream_target(
        &mut self,
        packet: &RelayPacket,
        now: Instant,
    ) -> Option<(SocketAddr, SocketAddr)> {
        let flow = self.flows.get_mut(&packet.src_port)?;
        flow.last_activity = now;
        Some((flow.src_addr, packet.dst))
    }

    /// Снимает протухшие флоу, возвращая их туннельные порты в пул.
    fn retire_expired(&mut self, timeout: Duration, now: Instant) -> usize {
        let expired: Vec<u16> = self
            .flows
            .iter()
            .filter(|(_, f)| now.duration_since(f.last_activity) >= timeout)
            .map(|(port, _)| *port)
            .collect();

        for port in &expired {
            if let Some(flow) = self.flows.remove(port) {
                self.ports.remove(&flow.src_addr);
            }
        }
        expired.len()
    }

    fn len(&self) -> usize {
        self.flows.len()
    }
}

/// Кэш спуфящих сокетов: адрес отправителя ответа -> сокет, забинденный на него
/// с `IP_TRANSPARENT`. Адресатов в таблице флоу нет (номер выдаётся на
/// устройство), поэтому живость сокета считается по нему самому: сокет живёт,
/// пока через него шлют, и уходит по простою вместе с флоу своего устройства.
struct SpoofCache {
    sockets: HashMap<SocketAddr, SpoofEntry>,
}

struct SpoofEntry {
    sock: Arc<std::net::UdpSocket>,
    last_used: Instant,
}

impl SpoofCache {
    fn new() -> Self {
        SpoofCache { sockets: HashMap::new() }
    }

    fn get(&mut self, addr: SocketAddr, now: Instant) -> Option<Arc<std::net::UdpSocket>> {
        let entry = self.sockets.get_mut(&addr)?;
        entry.last_used = now;
        Some(entry.sock.clone())
    }

    fn insert(&mut self, addr: SocketAddr, sock: Arc<std::net::UdpSocket>, now: Instant) {
        self.sockets.insert(addr, SpoofEntry { sock, last_used: now });
    }

    fn retire_idle(&mut self, timeout: Duration, now: Instant) -> usize {
        let before = self.sockets.len();
        self.sockets
            .retain(|_, e| now.duration_since(e.last_used) < timeout);
        before - self.sockets.len()
    }

    fn len(&self) -> usize {
        self.sockets.len()
    }
}

struct RelayState {
    flows: Mutex<FlowTable>,
    spoof_sockets: Mutex<SpoofCache>,
    obfuscator: Obfuscator,
    vps_addr: SocketAddr,
    flow_timeout: Duration,
    source_ips: Vec<Ipv4Addr>,
    exclude_ports: Vec<u16>,
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
        flows: Mutex::new(FlowTable::new(TUNNEL_PORT_POOL)),
        spoof_sockets: Mutex::new(SpoofCache::new()),
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

            let now = Instant::now();

            let mut flows = clean_state.flows.lock().await;
            let removed = flows.retire_expired(timeout, now);
            if removed > 0 {
                tracing::debug!("UDP relay: cleaned {} expired flows ({} active)", removed, flows.len());
            }
            drop(flows);

            // Спуфящий сокет уходит по собственному простою: адресатов в
            // таблице флоу нет, а через живого пира шлют чаще, чем раз в
            // flow_timeout.
            let mut spoof = clean_state.spoof_sockets.lock().await;
            let spoof_removed = spoof.retire_idle(timeout, now);
            if spoof_removed > 0 {
                tracing::debug!("UDP relay: cleaned {} spoof sockets ({} active)", spoof_removed, spoof.len());
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

            // Флоу заводится вместе со своим туннельным портом, он и уходит в
            // туннель вместо настоящего порта устройства.
            let packet = {
                let mut flows = up_state.flows.lock().await;
                match flows.upstream_packet(src_addr, orig_dst, buf[..n].to_vec(), Instant::now()) {
                    Some(p) => p,
                    None => continue,
                }
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
                    // packet.src_port = туннельный порт флоу
                    // We need to deliver payload to Switch with src = packet.dst (the server)
                    let target_info = {
                        let mut flows = down_state.flows.lock().await;
                        flows.downstream_target(&packet, Instant::now())
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
                            "UDP relay: no flow for tunnel port={} dst={}",
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
        let mut cache = state.spoof_sockets.lock().await;
        if let Some(sock) = cache.get(spoof_addr, Instant::now()) {
            return Ok(sock);
        }
    }

    // Create new spoofed socket
    let sock = create_spoof_socket(spoof_addr)?;
    let sock = Arc::new(sock);

    let mut cache = state.spoof_sockets.lock().await;
    cache.insert(spoof_addr, sock.clone(), Instant::now());

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
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | SOCK_NONBLOCK, 0);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn table() -> FlowTable {
        FlowTable::new(TUNNEL_PORT_POOL)
    }

    fn data(src_port: u16, dst: SocketAddr, payload: &[u8]) -> RelayPacket {
        RelayPacket {
            relay_type: RelayType::Data,
            dst,
            src_port,
            payload: payload.to_vec(),
        }
    }

    /// Две приставки одной модели сидят на одном порту (Xbox 3074) и играют на
    /// одном сервере. Пока ключом флоу был (src_port, dst), запись второй
    /// затирала первую и ответы обеим уходили последнему написавшему.
    #[test]
    fn two_consoles_on_one_port_get_own_replies() {
        let mut t = table();
        let now = Instant::now();
        let server = addr("203.0.113.5:3074");
        let first = addr("192.168.1.10:3074");
        let second = addr("192.168.1.11:3074");

        let up_first = t.upstream_packet(first, server, b"a".to_vec(), now).unwrap();
        let up_second = t.upstream_packet(second, server, b"b".to_vec(), now).unwrap();
        assert_ne!(
            up_first.src_port, up_second.src_port,
            "двум устройствам нельзя отдавать один туннельный порт"
        );

        let reply_first = data(up_first.src_port, server, b"a-reply");
        let reply_second = data(up_second.src_port, server, b"b-reply");
        assert_eq!(
            t.downstream_target(&reply_first, now),
            Some((first, server))
        );
        assert_eq!(
            t.downstream_target(&reply_second, now),
            Some((second, server))
        );
    }

    /// Маппинг endpoint-independent: одна приставка уходит на VPS одним и тем
    /// же портом ко всем пирам сразу, иначе NAT на VPS становится symmetric и
    /// мультиплеер у Switch и Xbox ломается.
    #[test]
    fn console_keeps_one_tunnel_port_for_all_peers() {
        let mut t = table();
        let now = Instant::now();
        let console = addr("192.168.1.10:3074");
        let matchmaking = addr("203.0.113.5:3074");
        let peer = addr("198.51.100.7:51820");

        let to_matchmaking = t.upstream_packet(console, matchmaking, b"a".to_vec(), now).unwrap();
        let to_peer = t.upstream_packet(console, peer, b"b".to_vec(), now).unwrap();

        assert_eq!(to_matchmaking.src_port, to_peer.src_port);
        assert_eq!(to_matchmaking.dst, matchmaking);
        assert_eq!(to_peer.dst, peer);
        assert_eq!(t.len(), 1, "устройству положен один флоу на все назначения");
    }

    #[test]
    fn first_console_keeps_its_real_port() {
        let mut t = table();
        let now = Instant::now();
        let server = addr("203.0.113.5:3074");

        let up = t
            .upstream_packet(addr("192.168.1.10:3074"), server, b"a".to_vec(), now)
            .unwrap();
        assert_eq!(up.src_port, 3074, "NAT приставок держится на настоящем порту");
    }

    #[test]
    fn substitute_port_comes_from_pool_head() {
        let now = Instant::now();

        let ports = || {
            let mut t = table();
            t.touch(addr("192.168.1.10:3074"), now).unwrap();
            (
                t.touch(addr("192.168.1.11:3074"), now).unwrap(),
                t.touch(addr("192.168.1.12:3074"), now).unwrap(),
            )
        };

        assert_eq!(ports(), (40000, 40001));
        assert_eq!(ports(), (40000, 40001), "выдача обязана быть детерминированной");
    }

    #[test]
    fn same_device_keeps_its_tunnel_port() {
        let mut t = table();
        let now = Instant::now();
        let console = addr("192.168.1.11:3074");

        t.touch(addr("192.168.1.10:3074"), now).unwrap();
        let first = t.touch(console, now).unwrap();
        let second = t.touch(console, now + Duration::from_secs(5)).unwrap();

        assert_eq!(first, second);
        assert_eq!(t.len(), 2, "второй пакет того же устройства не заводит новый флоу");
    }

    /// Разные порты одного устройства это разные флоу: на VPS они и так уходят
    /// разными сокетами, а спутать их между собой нельзя.
    #[test]
    fn two_ports_of_one_device_are_two_flows() {
        let mut t = table();
        let now = Instant::now();

        assert_eq!(t.touch(addr("192.168.1.10:3074"), now), Some(3074));
        assert_eq!(t.touch(addr("192.168.1.10:3478"), now), Some(3478));
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn expired_flow_returns_its_port_to_pool() {
        let mut t = table();
        let now = Instant::now();
        let timeout = Duration::from_secs(60);

        t.touch(addr("192.168.1.10:3074"), now).unwrap();
        assert_eq!(t.touch(addr("192.168.1.11:3074"), now), Some(40000));

        let later = now + timeout;
        assert_eq!(t.retire_expired(timeout, later), 2);
        assert_eq!(t.len(), 0);

        // Оба номера свободны снова: и настоящий порт, и подменный из пула.
        assert_eq!(t.touch(addr("192.168.1.12:3074"), later), Some(3074));
        assert_eq!(t.touch(addr("192.168.1.13:3074"), later), Some(40000));
    }

    #[test]
    fn retired_flow_is_started_over_by_next_packet() {
        let mut t = table();
        let now = Instant::now();
        let server = addr("203.0.113.5:3074");
        let console = addr("192.168.1.10:3074");
        let timeout = Duration::from_secs(60);

        t.touch(console, now).unwrap();
        let later = now + timeout;
        t.retire_expired(timeout, later);

        assert_eq!(t.touch(console, later), Some(3074));
        assert_eq!(t.len(), 1, "флоу обязан завестись заново, а не остаться ссылкой на снятый порт");
        assert_eq!(
            t.downstream_target(&data(3074, server, b"reply"), later),
            Some((console, server))
        );
    }

    #[test]
    fn live_flow_keeps_its_port_while_neighbour_expires() {
        let mut t = table();
        let now = Instant::now();
        let server = addr("203.0.113.5:3074");
        let live = addr("192.168.1.10:3074");
        let timeout = Duration::from_secs(60);

        t.touch(live, now).unwrap();
        t.touch(addr("192.168.1.11:3074"), now).unwrap();

        let later = now + timeout;
        t.touch(live, later).unwrap();
        assert_eq!(t.retire_expired(timeout, later), 1);
        assert_eq!(
            t.downstream_target(&data(3074, server, b"reply"), later),
            Some((live, server))
        );
    }

    #[test]
    fn exhausted_pool_drops_packet_instead_of_reusing_port() {
        let mut t = FlowTable::new(40000..=40000);
        let now = Instant::now();
        let server = addr("203.0.113.5:3074");

        assert_eq!(t.touch(addr("192.168.1.10:3074"), now), Some(3074));
        assert_eq!(t.touch(addr("192.168.1.11:3074"), now), Some(40000));
        assert!(t
            .upstream_packet(addr("192.168.1.12:3074"), server, b"c".to_vec(), now)
            .is_none());
        assert_eq!(t.len(), 2, "отброшенный пакет не оставляет флоу в таблице");
    }

    /// Входящий P2P: пакет пришёл от пира, которому мы не писали. Отдать его
    /// надо владельцу туннельного порта, а спуфить адрес самого пира.
    #[test]
    fn incoming_p2p_goes_to_owner_of_tunnel_port() {
        let mut t = table();
        let now = Instant::now();
        let server = addr("203.0.113.5:3074");
        let console = addr("192.168.1.11:3074");
        let peer = addr("198.51.100.7:51820");

        t.touch(addr("192.168.1.10:3074"), now).unwrap();
        let tunnel_port = t.touch(console, now).unwrap();
        t.upstream_packet(console, server, b"a".to_vec(), now).unwrap();

        assert_eq!(
            t.downstream_target(&data(tunnel_port, peer, b"hi"), now),
            Some((console, peer))
        );
    }

    #[test]
    fn reply_to_unknown_tunnel_port_is_dropped() {
        let mut t = table();
        let now = Instant::now();
        let server = addr("203.0.113.5:3074");

        t.touch(addr("192.168.1.10:3074"), now).unwrap();
        assert_eq!(t.downstream_target(&data(3478, server, b"x"), now), None);
    }

    #[test]
    fn reply_keeps_flow_alive() {
        let mut t = table();
        let now = Instant::now();
        let server = addr("203.0.113.5:3074");
        let console = addr("192.168.1.10:3074");
        let timeout = Duration::from_secs(60);

        t.touch(console, now).unwrap();
        let later = now + timeout;
        t.downstream_target(&data(3074, server, b"reply"), later);

        assert_eq!(t.retire_expired(timeout, later), 0);
    }

    fn spoof_socket() -> Arc<std::net::UdpSocket> {
        Arc::new(std::net::UdpSocket::bind("127.0.0.1:0").unwrap())
    }

    #[test]
    fn spoof_socket_survives_under_traffic() {
        let mut cache = SpoofCache::new();
        let now = Instant::now();
        let peer = addr("198.51.100.7:51820");
        let timeout = Duration::from_secs(60);

        cache.insert(peer, spoof_socket(), now);

        // Пир отвечает раз в полминуты, сокет обязан дожить.
        let mut t = now;
        for _ in 0..4 {
            t += timeout / 2;
            assert!(cache.get(peer, t).is_some());
            assert_eq!(cache.retire_idle(timeout, t), 0);
        }
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn idle_spoof_socket_is_released() {
        let mut cache = SpoofCache::new();
        let now = Instant::now();
        let quiet = addr("198.51.100.7:51820");
        let live = addr("203.0.113.5:3074");
        let timeout = Duration::from_secs(60);

        cache.insert(quiet, spoof_socket(), now);
        cache.insert(live, spoof_socket(), now);

        let later = now + timeout;
        cache.get(live, later).unwrap();
        assert_eq!(cache.retire_idle(timeout, later), 1);
        assert_eq!(cache.len(), 1);
        assert!(cache.get(quiet, later).is_none());
        assert!(cache.get(live, later).is_some());
    }
}
