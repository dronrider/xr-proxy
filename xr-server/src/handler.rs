/// Server-side connection handler: accept obfuscated connections,
/// decode Connect command, connect to target, relay data.
use std::io;
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Duration;
use xr_proto::protocol::{Codec, Command, Frame, TargetAddr};

const IDLE_TIMEOUT: Duration = Duration::from_secs(300);   // 5 min idle
const MAX_LIFETIME: Duration = Duration::from_secs(3600);  // 1 hour max

/// Configure TCP socket: keepalive + nodelay.
fn configure_socket(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
    let ka = socket2::TcpKeepalive::new()
        .with_time(std::time::Duration::from_secs(60))
        .with_interval(std::time::Duration::from_secs(15));
    let sock_ref = socket2::SockRef::from(stream);
    let _ = sock_ref.set_tcp_keepalive(&ka);
}

/// Handle a single client connection end-to-end.
pub async fn handle_client(
    mut client: TcpStream,
    client_addr: SocketAddr,
    codec: Codec,
    timeout: Duration,
    fallback_response: Option<Vec<u8>>,
    limits: crate::mux_handler::StreamLimits,
) -> io::Result<()> {
    configure_socket(&client);

    // Read first frame (Connect command) with timeout
    let mut buf = vec![0u8; 4096];

    let (connect_frame, filled) = match read_first_frame(&mut client, &mut buf, &codec, timeout).await? {
        FirstFrameOutcome::Ready(frame, leftover) => (frame, leftover),
        FirstFrameOutcome::NeedFallback(reason) => {
            if reason == FallbackReason::InvalidFrame {
                tracing::debug!("Invalid frame from {}, sending fallback", client_addr);
            }
            return send_fallback_and_close(&mut client, fallback_response).await;
        }
    };

    // Multiplexed or legacy single-stream?
    if connect_frame.command == Command::MuxInit {
        return crate::mux_handler::handle_mux_client(
            client, client_addr, codec, &connect_frame, limits,
        ).await;
    }

    if connect_frame.command != Command::Connect {
        tracing::debug!("Expected Connect from {}, got {:?}", client_addr, connect_frame.command);
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected Connect"));
    }

    let (target_addr, _) = TargetAddr::decode(&connect_frame.payload)?;

    // Send ConnectAck IMMEDIATELY — before DNS resolution and target connect.
    let ack = codec.encode_frame(Command::ConnectAck, &[0])?;
    client.write_all(&ack).await?;
    tracing::info!("{} ack sent for {}", client_addr, addr_display(&target_addr));

    let target_sockaddr = resolve_target(&target_addr).await?;
    tracing::info!("{} -> {} ({})", client_addr, target_sockaddr, addr_display(&target_addr));

    let mut target = tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(target_sockaddr),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "target connect timeout"))??;

    configure_socket(&target);

    relay_obfuscated(&mut client, &mut target, &codec, &buf[..filled]).await
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
            // Use tokio's async DNS resolution
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host(format!("{}:{}", domain, port))
                .await?
                .collect();
            addrs
                .into_iter()
                .next()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "DNS resolution failed"))
        }
    }
}

async fn send_fallback_and_close(
    client: &mut TcpStream,
    fallback_response: Option<Vec<u8>>,
) -> io::Result<()> {
    if let Some(response) = fallback_response {
        let _ = client.write_all(&response).await;
    }
    // Silently close - don't give probes any useful info
    Ok(())
}

