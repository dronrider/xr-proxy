//! Server-side multiplexed connection handler.
//!
//! Accepts MuxInit, then serves multiple concurrent streams over
//! one TCP connection. Each stream maps to an independent target.

use std::io;
use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Duration;

use xr_proto::mux::{mux_handshake_server, Multiplexer, NewStream};
use xr_proto::protocol::{Codec, Command, Frame, TargetAddr};

const TARGET_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_LIFETIME: Duration = Duration::from_secs(3600);

/// Handle a multiplexed client connection.
/// Called after the first frame was detected as MuxInit.
pub async fn handle_mux_client(
    mut client: TcpStream,
    client_addr: SocketAddr,
    codec: Codec,
    init_frame: &Frame,
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

    let result = tokio::time::timeout(MAX_LIFETIME, async {
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

    match result {
        Ok(r) => r,
        Err(_) => {
            tracing::debug!("{} mux max lifetime reached", client_addr);
            Ok(())
        }
    }
}

/// Relay data between a MuxStream and a target TCP connection.
async fn relay_stream(
    mut mux_stream: xr_proto::mux::MuxStream,
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

    // Use channels to decouple MuxStream recv (mutable) from send (shared).
    let (dl_tx, mut dl_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    // MuxStream → Target (upload) + receive download notifications.
    let upload = async {
        loop {
            tokio::select! {
                data = mux_stream.recv() => {
                    match data {
                        Some(d) if !d.is_empty() => tw.write_all(&d).await?,
                        _ => break,
                    }
                }
                // Forward downloaded data to MuxStream.
                data = dl_rx.recv() => {
                    match data {
                        Some(d) => mux_stream.send(&d).await?,
                        None => break,
                    }
                }
            }
        }
        Ok::<(), io::Error>(())
    };

    // Target → download channel.
    let download = async {
        let mut buf = vec![0u8; 8192];
        loop {
            let n = match tokio::time::timeout(IDLE_TIMEOUT, tr.read(&mut buf)).await {
                Ok(result) => result?,
                Err(_) => return Ok::<(), io::Error>(()),
            };
            if n == 0 { break; }
            if dl_tx.send(buf[..n].to_vec()).await.is_err() { break; }
        }
        Ok::<(), io::Error>(())
    };

    let result = tokio::time::timeout(MAX_LIFETIME, async {
        tokio::select! {
            r = upload => r,
            r = download => r,
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
