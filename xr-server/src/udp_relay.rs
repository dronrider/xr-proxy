/// UDP Relay server: receive obfuscated packets from router,
/// forward to internet preserving source port, relay responses back.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::time::{interval, Duration};
use xr_proto::obfuscation::Obfuscator;
use xr_proto::udp_relay::{self, RelayPacket, RelayType};

// ── Flow table ─────────────────────────────────────────────────────

/// Active outbound socket bound to a specific source port.
struct BoundPort {
    socket: Arc<UdpSocket>,
    #[allow(dead_code)]
    src_port: u16,
    last_activity: Instant,
}

/// A known remote endpoint for reverse mapping.
#[derive(Clone, Hash, Eq, PartialEq)]
struct RemoteKey {
    remote_addr: SocketAddr,
    local_port: u16,
}

struct FlowTable {
    /// src_port → bound socket for outgoing
    bound_ports: HashMap<u16, BoundPort>,
    /// (remote_addr, local_port) → client src_port for incoming
    reverse_map: HashMap<RemoteKey, u16>,
}

impl FlowTable {
    fn new() -> Self {
        Self {
            bound_ports: HashMap::new(),
            reverse_map: HashMap::new(),
        }
    }
}

struct ServerState {
    flows: Mutex<FlowTable>,
    obfuscator: Obfuscator,
    router_addr: Mutex<Option<SocketAddr>>,
    flow_timeout: Duration,
    #[allow(dead_code)]
    incoming_port_min: u16,
    #[allow(dead_code)]
    incoming_port_max: u16,
}

// ── Main entry ─────────────────────────────────────────────────────

pub async fn run_udp_relay_server(
    listen_port: u16,
    obfuscator: Obfuscator,
    flow_timeout_sec: u64,
    incoming_port_min: u16,
    incoming_port_max: u16,
) -> io::Result<()> {
    let listen_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, listen_port));
    let relay_socket = Arc::new(UdpSocket::bind(listen_addr).await?);
    tracing::info!("UDP relay server listening on {}", listen_addr);

    let state = Arc::new(ServerState {
        flows: Mutex::new(FlowTable::new()),
        obfuscator,
        router_addr: Mutex::new(None),
        flow_timeout: Duration::from_secs(flow_timeout_sec),
        incoming_port_min,
        incoming_port_max,
    });

    // Cleanup expired flows
    let clean_state = state.clone();
    tokio::spawn(async move {
        let mut timer = interval(Duration::from_secs(30));
        loop {
            timer.tick().await;
            cleanup_flows(&clean_state).await;
        }
    });

    // Main receive loop from router
    let mut buf = vec![0u8; 65536];
    loop {
        let (n, peer_addr) = relay_socket.recv_from(&mut buf).await?;
        if n == 0 {
            continue;
        }

        let packet = match udp_relay::decode_relay_packet(&state.obfuscator, &buf[..n]) {
            Some(p) => p,
            None => {
                tracing::debug!("UDP relay server: invalid packet from {}", peer_addr);
                continue;
            }
        };

        // Remember router address for sending responses back
        {
            let mut ra = state.router_addr.lock().await;
            if ra.as_ref() != Some(&peer_addr) {
                tracing::info!("UDP relay server: router at {}", peer_addr);
                *ra = Some(peer_addr);
            }
        }

        match packet.relay_type {
            RelayType::Keepalive => {
                // Reply with keepalive
                let reply = udp_relay::encode_keepalive(&state.obfuscator);
                let _ = relay_socket.send_to(&reply, peer_addr).await;
            }
            RelayType::Data => {
                handle_data_packet(
                    &state,
                    &relay_socket,
                    peer_addr,
                    packet,
                ).await;
            }
            _ => {}
        }
    }
}

