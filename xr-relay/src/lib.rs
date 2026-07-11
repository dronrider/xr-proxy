//! xr-relay: blind transit between a consumer and an agent behind NAT (LLD-23).
//!
//! The relay carries **ciphertext only**. An agent holds a persistent outgoing
//! mux to the relay and registers via challenge-response ([`RELAY_REGISTER_TARGET`]).
//! A consumer dials the same port, opens a stream on [`RELAY_CONNECT_TARGET`] and
//! sends a hub-minted [`RelayToken`] as the hello; the relay verifies it offline,
//! finds the agent in the registry, opens a reverse-stream to it
//! ([`RELAY_REVERSE_TARGET`]) and splices the two streams byte-for-byte. The TLS
//! inside the splice is end-to-end consumer<->agent, so the relay never sees
//! plaintext (LLD-23 §3.3).
//!
//! The pseudo-targets never resolve to the network: the relay matches on the
//! exact string and cannot be steered outward (SSRF-class excluded, §5.2).

pub mod config;
pub mod registry;

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use xr_proto::mux::{mux_handshake_server, mux_open_stream, Multiplexer};
use xr_proto::protocol::{
    Codec, Command, Frame, TargetAddr, CLOSE_REASON_AGENT_OFFLINE, CLOSE_REASON_RELAY_BUSY,
};
use xr_proto::relay_client::{
    RELAY_CONNECT_TARGET, RELAY_HELLO_OK, RELAY_REGISTER_TARGET, RELAY_REVERSE_TARGET,
};
use xr_proto::share::{verify_relay_register, verify_relay_token, RelayRegister, RelayToken};

pub use registry::{AgentRegistry, Counters, IpCaps};

/// Length of the registration challenge nonce (LLD-23 §2.1). 32 random bytes:
/// unpredictable and single-use, so the answer can't be replayed without a clock.
const REGISTER_NONCE_LEN: usize = 32;
/// How long the agent has to answer the registration challenge.
const CHALLENGE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a consumer has to send its hello after the stream opens.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Timeout on reading the first frame (MuxInit) of a fresh connection.
const HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Shared relay state: the pinned hub key plus the registry, counters and limits.
pub struct RelayState {
    pub hub_key: ed25519_dalek::VerifyingKey,
    pub registry: AgentRegistry,
    pub counters: Counters,
    pub ip_caps: Arc<IpCaps>,
    pub stream_sem: Arc<Semaphore>,
    pub splice_lifetime: Duration,
}

impl RelayState {
    pub fn new(
        hub_key: ed25519_dalek::VerifyingKey,
        max_streams: usize,
        max_reg_per_ip: usize,
        splice_lifetime: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            hub_key,
            registry: AgentRegistry::new(),
            counters: Counters::new(),
            ip_caps: IpCaps::new(max_reg_per_ip),
            stream_sem: Arc::new(Semaphore::new(max_streams)),
            splice_lifetime,
        })
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Accept loop: one spawned handler per connection, bounded by a connection
/// semaphore (backpressure, not reject). Runs until the listener errors.
pub async fn serve(
    listener: TcpListener,
    codec: Codec,
    state: Arc<RelayState>,
    max_connections: usize,
) {
    let conn_sem = Arc::new(Semaphore::new(max_connections));
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                // A persistent accept error (EMFILE and friends) would spin this
                // loop hot and flood the log; a short breather lets descriptors
                // free up while the agent tunnels stay alive.
                tracing::warn!("relay accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let permit = conn_sem.clone().acquire_owned().await.expect("sem open");
        tcp.set_nodelay(true).ok();
        let codec = codec.clone();
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(tcp, peer, codec, state).await {
                tracing::debug!("relay conn {peer} ended: {e}");
            }
            drop(permit);
        });
    }
}

/// Periodically log the per-share byte totals (LLD-23 §2.6).
pub fn spawn_counter_logger(state: Arc<RelayState>, interval: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.tick().await; // skip the immediate first tick
        loop {
            tick.tick().await;
            for (share, bytes) in state.counters.snapshot() {
                tracing::info!(share = %share, bytes, "relay transit");
            }
        }
    });
}

/// Read framed bytes until the first complete frame decodes (the MuxInit).
async fn read_first_frame(tcp: &mut TcpStream, codec: &Codec) -> io::Result<Frame> {
    let mut buf = vec![0u8; 512];
    let mut filled = 0;
    loop {
        let n = tokio::time::timeout(HANDSHAKE_READ_TIMEOUT, tcp.read(&mut buf[filled..]))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "mux init read timeout"))??;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "closed before mux init",
            ));
        }
        filled += n;
        if let Some((frame, _)) = codec.decode_frame(&buf[..filled])? {
            return Ok(frame);
        }
        if filled == buf.len() {
            buf.resize(buf.len() * 2, 0);
        }
    }
}

