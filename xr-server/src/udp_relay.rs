/// UDP Relay server: receive obfuscated packets from router,
/// forward to internet preserving source port, relay responses back.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
// Под мок-часами тестов свежесть записи обязана идти по виртуальному времени,
// иначе времена слипаются и жертва вытеснения выбирается произвольно.
use tokio::time::{Duration, Instant};
use xr_proto::obfuscation::Obfuscator;
use xr_proto::udp_relay::{self, RelayPacket, RelayType};

// -- Flow table -------------------------------------------------------

/// Сколько пакетов держит очередь одного потока. Запас нужен на время bind:
/// пока сокет поднимается, приём продолжает складывать сюда пакеты. Дальше
/// таск разгребает очередь быстрее, чем она набирается, а переполнение значит,
/// что назначение не успевает принимать - такой пакет отбрасываем, как отбросил
/// бы его любой промежуточный узел.
const FLOW_QUEUE: usize = 64;

/// Пакет из туннеля, поставленный в очередь своему потоку.
struct FlowPacket {
    dst: SocketAddr,
    payload: Vec<u8>,
}

/// Поток одного src_port со стороны таблицы: отправной конец очереди и время
/// последнего пакета в неё. Сокет и его судьба целиком у таска потока,
/// поэтому владелец сокета всегда ровно один и второй bind того же src_port
/// сделать просто некому.
struct Flow {
    tx: mpsc::Sender<FlowPacket>,
    last_seen: Instant,
}

/// Чей это поток: адрес пира из туннеля плюс его src_port. Ключ обфускации на
/// VPS общий, поэтому на relay-порт пишет не один роутер, а любой, кто знает
/// ключ. Одного src_port в ключе мало: два роутера с приставкой на 3074 делили
/// бы поток, а ответ из интернета уходил бы кому попало.
type FlowKey = (SocketAddr, u16);

struct ServerState {
    /// (пир, src_port) -> очередь его потока
    flows: Mutex<HashMap<FlowKey, Flow>>,
    obfuscator: Obfuscator,
    flow_timeout: Duration,
    /// Жёсткий потолок числа потоков: между уходами по простою таблица растёт
    /// без границы, а каждый поток это ещё и таск с сокетом.
    max_flows: usize,
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
    max_flows: usize,
    incoming_port_min: u16,
    incoming_port_max: u16,
) -> io::Result<()> {
    let listen_addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, listen_port));
    let relay_socket = Arc::new(UdpSocket::bind(listen_addr).await?);
    tracing::info!("UDP relay server listening on {}", listen_addr);

    let state = Arc::new(ServerState {
        flows: Mutex::new(HashMap::new()),
        obfuscator,
        flow_timeout: Duration::from_secs(flow_timeout_sec),
        max_flows,
        incoming_port_min,
        incoming_port_max,
    });

    // Main receive loop from router
    let mut buf = vec![0u8; 65536];
    loop {
        let (n, peer_addr) = relay_socket.recv_from(&mut buf).await?;
        if n == 0 {
            continue;
        }

        handle_datagram(&state, &relay_socket, peer_addr, &buf[..n], bind_source_port).await;
    }
}

/// Разобрать датаграмму с relay-порта и раздать её по назначению. Ключ
/// обфускации на VPS общий, поэтому мусор и чужие пакеты тут дело обычное:
/// нерасшифровавшееся уходит в лог и дальше не идёт.
async fn handle_datagram<F, Fut>(
    state: &Arc<ServerState>,
    relay_socket: &Arc<UdpSocket>,
    peer: SocketAddr,
    data: &[u8],
    bind: F,
) where
    F: FnOnce(u16) -> Fut + Send + 'static,
    Fut: Future<Output = io::Result<UdpSocket>> + Send,
{
    let packet = match udp_relay::decode_relay_packet(&state.obfuscator, data) {
        Some(p) => p,
        None => {
            tracing::debug!("UDP relay server: invalid packet from {}", peer);
            return;
        }
    };

    match packet.relay_type {
        RelayType::Keepalive => {
            // Keepalive отвечает написавшему: он и держит NAT роутера открытым.
            let reply = udp_relay::encode_keepalive(&state.obfuscator);
            let _ = relay_socket.send_to(&reply, peer).await;
        }
        RelayType::Data => {
            handle_data_packet(state, relay_socket, peer, packet, bind).await;
        }
        _ => {}
    }
}

