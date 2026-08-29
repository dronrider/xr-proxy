/// Server-side connection handler: accept obfuscated connections,
/// decode Connect command, connect to target, relay data.
use std::io;
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
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

/// Принятое соединение от accept-петли: permit лимита коннектов живёт ровно
/// столько, сколько соединение в руках `handle_client`, включая хендшейк.
/// Вынесено из main (XR-202), чтобы тест мог занять слот приёма медленным
/// клиентом и убедиться, что дедлайн хендшейка слот освобождает.
pub async fn serve_connection(
    client: TcpStream,
    client_addr: SocketAddr,
    codec: Codec,
    handshake_timeout: Duration,
    fallback_response: Option<Vec<u8>>,
    limits: crate::mux_handler::StreamLimits,
    connections: &Semaphore,
) {
    let _permit = match connections.try_acquire() {
        Ok(p) => p,
        Err(_) => {
            tracing::warn!("Connection limit reached, rejecting {}", client_addr);
            return;
        }
    };

    if let Err(e) =
        handle_client(client, client_addr, codec, handshake_timeout, fallback_response, limits).await
    {
        tracing::warn!("Client {} error: {}", client_addr, e);
    }
}

/// Handle a single client connection end-to-end.
pub async fn handle_client(
    mut client: TcpStream,
    client_addr: SocketAddr,
    codec: Codec,
    handshake_timeout: Duration,
    fallback_response: Option<Vec<u8>>,
    limits: crate::mux_handler::StreamLimits,
) -> io::Result<()> {
    configure_socket(&client);

    let mut buf = vec![0u8; 4096];

    // XR-202: хендшейк идёт под одним общим дедлайном, а не под таймаутом на
    // каждый read по отдельности. Медленный клиент, капающий байты с паузами
    // короче таймаута, держал permit из max_connections неограниченно долго:
    // каждый read укладывался в свой срок, и 256 таких коннектов запирали
    // приём новых туннелей. Дедлайн отсчитывается от accept и накрывает весь
    // путь до релея: первый кадр, DNS и connect к цели.
    let outcome = tokio::time::timeout(
        handshake_timeout,
        handshake(&mut client, client_addr, &codec, &mut buf, fallback_response),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handshake deadline"))??;

    match outcome {
        HandshakeOutcome::Done => Ok(()),
        HandshakeOutcome::Mux(connect_frame) => {
            crate::mux_handler::handle_mux_client(
                client, client_addr, codec, &connect_frame, limits,
            )
            .await
        }
        HandshakeOutcome::Relay { mut target, leftover } => {
            relay_obfuscated(&mut client, &mut target, &codec, &buf[..leftover]).await
        }
    }
}

/// Куда девать соединение, договорившееся с хендшейком.
enum HandshakeOutcome {
    /// Первый кадр это MuxInit: сессия уходит в mux-обработчик.
    Mux(Frame),
    /// Одиночный Connect: ConnectAck отправлен, цель подключена. `leftover` -
    /// байты после кадра, уже лежащие в начале `buf`.
    Relay { target: TcpStream, leftover: usize },
    /// Соединение закончено на стороне хендшейка (отдан fallback-ответ).
    Done,
}

async fn handshake(
    client: &mut TcpStream,
    client_addr: SocketAddr,
    codec: &Codec,
    buf: &mut Vec<u8>,
    fallback_response: Option<Vec<u8>>,
) -> io::Result<HandshakeOutcome> {
    let (connect_frame, filled) = match read_first_frame(client, buf, codec).await? {
        FirstFrameOutcome::Ready(frame, leftover) => (frame, leftover),
        FirstFrameOutcome::NeedFallback(reason) => {
            if reason == FallbackReason::InvalidFrame {
                tracing::debug!("Invalid frame from {}, sending fallback", client_addr);
            }
            send_fallback_and_close(client, fallback_response).await?;
            return Ok(HandshakeOutcome::Done);
        }
    };

    // Multiplexed or legacy single-stream?
    if connect_frame.command == Command::MuxInit {
        return Ok(HandshakeOutcome::Mux(connect_frame));
    }

    if connect_frame.command != Command::Connect {
        tracing::debug!("Expected Connect from {}, got {:?}", client_addr, connect_frame.command);
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected Connect"));
    }

    let (target_addr, _) = TargetAddr::decode(&connect_frame.payload)?;

    // Send ConnectAck IMMEDIATELY, before DNS resolution and target connect.
    let ack = codec.encode_frame(Command::ConnectAck, &[0])?;
    client.write_all(&ack).await?;
    tracing::info!("{} ack sent for {}", client_addr, addr_display(&target_addr));

    let target_sockaddr = resolve_target(&target_addr).await?;
    tracing::info!("{} -> {} ({})", client_addr, target_sockaddr, addr_display(&target_addr));

    let target = tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(target_sockaddr),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "target connect timeout"))??;

    configure_socket(&target);

    Ok(HandshakeOutcome::Relay { target, leftover: filled })
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