/// Handle one connection: mux handshake, then dispatch each incoming stream by
/// its pseudo-target. A register stream turns the connection into an agent
/// uplink (kept alive until it dies); connect streams are handled per-stream.
pub async fn handle_connection(
    mut tcp: TcpStream,
    peer: SocketAddr,
    codec: Codec,
    state: Arc<RelayState>,
) -> io::Result<()> {
    let init = read_first_frame(&mut tcp, &codec).await?;
    if !mux_handshake_server(&mut tcp, &codec, &init).await? {
        tracing::debug!("{peer} mux handshake rejected");
        return Ok(());
    }
    let mux = Multiplexer::new_server(tcp, codec);
    let mut new_stream_rx = mux
        .take_new_stream_rx()
        .await
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "new_stream_rx taken"))?;

    while let Some(ns) = new_stream_rx.recv().await {
        let stream_id = ns.stream_id;
        let target = match TargetAddr::decode(&ns.payload) {
            Ok((TargetAddr::Domain(d, _), _)) => d,
            _ => {
                let _ = mux.send_frame(stream_id, Command::Close, Vec::new()).await;
                continue;
            }
        };
        match target.as_str() {
            RELAY_REGISTER_TARGET => {
                // Agent uplink: this connection's whole purpose is the register
                // stream. Handle it inline and stop accepting more streams; the
                // mux keeps serving reverse-streams opened elsewhere.
                handle_register(stream_id, mux.clone(), peer, &state).await?;
                break;
            }
            RELAY_CONNECT_TARGET => {
                let mux = mux.clone();
                let state = state.clone();
                tokio::spawn(async move {
                    handle_connect(stream_id, mux, state).await;
                });
            }
            _ => {
                let _ = mux.send_frame(stream_id, Command::Close, Vec::new()).await;
            }
        }
    }
    // Uplink ended (register stream closed / mux died): make sure the socket
    // tears down so a half-dead reverse tunnel doesn't linger.
    mux.shutdown();
    Ok(())
}

/// Registration challenge-response (LLD-23 §2.1). Registers the stream first so
/// the agent's answer can't race the ack, sends the ack + nonce, verifies the
/// answer, admits the mux to the registry, then holds the stream open as the
/// liveness signal until it closes.
async fn handle_register(
    stream_id: u32,
    mux: Arc<Multiplexer>,
    peer: SocketAddr,
    state: &Arc<RelayState>,
) -> io::Result<()> {
    let mut stream = mux.register_stream(stream_id).await;
    mux.send_frame(stream_id, Command::ConnectAck, vec![0]).await?;

    let mut nonce = [0u8; REGISTER_NONCE_LEN];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut nonce);
    stream.send(&nonce).await?;

    let answer = match tokio::time::timeout(CHALLENGE_TIMEOUT, stream.recv()).await {
        Ok(Some(bytes)) => bytes,
        _ => {
            tracing::debug!("{peer} register: no answer");
            return Ok(());
        }
    };
    let reg: RelayRegister = match serde_json::from_slice(&answer) {
        Ok(r) => r,
        Err(_) => {
            tracing::debug!("{peer} register: malformed answer");
            return Ok(());
        }
    };
    let pubkey = match verify_relay_register(&reg, &state.hub_key, &nonce, now_unix()) {
        Ok(pk) => pk,
        Err(e) => {
            tracing::debug!("{peer} register rejected: {e}");
            return Ok(());
        }
    };

    let _cap = match state.ip_caps.try_acquire(peer.ip()) {
        Some(g) => g,
        None => {
            tracing::warn!("{peer} register rejected: per-IP cap reached");
            return Ok(());
        }
    };

    let generation = state.registry.register(pubkey.clone(), mux.clone()).await;
    let _ = stream.send(&[RELAY_HELLO_OK]).await;
    tracing::info!("{peer} agent registered");

    // Liveness: the register stream stays open until the agent leaves or the mux
    // dies. Any stray data is drained.
    while stream.recv().await.is_some() {}

    state.registry.deregister(&pubkey, generation).await;
    mux.shutdown();
    tracing::info!("{peer} agent deregistered");
    Ok(())
}

