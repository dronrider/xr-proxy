/// UDP Relay server: receive obfuscated packets from router,
/// forward to internet preserving source port, relay responses back.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, Notify};
use tokio::time::{interval, Duration};
use xr_proto::obfuscation::Obfuscator;
use xr_proto::udp_relay::{self, RelayPacket, RelayType};

// -- Flow table -------------------------------------------------------

/// Active outbound socket bound to a specific source port.
struct BoundPort {
    socket: Arc<UdpSocket>,
    #[allow(dead_code)]
    src_port: u16,
    last_activity: Instant,
}

/// Состояние слота src_port. Пока сокет поднимается, слот застолблён
/// `Binding`, и остальные пакеты того же src_port ждут его через `Notify`
/// вместо параллельного bind (XR-200: второй insert затирал `BoundPort`
/// первого, тот сокет вместе со своим receiver-таском выпадал из таблицы и
/// тёк до idle timeout).
enum PortState {
    Binding(Arc<Notify>),
    Bound(BoundPort),
}

/// A known remote endpoint for reverse mapping.
#[derive(Clone, Hash, Eq, PartialEq)]
struct RemoteKey {
    remote_addr: SocketAddr,
    local_port: u16,
}

struct FlowTable {
    /// src_port -> состояние привязки для исходящих
    bound_ports: HashMap<u16, PortState>,
    /// (remote_addr, local_port) -> client src_port for incoming
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

// -- Main entry ---------------------------------------------------------

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

    {
        let mut flows = state.flows.lock().await;
        let rkey = RemoteKey {
            remote_addr: dst,
            local_port: src_port,
        };
        flows.reverse_map.insert(rkey, src_port);
    }

    let send_socket = match acquire_bound_socket(state, relay_socket, src_port, bind_source_port).await {
        Some(sock) => sock,
        None => return,
    };

    // Send the original payload to the real destination
    if let Err(e) = send_socket.send_to(&packet.payload, dst).await {
        tracing::warn!("UDP relay: send to {} failed: {}", dst, e);
    }
}

