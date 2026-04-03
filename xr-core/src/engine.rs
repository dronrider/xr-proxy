//! VPN Engine — the main entry point for mobile/desktop clients.
//!
//! Processes raw IP packets from TUN:
//! 1. DNS queries (UDP:53) → intercepted, responded with fake IPs
//! 2. TCP SYN → new smoltcp socket + session + relay task
//! 3. TCP data → smoltcp ↔ relay task ↔ xr-server (or direct)

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::State as TcpState;
use tokio::sync::mpsc;
use tokio::time::Duration;

use xr_proto::config::{decode_key, RoutingConfig};
use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};
use xr_proto::protocol::Codec;
use xr_proto::routing::{Action, Router};

use crate::dns::FakeDns;
use crate::ip_stack::{IpStack, PacketQueue};
use crate::session::{relay_session, ProtectSocketFn, SessionContext, TcpSessionKey};
use crate::state::{StateHandle, VpnState};
use crate::stats::Stats;

/// Configuration for the VPN engine.
pub struct VpnConfig {
    pub server_address: String,
    pub server_port: u16,
    pub obfuscation_key: String,
    pub modifier: String,
    pub salt: u32,
    pub padding_min: u8,
    pub padding_max: u8,
    pub routing: RoutingConfig,
    pub geoip_path: Option<String>,
    pub on_server_down: String,
}

/// The VPN engine. Created once, started/stopped as needed.
pub struct VpnEngine {
    config: VpnConfig,
    state: StateHandle,
    stats: Stats,
    shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
}

impl VpnEngine {
    pub fn new(config: VpnConfig) -> Self {
        Self {
            config,
            state: StateHandle::new(),
            stats: Stats::new(),
            shutdown_tx: None,
        }
    }

    pub fn state(&self) -> &StateHandle {
        &self.state
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Start the VPN engine.
    ///
    /// `protect_socket` is called with socket fd before connecting — on Android
    /// this calls VpnService.protect() so the socket bypasses the TUN.
    pub fn start(&mut self, queue: PacketQueue, protect_socket: ProtectSocketFn) -> io::Result<()> {
        if self.shutdown_tx.is_some() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "already running"));
        }

        self.state.set(VpnState::Connecting);

        let key = decode_key(&self.config.obfuscation_key)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let strategy = ModifierStrategy::from_str(&self.config.modifier)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unknown modifier"))?;
        let obfuscator = Obfuscator::new(key, self.config.salt, strategy);
        let codec = Codec::new(obfuscator, self.config.padding_min, self.config.padding_max);

        let router = Router::new(&self.config.routing, self.config.geoip_path.as_deref());

        let server_addr: SocketAddr = format!("{}:{}", self.config.server_address, self.config.server_port)
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("bad server addr: {}", e)))?;

        let on_server_down = Action::from_str(&self.config.on_server_down);
        let fake_dns = Arc::new(FakeDns::new());

        let ctx = Arc::new(SessionContext {
            router,
            codec,
            server_addr,
            fake_dns: fake_dns.clone(),
            stats: self.stats.clone(),
            on_server_down,
            protect_socket,
        });

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);

        let state = self.state.clone();
        let stats = self.stats.clone();

        tokio::spawn(async move {
            stats.mark_started();
            state.set(VpnState::Connected);

            if let Err(e) = run_event_loop(queue, ctx, fake_dns, shutdown_rx).await {
                tracing::error!("VPN engine error: {}", e);
                state.set(VpnState::Error(e.to_string()));
            } else {
                state.set(VpnState::Disconnected);
            }
        });

        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            self.state.set(VpnState::Disconnecting);
            let _ = tx.send(true);
        }
    }

    pub fn is_running(&self) -> bool {
        self.shutdown_tx.is_some()
    }
}

// ── Session tracking ────────────────────────────────────────────────

struct ActiveSession {
    smol_handle: SocketHandle,
    /// Channel to send data FROM smoltcp TO the relay task.
    to_relay: mpsc::Sender<Vec<u8>>,
    /// Channel to receive data FROM the relay task TO smoltcp.
    from_relay: mpsc::Receiver<Vec<u8>>,
}

// ── Event loop ──────────────────────────────────────────────────────

