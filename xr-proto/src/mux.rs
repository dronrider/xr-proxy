//! Multiplexer: multiple logical streams over one TCP connection.
//!
//! Architecture (два плана записи, контрольный приоритетнее):
//! ```text
//!   ctrl_tx (Connect/ConnectAck/Ping/Pong) --\  biased
//! Stream 1 --\                                |
//! Stream 2 ---+-- writer_tx (Data + Close) ---+--> Writer Task --> TCP --> Reader Task --> dispatch
//! Stream 3 --/
//! ```
//!
//! Each MuxStream is an independent bidirectional channel that looks like
//! a TCP connection to the caller. The Multiplexer owns the real TCP
//! connection and routes frames by stream_id.
//!
//! Два плана записи: контрольный (`ctrl_tx`) и балк-данные (`writer_tx`). Writer
//! сливает их одним biased-select'ом с приоритетом контрольного, поэтому
//! ConnectAck нового стрима не залипает за мегабайтами Data чужих загрузок
//! (head-of-line, корень XR-086).

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::time::Duration;

use crate::protocol::{
    decode_mux_payload, encode_mux_payload, Codec, Command, Frame, TargetAddr,
};

// ── Constants ───────────────────────────────────────────────────────

// Per-stream channel: holds frames pending consumption by the LAN/target
// reader. CDN bursts (Cloudflare/googlevideo) can deliver tens of frames in
// a single millisecond, so this must be large enough to absorb a burst while
// the consumer drains it. 256 was too small — the consumer side of `relay_*`
// used to share one tokio::select! with the slow LAN write, so a single TLS
// handshake burst overflowed the channel and killed the stream.
const STREAM_CHANNEL_SIZE: usize = 1024;
// Shared writer channel: every stream's send() funnels through this single
// queue. Under torrent-like load (many parallel streams) the previous 512
// became a contention bottleneck.
const WRITER_CHANNEL_SIZE: usize = 2048;
// Очередь новых входящих стримов (server-side): reader кладёт сюда каждый Connect,
// mux_handler разбирает. Было 64: под всплеском Connect'ов (флуд/много устройств)
// канал переполнялся, и reader МОЛЧА ронял Connect через try_send -> клиент не
// получал ConnectAck и ловил "open timed out" (кандидат в корень XR-086). Подняли
// с запасом; дроп теперь логируется (см. dispatch_frame).
const NEW_STREAM_CHANNEL_SIZE: usize = 1024;
// КОНТРОЛЬНЫЙ план mux: Connect/ConnectAck/Ping/Pong идут ОТДЕЛЬНЫМ каналом от
// балка Data и пишутся writer'ом с приоритетом. Раньше всё шло одним FIFO
// writer_tx, и ConnectAck нового стрима вставал в хвост за мегабайтами Data
// существующих загрузок; на медленном линке он уходил в провод дольше
// PER_SERVER_OPEN_TIMEOUT=5с -> клиент ловил "open timed out" (корень XR-086,
// head-of-line блокировка контрольных кадров). Отдельный приоритетный план это
// снимает. Close СОЗНАТЕЛЬНО остаётся в балк-плане рядом с Data того же стрима
// (иначе обгонит недописанные Data и обрежет выгрузку). Канал маленький:
// контрольные кадры редки и крошечны.
const CTRL_CHANNEL_SIZE: usize = 1024;
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
/// Force mux reconnection every 4 hours to prevent TCP degradation.
const MUX_MAX_LIFETIME: Duration = Duration::from_secs(4 * 3600);
/// Мёртвый линк: если по соединению не пришло НИЧЕГО (даже Pong на наши Ping)
/// дольше этого срока, считаем mux сдохшим и рвём его (XR-083). Здоровый линк
/// получает Pong на каждый keepalive-Ping (сервер отвечает симметрично), поэтому
/// на нём входящие идут не реже KEEPALIVE_INTERVAL. Порог с запасом на один
/// потерянный Pong: без детекта blackhole-mux (egress тихо дропается, TCP без
/// RST, read висит без EOF) числился бы «живым» до MUX_MAX_LIFETIME=4ч, отравляя
/// слот пула до рестарта процесса.
const DEAD_LINK_TIMEOUT: Duration = Duration::from_secs(75);
/// Верхняя граница каждого шага открытия стрима (взятие локов, отправка Connect).
/// Меньше `PER_SERVER_OPEN_TIMEOUT`=5с, чтобы конкретный залипший шаг залогировался
/// поимённо и превратился в failover, а не в немой таймаут пула (XR-086).
const OPEN_STEP_TIMEOUT: Duration = Duration::from_secs(4);
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
    rx: Option<mpsc::Receiver<Vec<u8>>>,
    /// Единый FIFO этого стрима: и Data, и Close. Close НЕ уводим в контрольный
    /// план, иначе он обгонит ещё не записанные Data того же стрима и обрежет
    /// выгрузку (пир закроет стрим раньше, чем дочитает данные).
    writer_tx: mpsc::Sender<OutFrame>,
    alive: Arc<AtomicBool>,
    closed: bool,
    /// Set by `split()` so Drop on the husk skips Close — the WriteHalf now
    /// owns that contract.
    detached: bool,
}