/// Итог накопления первого кадра хендшейка: либо кадр собрался, либо
/// приёмник обязан уйти в fallback (буфер кончился или заголовок не наш).
#[derive(Debug)]
enum FirstFrameOutcome {
    /// Кадр разобран; второе поле - сколько байт после него уже лежит в
    /// начале `buf` (хвост, прочитанный тем же read, что и сам кадр).
    Ready(Frame, usize),
    NeedFallback(FallbackReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FallbackReason {
    /// Буфер заполнен целиком, а кадр так и не собран.
    Overflow,
    /// `decode_frame` отверг заголовок - это не наш протокол.
    InvalidFrame,
}

/// Копит первый кадр хендшейка из `reader` в `buf`, по одному `read` за раз, с
/// таймаутом на каждый read. Обобщена по `AsyncRead`, а не завязана на
/// `TcpStream`: тесту счастливого пути на разбиение кадра между двумя read
/// нужен приёмник, у которого границы чтений заданы явно, а не тем, что
/// успеет накопиться в сокете к моменту вызова `read` - на живом `TcpStream`
/// это гонка (см. XR-215).
async fn read_first_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    codec: &Codec,
    timeout: Duration,
) -> io::Result<FirstFrameOutcome> {
    let mut filled = 0;

    loop {
        let n = tokio::time::timeout(timeout, reader.read(&mut buf[filled..]))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handshake timeout"))??;

        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::ConnectionReset, "client closed"));
        }
        filled += n;

        match codec.decode_frame(&buf[..filled]) {
            Ok(Some((frame, consumed))) => {
                buf.copy_within(consumed..filled, 0);
                return Ok(FirstFrameOutcome::Ready(frame, filled - consumed));
            }
            Ok(None) => {
                if filled >= buf.len() {
                    // Буфер заполнен целиком, а кадр всё ещё не собран (например,
                    // header врёт про огромный payload_len, либо это вообще не
                    // наш протокол). Раньше условие сравнивалось с тем же
                    // размером буфера и никогда не срабатывало: цикл шёл на
                    // следующий read в пустой остаток среза, тот немедленно
                    // возвращал Ok(0), и код принимал это за закрытие клиентом -
                    // соединение рвалось молча вместо fallback-ответа, и зонд
                    // видел не то поведение, что у веб-сервера.
                    return Ok(FirstFrameOutcome::NeedFallback(FallbackReason::Overflow));
                }
                continue;
            }
            Err(_) => {
                return Ok(FirstFrameOutcome::NeedFallback(FallbackReason::InvalidFrame));
            }
        }
    }
}

