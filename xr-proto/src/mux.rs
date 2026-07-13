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
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::time::Duration;

use crate::protocol::{
    decode_mux_payload, encode_mux_payload, Codec, Command, Frame, TargetAddr,
    CLOSE_REASON_CONNECT_FAIL, CLOSE_REASON_RESOLVE_FAIL,
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
/// Бит capability в байте флагов MuxInit/MuxInitAck: пир умеет оконный flow
/// control стримов (WindowUpdate, LLD-27).
const MUX_FLAG_WINDOW: u8 = 0x01;
/// Начальное окно приёма стрима (LLD-27): столько байт Data пир шлёт без
/// возврата кредита. Покрывает BDP наших линков (~640 КБ при 50 Мбит/с и RTT
/// 100мс) и режет память на медленный стрим до 1 МиБ вместо полного канала
/// (1024 кадра, до 16 МБ). Обе стороны держат одну константу, обмена размером
/// в хендшейке нет (yamux-подход); менять размер - новым флагом.
const STREAM_RECV_WINDOW: u32 = 1024 * 1024;
/// Порог возврата кредита: вычитанное копится и уезжает одним WindowUpdate раз
/// в полокна, а не на каждый recv.
const WINDOW_UPDATE_THRESHOLD: u32 = STREAM_RECV_WINDOW / 2;

/// Возможности mux, согласованные хендшейком (LLD-27). Живут байтом флагов:
/// второй байт MuxInit, третий байт MuxInitAck (пересечение сторон). Старый пир
/// байта не шлёт и не читает, отсутствие = пустые флаги, поэтому смешанные
/// версии совместимы в обе стороны и выкат не требует лок-степа.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MuxCaps {
    /// Оконный flow control стримов: слать WindowUpdate и уважать окно пира.
    pub window: bool,
}

impl MuxCaps {
    /// Что умеет эта сборка; уходит в хендшейк, пересекается с флагами пира.
    pub const LOCAL: MuxCaps = MuxCaps { window: true };

    fn to_flags(self) -> u8 {
        if self.window {
            MUX_FLAG_WINDOW
        } else {
            0
        }
    }

    fn from_flags(flags: u8) -> Self {
        MuxCaps {
            window: flags & MUX_FLAG_WINDOW != 0,
        }
    }
}

// ── Relay health (XR-094) ───────────────────────────────────────────

/// Длина одного бакета окна исходов relay. Смотрим текущий бакет плюс
/// предыдущий, то есть картина живёт 30-60с: достаточно быстро для failover
/// «в пределах минуты» и достаточно долго, чтобы одиночные сбои растворялись
/// в фоне успешных стримов.
const RELAY_WINDOW: Duration = Duration::from_secs(30);
/// Минимум сбоев одного класса в окне, ниже которого деградацию не объявляем:
/// фон одиночных connect timeout (мусорные IP, закрытые порты) и редких
/// NXDOMAIN не должен дёргать failover.
const RELAY_FAIL_MIN: u32 = 5;

#[derive(Default, Clone, Copy)]
struct RelayBucket {
    /// Стримы с доменным таргетом, получившие хотя бы один Data-кадр.
    domain_ok: u32,
    /// Все стримы, получившие хотя бы один Data-кадр (домены + IP).
    total_ok: u32,
    resolve_fail: u32,
    connect_fail: u32,
}

struct RelayWindow {
    cur_start: tokio::time::Instant,
    cur: RelayBucket,
    prev: RelayBucket,
}

impl RelayWindow {
    /// Прокрутить окно: устаревший текущий бакет уезжает в prev, совсем
    /// старая история выбрасывается.
    fn rotate(&mut self) {
        let elapsed = self.cur_start.elapsed();
        if elapsed < RELAY_WINDOW {
            return;
        }
        self.prev = if elapsed < RELAY_WINDOW * 2 {
            self.cur
        } else {
            RelayBucket::default()
        };
        self.cur = RelayBucket::default();
        self.cur_start = tokio::time::Instant::now();
    }
}

/// Исходы relay глазами клиента: успехи (стрим получил данные) против сбоев
/// установки relay на сервере (причина из Close, см. `CLOSE_REASON_*`).
///
/// Закрывает дыру health из XR-094: у сервера с мёртвым DNS или egress туннель
/// и keepalive живы, ConnectAck приходит, и ни breaker, ни dead-link-детект
/// (XR-083) не срабатывают, хотя каждый Connect кончается relay error и
/// полезная работа нулевая. Здесь live-трафик сам становится health-сигналом.
///
/// Resolve-сбои сравниваются только с доменными успехами: при мёртвом DNS
/// IP-таргеты (например, CIDR-роутинг Telegram) продолжают работать и не
/// должны маскировать полностью лежащие домены.
pub struct RelayHealth {
    inner: std::sync::Mutex<RelayWindow>,
}

impl RelayHealth {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(RelayWindow {
                cur_start: tokio::time::Instant::now(),
                cur: RelayBucket::default(),
                prev: RelayBucket::default(),
            }),
        }
    }

    pub fn record_success(&self, domain: bool) {
        let mut w = self.inner.lock().unwrap();
        w.rotate();
        w.cur.total_ok += 1;
        if domain {
            w.cur.domain_ok += 1;
        }
    }

    pub fn record_resolve_fail(&self) {
        let mut w = self.inner.lock().unwrap();
        w.rotate();
        w.cur.resolve_fail += 1;
    }

    pub fn record_connect_fail(&self) {
        let mut w = self.inner.lock().unwrap();
        w.rotate();
        w.cur.connect_fail += 1;
    }

    /// Relay сервера деградировал: сбоев одного класса в окне не меньше
    /// `RELAY_FAIL_MIN` и строго больше, чем сопоставимых успехов. Требование
    /// «больше успехов» отсекает фон (единичные мёртвые домены и IP есть
    /// всегда), минимум по счёту отсекает малую выборку.
    pub fn degraded(&self) -> bool {
        let mut w = self.inner.lock().unwrap();
        w.rotate();
        let domain_ok = w.cur.domain_ok + w.prev.domain_ok;
        let total_ok = w.cur.total_ok + w.prev.total_ok;
        let resolve_fail = w.cur.resolve_fail + w.prev.resolve_fail;
        let connect_fail = w.cur.connect_fail + w.prev.connect_fail;
        (resolve_fail >= RELAY_FAIL_MIN && resolve_fail > domain_ok)
            || (connect_fail >= RELAY_FAIL_MIN && connect_fail > total_ok)
    }

    /// Сбросить окно. Вызывается при уходе с деградировавшего сервера и при
    /// смене сети: после возврата деградацию должен подтвердить свежий
    /// трафик, а не хвост старых сбоев.
    pub fn reset(&self) {
        let mut w = self.inner.lock().unwrap();
        w.cur = RelayBucket::default();
        w.prev = RelayBucket::default();
        w.cur_start = tokio::time::Instant::now();
    }

    #[cfg(test)]
    fn snapshot(&self) -> (u32, u32, u32, u32) {
        let mut w = self.inner.lock().unwrap();
        w.rotate();
        (
            w.cur.domain_ok + w.prev.domain_ok,
            w.cur.total_ok + w.prev.total_ok,
            w.cur.resolve_fail + w.prev.resolve_fail,
            w.cur.connect_fail + w.prev.connect_fail,
        )
    }
}

impl Default for RelayHealth {
    fn default() -> Self {
        Self::new()
    }
}

// ── Outgoing frame ──────────────────────────────────────────────────