impl MuxStream {
    /// Receive data from this stream. Returns None if the stream or
    /// mux connection is closed.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        match self.rx.as_mut() {
            Some(rx) => rx.recv().await,
            None => None,
        }
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

    /// Split into independent read and write halves, so a download (recv→LAN)
    /// loop and an upload (LAN→send) loop can run as separate tasks. Without
    /// this, both directions live in one `tokio::select!`, and a slow LAN
    /// writer stalls the recv side — overflowing the per-stream channel and
    /// triggering "channel full, closing".
    pub fn split(mut self) -> (MuxReadHalf, MuxWriteHalf) {
        let rx = self.rx.take().expect("MuxStream already split");
        self.detached = true;
        let write = MuxWriteHalf {
            stream_id: self.stream_id,
            writer_tx: self.writer_tx.clone(),
            alive: self.alive.clone(),
            closed: self.closed,
        };
        // self drops here; Drop honors `detached` and skips Close.
        (MuxReadHalf { rx }, write)
    }
}

impl Drop for MuxStream {
    fn drop(&mut self) {
        if self.detached {
            return;
        }
        if !self.closed && self.alive.load(Ordering::Relaxed) {
            close_on_drop(&self.writer_tx, self.stream_id);
        }
    }
}

/// Best-effort Close при дропе стрима, БЕЗ `tokio::spawn`. Раньше здесь
/// спавнился таск, ждущий `send(Close).await`; под churn'ом соединений и медленным
/// writer'ом (деградирующий линк к серверу) канал переполнялся, и эти ждущие таски
/// копились неограниченно, утягивая память (XR-079). `try_send` не блокирует и не
/// спавнит: если очередь полна, Close теряется, а сервер закрывает стрим сам по
/// своему idle-таймауту. Идёт по балк-плану (`writer_tx`) вместе с Data, чтобы не
/// обгонять ещё не записанные Data того же стрима.
fn close_on_drop(writer_tx: &mpsc::Sender<OutFrame>, stream_id: u32) {
    let _ = writer_tx.try_send(OutFrame {
        stream_id,
        command: Command::Close,
        payload: Vec::new(),
    });
}

// ── MuxStream split halves ──────────────────────────────────────────

/// Read half of a split MuxStream. Owns the per-stream receive channel.
#[derive(Debug)]
pub struct MuxReadHalf {
    rx: mpsc::Receiver<Vec<u8>>,
}

impl MuxReadHalf {
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        self.rx.recv().await
    }
}

/// Write half of a split MuxStream. Owns the Close contract: dropping it
/// without prior `close()` enqueues a Close frame, mirroring the original
/// MuxStream Drop behavior.
#[derive(Debug)]
pub struct MuxWriteHalf {
    stream_id: u32,
    /// Единый FIFO стрима (Data + Close), см. MuxStream::writer_tx.
    writer_tx: mpsc::Sender<OutFrame>,
    alive: Arc<AtomicBool>,
    closed: bool,
}

