/// Server-side connection handler: accept obfuscated connections,
/// decode Connect command, connect to target, relay data.
use std::io;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Duration;
use xr_proto::protocol::{Codec, Command, TargetAddr};

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
) -> io::Result<()> {
    configure_socket(&client);

    // Read first frame (Connect command) with timeout
    let mut buf = vec![0u8; 4096];
    let mut filled = 0;

    let connect_frame = loop {
        let n = tokio::time::timeout(timeout, client.read(&mut buf[filled..]))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handshake timeout"))??;

        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::ConnectionReset, "client closed"));
        }
        filled += n;

        match codec.decode_frame(&buf[..filled]) {
            Ok(Some((frame, consumed))) => {
                buf.copy_within(consumed..filled, 0);
                filled -= consumed;
                break frame;
            }
            Ok(None) => {
                if filled > 4096 {
                    return send_fallback_and_close(&mut client, &buf[..filled], fallback_response).await;
                }
                continue;
            }
            Err(_) => {
                tracing::debug!("Invalid frame from {}, sending fallback", client_addr);
                return send_fallback_and_close(&mut client, &buf[..filled], fallback_response).await;
            }
        }
    };

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
    _initial_data: &[u8],
    fallback_response: Option<Vec<u8>>,
) -> io::Result<()> {
    if let Some(response) = fallback_response {
        let _ = client.write_all(&response).await;
    }
    // Silently close — don't give probes any useful info
    Ok(())
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