/// Положить пакет в очередь его потока, подняв поток, если пакет первый.
/// Ждать тут нечего: bind нового src_port уходит внутрь таска, и приём
/// продолжает разбирать пакеты остальных потоков, пока сокет поднимается.
/// Очередь заводится и попадает в таблицу под тем же локом, под которым в неё
/// кладут пакеты, поэтому порядок внутри потока сохраняется, а два первых
/// пакета одного src_port не могут завести себе по потоку.
async fn handle_data_packet<F, Fut>(
    state: &Arc<ServerState>,
    relay_socket: &Arc<UdpSocket>,
    peer: SocketAddr,
    packet: RelayPacket,
    bind: F,
) where
    F: FnOnce(u16) -> Fut + Send + 'static,
    Fut: Future<Output = io::Result<UdpSocket>> + Send,
{
    let src_port = packet.src_port;
    let key = (peer, src_port);
    let queued = FlowPacket {
        dst: packet.dst,
        payload: packet.payload,
    };

    let mut flows = state.flows.lock().await;
    let tx = match flows.get_mut(&key) {
        Some(flow) => {
            flow.last_seen = Instant::now();
            flow.tx.clone()
        }
        None => {
            if flows.len() >= state.max_flows {
                // Потолок таблицы. Отказ новому оставил бы его в руках одного
                // пишущего до конца flow_timeout, поэтому вытесняем наименее
                // свежий поток: чистка по простою сняла бы его первым. Слот
                // уходит из таблицы вместе с отправным концом очереди, таск
                // потока видит закрытый канал и выходит тем же путём, что и по
                // простою, забирая сокет с собой.
                let victim = flows
                    .iter()
                    .min_by_key(|(_, flow)| flow.last_seen)
                    .map(|(key, _)| *key);
                if let Some(victim) = victim {
                    flows.remove(&victim);
                    tracing::warn!(
                        "UDP relay: flow table full ({} flows), evicted flow {} of {}",
                        state.max_flows,
                        victim.1,
                        victim.0
                    );
                }
            }
            let (tx, rx) = mpsc::channel(FLOW_QUEUE);
            flows.insert(
                key,
                Flow {
                    tx: tx.clone(),
                    last_seen: Instant::now(),
                },
            );

            let flow_state = state.clone();
            let flow_relay = relay_socket.clone();
            tokio::spawn(async move {
                run_flow(flow_state, flow_relay, key, rx, bind).await;
            });
            tx
        }
    };

    if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(queued) {
        tracing::debug!(
            "UDP relay: flow {} of {} queue full, packet dropped",
            src_port,
            peer
        );
    }
}

/// Что случилось с потоком раньше: пакет из туннеля наружу либо ответ из
/// интернета обратно роутеру.
enum FlowEvent {
    /// `None` значит, что слот потока сняли и класть в очередь больше некому.
    Outbound(Option<FlowPacket>),
    Inbound(io::Result<(usize, SocketAddr)>),
}

