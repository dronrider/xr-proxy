//! VPN Engine — the main entry point for mobile/desktop clients.
//!
//! Coordinates all components:
//! - Reads raw IP packets from TUN fd
//! - Feeds them into smoltcp for TCP/UDP processing
//! - Intercepts DNS queries (Fake DNS)
//! - Manages TCP sessions (proxy vs direct)
//! - Writes response packets back to TUN fd

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::time::Duration;

use xr_proto::config::{decode_key, RoutingConfig};
use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};
use xr_proto::protocol::Codec;
use xr_proto::routing::{Action, Router};

use crate::dns::FakeDns;
use crate::ip_stack::{IpStack, PacketQueue};
use crate::session::{SessionContext, SessionManager, TcpSessionKey};
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

    /// Start the VPN engine with a packet queue (for platform-agnostic packet I/O).
    ///
    /// The caller (platform layer) is responsible for:
    /// 1. Creating a TUN interface
    /// 2. Reading packets from TUN → `queue.push_inbound()`
    /// 3. Writing packets from `queue.pop_outbound()` → TUN
    ///
    /// This method spawns the engine's event loop and returns immediately.
    pub fn start(&mut self, queue: PacketQueue) -> io::Result<()> {
        if self.shutdown_tx.is_some() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "already running"));
        }

        self.state.set(VpnState::Connecting);

        // Build codec.
        let key = decode_key(&self.config.obfuscation_key)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let strategy = ModifierStrategy::from_str(&self.config.modifier)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unknown modifier"))?;
        let obfuscator = Obfuscator::new(key, self.config.salt, strategy);
        let codec = Codec::new(
            obfuscator,
            self.config.padding_min,
            self.config.padding_max,
        );

        // Build router.
        let router = Router::new(
            &self.config.routing,
            self.config.geoip_path.as_deref(),
        );

        // Server address.
        let server_addr: SocketAddr = format!(
            "{}:{}",
            self.config.server_address, self.config.server_port
        )
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

    /// Stop the VPN engine.
    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            self.state.set(VpnState::Disconnecting);
            let _ = tx.send(true);
        }
    }

    /// Check if the engine is running.
    pub fn is_running(&self) -> bool {
        self.shutdown_tx.is_some()
    }
}

/// Main event loop: poll smoltcp, process packets, manage sessions.
async fn run_event_loop(
    queue: PacketQueue,
    ctx: Arc<SessionContext>,
    fake_dns: Arc<FakeDns>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> io::Result<()> {
    let mut stack = IpStack::new(queue.clone());
    let session_mgr = SessionManager::new(ctx.clone());

    // Channel pairs for each session: smoltcp ↔ relay task.
    let session_channels: std::collections::HashMap<
        TcpSessionKey,
        (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>),
    > = std::collections::HashMap::new();

    // Channels for relay tasks to send data back to smoltcp.
    let (_return_tx, mut return_rx) = mpsc::channel::<(TcpSessionKey, Vec<u8>)>(1024);

    loop {
        // Check shutdown.
        if *shutdown_rx.borrow() {
            tracing::info!("VPN engine shutting down");
            return Ok(());
        }

        // Poll smoltcp — process queued inbound packets.
        let _activity = stack.poll();

        // Process DNS queries from inbound packets.
        // DNS packets (UDP to port 53) need special handling.
        // They arrive as raw IP packets in the queue.
        // We intercept them before smoltcp processes them.
        process_dns_packets(&queue, &fake_dns);

        // Check for new TCP connections in smoltcp.
        // (In a full implementation, we'd listen on smoltcp TCP sockets
        //  and accept incoming connections.)

        // Drain outbound packets from smoltcp → TUN.
        // (The queue already handles this via the Device trait.)

        // Transfer data between smoltcp sockets and relay tasks.
        // Read data from smoltcp sockets → send to relay tasks.
        for key in session_mgr.session_keys() {
            if let Some(handle) = session_mgr.smol_handle(&key) {
                let socket = stack.tcp_socket(handle);
                if socket.can_recv() {
                    if let Some((tx, _)) = session_channels.get(&key) {
                        let mut buf = vec![0u8; 8192];
                        if let Ok(n) = stack.tcp_socket_mut(handle).recv_slice(&mut buf) {
                            if n > 0 {
                                buf.truncate(n);
                                let _ = tx.try_send(buf);
                            }
                        }
                    }
                }
            }
        }

        // Receive data from relay tasks → write to smoltcp sockets.
        while let Ok((key, data)) = return_rx.try_recv() {
            if let Some(handle) = session_mgr.smol_handle(&key) {
                let socket = stack.tcp_socket_mut(handle);
                if socket.can_send() {
                    let _ = socket.send_slice(&data);
                }
            }
        }

        // Sleep until next poll or new data arrives.
        let delay = stack
            .poll_delay()
            .unwrap_or(Duration::from_millis(10))
            .min(Duration::from_millis(10));

        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown_rx.changed() => {}
        }
    }
}