impl MuxWriteHalf {
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

    pub async fn close(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let _ = self
            .writer_tx
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
}

impl Drop for MuxWriteHalf {
    fn drop(&mut self) {
        if !self.closed && self.alive.load(Ordering::Relaxed) {
            close_on_drop(&self.writer_tx, self.stream_id);
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
    /// Балк-план: Command::Data и Close (Close ордерится за Data того же стрима).
    writer_tx: mpsc::Sender<OutFrame>,
    /// Контрольный план (приоритет в writer'е): Connect/ConnectAck/Ping/Pong.
    ctrl_tx: mpsc::Sender<OutFrame>,
    streams: Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>,
    next_stream_id: AtomicU32,
    alive: Arc<AtomicBool>,
    _close_notify: Arc<Notify>,
    /// Channel for incoming Connect frames for unregistered stream_ids.
    /// Server reads from this to create target connections.
    new_stream_rx: Mutex<Option<mpsc::Receiver<NewStream>>>,
    /// Externally-triggered shutdown signal. When the pool decides a slot
    /// is zombie (alive=true but server-state lost), calling shutdown()
    /// drops the write half, which propagates FIN → server closes → our
    /// reader gets EOF → TCP fully closes. Without this the orphaned
    /// reader/writer tasks keep the socket ESTABLISHED for up to
    /// MUX_MAX_LIFETIME (4h), accumulating ghost connections on the server.
    shutdown_notify: Arc<Notify>,
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
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<OutFrame>(CTRL_CHANNEL_SIZE);
        let streams: Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let close_notify = Arc::new(Notify::new());
        let shutdown_notify = Arc::new(Notify::new());
        let (new_stream_tx, new_stream_rx) = mpsc::channel::<NewStream>(NEW_STREAM_CHANNEL_SIZE);

        let (read_half, write_half) = tokio::io::split(stream);

        // Spawn reader task. Reader шлёт только контрольные кадры (Ping/Pong),
        // поэтому получает КОНТРОЛЬНЫЙ канал.
        {
            let streams = streams.clone();
            let alive = alive.clone();
            let close_notify = close_notify.clone();
            let codec = codec.clone();
            let reader_ctrl = ctrl_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = reader_task(read_half, codec, streams.clone(), reader_ctrl, new_stream_tx).await {
                    tracing::debug!("mux reader ended: {}", e);
                }
                alive.store(false, Ordering::Relaxed);
                // Close all stream channels.
                streams.lock().await.clear();
                close_notify.notify_waiters();
            });
        }

        // Spawn writer task. select against shutdown_notify so an external
        // shutdown() call drops write_half promptly, propagating FIN to
        // the peer and closing the TCP cleanly.
        {
            let alive = alive.clone();
            let codec = codec.clone();
            let shutdown_notify = shutdown_notify.clone();
            tokio::spawn(async move {
                tokio::select! {
                    res = writer_task(write_half, codec, ctrl_rx, writer_rx) => {
                        if let Err(e) = res {
                            tracing::debug!("mux writer ended: {}", e);
                        }
                    }
                    _ = shutdown_notify.notified() => {
                        tracing::debug!("mux writer shutdown by request");
                        // write_half drops here → FIN sent → reader on
                        // other side gets EOF → its reader_task exits.
                    }
                }
                alive.store(false, Ordering::Relaxed);
            });
        }

        Arc::new(Self {
            writer_tx,
            ctrl_tx,
            streams,
            next_stream_id: AtomicU32::new(first_stream_id),
            alive,
            _close_notify: close_notify,
            new_stream_rx: Mutex::new(Some(new_stream_rx)),
            shutdown_notify,
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
            rx: Some(data_rx),
            writer_tx: self.writer_tx.clone(),
            alive: self.alive.clone(),
            closed: false,
            detached: false,
        }
    }