/// Таск одного потока: поднимает сокет на src_port и дальше сам гоняет обе
/// стороны, пока поток жив. Наружу отправляет пакеты из очереди по порядку,
/// обратно заворачивает ответы тому пиру, который поток завёл. Слот в таблице
/// снимает он же, так что осиротеть сокету не с чего: сокет умирает вместе с
/// таском.
async fn run_flow<F, Fut>(
    state: Arc<ServerState>,
    relay_socket: Arc<UdpSocket>,
    key: FlowKey,
    mut rx: mpsc::Receiver<FlowPacket>,
    bind: F,
) where
    F: FnOnce(u16) -> Fut,
    Fut: Future<Output = io::Result<UdpSocket>>,
{
    let (peer, src_port) = key;
    let socket = match bind(src_port).await {
        Ok(sock) => sock,
        Err(e) => {
            tracing::warn!("UDP relay: failed to bind port {}: {}", src_port, e);
            // Слот снимаем сразу, иначе очередь копила бы пакеты в никуда:
            // забирать их некому, а новый поток на этот src_port уже не завести.
            state.flows.lock().await.remove(&key);
            return;
        }
    };
    tracing::info!("UDP relay: bound source port {} for {}", src_port, peer);

    let mut buf = vec![0u8; 65536];
    loop {
        let tick = tokio::time::timeout(state.flow_timeout, async {
            tokio::select! {
                queued = rx.recv() => FlowEvent::Outbound(queued),
                res = socket.recv_from(&mut buf) => FlowEvent::Inbound(res),
            }
        })
        .await;

        let event = match tick {
            Ok(event) => event,
            Err(_) => {
                // Простой дольше flow_timeout. Уходим под локом таблицы и там же
                // перепроверяем очередь: класть в неё можно только с этим локом,
                // поэтому пакет, проскочивший в последний момент, не теряется.
                let mut flows = state.flows.lock().await;
                match rx.try_recv() {
                    Ok(queued) => FlowEvent::Outbound(Some(queued)),
                    Err(_) => {
                        flows.remove(&key);
                        tracing::info!(
                            "UDP relay: released port {} of {} ({} active)",
                            src_port,
                            peer,
                            flows.len()
                        );
                        return;
                    }
                }
            }
        };

        match event {
            FlowEvent::Outbound(Some(queued)) => {
                if let Err(e) = socket.send_to(&queued.payload, queued.dst).await {
                    tracing::warn!("UDP relay: send to {} failed: {}", queued.dst, e);
                }
            }
            FlowEvent::Outbound(None) => return,
            FlowEvent::Inbound(Ok((n, from_addr))) => {
                if n == 0 {
                    continue;
                }

                // Ответ уходит владельцу потока, а не тому, кто писал на
                // relay-порт последним.
                let response = RelayPacket {
                    relay_type: RelayType::Data,
                    dst: from_addr,
                    src_port,
                    payload: buf[..n].to_vec(),
                };
                let wire = udp_relay::encode_relay_packet(&state.obfuscator, &response);
                if let Err(e) = relay_socket.send_to(&wire, peer).await {
                    tracing::warn!("UDP relay: send response to router failed: {}", e);
                }
            }
            FlowEvent::Inbound(Err(e)) => {
                tracing::debug!("Bound port {} recv error: {}", src_port, e);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;
    use tokio::time::timeout;
    use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};

    /// Запас на ожидание в тестах: успешный путь до него не доходит, а на
    /// сломанном коде тест обязан упасть, а не повиснуть.
    const WAIT: Duration = Duration::from_secs(5);

    fn test_state(flow_timeout: Duration) -> Arc<ServerState> {
        limited_state(flow_timeout, usize::MAX)
    }

    fn limited_state(flow_timeout: Duration, max_flows: usize) -> Arc<ServerState> {
        Arc::new(ServerState {
            flows: Mutex::new(HashMap::new()),
            obfuscator: Obfuscator::new(b"test-key".to_vec(), 7, ModifierStrategy::PositionalXorRotate),
            flow_timeout,
            max_flows,
            incoming_port_min: 0,
            incoming_port_max: 0,
        })
    }

    async fn local_socket() -> Arc<UdpSocket> {
        Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap())
    }

    fn data_packet(src_port: u16, dst: SocketAddr, payload: &[u8]) -> RelayPacket {
        RelayPacket {
            relay_type: RelayType::Data,
            dst,
            src_port,
            payload: payload.to_vec(),
        }
    }

    /// В бою сокет потока садится на настоящий src_port, но в тестах порт
    /// берём эфемерный: конкретный номер ничего не проверяет, а занять его на
    /// машине могли и без нас.
    async fn bind_ephemeral() -> io::Result<UdpSocket> {
        UdpSocket::bind("127.0.0.1:0").await
    }

    /// Пир из туннеля там, где тесту всё равно, кто именно написал.
    fn any_peer() -> SocketAddr {
        "127.0.0.1:41000".parse().unwrap()
    }

    /// Дождаться, когда таск потока снимет свой слот. Ждём на мок-часах: пауза
    /// между опросами настоящего времени не тратит, зато отпускает рантайм и
    /// даёт таску дойти до своего шага.
    async fn wait_slot_released(state: &Arc<ServerState>, key: FlowKey) {
        timeout(WAIT, async {
            while state.flows.lock().await.contains_key(&key) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("слот потока обязан освободиться");
    }

    /// XR-200: приём не должен ждать bind. Раньше пакет обрабатывался прямо в
    /// теле цикла recv_from, поэтому первый пакет нового src_port останавливал
    /// relay целиком: пока поднимался его сокет, пакеты всех остальных потоков
    /// лежали в буфере и ждали. Тест держит bind первого потока подвешенным и
    /// проверяет, что второй поток за это время успевает подняться.
    #[tokio::test(start_paused = true)]
    async fn slow_bind_does_not_block_other_flows() {
        let state = test_state(Duration::from_secs(3600));
        let relay = local_socket().await;
        let dst = local_socket().await.local_addr().unwrap();

        let (_hold_tx, hold_rx) = oneshot::channel::<()>();
        let slow_bind = move |_port| async move {
            let _ = hold_rx.await;
            bind_ephemeral().await
        };
        timeout(
            WAIT,
            handle_data_packet(
                &state,
                &relay,
                any_peer(),
                data_packet(41001, dst, b"first"),
                slow_bind,
            ),
        )
        .await
        .expect("приём не должен ждать bind первого потока");

        let (bound_tx, bound_rx) = oneshot::channel::<()>();
        let fast_bind = move |_port| async move {
            let sock = bind_ephemeral().await;
            let _ = bound_tx.send(());
            sock
        };
        timeout(
            WAIT,
            handle_data_packet(
                &state,
                &relay,
                any_peer(),
                data_packet(41002, dst, b"second"),
                fast_bind,
            ),
        )
        .await
        .expect("приём не должен ждать bind второго потока");

        timeout(WAIT, bound_rx)
            .await
            .expect("второй поток обязан подняться, пока первый висит в bind")
            .unwrap();
    }

    /// Пакеты, накопившиеся за время bind, уходят наружу в том же порядке, в
    /// каком пришли из туннеля. Таск на пакет вместо очереди на поток этот
    /// порядок бы и сломал.
    #[tokio::test]
    async fn flow_keeps_packet_order() {
        let state = test_state(Duration::from_secs(3600));
        let relay = local_socket().await;
        let peer = local_socket().await;
        let dst = peer.local_addr().unwrap();

        let (release_tx, release_rx) = oneshot::channel::<()>();
        let slow_bind = move |_port| async move {
            let _ = release_rx.await;
            bind_ephemeral().await
        };

        handle_data_packet(
            &state,
            &relay,
            any_peer(),
            data_packet(41003, dst, b"1"),
            slow_bind,
        )
        .await;
        for payload in [b"2", b"3"] {
            handle_data_packet(
                &state,
                &relay,
                any_peer(),
                data_packet(41003, dst, payload),
                |_port| async { unreachable!("поток уже поднят, второй bind ему не нужен") },
            )
            .await;
        }

        release_tx.send(()).unwrap();

        let mut got: Vec<Vec<u8>> = Vec::new();
        let mut buf = [0u8; 64];
        for _ in 0..3 {
            let (n, _) = timeout(WAIT, peer.recv_from(&mut buf))
                .await
                .expect("пакеты из очереди обязаны дойти до назначения")
                .unwrap();
            got.push(buf[..n].to_vec());
        }
        assert_eq!(got, vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]);
    }

    /// Неудачный bind снимает слот, а не оставляет поток с очередью, в которую
    /// некому смотреть: следующий пакет того же src_port обязан поднять поток
    /// заново.
    #[tokio::test(start_paused = true)]
    async fn failed_bind_releases_slot() {
        let state = test_state(Duration::from_secs(3600));
        let relay = local_socket().await;
        let dst = local_socket().await.local_addr().unwrap();
        let src_port = 41004;

        let (failed_tx, failed_rx) = oneshot::channel::<()>();
        let failing_bind = move |_port| async move {
            let _ = failed_tx.send(());
            Err(io::Error::new(io::ErrorKind::AddrInUse, "порт занят"))
        };
        handle_data_packet(
            &state,
            &relay,
            any_peer(),
            data_packet(src_port, dst, b"first"),
            failing_bind,
        )
        .await;
        timeout(WAIT, failed_rx).await.unwrap().unwrap();
        wait_slot_released(&state, (any_peer(), src_port)).await;

        let retry_calls = Arc::new(AtomicUsize::new(0));
        let calls = retry_calls.clone();
        let (bound_tx, bound_rx) = oneshot::channel::<()>();
        let retry_bind = move |_port| async move {
            calls.fetch_add(1, Ordering::SeqCst);
            let sock = bind_ephemeral().await;
            let _ = bound_tx.send(());
            sock
        };
        handle_data_packet(
            &state,
            &relay,
            any_peer(),
            data_packet(src_port, dst, b"second"),
            retry_bind,
        )
        .await;

        timeout(WAIT, bound_rx)
            .await
            .expect("следующий пакет обязан поднять поток заново")
            .unwrap();
        assert_eq!(retry_calls.load(Ordering::SeqCst), 1);
    }

    /// Ответ из интернета возвращается роутеру тем же таском потока.
    #[tokio::test]
    async fn flow_relays_response_to_router() {
        let state = test_state(Duration::from_secs(3600));
        let relay = local_socket().await;
        let router = local_socket().await;
        let router_addr = router.local_addr().unwrap();

        let peer = local_socket().await;
        let peer_addr = peer.local_addr().unwrap();
        let src_port = 41005;

        handle_data_packet(
            &state,
            &relay,
            router_addr,
            data_packet(src_port, peer_addr, b"ping"),
            |_port| bind_ephemeral(),
        )
        .await;

        let mut buf = [0u8; 64];
        let (n, from) = timeout(WAIT, peer.recv_from(&mut buf))
            .await
            .expect("пакет обязан дойти до назначения")
            .unwrap();
        assert_eq!(&buf[..n], b"ping");
        peer.send_to(b"pong", from).await.unwrap();

        let mut wire = [0u8; 256];
        let (n, _) = timeout(WAIT, router.recv_from(&mut wire))
            .await
            .expect("ответ обязан вернуться роутеру")
            .unwrap();
        let response = udp_relay::decode_relay_packet(&state.obfuscator, &wire[..n]).unwrap();
        assert_eq!(response.src_port, src_port);
        assert_eq!(response.dst, peer_addr);
        assert_eq!(response.payload, b"pong".to_vec());
    }

    /// XR-208: ответ уходит владельцу потока, а не тому, кто написал на
    /// relay-порт последним. Раньше адрес роутера жил одним полем на весь
    /// сервер, и любой расшифровавшийся пакет перетирал его: второй роутер на
    /// том же VPS (ключ обфускации общий) уводил к себе входящий трафик первого.
    #[tokio::test]
    async fn response_goes_to_flow_owner_not_last_writer() {
        let state = test_state(Duration::from_secs(3600));
        let relay = local_socket().await;
        let owner = local_socket().await;
        let owner_addr = owner.local_addr().unwrap();
        let hijacker = local_socket().await;
        let hijacker_addr = hijacker.local_addr().unwrap();

        let peer = local_socket().await;
        let peer_addr = peer.local_addr().unwrap();
        let src_port = 41008;

        // Поток заводит владелец.
        handle_data_packet(
            &state,
            &relay,
            owner_addr,
            data_packet(src_port, peer_addr, b"ping"),
            |_port| bind_ephemeral(),
        )
        .await;

        let mut buf = [0u8; 64];
        let (n, owner_flow) = timeout(WAIT, peer.recv_from(&mut buf))
            .await
            .expect("пакет владельца обязан дойти до назначения")
            .unwrap();
        assert_eq!(&buf[..n], b"ping");

        // Следом на relay-порт пишет другой пир с тем же src_port.
        handle_data_packet(
            &state,
            &relay,
            hijacker_addr,
            data_packet(src_port, peer_addr, b"hijack"),
            |_port| bind_ephemeral(),
        )
        .await;
        let (n, _) = timeout(WAIT, peer.recv_from(&mut buf))
            .await
            .expect("пакет второго пира обязан дойти до назначения")
            .unwrap();
        assert_eq!(&buf[..n], b"hijack");

        // Ответ приходит на сокет потока владельца.
        peer.send_to(b"pong", owner_flow).await.unwrap();

        let mut wire = [0u8; 256];
        let (n, _) = timeout(WAIT, owner.recv_from(&mut wire))
            .await
            .expect("ответ обязан уйти тому, кто завёл поток")
            .unwrap();
        let response = udp_relay::decode_relay_packet(&state.obfuscator, &wire[..n]).unwrap();
        assert_eq!(response.payload, b"pong".to_vec());
        assert!(
            hijacker.try_recv_from(&mut wire).is_err(),
            "чужому пиру ответ не достаётся"
        );
    }

    /// Один и тот же src_port у разных пиров это разные потоки: у каждого свой
    /// сокет наружу, общего потока на двоих не заводится.
    #[tokio::test]
    async fn same_src_port_of_different_peers_gives_two_flows() {
        let state = test_state(Duration::from_secs(3600));
        let relay = local_socket().await;
        let first = local_socket().await.local_addr().unwrap();
        let second = local_socket().await.local_addr().unwrap();

        let peer = local_socket().await;
        let peer_addr = peer.local_addr().unwrap();
        let src_port = 41009;

        let binds = Arc::new(AtomicUsize::new(0));
        for sender in [first, second] {
            let calls = binds.clone();
            handle_data_packet(
                &state,
                &relay,
                sender,
                data_packet(src_port, peer_addr, b"ping"),
                move |_port| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    bind_ephemeral().await
                },
            )
            .await;
        }

        let mut buf = [0u8; 64];
        let mut flow_addrs = Vec::new();
        for _ in 0..2 {
            let (n, from) = timeout(WAIT, peer.recv_from(&mut buf))
                .await
                .expect("пакеты обоих пиров обязаны дойти до назначения")
                .unwrap();
            assert_eq!(&buf[..n], b"ping");
            flow_addrs.push(from);
        }
        assert_ne!(
            flow_addrs[0], flow_addrs[1],
            "потоки разных пиров не делят один сокет"
        );
        assert_eq!(binds.load(Ordering::SeqCst), 2);

        let flows = state.flows.lock().await;
        assert!(flows.contains_key(&(first, src_port)));
        assert!(flows.contains_key(&(second, src_port)));
        assert_eq!(flows.len(), 2);
    }

    /// Пакет, попавший в очередь ровно в тот момент, когда поток пошёл сниматься
    /// по таймауту, не теряется: слот снимается под локом таблицы, и под тем же
    /// локом очередь перепроверяется. Тест держит лок сам, дожидается, пока
    /// таймаут потока сработает, и кладёт пакет в очередь до того, как таск
    /// доберётся до таблицы.
    #[tokio::test(start_paused = true)]
    async fn packet_queued_at_retire_is_not_lost() {
        let flow_timeout = Duration::from_secs(1);
        let state = test_state(flow_timeout);
        let relay = local_socket().await;
        let dst = local_socket().await.local_addr().unwrap();
        let src_port = 41007;

        let (bound_tx, bound_rx) = oneshot::channel::<()>();
        let bind = move |_port| async move {
            let sock = bind_ephemeral().await;
            let _ = bound_tx.send(());
            sock
        };
        handle_data_packet(
            &state,
            &relay,
            any_peer(),
            data_packet(src_port, dst, b"first"),
            bind,
        )
        .await;
        bound_rx.await.unwrap();

        let flows = state.flows.lock().await;
        let tx = flows.get(&(any_peer(), src_port)).unwrap().tx.clone();
        // Пока лок наш, таск потока успевает протухнуть, упереться в таблицу и
        // застрять на ней: снять слот без лока он не может.
        tokio::time::sleep(flow_timeout * 2).await;
        tx.try_send(FlowPacket {
            dst,
            payload: b"second".to_vec(),
        })
        .unwrap();
        drop(flows);

        // Таск забирает лок, видит непустую очередь и остаётся работать.
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            state.flows.lock().await.contains_key(&(any_peer(), src_port)),
            "поток с непустой очередью не снимается"
        );
        assert!(!tx.is_closed(), "очередь потока обязана остаться живой");
        assert_eq!(
            tx.capacity(),
            FLOW_QUEUE,
            "пакет обязан быть разобран, а не пропасть вместе с потоком"
        );
    }

    /// Keepalive отвечает тому, кто написал: этим ответом роутер и судит, живо
    /// ли туннельное плечо relay.
    #[tokio::test]
    async fn keepalive_is_answered_to_writer() {
        let state = test_state(Duration::from_secs(3600));
        let relay = local_socket().await;
        let router = local_socket().await;

        handle_datagram(
            &state,
            &relay,
            router.local_addr().unwrap(),
            &udp_relay::encode_keepalive(&state.obfuscator),
            |_port| async { unreachable!("keepalive не заводит поток") },
        )
        .await;

        let mut wire = [0u8; 256];
        let (n, _) = timeout(WAIT, router.recv_from(&mut wire))
            .await
            .expect("на keepalive обязан прийти ответ")
            .unwrap();
        let reply = udp_relay::decode_relay_packet(&state.obfuscator, &wire[..n]).unwrap();
        assert_eq!(reply.relay_type, RelayType::Keepalive);
        assert!(state.flows.lock().await.is_empty());
    }

    /// Нерасшифровавшаяся датаграмма не заводит поток и не получает ответа: на
    /// открытый relay-порт пишут и сканеры, а поток это занятый порт на VPS.
    #[tokio::test]
    async fn undecodable_datagram_starts_no_flow() {
        let state = test_state(Duration::from_secs(3600));
        let relay = local_socket().await;
        let stranger = local_socket().await;

        handle_datagram(
            &state,
            &relay,
            stranger.local_addr().unwrap(),
            b"xx",
            |_port| async { unreachable!("мусор не заводит поток") },
        )
        .await;

        assert!(state.flows.lock().await.is_empty());

        // Отсутствие ответа судим по порядку, а не по паузе: маркер отправлен с
        // того же сокета позже, и приди ответ, он лежал бы в очереди первым.
        relay
            .send_to(b"marker", stranger.local_addr().unwrap())
            .await
            .unwrap();
        let mut buf = [0u8; 256];
        let (n, _) = timeout(WAIT, stranger.recv_from(&mut buf))
            .await
            .expect("маркер обязан дойти")
            .unwrap();
        assert_eq!(&buf[..n], b"marker", "чужому на мусор не отвечаем");
    }

    /// Очередь потока не резиновая: пока поднимается сокет, лишние пакеты
    /// отбрасываются, как отбросил бы их любой промежуточный узел. Порядок
    /// принятых при этом не ломается, а поток остаётся живым.
    #[tokio::test]
    async fn overflowing_queue_drops_newest_packets() {
        let state = test_state(Duration::from_secs(3600));
        let relay = local_socket().await;
        let peer = local_socket().await;
        let dst = peer.local_addr().unwrap();
        let src_port = 41010;
        let extra = 2;

        let (release_tx, release_rx) = oneshot::channel::<()>();
        let slow_bind = move |_port| async move {
            let _ = release_rx.await;
            bind_ephemeral().await
        };

        // Все пакеты кладём, пока bind держится: очередь никто не разгребает.
        handle_data_packet(
            &state,
            &relay,
            any_peer(),
            data_packet(src_port, dst, &0u32.to_be_bytes()),
            slow_bind,
        )
        .await;
        for i in 1..(FLOW_QUEUE + extra) as u32 {
            handle_data_packet(
                &state,
                &relay,
                any_peer(),
                data_packet(src_port, dst, &i.to_be_bytes()),
                |_port| async { unreachable!("поток уже поднят") },
            )
            .await;
        }
        // Очередь заполнена до края, и лишнее в неё уже не попало: сокет ещё не
        // поднят, забирать пакеты некому.
        {
            let flows = state.flows.lock().await;
            let tx = flows.get(&(any_peer(), src_port)).unwrap().tx.clone();
            assert_eq!(tx.capacity(), 0, "в очереди не должно остаться места");
        }
        release_tx.send(()).unwrap();

        let mut got = Vec::new();
        let mut buf = [0u8; 64];
        for _ in 0..FLOW_QUEUE {
            let (n, _) = timeout(WAIT, peer.recv_from(&mut buf))
                .await
                .expect("принятые в очередь пакеты обязаны дойти")
                .unwrap();
            got.push(u32::from_be_bytes(buf[..n].try_into().unwrap()));
        }
        assert_eq!(got, (0..FLOW_QUEUE as u32).collect::<Vec<_>>());
        assert!(
            peer.try_recv_from(&mut buf).is_err(),
            "лишние пакеты обязаны быть отброшены, а не доехать позже"
        );
        assert!(
            state.flows.lock().await.contains_key(&(any_peer(), src_port)),
            "переполнение очереди не снимает поток"
        );
    }

    /// XR-204: потолок таблицы потоков. Пачка датаграмм сверх лимита не
    /// растит карту: лишний поток вытесняет наименее свежий, и размер держится
    /// на потолке вместо роста до чистки по простою.
    #[tokio::test(start_paused = true)]
    async fn flood_beyond_limit_keeps_table_at_ceiling() {
        let state = limited_state(Duration::from_secs(3600), 4);
        let relay = local_socket().await;
        let dst = local_socket().await.local_addr().unwrap();

        for i in 0u16..8 {
            handle_data_packet(
                &state,
                &relay,
                any_peer(),
                data_packet(41000 + i, dst, b"x"),
                |_port| bind_ephemeral(),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(
            state.flows.lock().await.len(),
            4,
            "потоки сверх лимита не растят таблицу"
        );
    }

    /// XR-204: вытеснение забирает наименее свежий поток, а не живой. Поток,
    /// по которому продолжают идти пакеты, переживает напор сверх лимита и
    /// продолжает носить пакеты наружу. Времена здесь настоящие: последний
    /// пакет ждём на реальном сокете, и мок-часы обгоняли бы его доставку
    /// виртуальным таймаутом.
    #[tokio::test]
    async fn living_flow_survives_flood_beyond_limit() {
        let state = limited_state(Duration::from_secs(3600), 4);
        let relay = local_socket().await;
        let dst_socket = local_socket().await;
        let dst = dst_socket.local_addr().unwrap();

        // Живой поток плюс три потока напора заполняют потолок.
        handle_data_packet(
            &state,
            &relay,
            any_peer(),
            data_packet(3074, dst, b"hello"),
            |_port| bind_ephemeral(),
        )
        .await;
        for i in 0u16..3 {
            handle_data_packet(
                &state,
                &relay,
                any_peer(),
                data_packet(42000 + i, dst, b"flood"),
                |_port| bind_ephemeral(),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Дальше чередуем: свежий пакет живого потока, затем новый поток сверх
        // лимита. Жертвой становится поток напора, а не живой.
        for i in 0u16..3 {
            handle_data_packet(
                &state,
                &relay,
                any_peer(),
                data_packet(3074, dst, b"still-here"),
                |_port| bind_ephemeral(),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(10)).await;
            handle_data_packet(
                &state,
                &relay,
                any_peer(),
                data_packet(43000 + i, dst, b"flood"),
                |_port| bind_ephemeral(),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert!(
            state.flows.lock().await.contains_key(&(any_peer(), 3074)),
            "живой поток не вытесняется"
        );
        assert_eq!(state.flows.lock().await.len(), 4);

        let mut seen_still_here = false;
        let mut buf = [0u8; 64];
        loop {
            match timeout(WAIT, dst_socket.recv_from(&mut buf)).await {
                Ok(Ok((n, _))) => {
                    if &buf[..n] == b"still-here" {
                        seen_still_here = true;
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(seen_still_here, "живой поток продолжает ходить наружу");
    }

    /// XR-204: вытесненный поток возвращает и слот, и сокет. Слот уходит из
    /// таблицы сразу, а таск видит закрытый канал, выходит и отпускает порт.
    #[tokio::test(start_paused = true)]
    async fn evicted_flow_releases_its_socket() {
        let state = limited_state(Duration::from_secs(3600), 2);
        let relay = local_socket().await;
        let dst = local_socket().await.local_addr().unwrap();

        // Фиксированный порт для потока-жертвы: способность занять его снова
        // и есть свидетельство, что сокет потока умер.
        let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let fixed = probe.local_addr().unwrap();
        drop(probe);
        let bind_fixed = move |_port| async move { UdpSocket::bind(fixed).await };

        handle_data_packet(&state, &relay, any_peer(), data_packet(41010, dst, b"a"), bind_fixed)
            .await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        handle_data_packet(
            &state,
            &relay,
            any_peer(),
            data_packet(41011, dst, b"b"),
            |_port| bind_ephemeral(),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Третий поток вытесняет первый: тот наименее свежий.
        handle_data_packet(
            &state,
            &relay,
            any_peer(),
            data_packet(41012, dst, b"c"),
            |_port| bind_ephemeral(),
        )
        .await;

        assert!(
            !state.flows.lock().await.contains_key(&(any_peer(), 41010)),
            "слот вытесненного потока освобождён"
        );

        timeout(WAIT, async {
            loop {
                if UdpSocket::bind(fixed).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("сокет вытесненного потока обязан освободиться");
    }

    /// Занятый порт не повод потерять поток: сокет садится на соседний номер.
    /// Точное совпадение с портом устройства нужно для NAT приставок, но своё
    /// плечо туннеля важнее, и оно работает и на подменном номере.
    #[tokio::test]
    async fn busy_source_port_falls_back_to_neighbour() {
        let occupied = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        let port = occupied.local_addr().unwrap().port();

        let sock = bind_source_port(port).await.unwrap();
        let got = sock.local_addr().unwrap().port();
        assert_ne!(got, port);
        assert!(
            (1..=10).contains(&got.wrapping_sub(port)),
            "подменный номер берётся рядом с занятым, получили {} вместо {}",
            got,
            port
        );
    }

    #[tokio::test]
    async fn free_source_port_is_taken_as_is() {
        let probe = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let sock = bind_source_port(port).await.unwrap();
        assert_eq!(sock.local_addr().unwrap().port(), port);
    }

    /// Все соседние номера заняты: тут поток не поднимается, и таск обязан
    /// узнать об этом ошибкой, а не сесть на случайный порт.
    #[tokio::test]
    async fn crowded_neighbourhood_fails_bind() {
        // Ищем свободный отрезок из 11 портов: занимать чужие мы не вправе, а
        // проверять нужно именно полностью занятую округу.
        let mut held = Vec::new();
        let mut base = None;
        for _ in 0..64 {
            let first = UdpSocket::bind("0.0.0.0:0").await.unwrap();
            let port = first.local_addr().unwrap().port();
            if port > u16::MAX - 11 {
                continue;
            }
            let mut run = vec![first];
            for offset in 1..=10 {
                match UdpSocket::bind(("0.0.0.0", port + offset)).await {
                    Ok(sock) => run.push(sock),
                    Err(_) => break,
                }
            }
            if run.len() == 11 {
                held = run;
                base = Some(port);
                break;
            }
        }
        let base = base.expect("на машине обязан найтись свободный отрезок из 11 портов");

        let err = bind_source_port(base).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        drop(held);
    }

    /// Протухший поток уходит сам и уносит с собой сокет: порт снова свободен,
    /// в таблице пусто, таск не остаётся висеть. Часы на паузе, поэтому
    /// flow_timeout истекает не по настоящим секундам, а ровно тогда, когда
    /// рантайму больше нечего делать.
    #[tokio::test(start_paused = true)]
    async fn idle_flow_releases_socket() {
        let state = test_state(Duration::from_secs(1));
        let relay = local_socket().await;
        let dst = local_socket().await.local_addr().unwrap();
        let src_port = 41006;

        let (port_tx, port_rx) = oneshot::channel::<u16>();
        let bind = move |_port| async move {
            let sock = bind_ephemeral().await?;
            let _ = port_tx.send(sock.local_addr().unwrap().port());
            Ok(sock)
        };
        handle_data_packet(
            &state,
            &relay,
            any_peer(),
            data_packet(src_port, dst, b"first"),
            bind,
        )
        .await;
        let bound_port = timeout(WAIT, port_rx).await.unwrap().unwrap();

        // Порт занят, пока поток жив: сокетом владеет его таск.
        assert!(
            std::net::UdpSocket::bind(("127.0.0.1", bound_port)).is_err(),
            "сокет живого потока обязан держать свой порт"
        );

        wait_slot_released(&state, (any_peer(), src_port)).await;

        // Сокет живёт ровно столько, сколько таск: занять его порт заново
        // получится только после того, как таск завершился.
        std::net::UdpSocket::bind(("127.0.0.1", bound_port))
            .expect("сокет протухшего потока обязан быть закрыт");
    }
}
