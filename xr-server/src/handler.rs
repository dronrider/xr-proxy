/// Server-side connection handler: accept obfuscated connections,
/// decode Connect command, connect to target, relay data.
use std::io;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Duration;
use xr_proto::protocol::{Codec, Command, TargetAddr};

/// Handle a single client connection end-to-end.
pub async fn handle_client(
    mut client: TcpStream,
    client_addr: SocketAddr,
    codec: Codec,
    timeout: Duration,
    fallback_response: Option<Vec<u8>>,
) -> io::Result<()> {
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
                // Shift buffer for any remaining data
                buf.copy_within(consumed..filled, 0);
                filled -= consumed;
                break frame;
            }
            Ok(None) => {
                if filled > 4096 {
                    // Too much data without a valid frame — probably not our protocol
                    return send_fallback_and_close(&mut client, &buf[..filled], fallback_response).await;
                }
                continue;
            }
            Err(_) => {
                // Decode error (wrong key / not our protocol)
                tracing::debug!("Invalid frame from {}, sending fallback", client_addr);
                return send_fallback_and_close(&mut client, &buf[..filled], fallback_response).await;
            }
        }
    };

    if connect_frame.command != Command::Connect {
        tracing::debug!("Expected Connect from {}, got {:?}", client_addr, connect_frame.command);
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected Connect"));
    }

    // Parse target address from payload
    let (target_addr, _) = TargetAddr::decode(&connect_frame.payload)?;
    let target_sockaddr = resolve_target(&target_addr).await?;

    tracing::info!("{} -> {} ({})", client_addr, target_sockaddr, addr_display(&target_addr));

    // Connect to target
    let mut target = tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect(target_sockaddr),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "target connect timeout"))??;

    // Send ConnectAck (status=0 for success)
    let ack = codec.encode_frame(Command::ConnectAck, &[0])?;
    client.write_all(&ack).await?;

    // If there was leftover data after the Connect frame, it's the first Data frame
    // We need to handle it properly in the relay loop

    // Relay data bidirectionally with obfuscation
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

        // Process any initial data leftover from handshake
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

            // Read more data from client
            let n = cr.read(&mut buf[filled..]).await?;
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
            let n = tr.read(&mut buf).await?;
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

    tokio::select! {
        result = upstream => result,
        result = downstream => result,
    }
}