async fn run_event_loop(
    queue: PacketQueue,
    ctx: Arc<SessionContext>,
    fake_dns: Arc<FakeDns>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> io::Result<()> {
    let mut stack = IpStack::new(queue.clone());
    let mut sessions: HashMap<TcpSessionKey, ActiveSession> = HashMap::new();
    let mut stale_keys: Vec<TcpSessionKey> = Vec::new();

    tracing::info!("VPN engine event loop started");

    loop {
        // ── 1. Check shutdown ───────────────────────────────────────
        if *shutdown_rx.borrow() {
            tracing::info!("VPN engine shutting down");
            return Ok(());
        }

        // ── 2. Pre-process inbound packets: intercept DNS ───────────
        // Drain inbound queue, intercept DNS, re-queue non-DNS packets.
        let mut non_dns_packets = Vec::new();
        while let Some(packet) = queue.pop_inbound_public() {
            if let Some(dns_response) = try_handle_dns(&packet, &fake_dns) {
                ctx.stats.add_dns_query();
                queue.push_outbound_public(dns_response);
            } else {
                // Not DNS — will be processed by smoltcp.
                non_dns_packets.push(packet);
            }
        }
        // Re-queue non-DNS packets for smoltcp.
        for pkt in non_dns_packets {
            // Before feeding to smoltcp: if it's a TCP SYN to a new destination,
            // create a smoltcp listening socket so it can accept the connection.
            if let Some(key) = detect_tcp_syn(&pkt) {
                if !sessions.contains_key(&key) {
                    // Create smoltcp socket to handle this connection.
                    // Buffer sizes: 32KB each — reasonable for mobile.
                    let handle = stack.add_tcp_socket(32768, 32768);
                    let socket = stack.tcp_socket_mut(handle);
                    // Listen on dst IP:port. Convert std Ipv4Addr → smoltcp Ipv4Address.
                    let listen_ep = match key.dst_addr {
                        SocketAddr::V4(v4) => smoltcp::wire::IpListenEndpoint {
                            addr: Some(smoltcp::wire::Ipv4Address::new(
                                v4.ip().octets()[0], v4.ip().octets()[1],
                                v4.ip().octets()[2], v4.ip().octets()[3],
                            ).into()),
                            port: v4.port(),
                        },
                        SocketAddr::V6(_) => {
                            stack.remove_socket(handle);
                            queue.push_inbound(pkt);
                            continue;
                        }
                    };
                    if socket.listen(listen_ep).is_ok() {
                        // Create channels for relay.
                        let (to_relay_tx, to_relay_rx) = mpsc::channel(256);
                        let (from_relay_tx, from_relay_rx) = mpsc::channel(256);

                        sessions.insert(key, ActiveSession {
                            smol_handle: handle,
                            to_relay: to_relay_tx,
                            from_relay: from_relay_rx,
                        });

                        // Spawn relay task.
                        let ctx_clone = ctx.clone();
                        let key_clone = key;
                        tokio::spawn(async move {
                            match relay_session(ctx_clone.clone(), key_clone, to_relay_rx, from_relay_tx).await {
                                Ok(()) => {}
                                Err(e) => {
                                    ctx_clone.stats.add_relay_error();
                                    ctx_clone.stats.set_debug(format!(
                                        "relay err: {} -> {}: {}",
                                        key_clone.src_addr, key_clone.dst_addr, e
                                    ));
                                }
                            }
                        });

                        ctx.stats.connection_opened();
                        ctx.stats.add_tcp_syn();
                    } else {
                        stack.remove_socket(handle);
                    }
                }
            }

            queue.push_inbound(pkt);
        }

        // ── 3. Poll smoltcp ─────────────────────────────────────────
        stack.poll();

        // ── 4. Transfer data: smoltcp sockets ↔ relay tasks ────────
        stale_keys.clear();

        for (key, session) in sessions.iter_mut() {
            let socket = stack.tcp_socket_mut(session.smol_handle);

            // smoltcp → relay task (upload direction).
            if socket.can_recv() {
                let mut buf = vec![0u8; 16384];
                match socket.recv_slice(&mut buf) {
                    Ok(n) if n > 0 => {
                        buf.truncate(n);
                        ctx.stats.add_smol_recv(n as u64);
                        let _ = session.to_relay.try_send(buf);
                    }
                    _ => {}
                }
            }

            // relay task → smoltcp (download direction).
            while socket.can_send() {
                match session.from_relay.try_recv() {
                    Ok(data) => {
                        ctx.stats.add_smol_send(data.len() as u64);
                        let _ = socket.send_slice(&data);
                    }
                    Err(_) => break,
                }
            }

            // Check if connection is done.
            let state = socket.state();
            if state == TcpState::Closed
                || state == TcpState::TimeWait
                || (state == TcpState::CloseWait && !socket.can_recv())
            {
                stale_keys.push(*key);
            }
        }

        // ── 5. Clean up closed sessions ─────────────────────────────
        for key in &stale_keys {
            if let Some(session) = sessions.remove(key) {
                stack.remove_socket(session.smol_handle);
                ctx.stats.connection_closed();
                tracing::debug!("Session closed: {} -> {}", key.src_addr, key.dst_addr);
            }
        }

        // ── 6. Poll smoltcp again (to send any data we queued) ──────
        stack.poll();

        // ── 7. Debug: report TCP states and device counters ─────────
        // Update debug message periodically.
        {
            let mut established = 0u32;
            let mut syn_recv = 0u32;
            let mut listen = 0u32;
            let mut other = 0u32;
            for (_key, session) in sessions.iter() {
                let state = stack.tcp_socket(session.smol_handle).state();
                match state {
                    TcpState::Established => established += 1,
                    TcpState::SynReceived => syn_recv += 1,
                    TcpState::Listen => listen += 1,
                    _ => other += 1,
                }
            }
            let rx = stack.device.rx_count;
            let tx = stack.device.tx_count;
            if sessions.len() > 0 {
                ctx.stats.set_debug(format!(
                    "dev rx:{} tx:{} | tcp L:{} SR:{} E:{} o:{} | sess:{}",
                    rx, tx, listen, syn_recv, established, other, sessions.len()
                ));
            }
        }

        // ── 8. Sleep ────────────────────────────────────────────────
        let delay = stack
            .poll_delay()
            .unwrap_or(Duration::from_millis(5))
            .min(Duration::from_millis(5));

        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown_rx.changed() => {}
        }
    }
}

