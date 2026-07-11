//! Consumer-side relay client (LLD-23 §2.2, §3.5).
//!
//! A consumer that can't reach the agent directly dials the **relay** with the
//! same obfuscated mux as the proxy uses, and opens one stream per HTTP
//! connection: `Connect` on the [`RELAY_CONNECT_TARGET`] pseudo-target, then the
//! [`RelayToken`] as the first Data frame (the hello). The relay replies with a
//! single [`RELAY_HELLO_OK`] byte and starts a blind splice to the agent, or
//! closes the stream (agent offline / token rejected).
//!
//! The consumer's HTTP stack is left untouched: [`LoopbackForwarder`] listens on
//! `127.0.0.1:0` and turns every accepted connection into a relay stream, so a
//! reqwest/ureq client just talks to a local address (resolve-override) and runs
//! pinned TLS **end to end** to the agent, and the relay only moves bytes.
//!
//! The pseudo-targets never resolve to the network: the relay matches on the
//! exact string and can't be steered outward (SSRF-class excluded by design,
//! LLD-23 §5.2).

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::mux::{mux_handshake_client, mux_open_stream, MuxStream, Multiplexer};
use crate::protocol::{Codec, TargetAddr};
use crate::share::{RelayGrant, RelayToken};

/// Pseudo-target the consumer leg opens against the relay (LLD-23 §3.5). The
/// relay routes on the exact string and never resolves it.
pub const RELAY_CONNECT_TARGET: &str = "xr-relay:connect";
/// Pseudo-target the agent uses to open its registration stream (LLD-23 §2.1).
pub const RELAY_REGISTER_TARGET: &str = "xr-relay:register";
/// Pseudo-target the relay opens **towards the agent** for each reverse-stream
/// (LLD-23 §2.2). The agent serves its HTTP router over such a stream.
pub const RELAY_REVERSE_TARGET: &str = "xr-relay:reverse";
/// The relay's hello verdict on success: transit granted, splice begins. A
/// failed hello is answered with a `Close` instead (agent offline / rejected).
pub const RELAY_HELLO_OK: u8 = 0x01;

/// Dial the relay and complete the mux handshake, yielding a client-side
/// multiplexer (odd stream ids). The obfuscation `codec` must match the relay's.
pub async fn connect_relay_mux(dial: &str, codec: Codec) -> io::Result<Arc<Multiplexer>> {
    let mut tcp = TcpStream::connect(dial).await?;
    tcp.set_nodelay(true).ok();
    if !mux_handshake_client(&mut tcp, &codec).await? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "relay rejected mux init",
        ));
    }
    Ok(Multiplexer::new_client(tcp, codec))
}

/// Open one relay stream to the agent: `Connect` on the connect pseudo-target,
/// send the relay token as the hello, and require the `OK` verdict before
/// handing the stream back for splicing. Any other reply (a close, a wrong byte,
/// a dead mux) is mapped to `ConnectionRefused`, so the caller shows "source
/// unavailable" (LLD-23 §2.5) and the higher layer retries.
pub async fn open_relay_stream(
    mux: &Arc<Multiplexer>,
    token: &RelayToken,
) -> io::Result<MuxStream> {
    let target = TargetAddr::Domain(RELAY_CONNECT_TARGET.to_string(), 0);
    let mut stream = mux_open_stream(mux, &target).await?;
    let hello = serde_json::to_vec(token)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    stream.send(&hello).await?;
    match stream.recv().await {
        Some(reply) if reply.first() == Some(&RELAY_HELLO_OK) => Ok(stream),
        _ => Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "relay: source unavailable",
        )),
    }
}

/// A relay the consumer can open streams against: dial address, obfuscation
/// codec and transit token. Keeps one mux alive and redials it on death, so a
/// dropped tunnel (relay restart, mux lifetime cap) is transparent to callers.
pub struct RelayEndpoint {
    dial: String,
    codec: Codec,
    token: RelayToken,
    mux: Mutex<Option<Arc<Multiplexer>>>,
}

impl RelayEndpoint {
    /// Build from the relay leg of a grant. Fails only if the obfuscation params
    /// are malformed (bad key / unknown modifier).
    pub fn from_grant(grant: &RelayGrant) -> Result<Self, String> {
        Ok(Self {
            dial: grant.dial(),
            codec: grant.obf.codec()?,
            token: grant.relay_token.clone(),
            mux: Mutex::new(None),
        })
    }