/// Handle an incoming data packet from the router.
async fn handle_data_packet(
    state: &Arc<ServerState>,
    relay_socket: &Arc<UdpSocket>,
    _router_addr: SocketAddr,
    packet: RelayPacket,
) {
    let src_port = packet.src_port;
    let dst = packet.dst;

    // Get or create a bound socket for this source port
    let send_socket = {
        let mut flows = state.flows.lock().await;

        // Update reverse map
        let rkey = RemoteKey {
            remote_addr: dst,
            local_port: src_port,
        };
        flows.reverse_map.insert(rkey, src_port);

        if let Some(bp) = flows.bound_ports.get_mut(&src_port) {
            bp.last_activity = Instant::now();
            bp.socket.clone()
        } else {
            // Need to bind a new socket to this source port
            drop(flows); // release lock before async bind
            match bind_source_port(src_port).await {
                Ok(sock) => {
                    let sock = Arc::new(sock);
                    let recv_sock = sock.clone();

                    // Start receiver task for this bound port
                    let recv_state = state.clone();
                    let recv_relay = relay_socket.clone();
                    tokio::spawn(async move {
                        if let Err(e) = bound_port_receiver(
                            recv_sock,
                            src_port,
                            recv_state,
                            recv_relay,
                        ).await {
                            tracing::debug!("Bound port {} receiver ended: {}", src_port, e);
                        }
                    });

                    let mut flows = state.flows.lock().await;
                    flows.bound_ports.insert(src_port, BoundPort {
                        socket: sock.clone(),
                        src_port,
                        last_activity: Instant::now(),
                    });
                    tracing::info!("UDP relay: bound source port {}", src_port);
                    sock
                }
                Err(e) => {
                    tracing::warn!("UDP relay: failed to bind port {}: {}", src_port, e);
                    return;
                }
            }
        }
    };

    // Send the original payload to the real destination
    if let Err(e) = send_socket.send_to(&packet.payload, dst).await {
        tracing::warn!("UDP relay: send to {} failed: {}", dst, e);
    }
}

/// Receiver task for a bound source port.
/// Listens for responses from the internet and relays them back to the router.
async fn bound_port_receiver(
    socket: Arc<UdpSocket>,
    src_port: u16,
    state: Arc<ServerState>,
    relay_socket: Arc<UdpSocket>,
) -> io::Result<()> {
    let mut buf = vec![0u8; 65536];

    loop {
        let result = tokio::time::timeout(
            state.flow_timeout,
            socket.recv_from(&mut buf),
        ).await;

        let (n, from_addr) = match result {
            Ok(Ok((n, addr))) => (n, addr),
            Ok(Err(e)) => {
                tracing::debug!("Bound port {} recv error: {}", src_port, e);
                continue;
            }
            Err(_) => {
                // Timeout — port is idle, will be cleaned up
                tracing::debug!("Bound port {} idle timeout", src_port);
                return Ok(());
            }
        };

        if n == 0 {
            continue;
        }

        // Get router address
        let router_addr = {
            let ra = state.router_addr.lock().await;
            match *ra {
                Some(addr) => addr,
                None => {
                    tracing::debug!("No router address known, dropping response");
                    continue;
                }
            }
        };

        // Wrap response and send back to router
        let response = RelayPacket {
            relay_type: RelayType::Data,
            dst: from_addr,
            src_port,
            payload: buf[..n].to_vec(),
        };
        let wire = udp_relay::encode_relay_packet(&state.obfuscator, &response);
        if let Err(e) = relay_socket.send_to(&wire, router_addr).await {
            tracing::warn!("UDP relay: send response to router failed: {}", e);
        }

        // Update flow activity
        {
            let mut flows = state.flows.lock().await;
            if let Some(bp) = flows.bound_ports.get_mut(&src_port) {
                bp.last_activity = Instant::now();
            }
            // Update reverse mapping for this remote
            let rkey = RemoteKey {
                remote_addr: from_addr,
                local_port: src_port,
            };
            flows.reverse_map.insert(rkey, src_port);
        }
    }
}

/// Bind a UDP socket to a specific source port.
async fn bind_source_port(port: u16) -> io::Result<UdpSocket> {
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port));

    match UdpSocket::bind(addr).await {
        Ok(sock) => Ok(sock),
        Err(e) => {
            // Port busy — try nearby ports
            tracing::warn!("Port {} busy ({}), trying nearby", port, e);
            for offset in 1..=10 {
                let try_port = port.wrapping_add(offset);
                if try_port == 0 {
                    continue;
                }
                let try_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, try_port));
                if let Ok(sock) = UdpSocket::bind(try_addr).await {
                    tracing::info!("Bound to fallback port {} (wanted {})", try_port, port);
                    return Ok(sock);
                }
            }
            Err(e)
        }
    }
}

/// Remove expired flows and their bound sockets.
async fn cleanup_flows(state: &ServerState) {
    let mut flows = state.flows.lock().await;
    let timeout = state.flow_timeout;

    let expired: Vec<u16> = flows
        .bound_ports
        .iter()
        .filter(|(_, bp)| bp.last_activity.elapsed() > timeout)
        .map(|(port, _)| *port)
        .collect();

    for port in &expired {
        flows.bound_ports.remove(port);
        flows.reverse_map.retain(|_, src| src != port);
        tracing::debug!("UDP relay: released port {}", port);
    }

    if !expired.is_empty() {
        tracing::info!(
            "UDP relay: cleaned {} expired ports ({} active)",
            expired.len(),
            flows.bound_ports.len()
        );
    }
}