    /// Send a raw frame (used for ConnectAck, Ping, Pong).
    ///
    /// Все кадры send_frame это КОНТРОЛЬ (Connect/ConnectAck/Ping/Pong), поэтому
    /// идут по приоритетному контрольному каналу и НЕ встают в хвост за балком
    /// Data (корень XR-086: ConnectAck нового стрима залипал за мегабайтами
    /// download в общей FIFO writer'а). Контрольный канал сливается writer-таском
    /// раньше Data (biased select), переполниться под балком не может.
    ///
    /// Инструментация: если отправка всё же виснет дольше 2с, значит стоит сам
    /// writer-таск (TCP send-буфер полон, сокет не принимает даже контроль),
    /// логируем WARN с командой. Поведение то же (дожидаемся), только лог.
    pub async fn send_frame(&self, stream_id: u32, command: Command, payload: Vec<u8>) -> io::Result<()> {
        let fut = self
            .ctrl_tx
            .send(OutFrame { stream_id, command, payload });
        tokio::pin!(fut);
        let broken = || io::Error::new(io::ErrorKind::BrokenPipe, "mux writer closed");
        match tokio::time::timeout(Duration::from_secs(2), &mut fut).await {
            Ok(r) => r.map_err(|_| broken()),
            Err(_) => {
                tracing::warn!(
                    "mux send_frame({:?}) blocked >2s on ctrl channel (writer task / TCP send stuck?)",
                    command
                );
                fut.await.map_err(|_| broken())
            }
        }
    }

    /// Take the new-stream notification receiver (server-side use).
    /// Can only be called once — returns None on subsequent calls.
    pub async fn take_new_stream_rx(&self) -> Option<mpsc::Receiver<NewStream>> {
        self.new_stream_rx.lock().await.take()
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Force-shutdown this Multiplexer. Marks it dead, wakes the writer
    /// task (which will drop write_half → TCP FIN → remote closes → our
    /// reader gets EOF and exits). Use this when the pool decides a slot
    /// is zombie (server-state lost while TCP still ESTABLISHED). Without
    /// this call, orphaned reader/writer tasks keep the socket open until
    /// MUX_MAX_LIFETIME (4h), and the server accumulates ghost ESTAB.
    /// Idempotent; safe to call multiple times.
    pub fn shutdown(&self) {
        self.alive.store(false, Ordering::Relaxed);
        self.shutdown_notify.notify_waiters();
    }
}

// ── Reader task ─────────────────────────────────────────────────────

async fn reader_task<R: AsyncReadExt + Unpin>(
    mut reader: R,
    codec: Codec,
    streams: Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>,
    ctrl_tx: mpsc::Sender<OutFrame>,
    new_stream_tx: mpsc::Sender<NewStream>,
) -> io::Result<()> {
    let mut buf = vec![0u8; 65536 + 256];
    let mut filled = 0;
    // tokio-часы (не std::Instant): в проде эквивалентно, но так MUX_MAX_LIFETIME
    // и детект мёртвого линка тестируются под `tokio::time::pause`.
    let started = tokio::time::Instant::now();
    // Последний момент, когда по линку пришли данные. Любой Pong на наш Ping его
    // обновляет, поэтому на живом линке он не стареет дольше KEEPALIVE_INTERVAL.
    let mut last_recv = tokio::time::Instant::now();
    let mut keepalive_interval = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive_interval.tick().await; // skip first immediate tick

    loop {
        // Max lifetime — force reconnection to prevent TCP degradation.
        if started.elapsed() >= MUX_MAX_LIFETIME {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "mux max lifetime reached"));
        }