    /// A live mux to the relay, redialing if the previous one died.
    async fn live_mux(&self) -> io::Result<Arc<Multiplexer>> {
        let mut guard = self.mux.lock().await;
        let alive = guard.as_ref().map(|m| m.is_alive()).unwrap_or(false);
        if !alive {
            *guard = Some(connect_relay_mux(&self.dial, self.codec.clone()).await?);
        }
        Ok(guard.as_ref().expect("mux just set").clone())
    }

    /// Open one authorized relay stream to the agent.
    pub async fn stream(&self) -> io::Result<MuxStream> {
        let mux = self.live_mux().await?;
        open_relay_stream(&mux, &self.token).await
    }
}

/// A running loopback forwarder (LLD-23 §2.2): a `127.0.0.1:port` listener whose
/// every accepted connection is spliced to a fresh relay stream. The consumer's
/// HTTP client connects to [`local_addr`](Self::local_addr) (resolve-override)
/// and speaks pinned TLS end-to-end to the agent; the forwarder is byte-blind.
/// Dropping it stops the listener and its in-flight splices.
pub struct LoopbackForwarder {
    local_addr: SocketAddr,
    handle: tokio::task::JoinHandle<()>,
}

impl LoopbackForwarder {
    /// Bind the loopback listener and spawn the accept loop. Returns once bound,
    /// so `local_addr()` is immediately usable.
    pub async fn spawn(endpoint: Arc<RelayEndpoint>) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let local_addr = listener.local_addr()?;
        let handle = tokio::spawn(async move {
            loop {
                let (sock, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let ep = endpoint.clone();
                tokio::spawn(async move {
                    match ep.stream().await {
                        Ok(stream) => {
                            if let Err(e) = splice(sock, stream).await {
                                tracing::debug!("relay splice ended: {e}");
                            }
                        }
                        // Dropping `sock` closes the loopback connection, so the
                        // consumer's client sees the failure and retries.
                        Err(e) => tracing::debug!("relay stream open failed: {e}"),
                    }
                });
            }
        });
        Ok(Self { local_addr, handle })
    }