/// A frame queued for writing to the TCP connection.
struct OutFrame {
    stream_id: u32,
    command: Command,
    payload: Vec<u8>,
}

// -- Flow control (LLD-27) -------------------------------------------

/// Окно отправки одного стрима: сколько байт Data ещё можно поставить в writer,
/// не дожидаясь возврата кредита пиром. Живёт в `StreamEntry` (reader пополняет
/// его по WindowUpdate) и шарится с хэндлами стрима. При выключенном окне (пир
/// без capability) кредит стартует с i64::MAX и на практике не кончается,
/// поэтому путь отправки один и ветвления на легаси нет.
#[derive(Debug)]
struct SendWindow {
    credit: AtomicI64,
    notify: Notify,
    /// Стрим снят (Close пира, kill при переполнении, смерть mux): ждущие
    /// кредита просыпаются ошибкой, а не висят на мёртвом стриме.
    closed: AtomicBool,
}

impl SendWindow {
    fn new(initial: i64) -> Arc<Self> {
        Arc::new(Self {
            credit: AtomicI64::new(initial),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
        })
    }

    /// Отдать до `want` (> 0) байт кредита, дождавшись WindowUpdate при пустом
    /// окне. Возврат меньше want значит кадр надо порезать.
    async fn acquire(&self, want: usize) -> io::Result<usize> {
        loop {
            match self.try_acquire(want)? {
                0 => {}
                n => return Ok(n),
            }
            let notified = self.notify.notified();
            // Перепроверка после подписки: WindowUpdate или закрытие, пришедшие
            // между try_acquire и notified(), иначе потерялись бы навсегда.
            if self.credit.load(Ordering::Acquire) > 0 || self.closed.load(Ordering::Acquire) {
                continue;
            }
            notified.await;
        }
    }

    /// Неблокирующая половина acquire: 0 = кредита сейчас нет.
    fn try_acquire(&self, want: usize) -> io::Result<usize> {
        loop {
            if self.closed.load(Ordering::Acquire) {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "mux stream closed"));
            }
            let cur = self.credit.load(Ordering::Acquire);
            if cur <= 0 {
                return Ok(0);
            }
            let take = cur.min(want as i64);
            if self
                .credit
                .compare_exchange(cur, cur - take, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(take as usize);
            }
        }
    }

    fn add(&self, n: u32) {
        self.credit.fetch_add(n as i64, Ordering::AcqRel);
        self.notify.notify_waiters();
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

/// Возврат кредита пиру по мере вычитывания. Копит вычитанные байты и раз в
/// полокна шлёт WindowUpdate КОНТРОЛЬНЫМ планом: кредит не должен стоять в
/// очереди за чужим балком Data, иначе при насыщении обеих сторон окно не
/// пополняется вовремя. Отправка через try_send без блокировки и без потери:
/// не влезло в канал - счётчик копится и уедет со следующим вычитыванием;
/// заодно кредит не привязан к await-точке и переживает отмену recv-future
/// потребителя (recv внутри tokio::select!).
#[derive(Debug)]
struct RecvCredit {
    stream_id: u32,
    ctrl_tx: mpsc::Sender<OutFrame>,
    pending: u32,
}

impl RecvCredit {
    fn consumed(&mut self, n: usize) {
        self.pending = self.pending.saturating_add(n as u32);
        if self.pending < WINDOW_UPDATE_THRESHOLD {
            return;
        }
        let frame = OutFrame {
            stream_id: self.stream_id,
            command: Command::WindowUpdate,
            payload: self.pending.to_be_bytes().to_vec(),
        };
        if self.ctrl_tx.try_send(frame).is_ok() {
            self.pending = 0;
        }
    }
}

/// Потолок Data-кадра в send_data: лимит payload кодека (u16) минус префикс
/// stream_id. Раньше кусок больше лимита молча ронял writer на encode_frame
/// (контракт держался на том, что все вызывающие читают сокет мелкими буферами);
/// с нарезкой по окну потолок закреплён здесь.
const SEND_CHUNK_MAX: usize = u16::MAX as usize - 4;

/// Общий путь отправки Data с оконным кредитом: кадр режется по доступному окну
/// и уходит частями, когда пир вернул меньше, чем просили. Err = mux мёртв или
/// стрим снят пиром (аналог EPIPE после чужого Close).
async fn send_data(
    writer_tx: &mpsc::Sender<OutFrame>,
    alive: &AtomicBool,
    window: &SendWindow,
    stream_id: u32,
    data: &[u8],
) -> io::Result<()> {
    if !alive.load(Ordering::Relaxed) {
        return Err(io::Error::new(io::ErrorKind::BrokenPipe, "mux connection dead"));
    }
    let broken = || io::Error::new(io::ErrorKind::BrokenPipe, "mux writer closed");
    if data.is_empty() {
        // Пустой Data-кадр окна не стоит: приёмнику нечего вычитывать.
        return writer_tx
            .send(OutFrame { stream_id, command: Command::Data, payload: Vec::new() })
            .await
            .map_err(|_| broken());
    }
    let mut off = 0;
    while off < data.len() {
        let n = window.acquire((data.len() - off).min(SEND_CHUNK_MAX)).await?;
        writer_tx
            .send(OutFrame {
                stream_id,
                command: Command::Data,
                payload: data[off..off + n].to_vec(),
            })
            .await
            .map_err(|_| broken())?;
        off += n;
    }
    Ok(())
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
    /// Set by `split()` so Drop on the husk skips Close - the WriteHalf now
    /// owns that contract.
    detached: bool,
    /// Окно отправки (LLD-27); reader пополняет его по WindowUpdate пира.
    window: Arc<SendWindow>,
    /// Возврат кредита пиру по мере вычитывания; None = окно не согласовано.
    recv_credit: Option<RecvCredit>,
}

impl MuxStream {
    /// Receive data from this stream. Returns None if the stream or
    /// mux connection is closed.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        let data = self.rx.as_mut()?.recv().await?;
        if let Some(credit) = self.recv_credit.as_mut() {
            credit.consumed(data.len());
        }
        Some(data)
    }

    /// Send data on this stream.
    pub async fn send(&self, data: &[u8]) -> io::Result<()> {
        send_data(&self.writer_tx, &self.alive, &self.window, self.stream_id, data).await
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
        let recv_credit = self.recv_credit.take();
        self.detached = true;
        let write = MuxWriteHalf {
            stream_id: self.stream_id,
            writer_tx: self.writer_tx.clone(),
            alive: self.alive.clone(),
            closed: self.closed,
            window: self.window.clone(),
        };
        // self drops here; Drop honors `detached` and skips Close.
        (MuxReadHalf { rx, recv_credit }, write)
    }
}

impl MuxStream {
    /// Adapt this stream into an `AsyncRead + AsyncWrite` handle (LLD-23 §3.4),
    /// so hyper can `serve_connection` over a reverse-stream on the agent and the
    /// relay can `tokio::io::copy` two streams into each other (the blind splice).
    /// Consumes the stream; Close travels on shutdown or on drop of the io handle,
    /// exactly as [`MuxStream`] does. Panics if the stream was already `split()`.
    pub fn into_io(mut self) -> MuxStreamIo {
        let rx = self.rx.take().expect("MuxStream already split");
        let recv_credit = self.recv_credit.take();
        self.detached = true; // husk drop must not also send Close
        MuxStreamIo {
            stream_id: self.stream_id,
            rx,
            writer_tx: self.writer_tx.clone(),
            alive: self.alive.clone(),
            window: self.window.clone(),
            recv_credit,
            read_buf: Vec::new(),
            read_pos: 0,
            write_fut: None,
            close_fut: None,
            closed: self.closed,
        }
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

// ── MuxStreamIo: AsyncRead + AsyncWrite over one stream ──────────────

/// Max Data payload per frame emitted by the io adapter. Below the mux payload
/// cap (65535 minus the 4-byte stream_id prefix); 16 KiB matches typical socket
/// reads and keeps single frames small enough not to starve the ctrl plane.
const IO_WRITE_CHUNK: usize = 16 * 1024;

fn mux_broken() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "mux stream closed")
}