        tokio::select! {
            result = reader.read(&mut buf[filled..]) => {
                let n = result?;
                if n == 0 { return Ok(()); }
                last_recv = tokio::time::Instant::now();
                filled += n;

                // Decode all complete frames.
                loop {
                    match codec.decode_frame(&buf[..filled])? {
                        Some((frame, consumed)) => {
                            dispatch_frame(&frame, &streams, &ctrl_tx, &new_stream_tx).await;
                            buf.copy_within(consumed..filled, 0);
                            filled -= consumed;
                        }
                        None => break,
                    }
                }
            }
            _ = keepalive_interval.tick() => {
                // Детект мёртвого линка: на blackhole (egress тихо дропается)
                // Pong'и не приходят и last_recv стареет. Рвём mux, чтобы слот
                // пула переподнялся, а не числился «живым» до 4ч (XR-083).
                if last_recv.elapsed() >= DEAD_LINK_TIMEOUT {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "mux dead link: no data within timeout",
                    ));
                }
                // Keepalive-Ping идёт по КОНТРОЛЬНОМУ каналу (отдельно от Data), и
                // всё равно через try_send без блокировки. Контрольный канал
                // сливается writer-таском с приоритетом и переполниться под
                // балком не может, но reader не должен блокироваться в принципе:
                // блокировка отправки = reader перестаёт читать сокет = mux
                // встаёт намертво (дедлок reader/writer, XR-084). Полный канал
                // -> Ping пропускаем (под нагрузкой last_recv и так свежий от
                // реального трафика), закрытый канал (writer умер) -> рвём mux.
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                match ctrl_tx.try_send(OutFrame {
                    stream_id: 0,
                    command: Command::Ping,
                    payload: ts.to_be_bytes().to_vec(),
                }) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        return Err(io::Error::new(io::ErrorKind::BrokenPipe, "writer closed"));
                    }
                }
            }
        }
    }
}

