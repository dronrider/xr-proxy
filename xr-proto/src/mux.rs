//! Multiplexer: multiple logical streams over one TCP connection.
//!
//! Architecture:
//! ```text
//! Stream 1 ─┐                              ┌─ Stream 1 channel
//! Stream 2 ─┼─ writer_tx ─→ Writer Task ─→ TCP ─→ Reader Task ─→ dispatch by stream_id
//! Stream 3 ─┘                              └─ Stream 3 channel
//! ```
//!
//! Each MuxStream is an independent bidirectional channel that looks like
//! a TCP connection to the caller. The Multiplexer owns the real TCP
//! connection and routes frames by stream_id.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::time::Duration;

use crate::protocol::{
    decode_mux_payload, encode_mux_payload, Codec, Command, Frame, TargetAddr,
};

// ── Constants ───────────────────────────────────────────────────────

const STREAM_CHANNEL_SIZE: usize = 64;
const WRITER_CHANNEL_SIZE: usize = 256;
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
const MUX_PROTOCOL_VERSION: u8 = 1;

// ── Outgoing frame ──────────────────────────────────────────────────

/// A frame queued for writing to the TCP connection.
struct OutFrame {
    stream_id: u32,
    command: Command,
    payload: Vec<u8>,
}

// ── MuxStream ───────────────────────────────────────────────────────

/// A single logical stream within a multiplexed connection.
/// Drop sends Close automatically.
#[derive(Debug)]
pub struct MuxStream {
    stream_id: u32,
    rx: mpsc::Receiver<Vec<u8>>,
    writer_tx: mpsc::Sender<OutFrame>,
    alive: Arc<AtomicBool>,
    closed: bool,
}

impl MuxStream {
    /// Receive data from this stream. Returns None if the stream or
    /// mux connection is closed.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        self.rx.recv().await
    }

    /// Send data on this stream.
    pub async fn send(&self, data: &[u8]) -> io::Result<()> {
        if !self.alive.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "mux connection dead"));
        }
        self.writer_tx
            .send(OutFrame {
                stream_id: self.stream_id,
                command: Command::Data,
                payload: data.to_vec(),
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "mux writer closed"))
    }

    /// Close this stream gracefully.
    pub async fn close(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let _ = self.writer_tx
            .send(OutFrame {
                stream_id: self.stream_id,
                command: Command::Close,
                payload: Vec::new(),
            })
            .await;
        Ok(())
    }

    pub fn stream_id(&self) -> u32 {
        self.stream_id
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

impl Drop for MuxStream {
    fn drop(&mut self) {
        if !self.closed && self.alive.load(Ordering::Relaxed) {
            let tx = self.writer_tx.clone();
            let sid = self.stream_id;
            tokio::spawn(async move {
                let _ = tx
                    .send(OutFrame {
                        stream_id: sid,
                        command: Command::Close,
                        payload: Vec::new(),
                    })
                    .await;
            });
        }
    }
}

// ── Multiplexer ─────────────────────────────────────────────────────

/// Notification about a new incoming stream (Connect from remote).
#[derive(Debug)]
pub struct NewStream {
    pub stream_id: u32,
    pub payload: Vec<u8>,
}

/// Manages a multiplexed TCP connection with multiple logical streams.
pub struct Multiplexer {
    writer_tx: mpsc::Sender<OutFrame>,
    streams: Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>,
    next_stream_id: AtomicU32,
    alive: Arc<AtomicBool>,
    _close_notify: Arc<Notify>,
    /// Channel for incoming Connect frames for unregistered stream_ids.
    /// Server reads from this to create target connections.
    new_stream_rx: Mutex<Option<mpsc::Receiver<NewStream>>>,
}

impl Multiplexer {
    /// Create a client-side multiplexer over an established TCP connection.
    /// The TCP connection must already have completed MuxInit/MuxInitAck.
    pub fn new_client<S>(stream: S, codec: Codec) -> Arc<Self>
    where
        S: AsyncReadExt + AsyncWriteExt + Send + Unpin + 'static,
    {
        Self::new_inner(stream, codec, 1) // client uses odd stream IDs
    }

    /// Create a server-side multiplexer over an established TCP connection.
    pub fn new_server<S>(stream: S, codec: Codec) -> Arc<Self>
    where
        S: AsyncReadExt + AsyncWriteExt + Send + Unpin + 'static,
    {
        Self::new_inner(stream, codec, 2) // server uses even stream IDs
    }

    fn new_inner<S>(stream: S, codec: Codec, first_stream_id: u32) -> Arc<Self>
    where
        S: AsyncReadExt + AsyncWriteExt + Send + Unpin + 'static,
    {
        let (writer_tx, writer_rx) = mpsc::channel::<OutFrame>(WRITER_CHANNEL_SIZE);
        let streams: Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let close_notify = Arc::new(Notify::new());
        let (new_stream_tx, new_stream_rx) = mpsc::channel::<NewStream>(64);

        let (read_half, write_half) = tokio::io::split(stream);

        // Spawn reader task.
        {
            let streams = streams.clone();
            let alive = alive.clone();
            let close_notify = close_notify.clone();
            let codec = codec.clone();
            let writer_tx = writer_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = reader_task(read_half, codec, streams.clone(), writer_tx, new_stream_tx).await {
                    tracing::debug!("mux reader ended: {}", e);
                }
                alive.store(false, Ordering::Relaxed);
                // Close all stream channels.
                streams.lock().await.clear();
                close_notify.notify_waiters();
            });
        }

        // Spawn writer task.
        {
            let alive = alive.clone();
            let codec = codec.clone();
            tokio::spawn(async move {
                if let Err(e) = writer_task(write_half, codec, writer_rx).await {
                    tracing::debug!("mux writer ended: {}", e);
                }
                alive.store(false, Ordering::Relaxed);
            });
        }

        Arc::new(Self {
            writer_tx,
            streams,
            next_stream_id: AtomicU32::new(first_stream_id),
            alive,
            _close_notify: close_notify,
            new_stream_rx: Mutex::new(Some(new_stream_rx)),
        })
    }

    /// Register a stream that was opened by the remote side (server-side use).
    pub async fn register_stream(
        self: &Arc<Self>,
        stream_id: u32,
    ) -> MuxStream {
        let (data_tx, data_rx) = mpsc::channel(STREAM_CHANNEL_SIZE);
        self.streams.lock().await.insert(stream_id, data_tx);

        MuxStream {
            stream_id,
            rx: data_rx,
            writer_tx: self.writer_tx.clone(),
            alive: self.alive.clone(),
            closed: false,
        }
    }

    /// Send a raw frame (used for ConnectAck, Ping, Pong).
    pub async fn send_frame(&self, stream_id: u32, command: Command, payload: Vec<u8>) -> io::Result<()> {
        self.writer_tx
            .send(OutFrame { stream_id, command, payload })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "mux writer closed"))
    }

    /// Take the new-stream notification receiver (server-side use).
    /// Can only be called once — returns None on subsequent calls.
    pub async fn take_new_stream_rx(&self) -> Option<mpsc::Receiver<NewStream>> {
        self.new_stream_rx.lock().await.take()
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }
}

