/// Generalized tunnel logic: connect through xr-server, relay obfuscated data.
///
/// This module extracts the core tunnel mechanics from xr-client/proxy.rs
/// so they can be reused by mobile/desktop clients (xr-core) as well.
use std::io;
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Duration;

use crate::protocol::{Codec, Command, TargetAddr};

/// Connect to the xr-server. Single attempt with fast timeout.
/// Server sends ConnectAck instantly, so if connection doesn't work
/// in 3 seconds — it won't work at all, fall back to Direct.
pub async fn connect_to_server(addr: &SocketAddr) -> io::Result<TcpStream> {
    tokio::time::timeout(
        Duration::from_secs(3),
        TcpStream::connect(addr),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut,
        format!("server connect timeout ({})", addr)))?
}

/// Connect to the xr-server with retry logic (legacy, used by older configs).
pub async fn connect_with_retry(addr: &SocketAddr, _max_retries: u32) -> io::Result<TcpStream> {
    connect_to_server(addr).await
}

/// Perform the tunnel handshake: send Connect, wait for ConnectAck.
///
/// After this returns Ok, the `server` stream is ready for obfuscated data relay.
pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
    server: &mut S,
    target_addr: &TargetAddr,
    codec: &Codec,
) -> io::Result<()> {
    let connect_payload = target_addr.encode();
    let connect_frame = codec.encode_frame(Command::Connect, &connect_payload)?;
    server.write_all(&connect_frame).await?;

    // Wait for ConnectAck
    let mut ack_buf = vec![0u8; 256];
    let mut ack_filled = 0;

    loop {
        let n = tokio::time::timeout(
            Duration::from_secs(3),
            server.read(&mut ack_buf[ack_filled..]),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect ack timeout"))??;

        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "server closed during handshake",
            ));
        }
        ack_filled += n;

        match codec.decode_frame(&ack_buf[..ack_filled])? {
            Some((frame, _)) => {
                if frame.command != Command::ConnectAck {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "expected ConnectAck",
                    ));
                }
                if frame.payload.first() != Some(&0) {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        "server rejected connect",
                    ));
                }
                return Ok(());
            }
            None => continue, // need more data
        }
    }
}

/// Obfuscated relay: frames client data into protocol frames and vice versa.
///
/// `client` is the local side (app/browser/TUN socket).
/// `server` is the xr-server connection.
///
/// Each read times out after `idle_timeout`; total connection limited to `max_lifetime`.
pub async fn relay_obfuscated<C, S>(
    client: &mut C,
    server: &mut S,
    codec: &Codec,
    idle_timeout: Duration,
    max_lifetime: Duration,
) -> io::Result<()>
where
    C: AsyncRead + AsyncWrite + Unpin,
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut cr, mut cw) = tokio::io::split(client);
    let (mut sr, mut sw) = tokio::io::split(server);

    let codec_up = codec.clone();
    let codec_down = codec.clone();

    // Client → Server (obfuscate)
    let upload = async move {
        let mut buf = vec![0u8; 8192];
        loop {
            let n = match tokio::time::timeout(idle_timeout, cr.read(&mut buf)).await {
                Ok(result) => result?,
                Err(_) => {
                    tracing::debug!("Upload idle timeout");
                    return Ok::<(), io::Error>(());
                }
            };
            if n == 0 {
                let close = codec_up.encode_frame(Command::Close, &[])?;
                sw.write_all(&close).await?;
                break;
            }
            let frame = codec_up.encode_frame(Command::Data, &buf[..n])?;
            sw.write_all(&frame).await?;
        }
        Ok::<(), io::Error>(())
    };

    // Server → Client (deobfuscate)
    let download = async move {
        let mut buf = vec![0u8; 65536 + 256]; // max frame size
        let mut filled = 0;
        loop {
            let n = match tokio::time::timeout(idle_timeout, sr.read(&mut buf[filled..])).await {
                Ok(result) => result?,
                Err(_) => {
                    tracing::debug!("Download idle timeout");
                    return Ok::<(), io::Error>(());
                }
            };
            if n == 0 {
                break;
            }
            filled += n;

            // Decode frames from buffer
            loop {
                match codec_down.decode_frame(&buf[..filled])? {
                    Some((frame, consumed)) => {
                        match frame.command {
                            Command::Data => {
                                cw.write_all(&frame.payload).await?;
                            }
                            Command::Close => return Ok(()),
                            _ => {}
                        }
                        // Shift buffer
                        buf.copy_within(consumed..filled, 0);
                        filled -= consumed;
                    }
                    None => break, // need more data
                }
            }
        }
        Ok::<(), io::Error>(())
    };

    let result = tokio::time::timeout(max_lifetime, async {
        tokio::select! {
            result = upload => result,
            result = download => result,
        }
    });

    match result.await {
        Ok(r) => r,
        Err(_) => {
            tracing::debug!("Tunnel relay timed out");
            Ok(())
        }
    }
}

/// Simple bidirectional relay for direct connections (no obfuscation).
///
/// Timeout if no data flows in either direction for `max_lifetime`.
pub async fn relay_bidirectional<A, B>(
    a: &mut A,
    b: &mut B,
    max_lifetime: Duration,
) -> io::Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut ar, mut aw) = tokio::io::split(a);
    let (mut br, mut bw) = tokio::io::split(b);

    let r1 = tokio::io::copy(&mut ar, &mut bw);
    let r2 = tokio::io::copy(&mut br, &mut aw);

    let timeout = tokio::time::timeout(max_lifetime, async {
        tokio::select! {
            result = r1 => result.map(|_| ()),
            result = r2 => result.map(|_| ()),
        }
    });

    match timeout.await {
        Ok(result) => result,
        Err(_) => {
            tracing::debug!("Direct relay timed out");
            Ok(())
        }
    }
}