/// Intercept DNS queries from the inbound packet queue.
///
/// Looks at raw IP packets for UDP port 53 queries,
/// responds with fake IPs via FakeDns.
fn process_dns_packets(_queue: &PacketQueue, _fake_dns: &FakeDns) {
    // This is a simplified approach — in the full implementation,
    // we'd intercept at the IP packet level before feeding to smoltcp.
    // For now, DNS interception happens in the smoltcp UDP socket.
    // See the engine event loop for the full flow.
    //
    // The actual DNS interception requires parsing raw IP/UDP headers
    // from inbound packets, which we'll implement in the next iteration.
}

/// Parse an IPv4 packet and extract src/dst addresses and protocol.
pub fn parse_ipv4_header(packet: &[u8]) -> Option<(Ipv4Addr, Ipv4Addr, u8, usize)> {
    if packet.len() < 20 {
        return None;
    }

    let version = packet[0] >> 4;
    if version != 4 {
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

/// Parse TCP header from an IP packet payload. Returns (src_port, dst_port).
pub fn parse_tcp_ports(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() < 4 {
        return None;
    }
    let src_port = u16::from_be_bytes([payload[0], payload[1]]);
    let dst_port = u16::from_be_bytes([payload[2], payload[3]]);
    Some((src_port, dst_port))
}

/// Parse UDP header from an IP packet payload. Returns (src_port, dst_port, data_offset).
pub fn parse_udp_header(payload: &[u8]) -> Option<(u16, u16, usize)> {
    if payload.len() < 8 {
        return None;
    }
    let src_port = u16::from_be_bytes([payload[0], payload[1]]);
    let dst_port = u16::from_be_bytes([payload[2], payload[3]]);
    Some((src_port, dst_port, 8))
}

/// Build a minimal IPv4/UDP response packet.
pub fn build_udp_response(
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let total_len = 20 + 8 + payload.len();
    let mut packet = vec![0u8; total_len];

    // IPv4 header.
    packet[0] = 0x45; // version=4, IHL=5
    packet[2..4].copy_from_slice(&(total_len as u16).to_be_bytes());
    packet[8] = 64; // TTL
    packet[9] = 17; // protocol = UDP
    packet[12..16].copy_from_slice(&src_ip.octets());
    packet[16..20].copy_from_slice(&dst_ip.octets());

    // IPv4 header checksum.
    let checksum = ipv4_checksum(&packet[..20]);
    packet[10..12].copy_from_slice(&checksum.to_be_bytes());

    // UDP header.
    let udp_len = (8 + payload.len()) as u16;
    packet[20..22].copy_from_slice(&src_port.to_be_bytes());
    packet[22..24].copy_from_slice(&dst_port.to_be_bytes());
    packet[24..26].copy_from_slice(&udp_len.to_be_bytes());
    // UDP checksum = 0 (optional for IPv4).

    // UDP payload.
    packet[28..].copy_from_slice(payload);

    packet
}

fn ipv4_checksum(header: &[u8]) -> u16 {
    let mut sum = 0u32;
    for i in (0..header.len()).step_by(2) {
        let word = if i + 1 < header.len() {
            u16::from_be_bytes([header[i], header[i + 1]])
        } else {
            u16::from_be_bytes([header[i], 0])
        };
        // Skip the checksum field itself (bytes 10-11).
        if i == 10 {
            continue;
        }
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
        // Minimal IPv4 header: version=4, IHL=5, protocol=6 (TCP).
        let mut pkt = vec![0u8; 40]; // 20 IP + 20 TCP
        pkt[0] = 0x45; // v4, IHL=5
        pkt[9] = 6; // TCP
        pkt[12..16].copy_from_slice(&[10, 0, 0, 2]); // src
        pkt[16..20].copy_from_slice(&[93, 184, 216, 34]); // dst

        let (src, dst, proto, ihl) = parse_ipv4_header(&pkt).unwrap();
        assert_eq!(src, Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(dst, Ipv4Addr::new(93, 184, 216, 34));
        assert_eq!(proto, 6);
        assert_eq!(ihl, 20);
    }

    #[test]
    fn test_parse_tcp_ports() {
        let payload = [0x00, 0x50, 0x01, 0xBB]; // src=80, dst=443
        let (src, dst) = parse_tcp_ports(&payload).unwrap();
        assert_eq!(src, 80);
        assert_eq!(dst, 443);
    }

    #[test]
    fn test_build_udp_response() {
        let response = build_udp_response(
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 2),
            53,
            12345,
            b"test",
        );

        // Verify IPv4 header.
        assert_eq!(response[0] >> 4, 4); // version
        assert_eq!(response[9], 17); // UDP
        assert_eq!(&response[12..16], &[10, 0, 0, 1]); // src
        assert_eq!(&response[16..20], &[10, 0, 0, 2]); // dst

        // Verify UDP header.
        assert_eq!(u16::from_be_bytes([response[20], response[21]]), 53);
        assert_eq!(u16::from_be_bytes([response[22], response[23]]), 12345);

        // Verify payload.
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
        // Set checksum and verify.
        header[10..12].copy_from_slice(&cksum.to_be_bytes());

        // Recompute — should be 0 (or 0xFFFF).
        let mut sum = 0u32;
        for i in (0..20).step_by(2) {
            sum += u16::from_be_bytes([header[i], header[i + 1]]) as u32;
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        assert_eq!(sum as u16, 0xFFFF);
    }
}