/// Passthrough io that counts the bytes read off the inner end into a shared
/// total. Wrapping both splice ends counts both transit directions as the bytes
/// flow, so the per-share total survives any splice outcome (peer reset, the
/// lifetime cap), unlike `copy_bidirectional`'s return value, which is lost on
/// error and timeout.
struct CountedIo<T> {
    inner: T,
    moved: Arc<AtomicU64>,
}

impl<T: AsyncRead + Unpin> AsyncRead for CountedIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let poll = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll {
            this.moved.fetch_add((buf.filled().len() - before) as u64, Ordering::Relaxed);
        }
        poll
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for CountedIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Handle one consumer transit stream (LLD-23 §2.2): read the hello token, verify
/// it offline, find the agent, open a reverse-stream and splice. Failures answer
/// with `Close` (agent offline gets [`CLOSE_REASON_AGENT_OFFLINE`], an exhausted
/// stream cap [`CLOSE_REASON_RELAY_BUSY`]).
async fn handle_connect(stream_id: u32, mux: Arc<Multiplexer>, state: Arc<RelayState>) {
    let permit = match state.stream_sem.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            tracing::warn!("relay stream cap reached, refusing");
            let _ = mux
                .send_frame(stream_id, Command::Close, vec![CLOSE_REASON_RELAY_BUSY])
                .await;
            return;
        }
    };

    let mut consumer = mux.register_stream(stream_id).await;
    if mux.send_frame(stream_id, Command::ConnectAck, vec![0]).await.is_err() {
        return;
    }

    let hello = match tokio::time::timeout(HELLO_TIMEOUT, consumer.recv()).await {
        Ok(Some(bytes)) => bytes,
        _ => return,
    };
    let token: RelayToken = match serde_json::from_slice(&hello) {
        Ok(t) => t,
        Err(_) => {
            let _ = mux.send_frame(stream_id, Command::Close, Vec::new()).await;
            return;
        }
    };
    // The token is self-describing; the signature binds share_id+agent_pubkey, so
    // verifying against its own fields checks the hub signature and expiry.
    if let Err(e) = verify_relay_token(
        &token,
        &state.hub_key,
        &token.share_id,
        &token.agent_pubkey,
        now_unix(),
    ) {
        tracing::debug!("relay token rejected: {e}");
        let _ = mux.send_frame(stream_id, Command::Close, Vec::new()).await;
        return;
    }

    let agent_mux = match state.registry.get(&token.agent_pubkey).await {
        Some(m) if m.is_alive() => m,
        _ => {
            let _ = mux
                .send_frame(stream_id, Command::Close, vec![CLOSE_REASON_AGENT_OFFLINE])
                .await;
            return;
        }
    };

    let reverse = match mux_open_stream(
        &agent_mux,
        &TargetAddr::Domain(RELAY_REVERSE_TARGET.to_string(), 0),
    )
    .await
    {
        Ok(s) => s,
        Err(_) => {
            let _ = mux
                .send_frame(stream_id, Command::Close, vec![CLOSE_REASON_AGENT_OFFLINE])
                .await;
            return;
        }
    };

    // Grant transit, then splice ciphertext both ways until either end closes.
    if consumer.send(&[RELAY_HELLO_OK]).await.is_err() {
        return;
    }
    let moved = Arc::new(AtomicU64::new(0));
    let mut c_io = CountedIo { inner: consumer.into_io(), moved: moved.clone() };
    let mut a_io = CountedIo { inner: reverse.into_io(), moved: moved.clone() };
    let _ = tokio::time::timeout(
        state.splice_lifetime,
        tokio::io::copy_bidirectional(&mut c_io, &mut a_io),
    )
    .await;
    state.counters.add(&token.share_id, moved.load(Ordering::Relaxed));
    drop(permit);
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ed25519_dalek::SigningKey;
    use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};
    use xr_proto::relay_client::{connect_relay_mux, open_relay_stream};
    use xr_proto::share::{sign_agent_credential, sign_relay_register, sign_relay_token};

    fn test_codec() -> Codec {
        let key = b"test-key-32-bytes-long-enough!!!".to_vec();
        Codec::new(
            Obfuscator::new(key, 0xDEADBEEF, ModifierStrategy::PositionalXorRotate),
            0,
            0,
        )
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// Start a relay on an ephemeral port; returns its address and shared state.
    async fn start_relay(hub: &SigningKey) -> (SocketAddr, Arc<RelayState>) {
        start_relay_tuned(hub, 64, Duration::from_secs(30)).await
    }

    /// Same, with the stream cap and splice lifetime under test control.
    async fn start_relay_tuned(
        hub: &SigningKey,
        max_streams: usize,
        splice_lifetime: Duration,
    ) -> (SocketAddr, Arc<RelayState>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = RelayState::new(hub.verifying_key(), max_streams, 8, splice_lifetime);
        let s = state.clone();
        let codec = test_codec();
        tokio::spawn(async move { serve(listener, codec, s, 64).await });
        (addr, state)
    }

    /// Run an agent: register over the real challenge-response, then serve every
    /// reverse-stream by echoing bytes (stands in for the HTTP router + E2E TLS).
    /// Returns once registered so the consumer can proceed.
    async fn spawn_agent(
        relay_addr: SocketAddr,
        hub: &SigningKey,
        identity: SigningKey,
    ) -> tokio::task::JoinHandle<()> {
        let agent_pk = b64(identity.verifying_key().as_bytes());
        let cred = sign_agent_credential(hub, &agent_pk, now_unix() + 3600);
        let codec = test_codec();
        let mux = connect_relay_mux(&relay_addr.to_string(), codec).await.unwrap();

        // Register: open the register stream, answer the nonce challenge.
        let mut reg_stream = mux_open_stream(
            &mux,
            &TargetAddr::Domain(RELAY_REGISTER_TARGET.to_string(), 0),
        )
        .await
        .unwrap();
        let nonce = reg_stream.recv().await.unwrap();
        let answer = sign_relay_register(&identity, &cred, &nonce);
        reg_stream.send(&serde_json::to_vec(&answer).unwrap()).await.unwrap();
        let ok = reg_stream.recv().await.unwrap();
        assert_eq!(ok, vec![RELAY_HELLO_OK], "relay must grant registration");

        // Serve reverse-streams: echo. Keep reg_stream alive by moving it in.
        tokio::spawn(async move {
            let _keepalive = reg_stream;
            let mut rx = mux.take_new_stream_rx().await.unwrap();
            while let Some(ns) = rx.recv().await {
                let (target, _) = TargetAddr::decode(&ns.payload).unwrap();
                match target {
                    TargetAddr::Domain(d, _) => assert_eq!(d, RELAY_REVERSE_TARGET),
                    _ => panic!("reverse stream must target the reverse pseudo-target"),
                }
                let mux = mux.clone();
                tokio::spawn(async move {
                    let mut stream = mux.register_stream(ns.stream_id).await;
                    mux.send_frame(ns.stream_id, Command::ConnectAck, vec![0]).await.unwrap();
                    while let Some(data) = stream.recv().await {
                        if stream.send(&data).await.is_err() {
                            break;
                        }
                    }
                });
            }
        })
    }

    /// Full path: agent registers, consumer presents a valid token, bytes
    /// round-trip through the blind splice, and the relay counts them per share.
    #[tokio::test]
    async fn test_relay_end_to_end() {
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let identity = SigningKey::from_bytes(&[7u8; 32]);
        let agent_pk = b64(identity.verifying_key().as_bytes());
        let (relay_addr, state) = start_relay(&hub).await;
        let _agent = spawn_agent(relay_addr, &hub, identity).await;

        // Consumer: valid relay token for (share, agent).
        let token = sign_relay_token(&hub, "share-1", &agent_pk, now_unix() + 3600);
        let mux = connect_relay_mux(&relay_addr.to_string(), test_codec()).await.unwrap();
        let mut stream = open_relay_stream(&mux, &token).await.unwrap();

        stream.send(b"tls-ciphertext-1").await.unwrap();
        assert_eq!(stream.recv().await.unwrap(), b"tls-ciphertext-1");
        stream.send(b"more").await.unwrap();
        assert_eq!(stream.recv().await.unwrap(), b"more");

        // Bytes were accounted to the share (both directions). Give the splice a
        // moment to close and record after we drop the stream.
        drop(stream);
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if state.counters.get("share-1") > 0 {
                break;
            }
        }
        assert!(state.counters.get("share-1") > 0, "relay must count transited bytes per share");
    }

    /// A token for an agent that isn't registered is refused with agent-offline
    /// semantics (LLD-23 §2.5): the consumer's open maps it to ConnectionRefused.
    #[tokio::test]
    async fn test_relay_agent_offline() {
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let (relay_addr, _state) = start_relay(&hub).await;

        let token = sign_relay_token(&hub, "share-x", "unknown-agent", now_unix() + 3600);
        let mux = connect_relay_mux(&relay_addr.to_string(), test_codec()).await.unwrap();
        let err = open_relay_stream(&mux, &token).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    /// A token the hub never signed (or for the wrong agent) is rejected before
    /// any reverse-stream is opened.
    #[tokio::test]
    async fn test_relay_rejects_forged_token() {
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let identity = SigningKey::from_bytes(&[7u8; 32]);
        let agent_pk = b64(identity.verifying_key().as_bytes());
        let (relay_addr, _state) = start_relay(&hub).await;
        let _agent = spawn_agent(relay_addr, &hub, identity).await;

        // Signed by a different key: the relay's hub-key verify fails.
        let forger = SigningKey::from_bytes(&[99u8; 32]);
        let token = sign_relay_token(&forger, "share-1", &agent_pk, now_unix() + 3600);
        let mux = connect_relay_mux(&relay_addr.to_string(), test_codec()).await.unwrap();
        let err = open_relay_stream(&mux, &token).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    /// The splice is byte-blind: arbitrary bytes (not HTTP/TLS) round-trip
    /// unchanged in both directions, proving the relay never parses content
    /// (LLD-23 §3.3, `test_relay_splice_opaque`).
    #[tokio::test]
    async fn test_relay_splice_opaque() {
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let identity = SigningKey::from_bytes(&[7u8; 32]);
        let agent_pk = b64(identity.verifying_key().as_bytes());
        let (relay_addr, _state) = start_relay(&hub).await;
        let _agent = spawn_agent(relay_addr, &hub, identity).await;

        let token = sign_relay_token(&hub, "share-1", &agent_pk, now_unix() + 3600);
        let mux = connect_relay_mux(&relay_addr.to_string(), test_codec()).await.unwrap();
        let mut stream = open_relay_stream(&mux, &token).await.unwrap();

        // Random-looking binary, including bytes that would be meaningful if the
        // relay tried to parse a protocol.
        let blob: Vec<u8> = (0u16..1024).map(|i| (i.wrapping_mul(37) & 0xFF) as u8).collect();
        stream.send(&blob).await.unwrap();
        let mut got = Vec::new();
        while got.len() < blob.len() {
            got.extend_from_slice(&stream.recv().await.unwrap());
        }
        assert_eq!(got, blob, "opaque bytes must pass through the splice unchanged");
    }

    /// Regression: bytes moved before the splice is cut must reach the counter.
    /// Only a clean `copy_bidirectional` exit used to be counted, so the lifetime
    /// cap (and any io error) discarded everything already transited.
    #[tokio::test]
    async fn test_counters_survive_splice_lifetime_cut() {
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let identity = SigningKey::from_bytes(&[7u8; 32]);
        let agent_pk = b64(identity.verifying_key().as_bytes());
        // A tiny lifetime: the cap cuts the splice, not either side closing.
        let (relay_addr, state) = start_relay_tuned(&hub, 64, Duration::from_millis(200)).await;
        let _agent = spawn_agent(relay_addr, &hub, identity).await;

        let token = sign_relay_token(&hub, "share-cut", &agent_pk, now_unix() + 3600);
        let mux = connect_relay_mux(&relay_addr.to_string(), test_codec()).await.unwrap();
        let mut stream = open_relay_stream(&mux, &token).await.unwrap();

        stream.send(b"bytes before the cut").await.unwrap();
        assert_eq!(stream.recv().await.unwrap(), b"bytes before the cut");

        // Keep the stream open until the cap fires and the total shows up.
        for _ in 0..100 {
            if state.counters.get("share-cut") > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            state.counters.get("share-cut") > 0,
            "bytes transited before the cut must be counted"
        );
    }

    /// An exhausted stream cap refuses the consumer (Close with
    /// [`CLOSE_REASON_RELAY_BUSY`]) instead of hanging or dropping the connection.
    #[tokio::test]
    async fn test_relay_stream_cap_refuses() {
        let hub = SigningKey::from_bytes(&[42u8; 32]);
        let identity = SigningKey::from_bytes(&[7u8; 32]);
        let agent_pk = b64(identity.verifying_key().as_bytes());
        let (relay_addr, _state) = start_relay_tuned(&hub, 0, Duration::from_secs(30)).await;
        let _agent = spawn_agent(relay_addr, &hub, identity).await;

        let token = sign_relay_token(&hub, "share-1", &agent_pk, now_unix() + 3600);
        let mux = connect_relay_mux(&relay_addr.to_string(), test_codec()).await.unwrap();
        // The refusal lands before ConnectAck, so it surfaces as a failed open
        // (not the post-hello ConnectionRefused mapping).
        assert!(open_relay_stream(&mux, &token).await.is_err());
    }
}