// ── Reader task ─────────────────────────────────────────────────────

async fn reader_task<R: AsyncReadExt + Unpin>(
    mut reader: R,
    codec: Codec,
    streams: Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>,
    writer_tx: mpsc::Sender<OutFrame>,
    new_stream_tx: mpsc::Sender<NewStream>,
) -> io::Result<()> {
    let mut buf = vec![0u8; 65536 + 256];
    let mut filled = 0;
    let mut last_activity = Instant::now();

    loop {
        // Read with keepalive timeout.
        let read_timeout = KEEPALIVE_INTERVAL + KEEPALIVE_TIMEOUT;
        let n = match tokio::time::timeout(read_timeout, reader.read(&mut buf[filled..])).await {
            Ok(result) => result?,
            Err(_) => {
                // No data for too long — connection dead.
                return Err(io::Error::new(io::ErrorKind::TimedOut, "mux keepalive timeout"));
            }
        };
        if n == 0 {
            return Ok(()); // clean close
        }
        filled += n;
        let _ = last_activity; // suppress unused warning, will be used for keepalive send
        last_activity = Instant::now();

        // Decode all complete frames.
        loop {
            match codec.decode_frame(&buf[..filled])? {
                Some((frame, consumed)) => {
                    dispatch_frame(&frame, &streams, &writer_tx, &new_stream_tx).await;
                    buf.copy_within(consumed..filled, 0);
                    filled -= consumed;
                }
                None => break,
            }
        }

        // Send keepalive if idle.
        if last_activity.elapsed() >= KEEPALIVE_INTERVAL {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let _ = writer_tx
                .send(OutFrame {
                    stream_id: 0,
                    command: Command::Ping,
                    payload: ts.to_be_bytes().to_vec(),
                })
                .await;
        }
    }
}

