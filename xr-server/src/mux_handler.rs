//! Server-side multiplexed connection handler.
//!
//! Accepts MuxInit, then serves multiple concurrent streams over
//! one TCP connection. Each stream maps to an independent target.

use std::io;
use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Duration;

use xr_proto::mux::{mux_handshake_server, Multiplexer};
use xr_proto::protocol::{Codec, Command, Frame, TargetAddr};

const TARGET_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_LIFETIME: Duration = Duration::from_secs(3600);

/// Handle a multiplexed client connection.
/// Called after the first frame was detected as MuxInit.
pub async fn handle_mux_client(
    client: TcpStream,
    client_addr: SocketAddr,
    codec: Codec,
    init_frame: &Frame,
) -> io::Result<()> {
    // Стагерим лайфтайм по эфемерному порту клиента (0..15 мин поверх базы), чтобы
    // 4 слота пула, поднятые почти одновременно, не упирались в кап и не
    // переподключались лок-степом (иначе разом закрылись бы и дали секундный
    // провал открытий раз в цикл).
    let lifetime = MAX_LIFETIME + Duration::from_secs((client_addr.port() as u64) % 900);
    handle_mux_client_lt(client, client_addr, codec, init_frame, lifetime).await
}

/// Тело с явным лайфтаймом accept-петли, чтобы тест мог задать короткий кап.
async fn handle_mux_client_lt(
    mut client: TcpStream,
    client_addr: SocketAddr,
    codec: Codec,
    init_frame: &Frame,
    lifetime: Duration,
) -> io::Result<()> {
    if !mux_handshake_server(&mut client, &codec, init_frame).await? {
        tracing::warn!("{} mux handshake rejected", client_addr);
        return Ok(());
    }

    tracing::info!("{} mux session started", client_addr);

    let mux = Multiplexer::new_server(client, codec.clone());

    let mut new_stream_rx = mux.take_new_stream_rx().await
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "new_stream_rx already taken"))?;

    let mut stream_count = 0u64;

    let result = tokio::time::timeout(lifetime, async {
        while let Some(new_stream) = new_stream_rx.recv().await {
            stream_count += 1;
            let stream_id = new_stream.stream_id;

            // Decode target address from Connect payload.
            let target_addr = match TargetAddr::decode(&new_stream.payload) {
                Ok((addr, _)) => addr,
                Err(e) => {
                    tracing::debug!("{} sid={} bad Connect payload: {}", client_addr, stream_id, e);
                    // Send Close for this stream.
                    let _ = mux.send_frame(stream_id, Command::Close, Vec::new()).await;
                    continue;
                }
            };

            tracing::info!("{} sid={} -> {}", client_addr, stream_id, addr_display(&target_addr));

            // Send ConnectAck immediately (before connecting to target).
            if let Err(e) = mux.send_frame(stream_id, Command::ConnectAck, vec![0]).await {
                tracing::debug!("{} sid={} ConnectAck send failed: {}", client_addr, stream_id, e);
                break;
            }

            // Register the stream on the multiplexer so Data frames are routed to it.
            let mux_stream = mux.register_stream(stream_id).await;

            // Spawn independent relay task for this stream.
            let mux_clone = mux.clone();
            let addr_str = addr_display(&target_addr);
            let client_addr_clone = client_addr;
            tokio::spawn(async move {
                if let Err(e) = relay_stream(mux_stream, target_addr).await {
                    tracing::debug!("{} sid={} {} relay error: {}", client_addr_clone, stream_id, addr_str, e);
                }
                // Send Close to notify the client.
                let _ = mux_clone.send_frame(stream_id, Command::Close, Vec::new()).await;
            });
        }
        Ok::<(), io::Error>(())
    })
    .await;

    tracing::info!("{} mux session ended ({} streams)", client_addr, stream_count);

    // КРИТИЧНО (XR-086, корень «намертво, лечит только рестарт»): когда accept-петля
    // завершилась ПО ЛЮБОЙ причине (лайфтайм-кап, обрыв new_stream-канала, break на
    // ошибке ConnectAck), ОБЯЗАТЕЛЬНО рвём mux. Иначе reader/writer-таски (они не
    // держат Arc mux и не завершаются с этой функцией) держат сокет живым до
    // MUX_MAX_LIFETIME=4ч: keepalive Ping/Pong идёт (клиент считает слот живым,
    // dead-link молчит), старые стримы ещё релеятся, НО new_stream_rx уже дропнут,
    // поэтому КАЖДЫЙ новый Connect молча теряется (try_send -> Closed) и клиент
    // ловит «open timed out». Между кап-лайфтаймом (1ч) и смертью reader (4ч) слот
    // это зомби: выглядит здоровым, но не принимает ни одного нового стрима.
    // shutdown() роняет write_half -> FIN -> клиент видит EOF и переподнимает слот.
    mux.shutdown();

    match result {
        Ok(r) => r,
        Err(_) => {
            tracing::debug!("{} mux lifetime cap reached", client_addr);
            Ok(())
        }
    }
}