/// [`MuxStream`] presented as an `AsyncRead + AsyncWrite` byte channel. Reads pull
/// whole Data payloads off the per-stream channel and keep any tail that didn't
/// fit the caller's buffer; writes chunk into Data frames on the same bulk plane
/// as `MuxStream::send`. Shutdown (or drop) sends Close.
///
/// The write path takes the fast `try_send` when the writer channel has room and
/// only boxes a send future when it is full, so the common case allocates nothing
/// beyond the payload copy the wire needs anyway.
pub struct MuxStreamIo {
    stream_id: u32,
    rx: mpsc::Receiver<Vec<u8>>,
    writer_tx: mpsc::Sender<OutFrame>,
    alive: Arc<AtomicBool>,
    /// Окно отправки (LLD-27): без кредита poll_write уходит в медленный путь
    /// и ждёт WindowUpdate пира.
    window: Arc<SendWindow>,
    /// Возврат кредита пиру по мере вычитывания; None = окно не согласовано.
    recv_credit: Option<RecvCredit>,
    /// Tail of the last received payload that didn't fit the caller's buffer.
    read_buf: Vec<u8>,
    read_pos: usize,
    /// In-flight Data send (writer channel was full or the window was empty);
    /// resolves to the byte count promised to the caller.
    write_fut: Option<Pin<Box<dyn Future<Output = Result<usize, ()>> + Send>>>,
    /// In-flight Close send during shutdown.
    close_fut: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    closed: bool,
}

/// Прокрутить отложенную отправку Data; по готовности вернуть принятые байты
/// и очистить слот.
fn poll_stored_write(
    write_fut: &mut Option<Pin<Box<dyn Future<Output = Result<usize, ()>> + Send>>>,
    cx: &mut Context<'_>,
) -> Poll<io::Result<usize>> {
    let fut = write_fut.as_mut().expect("write_fut present");
    match fut.as_mut().poll(cx) {
        Poll::Ready(Ok(n)) => {
            *write_fut = None;
            Poll::Ready(Ok(n))
        }
        Poll::Ready(Err(())) => {
            *write_fut = None;
            Poll::Ready(Err(mux_broken()))
        }
        Poll::Pending => Poll::Pending,
    }
}

impl MuxStreamIo {
    pub fn stream_id(&self) -> u32 {
        self.stream_id
    }
}