async fn dispatch_frame(
    frame: &Frame,
    streams: &Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>,
    writer_tx: &mpsc::Sender<OutFrame>,
    new_stream_tx: &mpsc::Sender<NewStream>,
) {
    match frame.command {
        Command::Ping => {
            let _ = writer_tx
                .send(OutFrame {
                    stream_id: 0,
                    command: Command::Pong,
                    payload: frame.payload.clone(),
                })
                .await;
        }
        Command::Pong => {}
        Command::Data | Command::ConnectAck => {
            if let Ok((stream_id, data)) = decode_mux_payload(&frame.payload) {
                let streams_guard = streams.lock().await;
                if let Some(tx) = streams_guard.get(&stream_id) {
                    let _ = tx.try_send(data.to_vec());
                }
            }
        }
        Command::Close => {
            if let Ok((stream_id, _)) = decode_mux_payload(&frame.payload) {
                // Remove stream and drop sender — receiver gets None.
                streams.lock().await.remove(&stream_id);
            }
        }
        Command::Connect => {
            // New stream from remote side (server receives these from clients).
            if let Ok((stream_id, data)) = decode_mux_payload(&frame.payload) {
                // If stream is already registered, deliver to it.
                let streams_guard = streams.lock().await;
                if let Some(tx) = streams_guard.get(&stream_id) {
                    let _ = tx.try_send(data.to_vec());
                } else {
                    // New unregistered stream — notify via new_stream channel.
                    drop(streams_guard);
                    let _ = new_stream_tx.try_send(NewStream {
                        stream_id,
                        payload: data.to_vec(),
                    });
                }
            }
        }
        _ => {}
    }
}

// ── Writer task ─────────────────────────────────────────────────────

async fn writer_task<W: AsyncWriteExt + Unpin>(
    mut writer: W,
    codec: Codec,
    mut rx: mpsc::Receiver<OutFrame>,
) -> io::Result<()> {
    while let Some(frame) = rx.recv().await {
        let payload = match frame.command {
            Command::Ping | Command::Pong => {
                // Control frames: no stream_id prefix.
                frame.payload
            }
            _ => {
                // Data/Connect/ConnectAck/Close: prefix with stream_id.
                encode_mux_payload(frame.stream_id, &frame.payload)
            }
        };

        let wire = codec.encode_frame(frame.command, &payload)?;
        writer.write_all(&wire).await?;
    }
    Ok(())
}

// ── Handshake helpers ───────────────────────────────────────────────

/// Client: send MuxInit, wait for MuxInitAck.
/// Returns Ok(true) if mux is supported, Ok(false) if server rejected,
/// Err on I/O error.
pub async fn mux_handshake_client<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
    codec: &Codec,
) -> io::Result<bool> {
    // Send MuxInit.
    let init_payload = vec![MUX_PROTOCOL_VERSION];
    let wire = codec.encode_frame(Command::MuxInit, &init_payload)?;
    stream.write_all(&wire).await?;

    // Wait for MuxInitAck.
    let mut buf = vec![0u8; 256];
    let mut filled = 0;

    loop {
        let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf[filled..]))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "mux init ack timeout"))??;

        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "server closed during mux init",
            ));
        }
        filled += n;

        match codec.decode_frame(&buf[..filled])? {
            Some((frame, _)) => {
                if frame.command != Command::MuxInitAck {
                    return Ok(false); // server doesn't support mux
                }
                if frame.payload.len() >= 2 && frame.payload[1] == 0 {
                    return Ok(true); // success
                }
                return Ok(false); // rejected
            }
            None => continue,
        }
    }
}

/// Server: check if frame is MuxInit, send MuxInitAck.
pub async fn mux_handshake_server<S: AsyncWriteExt + Unpin>(
    stream: &mut S,
    codec: &Codec,
    init_frame: &Frame,
) -> io::Result<bool> {
    if init_frame.command != Command::MuxInit {
        return Ok(false);
    }

    let version = init_frame.payload.first().copied().unwrap_or(0);
    if version != MUX_PROTOCOL_VERSION {
        // Unsupported version — reject.
        let ack = codec.encode_frame(Command::MuxInitAck, &[version, 1])?;
        stream.write_all(&ack).await?;
        return Ok(false);
    }

    // Accept.
    let ack = codec.encode_frame(Command::MuxInitAck, &[MUX_PROTOCOL_VERSION, 0])?;
    stream.write_all(&ack).await?;
    Ok(true)
}

// ── Client open_stream (standalone function) ────────────────────────