async fn dispatch_frame(
    frame: &Frame,
    streams: &Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>,
    ctrl_tx: &mpsc::Sender<OutFrame>,
    new_stream_tx: &mpsc::Sender<NewStream>,
) {
    match frame.command {
        Command::Ping => {
            // Ответный Pong best-effort через try_send по контрольному каналу:
            // reader НИКОГДА не должен блокироваться на отправке, иначе он
            // перестаёт читать сокет и mux встаёт намертво. Полный канал -> Pong
            // пропускаем (пир переживёт по своему таймауту).
            let _ = ctrl_tx.try_send(OutFrame {
                stream_id: 0,
                command: Command::Pong,
                payload: frame.payload.clone(),
            });
        }
        Command::Pong => {}
        Command::Data | Command::ConnectAck => {
            if let Ok((stream_id, data)) = decode_mux_payload(&frame.payload) {
                let mut remove = false;
                {
                    let streams_guard = streams.lock().await;
                    if let Some(tx) = streams_guard.get(&stream_id) {
                        // NEVER use send().await here — it blocks the reader task
                        // and deadlocks ALL other streams. Use try_send; if the
                        // channel is full, the stream consumer is stuck — kill it.
                        match tx.try_send(data.to_vec()) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                tracing::warn!("mux stream {} channel full, closing", stream_id);
                                remove = true;
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                remove = true;
                            }
                        }
                    }
                }
                if remove {
                    streams.lock().await.remove(&stream_id);
                }
            }
        }
        Command::Close => {
            if let Ok((stream_id, _)) = decode_mux_payload(&frame.payload) {
                streams.lock().await.remove(&stream_id);
            }
        }
        Command::Connect => {
            if let Ok((stream_id, data)) = decode_mux_payload(&frame.payload) {
                let streams_guard = streams.lock().await;
                if let Some(tx) = streams_guard.get(&stream_id) {
                    let _ = tx.try_send(data.to_vec());
                } else {
                    drop(streams_guard);
                    // Инструментация XR-086: при переполнении канала new_stream
                    // reader роняет Connect (try_send), клиент ловит "open timed
                    // out" без блокировок и ошибок. Логируем дроп, чтобы поймать
                    // этот механизм на живом эпизоде.
                    match new_stream_tx.try_send(NewStream {
                        stream_id,
                        payload: data.to_vec(),
                    }) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            tracing::warn!(
                                "mux new_stream channel FULL, DROPPING Connect sid={} (клиент словит open timed out)",
                                stream_id
                            );
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {}
                    }
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
    mut ctrl_rx: mpsc::Receiver<OutFrame>,
    mut data_rx: mpsc::Receiver<OutFrame>,
) -> io::Result<()> {
    // ПРИОРИТЕТ контрольного плана: `biased` select проверяет ctrl_rx раньше
    // data_rx, поэтому между любыми двумя балк-кадрами Data успевают уйти все
    // накопившиеся контрольные кадры (ConnectAck и т.п.). Так ConnectAck нового
    // стрима не стоит в очереди за мегабайтами Data (корень XR-086). Каждый канал
    // отключается из select своим guard'ом при закрытии, чтобы закрытый ctrl не
    // крутил busy-loop и не глотал недослитый data (и наоборот).
    let mut ctrl_open = true;
    let mut data_open = true;
    while ctrl_open || data_open {
        let frame = tokio::select! {
            biased;
            c = ctrl_rx.recv(), if ctrl_open => match c {
                Some(f) => f,
                None => { ctrl_open = false; continue; }
            },
            d = data_rx.recv(), if data_open => match d {
                Some(f) => f,
                None => { data_open = false; continue; }
            },
        };

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
    // Таймаут+WARN на взятие async-Mutex `streams`: если открытие вешается ЗДЕСЬ
    // (дедлок на этом локе), лог назовёт точку (XR-086, диагностика клиентского
    // зависания «open timed out, 0 пакетов на сервер»).
    match tokio::time::timeout(OPEN_STEP_TIMEOUT, mux.streams.lock()).await {
        Ok(mut streams) => {
            streams.insert(stream_id, data_tx);
        }
        Err(_) => {
            tracing::warn!("mux_open_stream wedged >4s taking streams lock (deadlock?)");
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "mux streams lock wedged",
            ));
        }
    }

    // Guard снимает регистрацию, если мы НЕ вернём MuxStream: по ошибке или по
    // ОТМЕНЕ future (наш bounded-таймаут в ServerPool рвёт ожидание ConnectAck на
    // полпути). Без этого на неотвечающем/blackhole сервере (ConnectAck не
    // приходит, а поздний try_send не срабатывает, ведь receiver жив пока future
    // не отменён) запись осиротевала бы и копила память (XR-079). disarm() только
    // на успехе.
    let mut guard = StreamRegGuard {
        streams: Some(mux.streams.clone()),
        stream_id,
    };

    // Send Connect(stream_id, target_addr) по контрольному плану. Таймаут+WARN:
    // если открытие вешается здесь, значит встал сам writer-таск (TCP send-буфер
    // полон, сокет не принимает даже контроль). Именно так выглядел живой хэнг DE
    // (0 пакетов на сервер, open timed out).
    match tokio::time::timeout(
        OPEN_STEP_TIMEOUT,
        mux.send_frame(stream_id, Command::Connect, target.encode()),
    )
    .await
    {
        Ok(r) => r?,
        Err(_) => {
            tracing::warn!(
                "mux_open_stream wedged >4s sending Connect (ctrl channel / writer task stuck?)"
            );
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "mux send_frame(Connect) wedged",
            ));
        }
    }

    // Wait for ConnectAck — delivered as first message on the channel.
    // The reader task dispatches ConnectAck payload (after stream_id prefix)
    // to this stream's channel.
    let result = match tokio::time::timeout(Duration::from_secs(10), data_rx.recv()).await {
        Ok(Some(_ack_payload)) => Ok(MuxStream {
            stream_id,
            rx: Some(data_rx),
            writer_tx: mux.writer_tx.clone(),
            alive: mux.alive.clone(),
            closed: false,
            detached: false,
        }),
        Ok(None) => Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "mux connection died during open",
        )),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "mux connect ack timeout",
        )),
    };
    if result.is_ok() {
        guard.disarm();
    }
    result
}