/// Вернуть сокет, привязанный к `src_port`, поднимая его при необходимости.
/// Пакет, заставший слот пустым, резервирует его состоянием `Binding` и
/// сам выполняет bind; все остальные пакеты того же src_port, пришедшие
/// параллельно, застают резервацию и ждут её завершения через `Notify`
/// вместо собственного bind (XR-200: без резервации второй insert затирал
/// `BoundPort` первого, и его сокет с receiver-таском тёк до idle timeout).
async fn acquire_bound_socket<F, Fut>(
    state: &Arc<ServerState>,
    relay_socket: &Arc<UdpSocket>,
    src_port: u16,
    bind: F,
) -> Option<Arc<UdpSocket>>
where
    F: FnOnce(u16) -> Fut,
    Fut: Future<Output = io::Result<UdpSocket>>,
{
    // Цикл крутится, только пока слот занят чужой резервацией; bind вызывается
    // ровно один раз - в итерации, которая застаёт слот пустым и сама его
    // резервирует.
    loop {
        let notify = {
            let mut flows = state.flows.lock().await;
            match flows.bound_ports.get_mut(&src_port) {
                Some(PortState::Bound(bp)) => {
                    bp.last_activity = Instant::now();
                    return Some(bp.socket.clone());
                }
                Some(PortState::Binding(notify)) => Some(notify.clone()),
                None => {
                    flows
                        .bound_ports
                        .insert(src_port, PortState::Binding(Arc::new(Notify::new())));
                    None
                }
            }
        };

        if let Some(notify) = notify {
            // Слот уже застолблён другим пакетом - ждём его bind и переиспользуем
            // готовый сокет вместо параллельного собственного.
            notify.notified().await;
            continue;
        }
        break;
    }

    // Слот застолблён этим вызовом - поднимаем сокет вне блокировки таблицы.
    match bind(src_port).await {
        Ok(sock) => {
            let sock = Arc::new(sock);
            let recv_sock = sock.clone();

            let recv_state = state.clone();
            let recv_relay = relay_socket.clone();
            tokio::spawn(async move {
                if let Err(e) = bound_port_receiver(recv_sock, src_port, recv_state, recv_relay).await {
                    tracing::debug!("Bound port {} receiver ended: {}", src_port, e);
                }
            });

            let mut flows = state.flows.lock().await;
            let prev = flows.bound_ports.insert(
                src_port,
                PortState::Bound(BoundPort {
                    socket: sock.clone(),
                    src_port,
                    last_activity: Instant::now(),
                }),
            );
            if let Some(PortState::Binding(notify)) = prev {
                notify.notify_waiters();
            }
            tracing::info!("UDP relay: bound source port {}", src_port);
            Some(sock)
        }
        Err(e) => {
            tracing::warn!("UDP relay: failed to bind port {}: {}", src_port, e);
            let mut flows = state.flows.lock().await;
            if let Some(PortState::Binding(notify)) = flows.bound_ports.remove(&src_port) {
                notify.notify_waiters();
            }
            None
        }
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
                // Timeout - port is idle, will be cleaned up
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
            if let Some(PortState::Bound(bp)) = flows.bound_ports.get_mut(&src_port) {
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
            // Port busy - try nearby ports
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
        .filter_map(|(port, ps)| match ps {
            PortState::Bound(bp) if bp.last_activity.elapsed() > timeout => Some(*port),
            _ => None,
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;
    use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};

    fn test_state() -> Arc<ServerState> {
        Arc::new(ServerState {
            flows: Mutex::new(FlowTable::new()),
            obfuscator: Obfuscator::new(b"test-key".to_vec(), 7, ModifierStrategy::PositionalXorRotate),
            router_addr: Mutex::new(None),
            flow_timeout: Duration::from_secs(60),
            incoming_port_min: 0,
            incoming_port_max: 0,
        })
    }

    /// XR-200: два первых пакета одного src_port застают слот пустым
    /// одновременно. До фикса каждый поднимал свой сокет и второй insert в
    /// bound_ports затирал BoundPort первого - тот сокет вместе со своим
    /// receiver-таском выпадал из таблицы и жил до idle timeout, никем не
    /// используемый. Тест держит первый bind в подвешенном состоянии и
    /// проверяет, что второй вызов в это время не поднимает собственный
    /// сокет, а дожидается первого и переиспользует его.
    #[tokio::test]
    async fn concurrent_first_packets_share_single_bind() {
        let state = test_state();
        let relay_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let src_port = 40777;

        let bind_a_calls = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = oneshot::channel::<()>();
        let (release_tx, release_rx) = oneshot::channel::<()>();

        let calls_a = bind_a_calls.clone();
        let bind_a = move |port: u16| async move {
            calls_a.fetch_add(1, Ordering::SeqCst);
            let _ = started_tx.send(());
            let _ = release_rx.await;
            bind_source_port(port).await
        };

        let task_a = tokio::spawn({
            let state = state.clone();
            let relay_socket = relay_socket.clone();
            async move { acquire_bound_socket(&state, &relay_socket, src_port, bind_a).await }
        });

        // Дождаться, что первый пакет застолбил слот и вошёл в свой bind -
        // без этого нет гарантии, что второй пакет вообще застанет резервацию.
        started_rx.await.unwrap();

        let bind_b_calls = Arc::new(AtomicUsize::new(0));
        let calls_b = bind_b_calls.clone();
        let bind_b = move |_port: u16| {
            calls_b.fetch_add(1, Ordering::SeqCst);
            async move { unreachable!("второй пакет не должен поднимать свой bind, пока первый ещё в процессе") }
        };

        let task_b = tokio::spawn({
            let state = state.clone();
            let relay_socket = relay_socket.clone();
            async move { acquire_bound_socket(&state, &relay_socket, src_port, bind_b).await }
        });

        // Отпустить первый bind - только теперь он завершится и разбудит второй.
        release_tx.send(()).unwrap();

        let sock_a = task_a.await.unwrap().expect("первый пакет обязан получить сокет");
        let sock_b = task_b.await.unwrap().expect("второй пакет обязан получить сокет");

        assert_eq!(bind_a_calls.load(Ordering::SeqCst), 1, "bind поднимается ровно один раз");
        assert_eq!(bind_b_calls.load(Ordering::SeqCst), 0, "второй пакет не должен поднимать свой bind");
        assert!(Arc::ptr_eq(&sock_a, &sock_b), "оба пакета обязаны получить один и тот же сокет");

        let flows = state.flows.lock().await;
        assert_eq!(flows.bound_ports.len(), 1, "в таблице обязан остаться единственный слот на src_port");
    }
}