    /// The `127.0.0.1:port` the consumer's HTTP client should target.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for LoopbackForwarder {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Blindly move bytes both ways between the loopback TCP socket and the relay
/// stream until either end closes. The bytes are TLS ciphertext (E2E to the
/// agent); this function never inspects them.
async fn splice(mut tcp: TcpStream, relay: MuxStream) -> io::Result<()> {
    let mut relay_io = relay.into_io();
    tokio::io::copy_bidirectional(&mut tcp, &mut relay_io).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obfuscation::{ModifierStrategy, Obfuscator};
    use crate::protocol::Command;
    use crate::share::RelayObf;
    use base64::Engine as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn test_codec() -> Codec {
        let key = b"test-key-32-bytes-long-enough!!!".to_vec();
        let obf = Obfuscator::new(key, 0xDEADBEEF, ModifierStrategy::PositionalXorRotate);
        Codec::new(obf, 0, 0)
    }

    fn dummy_token() -> RelayToken {
        RelayToken {
            share_id: "s".into(),
            agent_pubkey: "QQ==".into(),
            exp: 9_999_999_999,
            signature: base64::engine::general_purpose::STANDARD.encode([0u8; 64]),
        }
    }

    /// A minimal in-process relay for one connection: mux handshake, then for
    /// every consumer stream verify it targets the connect pseudo-target, ack,
    /// read the hello, reply `OK`, and echo everything after (stands in for the
    /// blind splice to the agent). `reject` flips it to close instead of `OK`.
    async fn run_test_relay(tcp: TcpStream, codec: Codec, reject: bool) {
        let mut tcp = tcp;
        let mut buf = vec![0u8; 512];
        let mut filled = 0;
        let init = loop {
            let n = tcp.read(&mut buf[filled..]).await.unwrap();
            if n == 0 {
                return;
            }
            filled += n;
            if let Some((frame, _)) = codec.decode_frame(&buf[..filled]).unwrap() {
                break frame;
            }
        };
        if !mux_handshake_server(&mut tcp, &codec, &init).await.unwrap() {
            return;
        }
        let mux = Multiplexer::new_server(tcp, codec);
        let mut rx = mux.take_new_stream_rx().await.unwrap();
        while let Some(ns) = rx.recv().await {
            let mux = mux.clone();
            tokio::spawn(async move {
                let (target, _) = TargetAddr::decode(&ns.payload).unwrap();
                match target {
                    TargetAddr::Domain(d, _) => assert_eq!(d, RELAY_CONNECT_TARGET),
                    _ => panic!("consumer leg must target the connect pseudo-target"),
                }
                let mut stream = mux.register_stream(ns.stream_id).await;
                mux.send_frame(ns.stream_id, Command::ConnectAck, vec![0])
                    .await
                    .unwrap();
                // The hello (relay token bytes).
                let _hello = stream.recv().await;
                if reject {
                    let _ = stream.close().await;
                    return;
                }
                stream.send(&[RELAY_HELLO_OK]).await.unwrap();
                while let Some(data) = stream.recv().await {
                    if stream.send(&data).await.is_err() {
                        break;
                    }
                }
            });
        }
    }

    use crate::mux::mux_handshake_server;

    #[tokio::test]
    async fn test_open_relay_stream_hello_ok() {
        let (client_io, server_io) = tokio::io::duplex(65536);
        let codec = test_codec();
        let client_mux = Multiplexer::new_client(client_io, codec.clone());
        let server_mux = Multiplexer::new_server(server_io, codec.clone());

        // Server plays the relay: accept the connect stream, ack, read hello,
        // reply OK, then echo.
        let s = server_mux.clone();
        tokio::spawn(async move {
            let mut rx = s.take_new_stream_rx().await.unwrap();
            let ns = rx.recv().await.unwrap();
            let mut stream = s.register_stream(ns.stream_id).await;
            s.send_frame(ns.stream_id, Command::ConnectAck, vec![0]).await.unwrap();
            let _hello = stream.recv().await;
            stream.send(&[RELAY_HELLO_OK]).await.unwrap();
            let data = stream.recv().await.unwrap();
            stream.send(&data).await.unwrap();
        });

        let mut stream = open_relay_stream(&client_mux, &dummy_token()).await.unwrap();
        stream.send(b"secret ciphertext").await.unwrap();
        assert_eq!(stream.recv().await.unwrap(), b"secret ciphertext");
    }

    #[tokio::test]
    async fn test_open_relay_stream_rejected_maps_to_refused() {
        let (client_io, server_io) = tokio::io::duplex(65536);
        let codec = test_codec();
        let client_mux = Multiplexer::new_client(client_io, codec.clone());
        let server_mux = Multiplexer::new_server(server_io, codec.clone());

        // Relay closes the stream without OK (agent offline / token rejected).
        let s = server_mux.clone();
        tokio::spawn(async move {
            let mut rx = s.take_new_stream_rx().await.unwrap();
            let ns = rx.recv().await.unwrap();
            let mut stream = s.register_stream(ns.stream_id).await;
            s.send_frame(ns.stream_id, Command::ConnectAck, vec![0]).await.unwrap();
            let _hello = stream.recv().await;
            let _ = stream.close().await;
        });

        let err = open_relay_stream(&client_mux, &dummy_token()).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    /// Full consumer path over a real loopback TCP relay: `RelayEndpoint` dials
    /// the relay, `LoopbackForwarder` turns a local TCP connection into a relay
    /// stream, and bytes round-trip through the blind splice. Exercises
    /// `connect_relay_mux` + `open_relay_stream` + the forwarder + the io splice.
    #[tokio::test]
    async fn test_loopback_forwarder_round_trips() {
        let codec = test_codec();
        let relay_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let relay_addr = relay_listener.local_addr().unwrap();
        let relay_codec = codec.clone();
        tokio::spawn(async move {
            let (tcp, _) = relay_listener.accept().await.unwrap();
            run_test_relay(tcp, relay_codec, false).await;
        });

        let key_b64 = base64::engine::general_purpose::STANDARD
            .encode(b"test-key-32-bytes-long-enough!!!");
        let grant = RelayGrant {
            addr: relay_addr.ip().to_string(),
            port: relay_addr.port(),
            obf: RelayObf {
                key: key_b64,
                salt: 0xDEADBEEF,
                modifier: "positional_xor_rotate".into(),
                padding_min: 0,
                padding_max: 0,
            },
            relay_token: dummy_token(),
        };
        let endpoint = Arc::new(RelayEndpoint::from_grant(&grant).unwrap());
        let fwd = LoopbackForwarder::spawn(endpoint).await.unwrap();

        // A "consumer HTTP client" connecting to the loopback address sees the
        // agent through the splice: write bytes, read the echo.
        let mut client = TcpStream::connect(fwd.local_addr()).await.unwrap();
        client.write_all(b"tls-record-bytes").await.unwrap();
        let mut got = vec![0u8; 16];
        client.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"tls-record-bytes");
    }
}