/// Снимает регистрацию стрима из `mux.streams`, если `mux_open_stream` не дошёл
/// до успешного возврата `MuxStream`. Ловит и обычный ранний выход, и ОТМЕНУ
/// future (bounded-таймаут в `ServerPool::open_stream`). Очистка идёт в
/// отдельном таске: `streams` за async-Mutex, синхронный Drop его не залочит.
struct StreamRegGuard {
    streams: Option<Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>>,
    stream_id: u32,
}

impl StreamRegGuard {
    fn disarm(&mut self) {
        self.streams = None;
    }
}

impl Drop for StreamRegGuard {
    fn drop(&mut self) {
        if let Some(streams) = self.streams.take() {
            let sid = self.stream_id;
            tokio::spawn(async move {
                streams.lock().await.remove(&sid);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obfuscation::{ModifierStrategy, Obfuscator};
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

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

    /// Регрессия XR-079: отмена `mux_open_stream` (наш bounded-таймаут в
    /// ServerPool рвёт ожидание ConnectAck) НЕ должна оставлять осиротевшую
    /// запись в `mux.streams`. Иначе на неотвечающем сервере регистрации
    /// копятся и утягивают память. Сервер тут молчит на ConnectAck.
    #[tokio::test]
    async fn test_open_stream_cancel_cleans_registration() {
        // `_server_io` держим живым, иначе duplex закроется, reader получит EOF
        // и mux станет !alive (open вернётся рано, минуя путь отмены).
        let (client_io, _server_io) = duplex(65536);
        let codec = test_codec();
        let mux = Multiplexer::new_client(client_io, codec);

        let target = TargetAddr::Domain("silent.test".to_string(), 443);
        // Внешний таймаут короче внутренних 10с ConnectAck: он отменяет
        // `mux_open_stream` на полпути, как это делает ServerPool.
        let r = tokio::time::timeout(
            Duration::from_millis(50),
            mux_open_stream(&mux, &target),
        )
        .await;
        assert!(r.is_err(), "open must be cancelled by the outer timeout");

        // Guard чистит регистрацию в отдельном таске: даём ему прокрутиться.
        let mut cleaned = false;
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            if mux.streams.lock().await.is_empty() {
                cleaned = true;
                break;
            }
        }
        assert!(
            cleaned,
            "cancelled open_stream must not leak the stream registration"
        );
    }

    /// Регрессия XR-083: blackhole-линк (сервер не шлёт ни данных, ни Pong, но
    /// TCP не закрыт) должен помечаться мёртвым по `DEAD_LINK_TIMEOUT`, а не
    /// числиться живым до `MUX_MAX_LIFETIME`=4ч. `_server_io` держим живым, чтобы
    /// не сработал путь EOF: детект должен идти именно по молчанию.
    #[tokio::test(start_paused = true)]
    async fn test_reader_detects_dead_link() {
        let (client_io, _server_io) = duplex(65536);
        let codec = test_codec();
        let mux = Multiplexer::new_client(client_io, codec);
        assert!(mux.is_alive(), "fresh mux must be alive");

        // Дать reader-таску запуститься и встать на select (skip-тик + await).
        tokio::task::yield_now().await;

        // Промотать paused-часы шагами по keepalive, прокручивая reader на каждом:
        // на шаге, где молчание перевалит DEAD_LINK_TIMEOUT, keepalive-ветка
        // вернёт Err и выставит alive=false.
        let mut dead = false;
        for _ in 0..6 {
            tokio::time::advance(KEEPALIVE_INTERVAL).await;
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
            if !mux.is_alive() {
                dead = true;
                break;
            }
        }
        assert!(dead, "a silent (blackhole) link must be detected as dead");
    }

    /// Регрессия XR-083b (дедлок reader/writer под флудом): пир заваливает
    /// клиента Ping-кадрами и НЕ читает ответные Pong, поэтому writer клиента
    /// упирается и writer_tx забивается. Раньше reader отвечал Pong через
    /// блокирующий `send().await` и повисал на переполненном канале, переставал
    /// читать сокет, и mux вставал намертво (лечился только kill). С `try_send`
    /// reader продолжает читать (Pong дропается), поэтому наш поток Ping уходит
    /// без зависания.
    #[tokio::test]
    async fn test_reader_survives_full_writer_flood() {
        let codec = test_codec();
        // Маленький буфер, чтобы writer клиента упёрся быстро.
        let (client_io, mut server_io) = duplex(512);
        let _client = Multiplexer::new_client(client_io, codec.clone());

        let ping = codec.encode_frame(Command::Ping, &0u64.to_be_bytes()).unwrap();
        let flood = tokio::time::timeout(Duration::from_secs(5), async {
            for _ in 0..5000 {
                server_io.write_all(&ping).await.unwrap();
            }
        })
        .await;
        assert!(
            flood.is_ok(),
            "reader must keep draining the socket under a full writer channel (no deadlock)"
        );
    }

    /// Регрессия XR-086 (head-of-line контрольных кадров): контрольный кадр
    /// (ConnectAck нового стрима), поставленный в очередь ПОСЛЕ того как балк-план
    /// уже забит мегабайтами Data, всё равно уходит в провод ПЕРВЫМ. Раньше всё
    /// шло одним FIFO writer'а, и ConnectAck вставал в хвост за всей балк-очередью;
    /// на медленном линке он не успевал за PER_SERVER_OPEN_TIMEOUT, и клиент ловил
    /// "open timed out" (прокси «зависало», лечил только рестарт).
    ///
    /// Детерминизм: оба канала заполнены ДО старта writer'а, а `biased`-select
    /// сливает контрольный раньше балка независимо от планировщика. Мутация,
    /// возвращающая баг (увести ConnectAck в `data_tx` или убрать приоритет), даёт
    /// позицию ConnectAck в хвосте -> тест краснеет.
    #[tokio::test]
    async fn test_writer_prioritizes_ctrl_over_bulk_data() {
        let codec = test_codec();
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<OutFrame>(CTRL_CHANNEL_SIZE);
        let (data_tx, data_rx) = mpsc::channel::<OutFrame>(WRITER_CHANNEL_SIZE);

        const BULK: usize = 500;
        for _ in 0..BULK {
            data_tx
                .try_send(OutFrame {
                    stream_id: 7,
                    command: Command::Data,
                    payload: vec![0u8; 64],
                })
                .unwrap();
        }
        // Контрольный кадр «пришёл» уже после того, как балк осел в очереди.
        ctrl_tx
            .try_send(OutFrame {
                stream_id: 42,
                command: Command::ConnectAck,
                payload: vec![1],
            })
            .unwrap();

        // Закрываем отправители, чтобы writer слил всё и вышел (дропнув свой конец
        // duplex -> EOF на приёмнике).
        drop(ctrl_tx);
        drop(data_tx);

        let (w, mut r) = duplex(1 << 20); // буфер вмещает весь балк, writer не встаёт
        let codec_w = codec.clone();
        let writer = tokio::spawn(async move {
            writer_task(w, codec_w, ctrl_rx, data_rx).await.unwrap();
        });

        let mut buf = Vec::new();
        r.read_to_end(&mut buf).await.unwrap();
        writer.await.unwrap();

        // Декодируем кадры по порядку и ищем позицию ConnectAck.
        let mut off = 0;
        let mut ack_pos = None;
        let mut count = 0;
        while off < buf.len() {
            match codec.decode_frame(&buf[off..]).unwrap() {
                Some((frame, consumed)) => {
                    if ack_pos.is_none() && matches!(frame.command, Command::ConnectAck) {
                        ack_pos = Some(count);
                    }
                    count += 1;
                    off += consumed;
                }
                None => break,
            }
        }

        assert_eq!(count, BULK + 1, "все кадры должны быть записаны");
        assert_eq!(
            ack_pos,
            Some(0),
            "ConnectAck обязан уйти первым, не в хвосте за балком Data"
        );
    }
}