// ── Packet parsing helpers ──────────────────────────────────────────

/// Try to handle a packet as a DNS query. Returns DNS response if intercepted.
fn try_handle_dns(packet: &[u8], fake_dns: &FakeDns) -> Option<Vec<u8>> {
    let (src_ip, dst_ip, protocol, ihl) = parse_ipv4_header(packet)?;

    // UDP = protocol 17.
    if protocol != 17 {
        return None;
    }

    let udp_payload = &packet[ihl..];
    let (src_port, dst_port, data_offset) = parse_udp_header(udp_payload)?;

    // Only intercept DNS (port 53).
    if dst_port != 53 {
        return None;
    }

    let dns_query = &udp_payload[data_offset..];
    let (dns_response, fake_ip) = fake_dns.handle_query(dns_query)?;

    tracing::debug!("DNS intercepted: {} -> fake IP {}",
        String::from_utf8_lossy(&dns_query[12..dns_query.len().min(60)]), fake_ip);

    // Build IP/UDP response packet: dst→src (reversed).
    Some(build_udp_response(dst_ip, src_ip, dst_port, src_port, &dns_response))
}

/// Detect a TCP SYN packet and return the session key.
fn detect_tcp_syn(packet: &[u8]) -> Option<TcpSessionKey> {
    let (src_ip, dst_ip, protocol, ihl) = parse_ipv4_header(packet)?;

    // TCP = protocol 6.
    if protocol != 6 {
        return None;
    }

    let tcp_payload = &packet[ihl..];
    if tcp_payload.len() < 14 {
        return None;
    }

    let src_port = u16::from_be_bytes([tcp_payload[0], tcp_payload[1]]);
    let dst_port = u16::from_be_bytes([tcp_payload[2], tcp_payload[3]]);
    let flags = tcp_payload[13];

    // SYN flag set, ACK flag not set → new connection.
    let syn = flags & 0x02 != 0;
    let ack = flags & 0x10 != 0;

    if syn && !ack {
        Some(TcpSessionKey {
            src_addr: SocketAddr::new(IpAddr::V4(src_ip), src_port),
            dst_addr: SocketAddr::new(IpAddr::V4(dst_ip), dst_port),
        })
    } else {
        None
    }
}

/// Parse an IPv4 packet header. Returns (src, dst, protocol, header_len).
pub fn parse_ipv4_header(packet: &[u8]) -> Option<(Ipv4Addr, Ipv4Addr, u8, usize)> {
    if packet.len() < 20 {
        return None;
    }
    if packet[0] >> 4 != 4 {
        return None;
    }
    let ihl = (packet[0] & 0x0F) as usize * 4;
    if ihl < 20 || packet.len() < ihl {
        return None;
    }
    let protocol = packet[9];
    let src = Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]);
    let dst = Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]);
    Some((src, dst, protocol, ihl))
}

/// Parse UDP header. Returns (src_port, dst_port, data_offset).
pub fn parse_udp_header(payload: &[u8]) -> Option<(u16, u16, usize)> {
    if payload.len() < 8 {
        return None;
    }
    let src_port = u16::from_be_bytes([payload[0], payload[1]]);
    let dst_port = u16::from_be_bytes([payload[2], payload[3]]);
    Some((src_port, dst_port, 8))
}

/// Parse TCP ports from TCP header.
pub fn parse_tcp_ports(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() < 4 {
        return None;
    }
    Some((
        u16::from_be_bytes([payload[0], payload[1]]),
        u16::from_be_bytes([payload[2], payload[3]]),
    ))
}