/// Relay data between a MuxStream and a target TCP connection.
///
/// MuxStream is split so the upstream (target→mux) and downstream
/// (mux→target) flows run as independent tasks. This avoids the
/// "channel full, closing" failure mode where a slow target write would
/// stall mux recv polling under a CDN burst on the upstream side.
async fn relay_stream(
    mux_stream: xr_proto::mux::MuxStream,
    target_addr: TargetAddr,
) -> io::Result<()> {
    // Resolve and connect to target.
    let target_sockaddr = resolve_target(&target_addr).await?;
    let mut target = tokio::time::timeout(
        TARGET_CONNECT_TIMEOUT,
        TcpStream::connect(target_sockaddr),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "target connect timeout"))??;

    configure_target(&target);

    let (mut tr, mut tw) = target.split();
    let (mut mux_r, mux_w) = mux_stream.split();

    // MuxStream → Target (downstream from server's POV: data going to origin).
    let to_target = async {
        while let Some(d) = mux_r.recv().await {
            if d.is_empty() { break; }
            tw.write_all(&d).await?;
        }
        Ok::<(), io::Error>(())
    };

    // Target → MuxStream (upstream: origin's response back to client).
    let to_mux = async {
        let mut buf = vec![0u8; 8192];
        loop {
            let n = match tokio::time::timeout(IDLE_TIMEOUT, tr.read(&mut buf)).await {
                Ok(result) => result?,
                Err(_) => return Ok::<(), io::Error>(()),
            };
            if n == 0 { break; }
            mux_w.send(&buf[..n]).await?;
        }
        Ok::<(), io::Error>(())
    };

    let result = tokio::time::timeout(MAX_LIFETIME, async {
        tokio::select! {
            r = to_target => r,
            r = to_mux => r,
        }
    });

    match result.await {
        Ok(r) => r,
        Err(_) => Ok(()),
    }
}

fn addr_display(addr: &TargetAddr) -> String {
    match addr {
        TargetAddr::Domain(d, p) => format!("{}:{}", d, p),
        TargetAddr::Ip(s) => s.to_string(),
    }
}

async fn resolve_target(addr: &TargetAddr) -> io::Result<SocketAddr> {
    match addr {
        TargetAddr::Ip(sockaddr) => Ok(*sockaddr),
        TargetAddr::Domain(domain, port) => {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host(format!("{}:{}", domain, port))
                    .await?
                    .collect();
            addrs
                .into_iter()
                .next()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "DNS resolution failed"))
        }
    }
}

fn configure_target(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
    let ka = socket2::TcpKeepalive::new()
        .with_time(std::time::Duration::from_secs(60))
        .with_interval(std::time::Duration::from_secs(15));
    let sock_ref = socket2::SockRef::from(stream);
    let _ = sock_ref.set_tcp_keepalive(&ka);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use xr_proto::mux::mux_handshake_client;
    use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};

    fn test_codec() -> Codec {
        let key = b"test-key-32-bytes-long-enough!!!".to_vec();
        let obfs = Obfuscator::new(key, 0xDEADBEEF, ModifierStrategy::PositionalXorRotate);
        Codec::new(obfs, 0, 0)
    }

    /// Регрессия XR-086 (корень «намертво, лечит только рестарт»): когда accept-петля
    /// сервера упирается в лайфтайм-кап, а reader ещё жив (keepalive держит слот
    /// «живым»), сервер ОБЯЗАН закрыть mux. Раньше `handle_mux_client` просто
    /// возвращался: reader/writer-таски (не держат Arc mux, reader держит клон
    /// ctrl_tx) оставляли сокет открытым до 4ч, keepalive шёл, но new_stream_rx был
    /// дропнут -> все новые Connect молча терялись, клиент вечно ловил "open timed
    /// out". Тест: короткий кап (300мс, меньше dead-link 75с, поэтому reader жив),
    /// клиент после хендшейка ждёт закрытия. На баговом коде (без `mux.shutdown()`)
    /// EOF не приходит и тест падает по таймауту; с фиксом клиент сразу видит EOF.
    #[tokio::test]
    async fn server_closes_mux_at_lifetime_cap_no_zombie() {
        let codec = test_codec();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let scodec = codec.clone();
        let server = tokio::spawn(async move {
            let (mut sock, peer) = listener.accept().await.unwrap();
            // Прочитать первый кадр (MuxInit), как это делает handler::handle_client.
            let mut buf = vec![0u8; 512];
            let mut filled = 0;
            let init = loop {
                let n = sock.read(&mut buf[filled..]).await.unwrap();
                assert!(n > 0, "клиент закрылся до MuxInit");
                filled += n;
                if let Some((f, _)) = scodec.decode_frame(&buf[..filled]).unwrap() {
                    break f;
                }
            };
            // Короткий кап accept-петли, чтобы он сработал раньше dead-link.
            handle_mux_client_lt(sock, peer, scodec, &init, Duration::from_millis(300)).await
        });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        assert!(mux_handshake_client(&mut client, &codec).await.unwrap(), "handshake ok");

        // Клиент НЕ шлёт keepalive и просто ждёт. Кап (300мс) < dead-link (75с),
        // поэтому reader жив в момент капа. С фиксом сервер сделает shutdown ->
        // придёт EOF. Без фикса сокет останется открытым (зомби) -> таймаут.
        let mut b = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(3), client.read(&mut b))
            .await
            .expect("сервер обязан закрыть mux после лайфтайм-капа (зомби-регрессия XR-086)")
            .expect("read без ошибки");
        assert_eq!(n, 0, "после капа клиент должен получить EOF (mux закрыт), а не данные");

        let _ = server.await;
    }
}