/// Open a stream on a client multiplexer: send Connect, wait for ConnectAck.
pub async fn mux_open_stream(
    mux: &Arc<Multiplexer>,
    target: &TargetAddr,
) -> io::Result<MuxStream> {
    if !mux.is_alive() {
        return Err(io::Error::new(io::ErrorKind::BrokenPipe, "mux connection dead"));
    }

    let stream_id = mux.next_stream_id.fetch_add(2, Ordering::Relaxed);
    let (data_tx, mut data_rx) = mpsc::channel(STREAM_CHANNEL_SIZE);

    // Register before sending Connect so we don't miss ConnectAck.
    mux.streams.lock().await.insert(stream_id, data_tx);

    // Send Connect(stream_id, target_addr).
    mux.send_frame(stream_id, Command::Connect, target.encode()).await
        .map_err(|e| {
            let streams = mux.streams.clone();
            let sid = stream_id;
            tokio::spawn(async move { streams.lock().await.remove(&sid); });
            e
        })?;

    // Wait for ConnectAck — delivered as first message on the channel.
    // The reader task dispatches ConnectAck payload (after stream_id prefix)
    // to this stream's channel.
    match tokio::time::timeout(Duration::from_secs(10), data_rx.recv()).await {
        Ok(Some(_ack_payload)) => {
            // ConnectAck received — stream is ready.
            Ok(MuxStream {
                stream_id,
                rx: data_rx,
                writer_tx: mux.writer_tx.clone(),
                alive: mux.alive.clone(),
                closed: false,
            })
        }
        Ok(None) => {
            // Channel closed — mux connection died.
            mux.streams.lock().await.remove(&stream_id);
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "mux connection died during open"))
        }
        Err(_) => {
            // Timeout.
            mux.streams.lock().await.remove(&stream_id);
            Err(io::Error::new(io::ErrorKind::TimedOut, "mux connect ack timeout"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obfuscation::{ModifierStrategy, Obfuscator};
    use tokio::io::duplex;

    fn test_codec() -> Codec {
        let key = b"test-key-32-bytes-long-enough!!!".to_vec();
        let obfs = Obfuscator::new(key, 0xDEADBEEF, ModifierStrategy::PositionalXorRotate);
        Codec::new(obfs, 0, 0) // no padding for simpler testing
    }

    #[tokio::test]
    async fn test_mux_handshake() {
        let (client_io, server_io) = duplex(65536);
        let codec = test_codec();

        let (mut client_half, mut server_half) = (client_io, server_io);

        let client_codec = codec.clone();
        let server_codec = codec.clone();

        let client_task = tokio::spawn(async move {
            mux_handshake_client(&mut client_half, &client_codec).await
        });

        let server_task = tokio::spawn(async move {
            // Server reads first frame.
            let mut buf = vec![0u8; 256];
            let mut filled = 0;
            loop {
                let n = server_half.read(&mut buf[filled..]).await.unwrap();
                filled += n;
                if let Some((frame, _)) = server_codec.decode_frame(&buf[..filled]).unwrap() {
                    let result = mux_handshake_server(&mut server_half, &server_codec, &frame).await;
                    return result;
                }
            }
        });

        let (client_result, server_result) = tokio::join!(client_task, server_task);
        assert!(client_result.unwrap().unwrap()); // client got MuxInitAck OK
        assert!(server_result.unwrap().unwrap()); // server accepted MuxInit
    }

    #[tokio::test]
    async fn test_mux_stream_data_roundtrip() {
        let (client_io, server_io) = duplex(65536);
        let codec = test_codec();

        // Create multiplexers (skip handshake for unit test).
        let client_mux = Multiplexer::new_client(client_io, codec.clone());
        let server_mux = Multiplexer::new_server(server_io, codec.clone());

        // Server: listen for incoming Connect on a background task.
        let server_mux_clone = server_mux.clone();
        let server_task = tokio::spawn(async move {
            // In a real server, the mux_handler would detect Connect frames
            // from the reader task. For this test, we simulate by registering
            // stream_id=1 (which the client will use).
            tokio::time::sleep(Duration::from_millis(50)).await;

            // Register stream 1 on server side.
            let mut stream = server_mux_clone.register_stream(1).await;

            // Send ConnectAck.
            server_mux_clone
                .send_frame(1, Command::ConnectAck, vec![0])
                .await
                .unwrap();

            // Receive data.
            let data = stream.recv().await.unwrap();
            assert_eq!(data, b"hello from client");

            // Send response.
            stream.send(b"hello from server").await.unwrap();

            // Close.
            stream.close().await.unwrap();
        });

        // Client: open stream.
        let target = TargetAddr::Domain("example.com".to_string(), 443);
        let mut client_stream = mux_open_stream(&client_mux, &target).await.unwrap();

        // Send data.
        client_stream.send(b"hello from client").await.unwrap();

        // Receive response.
        let response = client_stream.recv().await.unwrap();
        assert_eq!(response, b"hello from server");

        server_task.await.unwrap();
    }
}