impl AsyncRead for MuxStreamIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        // Serve the leftover tail before pulling a fresh payload.
        if this.read_pos < this.read_buf.len() {
            let n = (this.read_buf.len() - this.read_pos).min(buf.remaining());
            buf.put_slice(&this.read_buf[this.read_pos..this.read_pos + n]);
            this.read_pos += n;
            if this.read_pos >= this.read_buf.len() {
                this.read_buf.clear();
                this.read_pos = 0;
            }
            return Poll::Ready(Ok(()));
        }
        loop {
            match this.rx.poll_recv(cx) {
                Poll::Ready(Some(data)) => {
                    // Кредит возвращается при снятии кадра с канала (слот
                    // освободился), не при доедании хвоста caller'ом: переучёт
                    // ограничен одним кадром, как в h2.
                    if let Some(credit) = this.recv_credit.as_mut() {
                        credit.consumed(data.len());
                    }
                    // Empty payloads (e.g. a bare ConnectAck routed here) carry no
                    // bytes, so skip rather than signal EOF.
                    if data.is_empty() {
                        continue;
                    }
                    let n = data.len().min(buf.remaining());
                    buf.put_slice(&data[..n]);
                    if n < data.len() {
                        this.read_buf = data;
                        this.read_pos = n;
                    }
                    return Poll::Ready(Ok(()));
                }
                // Channel closed = peer Close or dead mux: clean EOF.
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl AsyncWrite for MuxStreamIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        // Finish a send that returned Pending last time.
        if this.write_fut.is_some() {
            return poll_stored_write(&mut this.write_fut, cx);
        }
        if !this.alive.load(Ordering::Relaxed) {
            return Poll::Ready(Err(mux_broken()));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let want = buf.len().min(IO_WRITE_CHUNK);
        // Окно (LLD-27): кадр режется по доступному кредиту. Пустое окно уводит
        // в медленный путь: future ждёт WindowUpdate пира, потом шлёт.
        let n = match this.window.try_acquire(want) {
            Err(_) => return Poll::Ready(Err(mux_broken())),
            Ok(0) => {
                let window = this.window.clone();
                let tx = this.writer_tx.clone();
                let sid = this.stream_id;
                let mut payload = buf[..want].to_vec();
                this.write_fut = Some(Box::pin(async move {
                    let granted = window.acquire(payload.len()).await.map_err(|_| ())?;
                    payload.truncate(granted);
                    tx.send(OutFrame { stream_id: sid, command: Command::Data, payload })
                        .await
                        .map_err(|_| ())?;
                    Ok(granted)
                }));
                return poll_stored_write(&mut this.write_fut, cx);
            }
            Ok(n) => n,
        };
        let frame = OutFrame {
            stream_id: this.stream_id,
            command: Command::Data,
            payload: buf[..n].to_vec(),
        };
        match this.writer_tx.try_send(frame) {
            Ok(()) => Poll::Ready(Ok(n)),
            Err(mpsc::error::TrySendError::Full(frame)) => {
                let tx = this.writer_tx.clone();
                this.write_fut = Some(Box::pin(async move {
                    tx.send(frame).await.map_err(|_| ())?;
                    Ok(n)
                }));
                poll_stored_write(&mut this.write_fut, cx)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Poll::Ready(Err(mux_broken())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // Data is flushed by the writer task once it leaves the channel; the only
        // buffered state here is an in-flight send, so drive it to completion.
        if this.write_fut.is_some() {
            poll_stored_write(&mut this.write_fut, cx).map_ok(|_| ())
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // Let a pending Data send finish before Close, or it would overtake still
        // unwritten Data of this stream (same ordering rule as MuxStream::close).
        if this.write_fut.is_some() {
            match poll_stored_write(&mut this.write_fut, cx) {
                Poll::Ready(Ok(_)) => {}
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        if this.closed {
            return Poll::Ready(Ok(()));
        }
        if this.close_fut.is_none() {
            let tx = this.writer_tx.clone();
            let sid = this.stream_id;
            this.close_fut = Some(Box::pin(async move {
                let _ = tx
                    .send(OutFrame {
                        stream_id: sid,
                        command: Command::Close,
                        payload: Vec::new(),
                    })
                    .await;
            }));
        }
        match this.close_fut.as_mut().unwrap().as_mut().poll(cx) {
            Poll::Ready(()) => {
                this.close_fut = None;
                this.closed = true;
                Poll::Ready(Ok(()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for MuxStreamIo {
    fn drop(&mut self) {
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
    /// Возврат кредита пиру (LLD-27); None = окно не согласовано.
    recv_credit: Option<RecvCredit>,
}

impl MuxReadHalf {
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        let data = self.rx.recv().await?;
        if let Some(credit) = self.recv_credit.as_mut() {
            credit.consumed(data.len());
        }
        Some(data)
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
    /// Окно отправки (LLD-27), общее с остальными хэндлами стрима.
    window: Arc<SendWindow>,
}

impl MuxWriteHalf {
    pub async fn send(&self, data: &[u8]) -> io::Result<()> {
        send_data(&self.writer_tx, &self.alive, &self.window, self.stream_id, data).await
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

/// Приёмная сторона зарегистрированного стрима плюс состояние для учёта
/// исходов relay (XR-094).
struct StreamEntry {
    tx: mpsc::Sender<Vec<u8>>,
    /// Первый Data-кадр уже пришёл (relay-успех засчитан, повторно не считаем).
    got_data: bool,
    /// Таргет стрима это домен: успех идёт и в доменный счётчик здоровья.
    domain: bool,
    /// Окно отправки нашей стороны (LLD-27); reader пополняет его по
    /// WindowUpdate пира.
    window: Arc<SendWindow>,
}

impl Drop for StreamEntry {
    fn drop(&mut self) {
        // Любое снятие записи (Close пира, kill при переполнении, очистка карты
        // при смерти reader) будит отправителей, заснувших на пустом окне.
        self.window.close();
    }
}

/// Manages a multiplexed TCP connection with multiple logical streams.
pub struct Multiplexer {
    /// Балк-план: Command::Data и Close (Close ордерится за Data того же стрима).
    writer_tx: mpsc::Sender<OutFrame>,
    /// Контрольный план (приоритет в writer'е): Connect/ConnectAck/Ping/Pong.
    ctrl_tx: mpsc::Sender<OutFrame>,
    streams: Arc<Mutex<HashMap<u32, StreamEntry>>>,
    next_stream_id: AtomicU32,
    alive: Arc<AtomicBool>,
    _close_notify: Arc<Notify>,
    /// Channel for incoming Connect frames for unregistered stream_ids.
    /// Server reads from this to create target connections.
    new_stream_rx: Mutex<Option<mpsc::Receiver<NewStream>>>,
    /// Externally-triggered shutdown signal. When the pool decides a slot
    /// is zombie (alive=true but server-state lost), calling shutdown()
    /// drops the write half, which propagates FIN → server closes → our
    /// reader gets EOF -> TCP fully closes. Without this the orphaned
    /// reader/writer tasks keep the socket ESTABLISHED for up to
    /// MUX_MAX_LIFETIME (4h), accumulating ghost connections on the server.
    shutdown_notify: Arc<Notify>,
    /// Возможности, согласованные хендшейком (LLD-27).
    caps: MuxCaps,
}

impl Multiplexer {
    /// Create a client-side multiplexer over an established TCP connection.
    /// The TCP connection must already have completed MuxInit/MuxInitAck;
    /// `caps` это результат хендшейка (LLD-27).
    pub fn new_client<S>(stream: S, codec: Codec, caps: MuxCaps) -> Arc<Self>
    where
        S: AsyncReadExt + AsyncWriteExt + Send + Unpin + 'static,
    {
        Self::new_inner(stream, codec, 1, None, caps) // client uses odd stream IDs
    }

    /// Клиентский мультиплексор с учётом исходов relay в общем здоровье пула
    /// (XR-094): причины из Close и первые Data-кадры стримов записываются в
    /// `health`, по нему `ServerPool` ловит «туннель жив, работа не идёт».
    pub fn new_client_tracked<S>(
        stream: S,
        codec: Codec,
        health: Arc<RelayHealth>,
        caps: MuxCaps,
    ) -> Arc<Self>
    where
        S: AsyncReadExt + AsyncWriteExt + Send + Unpin + 'static,
    {
        Self::new_inner(stream, codec, 1, Some(health), caps)
    }

    /// Create a server-side multiplexer over an established TCP connection.
    pub fn new_server<S>(stream: S, codec: Codec, caps: MuxCaps) -> Arc<Self>
    where
        S: AsyncReadExt + AsyncWriteExt + Send + Unpin + 'static,
    {
        Self::new_inner(stream, codec, 2, None, caps) // server uses even stream IDs
    }

    fn new_inner<S>(
        stream: S,
        codec: Codec,
        first_stream_id: u32,
        relay_health: Option<Arc<RelayHealth>>,
        caps: MuxCaps,
    ) -> Arc<Self>
    where
        S: AsyncReadExt + AsyncWriteExt + Send + Unpin + 'static,
    {
        let (writer_tx, writer_rx) = mpsc::channel::<OutFrame>(WRITER_CHANNEL_SIZE);
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<OutFrame>(CTRL_CHANNEL_SIZE);
        let streams: Arc<Mutex<HashMap<u32, StreamEntry>>> =
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
                if let Err(e) = reader_task(read_half, codec, streams.clone(), reader_ctrl, new_stream_tx, relay_health).await {
                    tracing::debug!("mux reader ended: {}", e);
                }
                alive.store(false, Ordering::Relaxed);
                // Close all stream channels.
                streams.lock().await.clear();
                close_notify.notify_waiters();
            });
        }

        // Spawn writer task. Внешний shutdown() будит его через shutdown_notify, и
        // writer ЯВНО делает writer.shutdown() (FIN) перед выходом. Просто дроп
        // write_half при tokio::io::split сокет НЕ закрывает (read-половина жива у
        // reader_task), поэтому FIN нужен явно, иначе пир не получит EOF.
        {
            let alive = alive.clone();
            let codec = codec.clone();
            let shutdown_notify = shutdown_notify.clone();
            tokio::spawn(async move {
                if let Err(e) = writer_task(write_half, codec, ctrl_rx, writer_rx, shutdown_notify).await {
                    tracing::debug!("mux writer ended: {}", e);
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
            caps,
        })
    }

    /// Окно отправки нового стрима: при выключенном flow control кредит
    /// бесконечен, путь отправки не ветвится (LLD-27).
    fn new_send_window(&self) -> Arc<SendWindow> {
        let initial = if self.caps.window {
            STREAM_RECV_WINDOW as i64
        } else {
            i64::MAX
        };
        SendWindow::new(initial)
    }

    /// Возврат кредита пиру: только при согласованном окне, иначе старый пир
    /// умрёт на неизвестной команде WindowUpdate.
    fn new_recv_credit(&self, stream_id: u32) -> Option<RecvCredit> {
        self.caps.window.then(|| RecvCredit {
            stream_id,
            ctrl_tx: self.ctrl_tx.clone(),
            pending: 0,
        })
    }

    /// Register a stream that was opened by the remote side (server-side use).
    pub async fn register_stream(
        self: &Arc<Self>,
        stream_id: u32,
    ) -> MuxStream {
        let (data_tx, data_rx) = mpsc::channel(STREAM_CHANNEL_SIZE);
        let window = self.new_send_window();
        self.streams.lock().await.insert(
            stream_id,
            StreamEntry { tx: data_tx, got_data: false, domain: false, window: window.clone() },
        );

        MuxStream {
            stream_id,
            rx: Some(data_rx),
            writer_tx: self.writer_tx.clone(),
            alive: self.alive.clone(),
            closed: false,
            detached: false,
            window,
            recv_credit: self.new_recv_credit(stream_id),
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
    /// task, which does an explicit writer.shutdown() (FIN) -> remote gets EOF and
    /// reconnects. Use this when the pool decides a slot is zombie (server-state
    /// lost while TCP still ESTABLISHED) or when the server accept-loop ends by
    /// its lifetime cap. Without this call, orphaned reader/writer tasks keep the
    /// socket open until MUX_MAX_LIFETIME (4h): keepalive still flows, so the peer
    /// считает слот живым, но новых стримов уже не принять (корень XR-086).
    /// `notify_one` даёт персистентный пермит (не теряется гонкой с writer'ом).
    /// Idempotent; safe to call multiple times.
    pub fn shutdown(&self) {
        self.alive.store(false, Ordering::Relaxed);
        self.shutdown_notify.notify_one();
    }
}

// ── Reader task ─────────────────────────────────────────────────────

async fn reader_task<R: AsyncReadExt + Unpin>(
    mut reader: R,
    codec: Codec,
    streams: Arc<Mutex<HashMap<u32, StreamEntry>>>,
    ctrl_tx: mpsc::Sender<OutFrame>,
    new_stream_tx: mpsc::Sender<NewStream>,
    relay_health: Option<Arc<RelayHealth>>,
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
                            dispatch_frame(&frame, &streams, &ctrl_tx, &new_stream_tx, &relay_health).await;
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
    streams: &Arc<Mutex<HashMap<u32, StreamEntry>>>,
    ctrl_tx: &mpsc::Sender<OutFrame>,
    new_stream_tx: &mpsc::Sender<NewStream>,
    relay_health: &Option<Arc<RelayHealth>>,
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
        Command::WindowUpdate => {
            // Пир вернул кредит окна (LLD-27): пополнить окно отправки стрима
            // и разбудить заснувших. Не блокируется (атомик + notify), для
            // снятого стрима кредит молча выбрасывается.
            if let Ok((stream_id, inc)) = decode_mux_payload(&frame.payload) {
                if inc.len() >= 4 {
                    let add = u32::from_be_bytes([inc[0], inc[1], inc[2], inc[3]]);
                    if let Some(entry) = streams.lock().await.get(&stream_id) {
                        entry.window.add(add);
                    }
                }
            }
        }
        Command::Data | Command::ConnectAck => {
            if let Ok((stream_id, data)) = decode_mux_payload(&frame.payload) {
                let mut remove = false;
                {
                    let mut streams_guard = streams.lock().await;
                    if let Some(entry) = streams_guard.get_mut(&stream_id) {
                        // Первый Data-кадр стрима это доказательство, что relay
                        // на сервере реально отработал (resolve + connect до
                        // апстрима), засчитываем успех в здоровье (XR-094).
                        // ConnectAck не считается: сервер шлёт его ДО resolve.
                        if frame.command == Command::Data && !entry.got_data {
                            entry.got_data = true;
                            if let Some(h) = relay_health {
                                h.record_success(entry.domain);
                            }
                        }
                        // NEVER use send().await here — it blocks the reader task
                        // and deadlocks ALL other streams. Use try_send; if the
                        // channel is full, the stream consumer is stuck — kill it.
                        match entry.tx.try_send(data.to_vec()) {
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
            if let Ok((stream_id, reason)) = decode_mux_payload(&frame.payload) {
                let removed = streams.lock().await.remove(&stream_id);
                // Ненулевая причина в Close = установка relay на VPS упала
                // (см. CLOSE_REASON_*). Считаем сбой в здоровье сервера, но
                // только для ещё зарегистрированного стрима, чтобы дубль
                // Close не удвоил счёт (XR-094).
                if removed.is_some() {
                    if let (Some(h), Some(&code)) = (relay_health, reason.first()) {
                        match code {
                            CLOSE_REASON_RESOLVE_FAIL => h.record_resolve_fail(),
                            CLOSE_REASON_CONNECT_FAIL => h.record_connect_fail(),
                            _ => {}
                        }
                    }
                }
            }
        }
        Command::Connect => {
            if let Ok((stream_id, data)) = decode_mux_payload(&frame.payload) {
                let streams_guard = streams.lock().await;
                if let Some(entry) = streams_guard.get(&stream_id) {
                    let _ = entry.tx.try_send(data.to_vec());
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
    shutdown: Arc<Notify>,
) -> io::Result<()> {
    // ПРИОРИТЕТ контрольного плана: `biased` select проверяет ctrl_rx раньше
    // data_rx, поэтому между любыми двумя балк-кадрами Data успевают уйти все
    // накопившиеся контрольные кадры (ConnectAck и т.п.). Так ConnectAck нового
    // стрима не стоит в очереди за мегабайтами Data (корень XR-086). Каждый канал
    // отключается из select своим guard'ом при закрытии, чтобы закрытый ctrl не
    // крутил busy-loop и не глотал недослитый data (и наоборот).
    let mut ctrl_open = true;
    let mut data_open = true;
    let mut res: io::Result<()> = Ok(());
    while ctrl_open || data_open {
        let frame = tokio::select! {
            biased;
            // Внешний shutdown() (пул решил, что слот зомби; серверная accept-петля
            // кончилась по лайфтайм-капу). notify_one даёт ПЕРСИСТЕНТНЫЙ пермит,
            // поэтому сигнал не теряется, даже если пришёл, пока мы были в write_all.
            _ = shutdown.notified() => break,
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

        let wire = match codec.encode_frame(frame.command, &payload) {
            Ok(w) => w,
            Err(e) => { res = Err(e); break; }
        };
        if let Err(e) = writer.write_all(&wire).await {
            res = Err(e);
            break;
        }
    }
    // ЯВНЫЙ полу-close: шлём FIN пиру. Дроп write_half при tokio::io::split сокет НЕ
    // закрывает (read-половину держит reader_task), поэтому без этого пир не получал
    // бы EOF и висел бы на «живом» зомби-mux (корень XR-086: серверный accept умирал
    // по 1ч-капу, а сокет жил до 4ч, keepalive шёл, новые Connect молча терялись).
    let _ = writer.shutdown().await;
    res
}

// ── Handshake helpers ───────────────────────────────────────────────

/// Client: send MuxInit, wait for MuxInitAck.
/// Returns Ok(Some(caps)) с согласованными возможностями, Ok(None) if the
/// server rejected, Err on I/O error.
pub async fn mux_handshake_client<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
    codec: &Codec,
) -> io::Result<Option<MuxCaps>> {
    // Send MuxInit: версия + байт флагов (LLD-27). Старый сервер читает только
    // первый байт и лишний игнорирует.
    let init_payload = vec![MUX_PROTOCOL_VERSION, MuxCaps::LOCAL.to_flags()];
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
                    return Ok(None); // server doesn't support mux
                }
                if frame.payload.len() >= 2 && frame.payload[1] == 0 {
                    // Третий байт это флаги сервера; старый сервер его не шлёт,
                    // отсутствие читается как пустые флаги (окно выключено).
                    let flags = frame.payload.get(2).copied().unwrap_or(0);
                    return Ok(Some(MuxCaps::from_flags(flags & MuxCaps::LOCAL.to_flags())));
                }
                return Ok(None); // rejected
            }
            None => continue,
        }
    }
}

/// Server: check if frame is MuxInit, send MuxInitAck. Возвращает
/// согласованные возможности, None = не mux / версия не наша.
pub async fn mux_handshake_server<S: AsyncWriteExt + Unpin>(
    stream: &mut S,
    codec: &Codec,
    init_frame: &Frame,
) -> io::Result<Option<MuxCaps>> {
    if init_frame.command != Command::MuxInit {
        return Ok(None);
    }

    let version = init_frame.payload.first().copied().unwrap_or(0);
    if version != MUX_PROTOCOL_VERSION {
        // Unsupported version - reject.
        let ack = codec.encode_frame(Command::MuxInitAck, &[version, 1])?;
        stream.write_all(&ack).await?;
        return Ok(None);
    }

    // Второй байт MuxInit это флаги клиента (LLD-27); старый клиент шлёт один
    // байт, отсутствие = пустые флаги. В ack уходит пересечение с нашими,
    // старый клиент лишний третий байт игнорирует.
    let peer_flags = init_frame.payload.get(1).copied().unwrap_or(0);
    let caps = MuxCaps::from_flags(peer_flags & MuxCaps::LOCAL.to_flags());

    // Accept.
    let ack =
        codec.encode_frame(Command::MuxInitAck, &[MUX_PROTOCOL_VERSION, 0, caps.to_flags()])?;
    stream.write_all(&ack).await?;
    Ok(Some(caps))
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
    let window = mux.new_send_window();

    // Register before sending Connect so we don't miss ConnectAck.
    // Таймаут+WARN на взятие async-Mutex `streams`: если открытие вешается ЗДЕСЬ
    // (дедлок на этом локе), лог назовёт точку (XR-086, диагностика клиентского
    // зависания «open timed out, 0 пакетов на сервер»).
    match tokio::time::timeout(OPEN_STEP_TIMEOUT, mux.streams.lock()).await {
        Ok(mut streams) => {
            streams.insert(
                stream_id,
                StreamEntry {
                    tx: data_tx,
                    got_data: false,
                    domain: matches!(target, TargetAddr::Domain(..)),
                    window: window.clone(),
                },
            );
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
            window,
            recv_credit: mux.new_recv_credit(stream_id),
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
    streams: Option<Arc<Mutex<HashMap<u32, StreamEntry>>>>,
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
        // Обе стороны новые: хендшейк проходит и согласовывает окно (LLD-27).
        assert_eq!(client_result.unwrap().unwrap(), Some(MuxCaps { window: true }));
        assert_eq!(server_result.unwrap().unwrap(), Some(MuxCaps { window: true }));
    }

    #[tokio::test]
    async fn test_mux_stream_data_roundtrip() {
        let (client_io, server_io) = duplex(65536);
        let codec = test_codec();

        // Create multiplexers (skip handshake for unit test).
        let client_mux = Multiplexer::new_client(client_io, codec.clone(), MuxCaps::LOCAL);
        let server_mux = Multiplexer::new_server(server_io, codec.clone(), MuxCaps::LOCAL);

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

    /// XR-103 / LLD-23 §3.4: обе стороны mux открывают стримы, id не сталкиваются.
    /// Инициатор соединения (client) берёт нечётные id, акцептор (server) чётные,
    /// поэтому реверс-стрим relay->агент не конфликтует с прямыми стримами. Тут
    /// клиент открывает прямой стрим, сервер реверс-стрим; каждый принимающий шлёт
    /// ConnectAck, чтобы открытие вернулось.
    #[tokio::test]
    async fn test_mux_stream_id_parity() {
        let (client_io, server_io) = duplex(65536);
        let codec = test_codec();
        let client_mux = Multiplexer::new_client(client_io, codec.clone(), MuxCaps::LOCAL);
        let server_mux = Multiplexer::new_server(server_io, codec.clone(), MuxCaps::LOCAL);

        let s = server_mux.clone();
        let server_accept = tokio::spawn(async move {
            let mut rx = s.take_new_stream_rx().await.unwrap();
            let ns = rx.recv().await.unwrap();
            let _stream = s.register_stream(ns.stream_id).await;
            s.send_frame(ns.stream_id, Command::ConnectAck, vec![0]).await.unwrap();
            ns.stream_id
        });
        let c = client_mux.clone();
        let client_accept = tokio::spawn(async move {
            let mut rx = c.take_new_stream_rx().await.unwrap();
            let ns = rx.recv().await.unwrap();
            let _stream = c.register_stream(ns.stream_id).await;
            c.send_frame(ns.stream_id, Command::ConnectAck, vec![0]).await.unwrap();
            ns.stream_id
        });

        let target = TargetAddr::Domain("x".into(), 1);
        let fwd = mux_open_stream(&client_mux, &target).await.unwrap();
        let rev = mux_open_stream(&server_mux, &target).await.unwrap();

        let (fwd_id, rev_id) = (fwd.stream_id(), rev.stream_id());
        assert_eq!(fwd_id % 2, 1, "инициатор (client) открывает нечётные id");
        assert_eq!(rev_id % 2, 0, "акцептор (server) открывает чётные id");
        assert_ne!(fwd_id, rev_id);

        assert_eq!(server_accept.await.unwrap(), fwd_id);
        assert_eq!(client_accept.await.unwrap(), rev_id);
    }

    /// XR-103 / LLD-23 §3.4: `MuxStream::into_io()` даёт AsyncRead+AsyncWrite,
    /// поверх которого едет hyper (на агенте) и слепой сплайс (на relay). Тут
    /// сервер эхом отражает байты через свой io-адаптер, клиент шлёт и читает.
    #[tokio::test]
    async fn test_mux_stream_io_roundtrip() {
        let (client_io, server_io) = duplex(65536);
        let codec = test_codec();
        let client_mux = Multiplexer::new_client(client_io, codec.clone(), MuxCaps::LOCAL);
        let server_mux = Multiplexer::new_server(server_io, codec.clone(), MuxCaps::LOCAL);

        let s = server_mux.clone();
        let server_task = tokio::spawn(async move {
            let mut rx = s.take_new_stream_rx().await.unwrap();
            let ns = rx.recv().await.unwrap();
            let stream = s.register_stream(ns.stream_id).await;
            s.send_frame(ns.stream_id, Command::ConnectAck, vec![0]).await.unwrap();
            let mut io = stream.into_io();
            let mut buf = vec![0u8; 64];
            let n = io.read(&mut buf).await.unwrap();
            io.write_all(&buf[..n]).await.unwrap();
            io.flush().await.unwrap();
            io.shutdown().await.unwrap();
        });

        let target = TargetAddr::Domain("x".into(), 1);
        let stream = mux_open_stream(&client_mux, &target).await.unwrap();
        let mut io = stream.into_io();
        io.write_all(b"ping over io").await.unwrap();
        io.flush().await.unwrap();
        let mut got = vec![0u8; 64];
        let n = io.read(&mut got).await.unwrap();
        assert_eq!(&got[..n], b"ping over io");
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
        let mux = Multiplexer::new_client(client_io, codec, MuxCaps::LOCAL);

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
        let mux = Multiplexer::new_client(client_io, codec, MuxCaps::LOCAL);
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
        let _client = Multiplexer::new_client(client_io, codec.clone(), MuxCaps::LOCAL);

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

    /// XR-094: Close с причиной от сервера (relay упал на resolve/connect)
    /// должен записываться в `RelayHealth`, а дубль Close по уже снятому
    /// стриму не должен удваивать счёт.
    #[tokio::test]
    async fn test_close_reason_records_relay_failure() {
        let (client_io, server_io) = duplex(65536);
        let codec = test_codec();
        let health = Arc::new(RelayHealth::new());
        let client_mux =
            Multiplexer::new_client_tracked(client_io, codec.clone(), health.clone(), MuxCaps::LOCAL);
        let server_mux = Multiplexer::new_server(server_io, codec.clone(), MuxCaps::LOCAL);

        // Сервер: принять Connect, ответить ConnectAck, затем Close с причиной
        // resolve-сбоя (ровно как mux_handler при мёртвом DNS), и Close-дубль.
        let server_task = tokio::spawn(async move {
            let mut rx = server_mux.take_new_stream_rx().await.unwrap();
            let ns = rx.recv().await.unwrap();
            server_mux
                .send_frame(ns.stream_id, Command::ConnectAck, vec![0])
                .await
                .unwrap();
            server_mux
                .send_frame(
                    ns.stream_id,
                    Command::Close,
                    vec![crate::protocol::CLOSE_REASON_RESOLVE_FAIL],
                )
                .await
                .unwrap();
            server_mux
                .send_frame(
                    ns.stream_id,
                    Command::Close,
                    vec![crate::protocol::CLOSE_REASON_RESOLVE_FAIL],
                )
                .await
                .unwrap();
        });

        let target = TargetAddr::Domain("dead-dns.example".to_string(), 443);
        let mut stream = mux_open_stream(&client_mux, &target).await.unwrap();
        // Стрим закрывается без единого байта данных.
        assert!(stream.recv().await.is_none());
        server_task.await.unwrap();

        // Дать reader'у дожевать Close-дубль.
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            if health.snapshot().2 >= 1 {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (domain_ok, total_ok, resolve_fail, connect_fail) = health.snapshot();
        assert_eq!(resolve_fail, 1, "resolve-сбой считается один раз, дубль Close не удваивает");
        assert_eq!((domain_ok, total_ok, connect_fail), (0, 0, 0));
    }

    /// XR-094: первый Data-кадр стрима засчитывает relay-успех ровно один раз
    /// (доменный таргет идёт и в доменный счётчик), ConnectAck успехом не
    /// считается: сервер шлёт его до resolve.
    #[tokio::test]
    async fn test_first_data_counts_relay_success_once() {
        let (client_io, server_io) = duplex(65536);
        let codec = test_codec();
        let health = Arc::new(RelayHealth::new());
        let client_mux =
            Multiplexer::new_client_tracked(client_io, codec.clone(), health.clone(), MuxCaps::LOCAL);
        let server_mux = Multiplexer::new_server(server_io, codec.clone(), MuxCaps::LOCAL);

        let server_task = tokio::spawn(async move {
            let mut rx = server_mux.take_new_stream_rx().await.unwrap();
            let ns = rx.recv().await.unwrap();
            let stream = server_mux.register_stream(ns.stream_id).await;
            server_mux
                .send_frame(ns.stream_id, Command::ConnectAck, vec![0])
                .await
                .unwrap();
            stream.send(b"first").await.unwrap();
            stream.send(b"second").await.unwrap();
        });

        let target = TargetAddr::Domain("alive.example".to_string(), 443);
        let mut stream = mux_open_stream(&client_mux, &target).await.unwrap();
        assert_eq!(stream.recv().await.unwrap(), b"first");
        assert_eq!(stream.recv().await.unwrap(), b"second");
        server_task.await.unwrap();

        let (domain_ok, total_ok, resolve_fail, connect_fail) = health.snapshot();
        assert_eq!(
            (domain_ok, total_ok),
            (1, 1),
            "успех считается по первому Data-кадру и только один раз"
        );
        assert_eq!((resolve_fail, connect_fail), (0, 0));
    }

    /// Порог деградации: меньше RELAY_FAIL_MIN сбоев это фон (одиночные
    /// connect timeout по DoD XR-094 не переключают), на пороге деградация.
    #[tokio::test]
    async fn test_relay_health_min_failures() {
        let health = RelayHealth::new();
        for _ in 0..RELAY_FAIL_MIN - 1 {
            health.record_connect_fail();
        }
        assert!(!health.degraded(), "фон одиночных сбоев не деградация");
        health.record_connect_fail();
        assert!(health.degraded(), "порог сбоев без успехов = деградация");
    }

    /// Сбои на фоне преобладающих успехов не объявляют деградацию, а мёртвый
    /// DNS не маскируется живыми IP-стримами: resolve-сбои сравниваются
    /// только с доменными успехами.
    #[tokio::test]
    async fn test_relay_health_success_majority_wins() {
        let health = RelayHealth::new();
        for _ in 0..RELAY_FAIL_MIN {
            health.record_resolve_fail();
        }
        for _ in 0..RELAY_FAIL_MIN + 1 {
            health.record_success(true);
        }
        assert!(
            !health.degraded(),
            "resolve-сбои при большинстве доменных успехов это фон"
        );

        let dns_dead = RelayHealth::new();
        for _ in 0..RELAY_FAIL_MIN {
            dns_dead.record_resolve_fail();
        }
        for _ in 0..100 {
            dns_dead.record_success(false); // IP-таргеты (CIDR-роутинг) живы
        }
        assert!(
            dns_dead.degraded(),
            "живые IP-стримы не должны маскировать полностью мёртвый DNS"
        );
    }

    /// Окно исходов скользит: старые сбои протухают, деградация не висит
    /// вечно после единичного всплеска.
    #[tokio::test(start_paused = true)]
    async fn test_relay_health_window_expires() {
        let health = RelayHealth::new();
        for _ in 0..RELAY_FAIL_MIN {
            health.record_resolve_fail();
        }
        assert!(health.degraded());
        tokio::time::advance(RELAY_WINDOW * 2 + Duration::from_secs(1)).await;
        assert!(!health.degraded(), "сбои старше окна не считаются");
    }

    /// Эмуляция СТАРОГО сервера (LLD-27): на MuxInit с байтом флагов он
    /// отвечает двухбайтовым ack [версия, 0]. Новый клиент обязан пройти
    /// хендшейк и выключить окно, а не счесть сервер несовместимым.
    #[tokio::test]
    async fn test_handshake_old_server_disables_window() {
        let (mut client_io, mut server_io) = duplex(1024);
        let codec = test_codec();

        let server_codec = codec.clone();
        let old_server = tokio::spawn(async move {
            let mut buf = vec![0u8; 256];
            let mut filled = 0;
            let init = loop {
                let n = server_io.read(&mut buf[filled..]).await.unwrap();
                filled += n;
                if let Some((f, _)) = server_codec.decode_frame(&buf[..filled]).unwrap() {
                    break f;
                }
            };
            assert_eq!(init.command, Command::MuxInit);
            // Старый сервер читает только версию, флаги игнорирует.
            assert_eq!(init.payload.first(), Some(&MUX_PROTOCOL_VERSION));
            let ack = server_codec
                .encode_frame(Command::MuxInitAck, &[MUX_PROTOCOL_VERSION, 0])
                .unwrap();
            server_io.write_all(&ack).await.unwrap();
        });

        let caps = mux_handshake_client(&mut client_io, &codec).await.unwrap();
        assert_eq!(
            caps,
            Some(MuxCaps { window: false }),
            "старый сервер без байта флагов = mux есть, окна нет"
        );
        old_server.await.unwrap();
    }

    /// Эмуляция СТАРОГО клиента (LLD-27): MuxInit из одного байта версии.
    /// Новый сервер принимает, окно выключено, ack совместим со старой
    /// проверкой `payload[1] == 0`.
    #[tokio::test]
    async fn test_handshake_old_client_disables_window() {
        let (mut server_io, mut client_io) = duplex(1024);
        let codec = test_codec();

        let init = Frame { command: Command::MuxInit, payload: vec![MUX_PROTOCOL_VERSION] };
        let caps = mux_handshake_server(&mut server_io, &codec, &init).await.unwrap();
        assert_eq!(caps, Some(MuxCaps { window: false }));

        // Ack глазами старого клиента: он смотрит только payload[0..2].
        let mut buf = vec![0u8; 256];
        let mut filled = 0;
        let ack = loop {
            let n = client_io.read(&mut buf[filled..]).await.unwrap();
            filled += n;
            if let Some((f, _)) = codec.decode_frame(&buf[..filled]).unwrap() {
                break f;
            }
        };
        assert_eq!(ack.command, Command::MuxInitAck);
        assert!(ack.payload.len() >= 2 && ack.payload[1] == 0);
    }

    /// Тотал и размер кадра для тестов окна: кадров больше ёмкости per-stream
    /// канала (1024), чтобы легаси-режим гарантированно переполнялся, а размер
    /// не делит окно нацело, чтобы отправка с окном прошла и путь частичного
    /// кредита (кадр режется по остатку окна).
    const FLOW_TOTAL: usize = 4 * 1024 * 1024;
    const FLOW_CHUNK: usize = 3000;

    /// Быстрый отправитель льёт в стрим, потребитель начинает вычитывать с
    /// опозданием. Отправка идёт `send` серверного хэндла, вычитывание recv
    /// клиента; байты проверяются по позиционному паттерну.
    async fn run_slow_consumer(caps: MuxCaps) -> usize {
        let (client_io, server_io) = duplex(65536);
        let codec = test_codec();
        let client_mux = Multiplexer::new_client(client_io, codec.clone(), caps);
        let server_mux = Multiplexer::new_server(server_io, codec.clone(), caps);

        let s = server_mux.clone();
        tokio::spawn(async move {
            let mut rx = s.take_new_stream_rx().await.unwrap();
            let ns = rx.recv().await.unwrap();
            let stream = s.register_stream(ns.stream_id).await;
            s.send_frame(ns.stream_id, Command::ConnectAck, vec![0]).await.unwrap();
            let mut sent = 0usize;
            let mut chunk = vec![0u8; FLOW_CHUNK];
            while sent < FLOW_TOTAL {
                let n = FLOW_CHUNK.min(FLOW_TOTAL - sent);
                for (i, b) in chunk[..n].iter_mut().enumerate() {
                    *b = ((sent + i) % 199) as u8;
                }
                // С окном отправитель здесь засыпает, пока потребитель не
                // вернёт кредит; без окна флудит весь тотал сразу.
                if stream.send(&chunk[..n]).await.is_err() {
                    break;
                }
                sent += n;
            }
        });

        let target = TargetAddr::Domain("bulk.example".to_string(), 443);
        let mut stream = mux_open_stream(&client_mux, &target).await.unwrap();

        // Потребитель отстаёт: к этому моменту отправитель без окна уже
        // переполнил per-stream канал (1398 кадров > 1024), а с окном спит,
        // отправив ровно окно.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let mut received = 0usize;
        while let Some(data) = stream.recv().await {
            for (i, b) in data.iter().enumerate() {
                assert_eq!(*b, ((received + i) % 199) as u8, "байты не по порядку");
            }
            received += data.len();
            if received >= FLOW_TOTAL {
                break;
            }
        }
        received
    }

    /// Регрессия XR-115: с оконным flow control медленный потребитель получает
    /// тело целиком, отправитель упирается в окно вместо переполнения канала.
    #[tokio::test]
    async fn test_window_slow_consumer_gets_all_bytes() {
        let received = run_slow_consumer(MuxCaps { window: true }).await;
        assert_eq!(
            received, FLOW_TOTAL,
            "с окном не должно теряться ни байта (стрим убит вместо backpressure?)"
        );
    }

    /// Легаси-пара (окно не согласовано) сохраняет старое поведение: reader
    /// убивает переполнившийся стрим, тело обрезается. Это же демонстрирует,
    /// что test_window_slow_consumer_gets_all_bytes падал бы на старом коде.
    #[tokio::test]
    async fn test_no_window_slow_consumer_stream_killed() {
        let received = run_slow_consumer(MuxCaps { window: false }).await;
        assert!(
            received < FLOW_TOTAL,
            "без окна медленный потребитель обязан потерять хвост (канал 1024 кадра), получено {}",
            received
        );
    }

    /// LLD-27: Close пира будит отправителя, заснувшего на исчерпанном окне,
    /// ошибкой, а не вечным зависанием.
    #[tokio::test]
    async fn test_close_wakes_sender_blocked_on_window() {
        let (client_io, server_io) = duplex(65536);
        let codec = test_codec();
        let caps = MuxCaps { window: true };
        let client_mux = Multiplexer::new_client(client_io, codec.clone(), caps);
        let server_mux = Multiplexer::new_server(server_io, codec.clone(), caps);

        let s = server_mux.clone();
        let sender = tokio::spawn(async move {
            let mut rx = s.take_new_stream_rx().await.unwrap();
            let ns = rx.recv().await.unwrap();
            let stream = s.register_stream(ns.stream_id).await;
            s.send_frame(ns.stream_id, Command::ConnectAck, vec![0]).await.unwrap();
            // Выесть окно целиком, следующий send засыпает на кредите.
            let fill = vec![0u8; STREAM_RECV_WINDOW as usize];
            stream.send(&fill).await.unwrap();
            stream.send(b"blocked").await
        });

        let target = TargetAddr::Domain("x".into(), 1);
        let stream = mux_open_stream(&client_mux, &target).await.unwrap();
        // Потребитель не читает и закрывает стрим: Close уезжает на сервер,
        // запись стрима снимается, окно закрывается.
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(stream);

        let r = tokio::time::timeout(Duration::from_secs(5), sender)
            .await
            .expect("заснувший на окне отправитель обязан проснуться по Close")
            .unwrap();
        assert!(r.is_err(), "send после Close пира должен вернуть ошибку");
    }

    /// LLD-27: io-адаптер гоняет объём больше окна через poll_write/poll_read
    /// (медленный путь с ожиданием кредита в write_fut), байты целы.
    #[tokio::test]
    async fn test_mux_stream_io_bulk_over_window() {
        let (client_io, server_io) = duplex(65536);
        let codec = test_codec();
        let caps = MuxCaps { window: true };
        let client_mux = Multiplexer::new_client(client_io, codec.clone(), caps);
        let server_mux = Multiplexer::new_server(server_io, codec.clone(), caps);

        const TOTAL: usize = 3 * 1024 * 1024;

        let s = server_mux.clone();
        let reader = tokio::spawn(async move {
            let mut rx = s.take_new_stream_rx().await.unwrap();
            let ns = rx.recv().await.unwrap();
            let stream = s.register_stream(ns.stream_id).await;
            s.send_frame(ns.stream_id, Command::ConnectAck, vec![0]).await.unwrap();
            let mut io = stream.into_io();
            // Дать писателю упереться в окно, прежде чем начать читать.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let mut got = Vec::with_capacity(TOTAL);
            io.read_to_end(&mut got).await.unwrap();
            got
        });

        let target = TargetAddr::Domain("io-bulk".into(), 1);
        let stream = mux_open_stream(&client_mux, &target).await.unwrap();
        let mut io = stream.into_io();
        let data: Vec<u8> = (0..TOTAL).map(|i| (i % 199) as u8).collect();
        io.write_all(&data).await.unwrap();
        io.shutdown().await.unwrap();

        let got = tokio::time::timeout(Duration::from_secs(10), reader)
            .await
            .expect("объём больше окна должен пройти через io-адаптер")
            .unwrap();
        assert_eq!(got.len(), TOTAL);
        assert_eq!(got, data, "байты через io-адаптер обязаны прийти целыми");
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
        let shutdown = Arc::new(Notify::new());
        let writer = tokio::spawn(async move {
            writer_task(w, codec_w, ctrl_rx, data_rx, shutdown).await.unwrap();
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