/// Копит первый кадр хендшейка из `reader` в `buf`, по одному `read` за раз.
/// Дедлайна внутри нет: срок всему хендшейку ставит вызывающий (XR-202), и
/// висящий здесь вечно `read` рвётся его общим `timeout`. Обобщена по
/// `AsyncRead`, а не завязана на `TcpStream`: тесту счастливого пути на
/// разбиение кадра между двумя read нужен приёмник, у которого границы
/// чтений заданы явно, а не тем, что успеет накопиться в сокете к моменту
/// вызова `read` - на живом `TcpStream` это гонка (см. XR-215).
async fn read_first_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    codec: &Codec,
) -> io::Result<FirstFrameOutcome> {
    let mut filled = 0;

    loop {
        let n = reader.read(&mut buf[filled..]).await?;

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

    /// XR-202: медленный клиент капает валидные байты незавершённого кадра
    /// порциями, каждая из которых укладывается в старый покомпонентный
    /// таймаут read. Хендшейк обязан оборваться по общему дедлайну: сервер
    /// закрывает соединение, а не копит кадр неограниченно долго.
    #[tokio::test]
    async fn slow_drip_handshake_is_cut_by_total_deadline() {
        let codec = make_codec();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_codec = codec.clone();
        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            handle_client(
                stream,
                peer,
                server_codec,
                Duration::from_secs(1),
                None,
                crate::mux_handler::StreamLimits::new(1024, 1024),
            )
            .await
        });

        let client = TcpStream::connect(addr).await.unwrap();
        let (mut read_half, mut write_half) = client.into_split();

        // Заголовок честно обещает большой payload, но по проводу идут лишь
        // первые байты, порциями по 64 с паузой 50 мс: любой одиночный read
        // укладывался бы в прежний таймаут на сам read.
        let oversized_payload = vec![0xCDu8; 5000];
        let wire = codec.encode_frame(Command::Connect, &oversized_payload).unwrap();

        let drip = tokio::spawn(async move {
            for chunk in wire.chunks(64) {
                if write_half.write_all(chunk).await.is_err() {
                    // Сервер закрыл соединение - капать больше некуда.
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });

        let mut received = Vec::new();
        let closed = tokio::time::timeout(Duration::from_secs(3), read_half.read_to_end(&mut received))
            .await
            .expect("сервер обязан закрыть соединение по дедлайну хендшейка, а не висеть");
        // Reset вместо чистого EOF допустим: непрочитанные байты дропа ещё
        // лежат в сокете, и закрытие отдаётся как RST.
       match closed {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            Err(e) => panic!("неожиданная ошибка чтения после дедлайна: {}", e),
        }

        drip.abort();

        let err = server
            .await
            .unwrap()
            .expect_err("недоговоривший хендшейк обязан вернуть ошибку");
        assert_eq!(
            err.kind(),
            io::ErrorKind::TimedOut,
            "причиной отказа должен быть общий дедлайн хендшейка"
        );
    }

    /// XR-202: слот приёма (permit из max_connections) освобождается по
    /// дедлайну хендшейка, а не по сумме покомпонентных таймаутов. Семфор на
    /// один коннект: медленный клиент занимает единственный слот, и честный
    /// коннект следом обязан быть обслужен, как только дедлайн первого
    /// истёк.
    #[tokio::test]
    async fn deadline_releases_connection_slot_for_next_client() {
        let codec = make_codec();

        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();

        let slow_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let slow_addr = slow_listener.local_addr().unwrap();
        let honest_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let honest_addr = honest_listener.local_addr().unwrap();

        let connections = std::sync::Arc::new(Semaphore::new(1));
        let limits = crate::mux_handler::StreamLimits::new(1024, 1024);

        // Медленный клиент занимает единственный слот приёма.
        let slow_codec = codec.clone();
        let slow_sem = connections.clone();
        let mut slow = tokio::spawn(async move {
            let (stream, peer) = slow_listener.accept().await.unwrap();
            serve_connection(
                stream,
                peer,
                slow_codec,
                Duration::from_secs(1),
                None,
                limits,
                &slow_sem,
            )
            .await;
        });

        let slow_client = TcpStream::connect(slow_addr).await.unwrap();
        let (_slow_read_half, mut slow_write_half) = slow_client.into_split();
        let oversized_payload = vec![0u8; 5000];
        let wire = codec.encode_frame(Command::Connect, &oversized_payload).unwrap();

        // Капание непрерывное, до самого конца теста: замолкший клиент рвётся
        // и старым покомпонентным таймаутом read, слот тогда освобождается
        // тем же путём и тест перестаёт различать семантики. Непрерывное
        // капанье держит permit на старом коде, и честный коннект получает
        // отказ от try_acquire вместо ConnectAck.
        let drip = tokio::spawn(async move {
            loop {
                for chunk in wire.chunks(64) {
                    if slow_write_half.write_all(chunk).await.is_err() {
                        // Сервер закрыл соединение - капать больше некуда.
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        });

        // Дедлайн медленного истёк - его таска вернулась и отдала слот.
        // JoinHandle не даёт результата: сама таска ничего не возвращает,
        // интересен только факт её завершения.
        let _ = tokio::time::timeout(Duration::from_secs(3), &mut slow)
            .await
            .expect("таска медленного клиента обязана вернуться по дедлайну хендшейка");

        // Честный коннект через ту же обёртку приёма должен получить слот.
        let honest_codec = codec.clone();
        let honest_sem = connections.clone();
        let honest = tokio::spawn(async move {
            let (stream, peer) = honest_listener.accept().await.unwrap();
            serve_connection(
                stream,
                peer,
                honest_codec,
                Duration::from_secs(2),
                None,
                crate::mux_handler::StreamLimits::new(1024, 1024),
                &honest_sem,
            )
            .await;
        });

        let mut honest_client = TcpStream::connect(honest_addr).await.unwrap();
        let payload = TargetAddr::Ip(target_addr).encode().unwrap();
        let connect_wire = codec.encode_frame(Command::Connect, &payload).unwrap();
        honest_client.write_all(&connect_wire).await.unwrap();

        let frame = read_one_frame(&mut honest_client, &codec).await;
        assert_eq!(
            frame.command,
            Command::ConnectAck,
            "после дедлайна медленного клиента слот приёма должен достаться честному"
        );

        drop(target);
        honest.abort();
        drip.abort();
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

        let outcome = read_first_frame(&mut reader, &mut buf, &codec)
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
