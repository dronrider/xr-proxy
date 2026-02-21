/// Transparent proxy core: accept connections, extract SNI, route, tunnel.
use crate::routing::{Action, Router};
use crate::sni;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, Duration};
use xr_proto::protocol::{Codec, Command, TargetAddr};

// ── SO_ORIGINAL_DST ──────────────────────────────────────────────────

/// Get the original destination address from a redirected (NAT) connection.
/// Uses the SO_ORIGINAL_DST socket option on Linux.
fn get_original_dst(stream: &TcpStream) -> io::Result<SocketAddr> {
    use std::os::unix::io::AsRawFd;

    let fd = stream.as_raw_fd();

    unsafe {
        let mut addr: libc::sockaddr_in = std::mem::zeroed();
        let mut len: libc::socklen_t = std::mem::size_of::<libc::sockaddr_in>() as u32;

        // SOL_IP = 0, SO_ORIGINAL_DST = 80
        let ret = libc::getsockopt(
            fd,
            0,  // SOL_IP
            80, // SO_ORIGINAL_DST
            &mut addr as *mut _ as *mut libc::c_void,
            &mut len,
        );

        if ret != 0 {
            return Err(io::Error::last_os_error());
        }

        let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
        let port = u16::from_be(addr.sin_port);
        Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)))
    }
}

// ── Shared state ─────────────────────────────────────────────────────

pub struct ProxyState {
    pub router: Router,
    pub codec: Codec,
    pub server_addr: SocketAddr,
    pub on_server_down: Action,
}

// ── Main proxy loop ──────────────────────────────────────────────────

pub async fn run_proxy(
    listen_port: u16,
    state: Arc<ProxyState>,
) -> io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", listen_port)).await?;
    tracing::info!("Transparent proxy listening on 0.0.0.0:{}", listen_port);

    loop {
        let (client_stream, client_addr) = listener.accept().await?;
        let state = state.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(client_stream, client_addr, state).await {
                // Log connection errors at warn level so they're visible with default settings
                tracing::warn!("Connection from {} failed: {}", client_addr, e);
            }
        });
    }
}

async fn handle_connection(
    mut client: TcpStream,
    client_addr: SocketAddr,
    state: Arc<ProxyState>,
) -> io::Result<()> {
    // Get original destination
    let orig_dst = get_original_dst(&client)?;
    let dest_ip = orig_dst.ip();

    // Peek at first bytes for SNI extraction (up to 4KB should cover ClientHello)
    let mut peek_buf = vec![0u8; 4096];
    let n = client.peek(&mut peek_buf).await?;
    let sni = sni::extract_sni(&peek_buf[..n]);

    let sni_display = sni.as_deref().unwrap_or("-");
    let action = state.router.resolve(sni.as_deref(), dest_ip);

    tracing::info!(
        "{} -> {} [SNI: {}] => {:?}",
        client_addr, orig_dst, sni_display, action
    );

    match action {
        Action::Direct => {
            // Connect directly to the original destination
            let mut target = TcpStream::connect(orig_dst).await?;
            relay_bidirectional(&mut client, &mut target).await
        }
        Action::Proxy => {
            // Connect through the obfuscated tunnel
            match tunnel_connection(&mut client, orig_dst, sni.as_deref(), &state).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    tracing::warn!("Tunnel to {} failed: {}, fallback={:?}",
                        orig_dst, e, state.on_server_down);
                    if state.on_server_down == Action::Direct {
                        // Fallback: try direct connection
                        let mut target = TcpStream::connect(orig_dst).await?;
                        relay_bidirectional(&mut client, &mut target).await
                    } else {
                        Err(e)
                    }
                }
            }
        }
    }
}

// ── Tunnel through server ────────────────────────────────────────────

async fn tunnel_connection(
    client: &mut TcpStream,
    orig_dst: SocketAddr,
    sni: Option<&str>,
    state: &ProxyState,
) -> io::Result<()> {
    // Connect to xr-proxy-server
    let mut server = connect_with_retry(&state.server_addr, 3).await?;

    // Build connect payload with target address
    let target_addr = if let Some(domain) = sni {
        TargetAddr::Domain(domain.to_string(), orig_dst.port())
    } else {
        TargetAddr::Ip(orig_dst)
    };

    let connect_payload = target_addr.encode();
    let connect_frame = state.codec.encode_frame(Command::Connect, &connect_payload)?;
    server.write_all(&connect_frame).await?;

    // Wait for ConnectAck
    let mut ack_buf = vec![0u8; 256];
    let mut ack_filled = 0;

    loop {
        let n = tokio::time::timeout(
            Duration::from_secs(10),
            server.read(&mut ack_buf[ack_filled..]),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect ack timeout"))??;

        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::ConnectionReset, "server closed during handshake"));
        }
        ack_filled += n;

        match state.codec.decode_frame(&ack_buf[..ack_filled])? {
            Some((frame, _)) => {
                if frame.command != Command::ConnectAck {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "expected ConnectAck"));
                }
                if frame.payload.first() != Some(&0) {
                    return Err(io::Error::new(io::ErrorKind::ConnectionRefused, "server rejected connect"));
                }
                break;
            }
            None => continue, // need more data
        }
    }

    // Now relay data: client <-> obfuscated tunnel <-> server
    relay_obfuscated(client, &mut server, &state.codec).await
}

async fn connect_with_retry(addr: &SocketAddr, max_retries: u32) -> io::Result<TcpStream> {
    let mut delay = Duration::from_secs(1);

    for attempt in 0..=max_retries {
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                if attempt == max_retries {
                    return Err(e);
                }
                tracing::warn!("Connect to server failed (attempt {}): {}", attempt + 1, e);
                sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(60));
            }
        }
    }

    unreachable!()
}

// ── Data relay ───────────────────────────────────────────────────────

/// Simple bidirectional relay for direct connections.
async fn relay_bidirectional(a: &mut TcpStream, b: &mut TcpStream) -> io::Result<()> {
    let (mut ar, mut aw) = a.split();
    let (mut br, mut bw) = b.split();

    let r1 = tokio::io::copy(&mut ar, &mut bw);
    let r2 = tokio::io::copy(&mut br, &mut aw);

    tokio::select! {
        result = r1 => result.map(|_| ()),
        result = r2 => result.map(|_| ()),
    }
}

/// Obfuscated relay: frames client data into protocol frames and vice versa.
async fn relay_obfuscated(
    client: &mut TcpStream,
    server: &mut TcpStream,
    codec: &Codec,
) -> io::Result<()> {
    let (mut cr, mut cw) = client.split();
    let (mut sr, mut sw) = server.split();

    let codec_up = codec.clone();
    let codec_down = codec.clone();

    // Client → Server (obfuscate)
    let upload = async move {
        let mut buf = vec![0u8; 8192];
        loop {
            let n = cr.read(&mut buf).await?;
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
            let n = sr.read(&mut buf[filled..]).await?;
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

    tokio::select! {
        result = upload => result,
        result = download => result,
    }
}