/// Relay data between client (obfuscated) and target (plaintext).
/// Idle timeout: 5 min without data in either direction.
/// Max lifetime: 1 hour absolute limit.
async fn relay_obfuscated(
    client: &mut TcpStream,
    target: &mut TcpStream,
    codec: &Codec,
    initial_buf: &[u8],
) -> io::Result<()> {
    let (mut cr, mut cw) = client.split();
    let (mut tr, mut tw) = target.split();

    let codec_decode = codec.clone();
    let codec_encode = codec.clone();

    let initial = initial_buf.to_vec();

    // Client → Target (deobfuscate protocol frames, write raw to target)
    let upstream = async move {
        let mut buf = vec![0u8; 65536 + 256];
        let mut filled = 0;

        if !initial.is_empty() {
            buf[..initial.len()].copy_from_slice(&initial);
            filled = initial.len();
        }

        loop {
            // Try to decode existing buffer first
            loop {
                if filled == 0 {
                    break;
                }
                match codec_decode.decode_frame(&buf[..filled])? {
                    Some((frame, consumed)) => {
                        match frame.command {
                            Command::Data => {
                                tw.write_all(&frame.payload).await?;
                            }
                            Command::Close => return Ok::<(), io::Error>(()),
                            _ => {}
                        }
                        buf.copy_within(consumed..filled, 0);
                        filled -= consumed;
                    }
                    None => break,
                }
            }

            // Read more data from client with idle timeout
            let n = match tokio::time::timeout(IDLE_TIMEOUT, cr.read(&mut buf[filled..])).await {
                Ok(result) => result?,
                Err(_) => {
                    tracing::debug!("Server upstream idle timeout (5m)");
                    return Ok(());
                }
            };
            if n == 0 {
                break;
            }
            filled += n;
        }
        Ok(())
    };

    // Target → Client (read raw, encode as protocol frames)
    let downstream = async move {
        let mut buf = vec![0u8; 8192];
        loop {
            let n = match tokio::time::timeout(IDLE_TIMEOUT, tr.read(&mut buf)).await {
                Ok(result) => result?,
                Err(_) => {
                    tracing::debug!("Server downstream idle timeout (5m)");
                    return Ok::<(), io::Error>(());
                }
            };
            if n == 0 {
                let close = codec_encode.encode_frame(Command::Close, &[])?;
                cw.write_all(&close).await?;
                break;
            }
            let frame = codec_encode.encode_frame(Command::Data, &buf[..n])?;
            cw.write_all(&frame).await?;
        }
        Ok::<(), io::Error>(())
    };

    let result = tokio::time::timeout(MAX_LIFETIME, async {
        tokio::select! {
            result = upstream => result,
            result = downstream => result,
        }
    });

    match result.await {
        Ok(r) => r,
        Err(_) => {
            tracing::debug!("Server relay timed out (1h max)");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};
    use xr_proto::protocol::Frame;

    fn make_codec() -> Codec {
        let obfs = Obfuscator::new(
            b"handler-test-key-0123456789ABCD".to_vec(),
            0xDEADBEEF,
            ModifierStrategy::PositionalXorRotate,
        );
        Codec::new(obfs, 0, 0)
    }

    /// Дочитать один кадр из потока, накапливая в буфере ровно как это делает
    /// сам handle_client - нужен тестам счастливого пути, которым важно не
    /// то, сколько раз пришёл read, а то, что кадр в итоге собрался.
    async fn read_one_frame(client: &mut TcpStream, codec: &Codec) -> Frame {
        let mut buf = vec![0u8; 4096];
        let mut filled = 0;
        loop {
            if let Some((frame, _)) = codec.decode_frame(&buf[..filled]).unwrap() {
                return frame;
            }
            let n = tokio::time::timeout(Duration::from_secs(3), client.read(&mut buf[filled..]))
                .await
                .expect("сервер обязан ответить на честный кадр")
                .unwrap();
            assert!(n > 0, "сервер закрыл соединение, не ответив ConnectAck");
            filled += n;
        }
    }

    /// XR-215: заголовок кадра честно обещает payload на 5000 байт, но по
    /// проводу приходят только первые 4096 - ровно ёмкость буфера первого
    /// кадра, кадр целиком никогда не собирается. Сервер обязан ответить
    /// fallback-страницей, а не рвать соединение.
    #[tokio::test]
    async fn overflow_during_handshake_sends_fallback_not_reset() {
        let codec = make_codec();
        let fallback_response = b"XR-215-FALLBACK".to_vec();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_codec = codec.clone();
        let server_fallback = fallback_response.clone();
        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            handle_client(
                stream,
                peer,
                server_codec,
                Duration::from_secs(2),
                Some(server_fallback),
                crate::mux_handler::StreamLimits::new(1024, 1024),
            )
            .await
        });

        let mut client = TcpStream::connect(addr).await.unwrap();

        let oversized_payload = vec![0xABu8; 5000];
        let wire = codec.encode_frame(Command::Connect, &oversized_payload).unwrap();
        assert!(wire.len() > 4096, "кадр должен быть больше буфера хендшейка");
        client.write_all(&wire[..4096]).await.unwrap();

        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), client.read_to_end(&mut received))
            .await
            .expect("сервер обязан ответить fallback и закрыть соединение, а не зависнуть")
            .unwrap();

        assert_eq!(
            received, fallback_response,
            "переполненный незавершённый кадр должен получать fallback-ответ, а не голый разрыв"
        );

        server
            .await
            .unwrap()
            .expect("handle_client не должен возвращать ошибку, когда отдан fallback");
    }

    /// XR-215 (замечание ревью): мутации M1-M3 проверяли только недосрабатывание
    /// условия (возврат старого бага), а от подмены в другую сторону -
    /// `if true` вместо `filled >= buf.len()`, то есть fallback уходит на любом
    /// `Ok(None)`, а не только на заполненном буфере - не было ни одного
    /// теста. Честный маленький Connect-кадр, пришедший целиком за один
    /// присед, обязан дособраться и дойти до ConnectAck.
    #[tokio::test]
    async fn honest_small_connect_frame_reaches_connect_ack() {
        let codec = make_codec();

        // Слушатель под "target" держим живым до конца теста: ConnectAck
        // сервер шлёт раньше, чем достучится до цели, но сам connect к цели
        // всё равно случится следом, и без живого листенера он бы упал.
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_codec = codec.clone();
        tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            let _ = handle_client(
                stream,
                peer,
                server_codec,
                Duration::from_secs(2),
                None,
                crate::mux_handler::StreamLimits::new(1024, 1024),
            )
            .await;
        });

        let mut client = TcpStream::connect(addr).await.unwrap();

        let payload = TargetAddr::Ip(target_addr).encode().unwrap();
        let wire = codec.encode_frame(Command::Connect, &payload).unwrap();
        client.write_all(&wire).await.unwrap();

        let frame = read_one_frame(&mut client, &codec).await;
        assert_eq!(
            frame.command,
            Command::ConnectAck,
            "честный кадр в один присед должен дойти до ConnectAck, а не до fallback"
        );

        drop(target);
    }

    /// Мок-приёмник для детерминированной проверки разбиения кадра на два
    /// read: границы чтений заданы явно списком чанков, а не тем, что успеет
    /// накопиться на живом сокете к моменту вызова read. На реальном
    /// TcpStream оба write_all без паузы между ними могут долежать в
    /// сокетном буфере до первого read целиком, и разбиение на два read не
    /// гарантировано - отсюда и был sleep, которого этот мок не требует.
    struct ChunkedReader {
        chunks: std::collections::VecDeque<Vec<u8>>,
    }

    impl ChunkedReader {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self { chunks: chunks.into() }
        }
    }

    impl AsyncRead for ChunkedReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            if let Some(chunk) = self.chunks.pop_front() {
                assert!(
                    chunk.len() <= buf.remaining(),
                    "тестовый чанк не помещается в буфер read"
                );
                buf.put_slice(&chunk);
            }
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// XR-215 (замечание ревью, второй проход): прежняя версия этого теста
    /// разводила два write_all на живом TcpStream реальным
    /// tokio::time::sleep(50ms), чтобы сервер успел прочитать первую половину
    /// кадра раньше второй записи - тест не флакал сам (30/30 прогонов, в том
    /// числе под нагрузкой), но его способность ловить мутацию M4 зависела от
    /// исхода этой гонки, а не была гарантирована. Цикл приёма первого кадра
    /// вынесен в read_first_frame и обобщён по AsyncRead ровно для этого
    /// случая: границы двух read задаются явно чанками мока, безо всякого сна.
    #[tokio::test]
    async fn honest_connect_frame_split_across_reads() {
        let codec = make_codec();

        let target_addr: SocketAddr = "127.0.0.1:443".parse().unwrap();
        let payload = TargetAddr::Ip(target_addr).encode().unwrap();
        let wire = codec.encode_frame(Command::Connect, &payload).unwrap();
        assert!(wire.len() > 2, "кадру есть что делить на части");
        let split = wire.len() / 2;

        let mut reader = ChunkedReader::new(vec![wire[..split].to_vec(), wire[split..].to_vec()]);
        let mut buf = vec![0u8; 4096];

        let outcome = read_first_frame(&mut reader, &mut buf, &codec, Duration::from_secs(2))
            .await
            .expect("кадр, пришедший двумя read, должен дособраться без ошибки");

        match outcome {
            FirstFrameOutcome::Ready(frame, leftover) => {
                assert_eq!(
                    frame.command,
                    Command::Connect,
                    "кадр, дособранный из двух read, должен разобраться как Connect"
                );
                assert_eq!(leftover, 0, "после кадра в буфере не должно остаться лишних байт");
            }
            FirstFrameOutcome::NeedFallback(reason) => {
                panic!(
                    "кадр по частям ушёл в fallback ({:?}), а должен был дособраться",
                    reason
                );
            }
        }
    }
}