/// Build an IPv4/UDP response packet.
pub fn build_udp_response(
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let total_len = 20 + 8 + payload.len();
    let mut pkt = vec![0u8; total_len];

    // IPv4 header.
    pkt[0] = 0x45;
    pkt[2..4].copy_from_slice(&(total_len as u16).to_be_bytes());
    pkt[8] = 64; // TTL
    pkt[9] = 17; // UDP
    pkt[12..16].copy_from_slice(&src_ip.octets());
    pkt[16..20].copy_from_slice(&dst_ip.octets());
    let cksum = ipv4_checksum(&pkt[..20]);
    pkt[10..12].copy_from_slice(&cksum.to_be_bytes());

    // UDP header.
    pkt[20..22].copy_from_slice(&src_port.to_be_bytes());
    pkt[22..24].copy_from_slice(&dst_port.to_be_bytes());
    pkt[24..26].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());

    // UDP payload.
    pkt[28..].copy_from_slice(payload);

    pkt
}

fn ipv4_checksum(header: &[u8]) -> u16 {
    let mut sum = 0u32;
    for i in (0..header.len()).step_by(2) {
        if i == 10 { continue; } // skip checksum field
        let word = if i + 1 < header.len() {
            u16::from_be_bytes([header[i], header[i + 1]])
        } else {
            u16::from_be_bytes([header[i], 0])
        };
        sum += word as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ipv4_header() {
        let mut pkt = vec![0u8; 40];
        pkt[0] = 0x45;
        pkt[9] = 6;
        pkt[12..16].copy_from_slice(&[10, 0, 0, 2]);
        pkt[16..20].copy_from_slice(&[93, 184, 216, 34]);
        let (src, dst, proto, ihl) = parse_ipv4_header(&pkt).unwrap();
        assert_eq!(src, Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(dst, Ipv4Addr::new(93, 184, 216, 34));
        assert_eq!(proto, 6);
        assert_eq!(ihl, 20);
    }

    #[test]
    fn test_detect_tcp_syn() {
        // Build a minimal TCP SYN packet.
        let mut pkt = vec![0u8; 40];
        pkt[0] = 0x45; // IPv4, IHL=5
        pkt[2..4].copy_from_slice(&40u16.to_be_bytes()); // total length
        pkt[9] = 6; // TCP
        pkt[12..16].copy_from_slice(&[10, 0, 0, 2]); // src IP
        pkt[16..20].copy_from_slice(&[198, 18, 0, 1]); // dst IP (fake)
        // TCP header at offset 20:
        pkt[20..22].copy_from_slice(&12345u16.to_be_bytes()); // src port
        pkt[22..24].copy_from_slice(&443u16.to_be_bytes()); // dst port
        pkt[32] = 0x50; // data offset = 5 words
        pkt[33] = 0x02; // flags: SYN

        let key = detect_tcp_syn(&pkt).unwrap();
        assert_eq!(key.src_addr, SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 12345));
        assert_eq!(key.dst_addr, SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1)), 443));
    }

    #[test]
    fn test_detect_tcp_non_syn() {
        // TCP ACK packet (not SYN) should not be detected.
        let mut pkt = vec![0u8; 40];
        pkt[0] = 0x45;
        pkt[9] = 6;
        pkt[12..16].copy_from_slice(&[10, 0, 0, 2]);
        pkt[16..20].copy_from_slice(&[198, 18, 0, 1]);
        pkt[20..22].copy_from_slice(&12345u16.to_be_bytes());
        pkt[22..24].copy_from_slice(&443u16.to_be_bytes());
        pkt[32] = 0x50;
        pkt[33] = 0x10; // ACK only

        assert!(detect_tcp_syn(&pkt).is_none());
    }

    #[test]
    fn test_build_udp_response() {
        let response = build_udp_response(
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 2),
            53, 12345, b"test",
        );
        assert_eq!(response[0] >> 4, 4);
        assert_eq!(response[9], 17);
        assert_eq!(&response[28..], b"test");
    }

    #[test]
    fn test_ipv4_checksum() {
        let mut header = vec![0u8; 20];
        header[0] = 0x45;
        header[8] = 64;
        header[9] = 17;
        header[12..16].copy_from_slice(&[10, 0, 0, 1]);
        header[16..20].copy_from_slice(&[10, 0, 0, 2]);
        let cksum = ipv4_checksum(&header);
        header[10..12].copy_from_slice(&cksum.to_be_bytes());
        let mut sum = 0u32;
        for i in (0..20).step_by(2) {
            sum += u16::from_be_bytes([header[i], header[i + 1]]) as u32;
        }
        while sum >> 16 != 0 { sum = (sum & 0xFFFF) + (sum >> 16); }
        assert_eq!(sum as u16, 0xFFFF);
    }
}
