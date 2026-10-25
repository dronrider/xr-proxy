//! Локальный DNS-форвардер клиента (XR-285).
//!
//! dnsmasq роутера спрашивает форвардер на петле, а тот уносит запрос в
//! туннель и говорит с публичным резолвером по DoT. Из LAN не уходит ни
//! одного открытого DNS-пакета: провайдер видит соединение с адресом нашего
//! же VPS и подменить ответ ему нечем. Раньше dnsmasq ходил на Quad9 голым
//! UDP:53, и поддельный NXDOMAIN клал сайт раньше, чем перехвату было что
//! перехватывать: без адреса LAN-клиент не открывает TCP.
//!
//! Транспорт до апстрима задаётся вызывающим: в бою это TLS поверх
//! туннельного стрима, в тестах голый TCP до заглушки резолвера.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot};

use xr_proto::config::DnsClientConfig;

/// Потолок принимаемой датаграммы: больше EDNS0 не просит.
const MAX_MSG: usize = 4096;

/// Классический потолок UDP-ответа без EDNS0 (RFC 1035).
const MIN_UDP_PAYLOAD: usize = 512;

/// Как часто повторять в журнале жалобу на недоступный апстрим. Первый отказ
/// пишется сразу, дальше редко: dnsmasq переспрашивает, и без этого порога
/// лежащий туннель залил бы logread роутера.
const UPSTREAM_ERROR_QUIET: Duration = Duration::from_secs(60);

/// Пауза между попытками поднять апстрим. Без неё каждый запрос из очереди
/// ждёт свой полный таймаут подключения, очередь копится, и спрашивающий
/// перестаёт получать даже отказ.
const UPSTREAM_RETRY_PAUSE: Duration = Duration::from_secs(1);
const MAX_RETRIES: usize = 256;

struct Job {
    query: Vec<u8>,
    reply: oneshot::Sender<Vec<u8>>,
    timed_out: Arc<AtomicBool>,
    deadline: tokio::time::Instant,
    attempt: u8,
}

struct Pending {
    job: Job,
    sent_at: tokio::time::Instant,
}

fn retryable_disconnect(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe
        | io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::NotConnected | io::ErrorKind::WriteZero)
}

fn queue_retry(retries: &mut VecDeque<Job>, mut job: Job) -> bool {
    if job.attempt != 0 || retries.len() >= MAX_RETRIES
        || job.query.len() < 12 || job.query[2] & 0xf8 != 0
        || job.reply.is_closed() || job.timed_out.load(Ordering::SeqCst)
        || tokio::time::Instant::now() >= job.deadline
    {
        return false;
    }
    job.attempt = 1;
    retries.push_back(job);
    true
}

async fn next_job(jobs: &mut mpsc::Receiver<Job>, retries: &mut VecDeque<Job>,
    prefer_retry: bool) -> Option<Job>
{
    if !prefer_retry {
        if let Ok(job) = jobs.try_recv() { return Some(job); }
    }
    if let Some(job) = retries.pop_front() { return Some(job); }
    jobs.recv().await
}

fn retry_pending<W>(conn: Option<Conn<W>>, retries: &mut VecDeque<Job>, retryable: bool,
    next_attempt: &mut Option<tokio::time::Instant>)
{
    let Some(conn) = conn else { return; };
    let mut queued = 0;
    let mut exhausted = false;
    for pending in conn.pending.into_values() {
        exhausted |= pending.job.attempt != 0 && !pending.job.reply.is_closed()
            && !pending.job.timed_out.load(Ordering::SeqCst)
            && tokio::time::Instant::now() < pending.job.deadline;
        if retryable && queue_retry(retries, pending.job) { queued += 1; }
    }
    if queued > 0 {
        tracing::info!("dns: retrying {queued} outstanding queries after upstream disconnect");
    } else if exhausted {
        *next_attempt = Some(tokio::time::Instant::now() + UPSTREAM_RETRY_PAUSE);
    }
}

struct Conn<W> {
    writer: W,
    pending: HashMap<u16, Pending>,
    next_id: u16,
}

impl<W> Conn<W> {
    /// Свободный идентификатор для запроса в апстрим. Занятые пропускаются:
    /// столкновение увело бы ответ не тому спрашивающему.
    fn next_free_id(&mut self) -> u16 {
        for _ in 0..=u16::MAX {
            self.next_id = self.next_id.wrapping_add(1);
            if !self.pending.contains_key(&self.next_id) {
                return self.next_id;
            }
        }
        self.next_id
    }
}

/// Ручка форвардера: кладёт запрос в очередь единственного разговора с
/// апстримом и ждёт свой ответ.
#[derive(Clone)]
pub struct Forwarder {
    jobs: mpsc::Sender<Job>,
    timeout: Duration,
}

impl Forwarder {
    /// Ответ апстрима либо SERVFAIL. Молчать нельзя: молчание спрашивающий
    /// ждёт своим таймаутом, а SERVFAIL видит сразу. Отката на провайдерский
    /// резолвер тут нет и быть не может, ради этого всё и затевалось.
    pub async fn resolve(&self, query: &[u8]) -> Vec<u8> {
        let (tx, mut rx) = oneshot::channel();
        let timed_out = Arc::new(AtomicBool::new(false));
        let deadline = tokio::time::Instant::now() + self.timeout;
        let job = Job {
            query: query.to_vec(),
            reply: tx,
            timed_out: timed_out.clone(),
            deadline,
            attempt: 0,
        };
        // Очередь забита значит апстрим не отвечает, и ждать в ней места это
        // то самое молчание, вместо которого спрашивающему нужен отказ.
        if self.jobs.try_send(job).is_err() {
            return servfail(query);
        }
        tokio::select! {
            result = &mut rx => result.unwrap_or_else(|_| servfail(query)),
            _ = tokio::time::sleep_until(deadline) => {
                // Метка ставится до закрытия rx. Простая отмена resolve
                // метку не ставит и живое соединение не перезапускает.
                timed_out.store(true, Ordering::SeqCst);
                servfail(query)
            }
        }
    }
}

/// Поднять форвардер на `cfg.listen` и держать его до ошибки листенера.
/// `connect` открывает соединение до апстрима и зовётся заново, когда
/// прежнее порвалось.
pub async fn run_forwarder<S, F, Fut>(cfg: &DnsClientConfig, connect: F) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = io::Result<S>> + Send + 'static,
{
    let listen: SocketAddr = cfg
        .listen
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("dns listen '{}': {e}", cfg.listen)))?;
    // TCP слушаем наравне с UDP: усечённый ответ спрашивающий переспрашивает
    // именно по нему, и без второго листенера длинные записи не доедут.
    let udp = UdpSocket::bind(listen).await?;
    let tcp = TcpListener::bind(listen).await?;

    tracing::info!(
        "DNS forwarder listening on {} (DoT through the tunnel, upstreams {})",
        listen,
        cfg.upstreams.join(", ")
    );

    serve_forwarder(udp, tcp, Duration::from_millis(cfg.timeout_ms), connect).await
}

/// Обслуживание уже занятых листенеров. Отдельно от [`run_forwarder`], чтобы
/// стенд занимал порт сам и не гонялся за ним с соседними тестами.
async fn serve_forwarder<S, F, Fut>(
    udp: UdpSocket,
    tcp: TcpListener,
    timeout: Duration,
    connect: F,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = io::Result<S>> + Send + 'static,
{
    let (jobs_tx, jobs_rx) = mpsc::channel(256);
    tokio::spawn(upstream_loop(jobs_rx, connect, timeout));
    let forwarder = Forwarder { jobs: jobs_tx, timeout };

    tokio::select! {
        r = serve_udp(udp, forwarder.clone()) => r,
        r = serve_tcp(tcp, forwarder) => r,
    }
}

/// Что случилось раньше прочего. Событие вынуто из `select!` нарочно: тела
/// веток трогают то же состояние, что фьючи в их условиях, и без промежуточного
/// значения заимствования не сходятся.
enum Event {
    Job(Job),
    /// Ответ апстрима, либо `None`, когда его соединение кончилось.
    Answer(Option<io::Result<Vec<u8>>>),
    Sweep,
    Stop,
}

async fn recv_answer(answers: &mut Option<mpsc::Receiver<io::Result<Vec<u8>>>>) -> Option<io::Result<Vec<u8>>> {
    match answers {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Единственный разговор с апстримом: одно соединение на все запросы (RFC 7858
/// ровно про это), свои идентификаторы на проводе и разбор ответов обратно
/// спрашивающим.
async fn upstream_loop<S, F, Fut>(mut jobs: mpsc::Receiver<Job>, connect: F, timeout: Duration)
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    F: Fn() -> Fut,
    Fut: Future<Output = io::Result<S>>,
{
    let mut conn: Option<Conn<tokio::io::WriteHalf<S>>> = None;
    let mut answers: Option<mpsc::Receiver<io::Result<Vec<u8>>>> = None;
    let mut last_complaint: Option<Instant> = None;
    let mut next_attempt: Option<tokio::time::Instant> = None;
    let mut retries = VecDeque::new();
    let mut prefer_retry = true;
    let mut sweep = tokio::time::interval(Duration::from_secs(1));

    loop {
        let event = tokio::select! {
            job = next_job(&mut jobs, &mut retries, prefer_retry) => match job {
                Some(job) => Event::Job(job),
                None => Event::Stop,
            },
            answer = recv_answer(&mut answers) => Event::Answer(answer),
            _ = sweep.tick() => Event::Sweep,
        };

        if conn.as_ref().is_some_and(|c| c.pending.values().any(|p| {
            p.job.timed_out.load(Ordering::SeqCst)
                || (!p.job.reply.is_closed() && p.sent_at.elapsed() >= timeout)
        })) {
            complain(&mut last_complaint, "upstream response timed out; reconnecting".into());
            conn = None;
            answers = None;
            next_attempt = None;
        }

        match event {
            Event::Stop => break,
            Event::Sweep => {
                // Запись спрашивающего, который уже не ждёт (сработал таймаут
                // и ушёл SERVFAIL), держать незачем: без чистки таблица растёт
                // на каждом молчании апстрима.
                if let Some(c) = conn.as_mut() {
                    c.pending.retain(|_, p| !p.job.reply.is_closed() || p.job.timed_out.load(Ordering::SeqCst));
                }
            }
            Event::Answer(Some(Ok(msg))) => {
                if msg.len() < 12 {
                    continue;
                }
                let wire_id = u16::from_be_bytes([msg[0], msg[1]]);
                if let Some(c) = conn.as_mut() {
                    if let Some(p) = c.pending.remove(&wire_id) {
                        let mut msg = msg;
                        msg[0..2].copy_from_slice(&p.job.query[0..2]);
                        let _ = p.job.reply.send(msg);
                    }
                }
            }
            Event::Answer(failure) => {
                let retryable = matches!(failure, Some(Err(ref error)) if retryable_disconnect(error));
                tracing::warn!("dns: upstream closed the connection, will reconnect on next query");
                retry_pending(conn.take(), &mut retries, retryable, &mut next_attempt);
                answers = None;
            }
            Event::Job(job) => {
                prefer_retry = !prefer_retry;
                if job.query.len() < 12 {
                    continue;
                }
                // Спрашивающий уже ушёл по своему таймауту: тратить на него
                // попытку подключения значит задерживать тех, кто ещё ждёт.
                if job.reply.is_closed() || tokio::time::Instant::now() >= job.deadline {
                    continue;
                }
                if conn.is_none() {
                    if next_attempt.is_some_and(|t| tokio::time::Instant::now() < t) {
                        continue;
                    }
                    match tokio::time::timeout_at(job.deadline, connect()).await {
                        Ok(Ok(stream)) => {
                            let (reader, writer) = tokio::io::split(stream);
                            let (atx, arx) = mpsc::channel(64);
                            tokio::spawn(read_answers(reader, atx));
                            answers = Some(arx);
                            conn = Some(Conn { writer, pending: HashMap::new(), next_id: 0 });
                            next_attempt = None;
                            if last_complaint.take().is_some() {
                                tracing::info!("dns: upstream is reachable again");
                            }
                        }
                        Ok(Err(e)) => {
                            complain(&mut last_complaint, format!("{e}"));
                            next_attempt = Some(tokio::time::Instant::now() + UPSTREAM_RETRY_PAUSE);
                            continue;
                        }
                        Err(_) => {
                            complain(&mut last_complaint, "connect timed out".to_string());
                            next_attempt = Some(tokio::time::Instant::now() + UPSTREAM_RETRY_PAUSE);
                            continue;
                        }
                    }
                }

                if job.reply.is_closed() || tokio::time::Instant::now() >= job.deadline {
                    continue;
                }
                let c = conn.as_mut().expect("connection built above");
                let wire_id = c.next_free_id();
                let mut msg = job.query.clone();
                msg[0..2].copy_from_slice(&wire_id.to_be_bytes());

                let mut framed = Vec::with_capacity(msg.len() + 2);
                framed.extend_from_slice(&(msg.len() as u16).to_be_bytes());
                framed.extend_from_slice(&msg);
                match tokio::time::timeout_at(job.deadline, c.writer.write_all(&framed)).await {
                    Ok(Ok(())) => {}
                    failure => {
                        let retryable = matches!(failure, Ok(Err(ref error)) if retryable_disconnect(error));
                        tracing::warn!("dns: write to upstream failed or timed out: {failure:?}");
                        // При частичной записи QUERY допустимо повторить один
                        // раз: opcode с побочным эффектом сюда не попадает.
                        c.pending.insert(wire_id, Pending { job, sent_at: tokio::time::Instant::now() });
                        retry_pending(conn.take(), &mut retries, retryable, &mut next_attempt);
                        answers = None;
                        continue;
                    }
                }
                c.pending.insert(wire_id, Pending { job, sent_at: tokio::time::Instant::now() });
            }
        }
    }
}

/// Жалоба на недоступный апстрим: первая уходит в журнал сразу, следующие не
/// чаще, чем раз в [`UPSTREAM_ERROR_QUIET`]. Молчание тут недопустимо: снаружи
/// оно неотличимо от работающего резолва.
fn complain(last: &mut Option<Instant>, reason: String) {
    let now = Instant::now();
    let quiet = last.is_some_and(|t| now.duration_since(t) < UPSTREAM_ERROR_QUIET);
    if !quiet {
        tracing::error!(
            "dns: upstream unreachable ({reason}); queries answer SERVFAIL, \
             no fallback to the provider resolver"
        );
        *last = Some(now);
    }
}

async fn read_answers<R: AsyncRead + Unpin>(mut reader: R, tx: mpsc::Sender<io::Result<Vec<u8>>>) {
    loop {
        let read = async {
            let mut len = [0u8; 2];
            if let Err(error) = reader.read_exact(&mut len).await {
                tracing::warn!("dns: upstream read length failed ({:?}): {error}", error.kind());
                return Err(error);
            }
            let n = u16::from_be_bytes(len) as usize;
            if n == 0 {
                tracing::warn!("dns: upstream sent a zero-length DNS frame");
                return Err(io::Error::new(io::ErrorKind::InvalidData, "zero-length DNS frame"));
            }
            let mut msg = vec![0u8; n];
            if let Err(error) = reader.read_exact(&mut msg).await {
                tracing::warn!("dns: upstream read payload failed ({:?}): {error}", error.kind());
                return Err(error);
            }
            Ok(msg)
        };
        let msg = tokio::select! {
            // При замене апстрима reader должен отпустить старый поток,
            // даже если от него больше никогда не придёт ни байта.
            _ = tx.closed() => break,
            msg = read => msg,
        };
        let disconnected = msg.is_err();
        if tx.send(msg).await.is_err() || disconnected { break; }
    }
}

async fn serve_udp(sock: UdpSocket, forwarder: Forwarder) -> io::Result<()> {
    let sock = Arc::new(sock);
    let mut buf = vec![0u8; MAX_MSG];
    loop {
        let (n, from) = sock.recv_from(&mut buf).await?;
        let query = buf[..n].to_vec();
        let sock = sock.clone();
        let forwarder = forwarder.clone();
        tokio::spawn(async move {
            let answer = forwarder.resolve(&query).await;
            if answer.is_empty() {
                return;
            }
            // Апстрим отвечает по TCP, где длина не ограничена, а спрашивающий
            // ждёт датаграмму своего размера. Длинный ответ уходит усечённым,
            // и спрашивающий переспрашивает по TCP.
            let limit = udp_payload_limit(&query);
            let answer = if answer.len() > limit { truncated(&answer) } else { answer };
            let _ = sock.send_to(&answer, from).await;
        });
    }
}

async fn serve_tcp(listener: TcpListener, forwarder: Forwarder) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let forwarder = forwarder.clone();
        tokio::spawn(async move {
            let _ = serve_tcp_conn(stream, forwarder).await;
        });
    }
}

async fn serve_tcp_conn(mut stream: TcpStream, forwarder: Forwarder) -> io::Result<()> {
    loop {
        let mut len = [0u8; 2];
        if stream.read_exact(&mut len).await.is_err() {
            return Ok(());
        }
        // Своего потолка тут нет: длина на проводе лежит в двух байтах, и
        // сужать её нельзя. Апстрим отвечает по TLS, где размер приёмного
        // буфера спрашивающего его не связывает, а DNSKEY и длинный TXT
        // законно не влезают в датаграмму.
        let n = u16::from_be_bytes(len) as usize;
        if n == 0 {
            return Ok(());
        }
        let mut query = vec![0u8; n];
        stream.read_exact(&mut query).await?;
        let answer = forwarder.resolve(&query).await;
        if answer.is_empty() {
            return Ok(());
        }
        stream.write_all(&(answer.len() as u16).to_be_bytes()).await?;
        stream.write_all(&answer).await?;
    }
}

/// Конец секции вопроса, либо `None`, если это не разбираемый запрос с ровно
/// одним вопросом. Сжатых имён в вопросе не бывает, указатель тут это мусор.
fn question_end(msg: &[u8]) -> Option<usize> {
    if msg.len() < 12 || u16::from_be_bytes([msg[4], msg[5]]) != 1 {
        return None;
    }
    let mut i = 12;
    loop {
        let label = *msg.get(i)? as usize;
        if label & 0xC0 != 0 {
            return None;
        }
        i += 1;
        if label == 0 {
            break;
        }
        i += label;
        if i >= msg.len() {
            return None;
        }
    }
    let end = i + 4; // QTYPE + QCLASS
    if end > msg.len() { None } else { Some(end) }
}

/// SERVFAIL на тот же вопрос. Пустой результат значит, что запрос не разобрать
/// и отвечать не на что: такой пакет молча отбрасывается.
fn servfail(query: &[u8]) -> Vec<u8> {
    let Some(end) = question_end(query) else {
        return Vec::new();
    };
    let mut out = query[..end].to_vec();
    out[2] |= 0x80; // QR: это ответ
    out[2] &= !0x02; // TC: усекать нечего
    out[3] = 0x80 | 2; // RA + RCODE=SERVFAIL
    out[6..12].fill(0); // ни ответных, ни авторитетных, ни дополнительных записей
    out
}

/// Тот же ответ, урезанный до вопроса с флагом TC: спрашивающий по этому флагу
/// переспрашивает по TCP.
fn truncated(answer: &[u8]) -> Vec<u8> {
    let Some(end) = question_end(answer) else {
        return answer.to_vec();
    };
    let mut out = answer[..end].to_vec();
    out[2] |= 0x82; // QR + TC
    out[6..12].fill(0);
    out
}

/// Сколько байт готов принять по UDP тот, кто прислал этот запрос: размер из
/// EDNS0-записи OPT, а без неё классические 512.
fn udp_payload_limit(query: &[u8]) -> usize {
    let Some(i) = question_end(query) else {
        return MIN_UDP_PAYLOAD;
    };
    let counts_after_question = [
        u16::from_be_bytes([query[6], query[7]]),   // ANCOUNT
        u16::from_be_bytes([query[8], query[9]]),   // NSCOUNT
    ];
    let arcount = u16::from_be_bytes([query[10], query[11]]);
    // OPT лежит в additional, а секции перед ним в запросе пустые. Непустые
    // значат нестандартный запрос, разбирать который тут нечем.
    if arcount == 0 || counts_after_question != [0, 0] || query.get(i) != Some(&0) {
        return MIN_UDP_PAYLOAD;
    }
    let (Some(rtype), Some(class)) = (query.get(i + 1..i + 3), query.get(i + 3..i + 5)) else {
        return MIN_UDP_PAYLOAD;
    };
    if u16::from_be_bytes([rtype[0], rtype[1]]) != 41 {
        return MIN_UDP_PAYLOAD;
    }
    // У OPT поле class это заявленный размер приёмного буфера.
    (u16::from_be_bytes([class[0], class[1]]) as usize).clamp(MIN_UDP_PAYLOAD, MAX_MSG)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Запрос A/IN с одним вопросом, при `edns` с записью OPT заявленного
    /// размера.
    fn query(id: u16, name: &str, edns: Option<u16>) -> Vec<u8> {
        let mut q = Vec::new();
        q.extend_from_slice(&id.to_be_bytes());
        q.extend_from_slice(&[0x01, 0x00]); // RD
        q.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
        q.extend_from_slice(&[0, 0, 0, 0]); // ANCOUNT, NSCOUNT
        q.extend_from_slice(&(u16::from(edns.is_some())).to_be_bytes()); // ARCOUNT
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&1u16.to_be_bytes()); // QTYPE = A
        q.extend_from_slice(&1u16.to_be_bytes()); // QCLASS = IN
        if let Some(size) = edns {
            q.push(0); // корневое имя записи OPT
            q.extend_from_slice(&41u16.to_be_bytes());
            q.extend_from_slice(&size.to_be_bytes()); // размер приёмного буфера
            q.extend_from_slice(&[0, 0, 0, 0]); // TTL
            q.extend_from_slice(&0u16.to_be_bytes()); // RDLENGTH
        }
        q
    }

    /// Настоящий ответ с одной записью A.
    fn answer_a(query: &[u8], ip: Ipv4Addr) -> Vec<u8> {
        let end = question_end(query).expect("тестовый запрос разбирается");
        let mut a = query[..end].to_vec();
        a[2] |= 0x80; // QR
        a[3] = 0x80; // RA, RCODE = NOERROR
        a[6..8].copy_from_slice(&1u16.to_be_bytes()); // ANCOUNT
        a[8..12].fill(0);
        a.extend_from_slice(&[0xC0, 0x0C]); // указатель на имя из вопроса
        a.extend_from_slice(&1u16.to_be_bytes()); // A
        a.extend_from_slice(&1u16.to_be_bytes()); // IN
        a.extend_from_slice(&60u32.to_be_bytes()); // TTL
        a.extend_from_slice(&4u16.to_be_bytes()); // RDLENGTH
        a.extend_from_slice(&ip.octets());
        a
    }

    fn rcode(msg: &[u8]) -> u8 {
        msg[3] & 0x0F
    }

    fn authoritative(msg: &[u8]) -> bool {
        msg[2] & 0x04 != 0
    }

    fn truncated_flag(msg: &[u8]) -> bool {
        msg[2] & 0x02 != 0
    }

    /// Адрес из первой записи ответа, собранного `answer_a`.
    fn first_a(msg: &[u8]) -> Option<Ipv4Addr> {
        let end = question_end(msg)?;
        let rdata = msg.get(end + 12..end + 16)?;
        Some(Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]))
    }

    /// Пара листенеров на одном порту, занятая сразу и до конца теста. Порт
    /// не освобождается между выбором и захватом: прежний стенд отпускал его
    /// и на полном прогоне ловил соседа, а форвардер молча не поднимался.
    async fn bind_local() -> (UdpSocket, TcpListener, SocketAddr) {
        for _ in 0..50 {
            let udp = UdpSocket::bind("127.0.0.1:0").await.expect("bind udp");
            let addr = udp.local_addr().unwrap();
            if let Ok(tcp) = TcpListener::bind(addr).await {
                return (udp, tcp, addr);
            }
        }
        panic!("не нашёл свободной пары листенеров под стенд");
    }

    /// Честный резолвер за туннелем: говорит DNS-over-TCP и на всё отвечает
    /// одним адресом. В бою на его месте стоит Quad9 за TLS.
    async fn spawn_upstream(
        ip: Ipv4Addr,
        answers_per_conn: usize,
    ) -> (SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let conns = Arc::new(AtomicUsize::new(0));
        let counter = conns.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    for _ in 0..answers_per_conn {
                        let mut len = [0u8; 2];
                        if sock.read_exact(&mut len).await.is_err() {
                            return;
                        }
                        let mut msg = vec![0u8; u16::from_be_bytes(len) as usize];
                        if sock.read_exact(&mut msg).await.is_err() {
                            return;
                        }
                        let a = answer_a(&msg, ip);
                        let _ = sock.write_all(&(a.len() as u16).to_be_bytes()).await;
                        let _ = sock.write_all(&a).await;
                    }
                });
            }
        });
        (addr, conns)
    }

    /// Подложный резолвер провайдера: на UDP-запрос отвечает NXDOMAIN с флагом
    /// aa, как это выглядело в домашней сети (XR-285).
    async fn spawn_spoofer() -> SocketAddr {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_MSG];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                    return;
                };
                let Some(end) = question_end(&buf[..n]) else {
                    continue;
                };
                let mut fake = buf[..end].to_vec();
                fake[2] |= 0x84; // QR + AA
                fake[3] = 0x80 | 3; // RA + NXDOMAIN
                fake[6..12].fill(0);
                let _ = sock.send_to(&fake, from).await;
            }
        });
        addr
    }

    /// Поднять форвардер поверх голого TCP до заглушки апстрима и вернуть его
    /// адрес.
    async fn spawn_forwarder(upstream: SocketAddr, timeout: Duration) -> SocketAddr {
        let (udp, tcp, addr) = bind_local().await;
        tokio::spawn(async move {
            let connect = move || async move { TcpStream::connect(upstream).await };
            let _ = serve_forwarder(udp, tcp, timeout, connect).await;
        });
        addr
    }

    /// Спросить форвардер по UDP и дождаться ответа. Датаграмма, пришедшая до
    /// первого `recv_from`, лежит в буфере сокета, так что гонки со стартом
    /// таска тут нет, а повтор страхует от потери на петле.
    async fn ask_udp(forwarder: SocketAddr, q: &[u8]) -> Vec<u8> {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for _ in 0..20 {
            sock.send_to(q, forwarder).await.unwrap();
            let mut buf = vec![0u8; MAX_MSG];
            match tokio::time::timeout(Duration::from_millis(500), sock.recv_from(&mut buf)).await {
                Ok(Ok((n, _))) => return buf[..n].to_vec(),
                _ => continue,
            }
        }
        panic!("форвардер не ответил");
    }

    /// Стенд по DoD XR-285: рядом стоит подложный резолвер, который отвечает
    /// NXDOMAIN с aa, а форвардер всё равно отдаёт настоящий адрес, потому что
    /// спрашивает не его, а апстрим за туннелем.
    #[tokio::test]
    async fn spoofed_resolver_does_not_reach_the_forwarder() {
        let real = Ipv4Addr::new(104, 21, 32, 8);
        let spoofer = spawn_spoofer().await;
        let q = query(0x1234, "rutracker.org", None);

        // Контроль стенда: прямой запрос к провайдерскому резолверу отдаёт
        // ровно ту подделку, из-за которой сайт и лежал.
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.send_to(&q, spoofer).await.unwrap();
        let mut buf = vec![0u8; MAX_MSG];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
            .await
            .expect("подложный резолвер молчит")
            .unwrap();
        assert_eq!(rcode(&buf[..n]), 3, "стенд обязан отвечать NXDOMAIN");
        assert!(authoritative(&buf[..n]), "стенд обязан ставить флаг aa");

        let (upstream, _) = spawn_upstream(real, 8).await;
        let forwarder = spawn_forwarder(upstream, Duration::from_millis(1500)).await;

        let answer = ask_udp(forwarder, &q).await;
        assert_eq!(rcode(&answer), 0, "форвардер отдал не NOERROR");
        assert_eq!(&answer[0..2], &q[0..2], "идентификатор запроса не сохранён");
        assert_eq!(first_a(&answer), Some(real));
    }

    /// Разные спрашивающие мультиплексируются в одно соединение, и ответ
    /// возвращается со своим идентификатором каждому.
    #[tokio::test]
    async fn answers_return_to_their_own_asker() {
        let real = Ipv4Addr::new(93, 184, 216, 34);
        let (upstream, conns) = spawn_upstream(real, 16).await;
        let addr = spawn_forwarder(upstream, Duration::from_millis(1500)).await;

        let first = ask_udp(addr, &query(0x0001, "one.example", None)).await;
        let second = ask_udp(addr, &query(0xBEEF, "two.example", None)).await;

        assert_eq!(&first[0..2], &[0x00, 0x01]);
        assert_eq!(&second[0..2], &[0xBE, 0xEF]);
        assert_eq!(conns.load(Ordering::SeqCst), 1, "на каждый запрос заводится своё соединение");
    }

    /// Апстрим закрыл соединение: следующий запрос поднимает его заново, а не
    /// уходит в SERVFAIL навсегда.
    #[tokio::test]
    async fn upstream_drop_is_reconnected() {
        let real = Ipv4Addr::new(1, 2, 3, 4);
        // Заглушка отвечает по одному разу на соединение и закрывает его.
        let (upstream, conns) = spawn_upstream(real, 1).await;
        let addr = spawn_forwarder(upstream, Duration::from_millis(1500)).await;

        assert_eq!(rcode(&ask_udp(addr, &query(1, "a.example", None)).await), 0);
        let second = ask_udp(addr, &query(2, "b.example", None)).await;
        assert_eq!(rcode(&second), 0, "после обрыва форвардер не поднялся заново");
        assert_eq!(first_a(&second), Some(real));
        assert_eq!(conns.load(Ordering::SeqCst), 2);
    }

    /// Туннель лежит: спрашивающий получает SERVFAIL сразу, а не молчание и не
    /// ответ провайдерского резолвера.
    #[tokio::test]
    async fn dead_upstream_answers_servfail() {
        let dead = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a = l.local_addr().unwrap();
            drop(l);
            a
        };
        let addr = spawn_forwarder(dead, Duration::from_millis(1500)).await;

        let q = query(0x7777, "rutracker.org", None);
        let answer = ask_udp(addr, &q).await;
        assert_eq!(rcode(&answer), 2, "ожидался SERVFAIL");
        assert_eq!(&answer[0..2], &q[0..2]);
        assert_eq!(&answer[6..12], &[0u8; 6], "в отказе не должно быть записей");
    }

    /// TCP-листенер отвечает наравне с UDP: по нему приходит переспрос за
    /// усечённым ответом.
    #[tokio::test]
    async fn tcp_listener_answers_too() {
        let real = Ipv4Addr::new(8, 8, 4, 4);
        let (upstream, _) = spawn_upstream(real, 4).await;
        let addr = spawn_forwarder(upstream, Duration::from_millis(1500)).await;

        let q = query(0x0042, "tcp.example", None);
        let mut sock = TcpStream::connect(addr).await.expect("форвардер не слушает TCP");
        sock.write_all(&(q.len() as u16).to_be_bytes()).await.unwrap();
        sock.write_all(&q).await.unwrap();
        let mut len = [0u8; 2];
        sock.read_exact(&mut len).await.unwrap();
        let mut answer = vec![0u8; u16::from_be_bytes(len) as usize];
        sock.read_exact(&mut answer).await.unwrap();

        assert_eq!(rcode(&answer), 0);
        assert_eq!(first_a(&answer), Some(real));
    }

    /// Туннель не отказывает, а молчит: подключение висит. Спрашивающий всё
    /// равно получает SERVFAIL, и получает его каждый раз, а не до тех пор,
    /// пока очередь форвардера не забьётся (замечание ревью 1).
    #[tokio::test]
    async fn silent_upstream_still_answers_every_asker() {
        let (udp, tcp, addr) = bind_local().await;
        tokio::spawn(async move {
            // Заглушка апстрима не отказывает и не отвечает: ровно так
            // выглядит лежащий туннель, где open_stream ждёт свой таймаут.
            let connect = move || async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Err::<TcpStream, _>(io::Error::new(io::ErrorKind::TimedOut, "молчит"))
            };
            let _ = serve_forwarder(udp, tcp, Duration::from_millis(300), connect).await;
        });
        // Запросов заведомо больше, чем помещается в очередь форвардера.
        for i in 0..300u16 {
            let q = query(i, "rutracker.org", None);
            let answer = ask_udp(addr, &q).await;
            assert_eq!(rcode(&answer), 2, "запрос {i} остался без отказа");
            assert_eq!(&answer[0..2], &q[0..2]);
        }
    }

    /// Ответ длиннее датаграммы не рвёт разговор с апстримом: он доезжает по
    /// TCP целиком, и соседний запрос по тому же соединению доходит следом
    /// (замечание ревью 2).
    #[tokio::test]
    async fn long_answer_survives_and_keeps_the_connection() {
        // Заглушка отвечает записью, которая заведомо не влезает в 4096.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = listener.local_addr().unwrap();
        let conns = Arc::new(AtomicUsize::new(0));
        let counter = conns.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    loop {
                        let mut len = [0u8; 2];
                        if sock.read_exact(&mut len).await.is_err() {
                            return;
                        }
                        let mut msg = vec![0u8; u16::from_be_bytes(len) as usize];
                        if sock.read_exact(&mut msg).await.is_err() {
                            return;
                        }
                        let mut a = answer_a(&msg, Ipv4Addr::new(5, 6, 7, 8));
                        a.resize(5000, 0);
                        let _ = sock.write_all(&(a.len() as u16).to_be_bytes()).await;
                        let _ = sock.write_all(&a).await;
                    }
                });
            }
        });

        let addr = spawn_forwarder(upstream, Duration::from_millis(1500)).await;
        let mut sock = TcpStream::connect(addr).await.expect("форвардер не слушает TCP");

        for id in [0x0001u16, 0x0002] {
            let q = query(id, "big.example", None);
            sock.write_all(&(q.len() as u16).to_be_bytes()).await.unwrap();
            sock.write_all(&q).await.unwrap();
            let mut len = [0u8; 2];
            sock.read_exact(&mut len).await.unwrap();
            let mut answer = vec![0u8; u16::from_be_bytes(len) as usize];
            sock.read_exact(&mut answer).await.unwrap();
            assert_eq!(answer.len(), 5000, "длинный ответ доехал не целиком");
            assert_eq!(&answer[0..2], &q[0..2]);
            assert_eq!(first_a(&answer), Some(Ipv4Addr::new(5, 6, 7, 8)));
        }
        assert_eq!(
            conns.load(Ordering::SeqCst),
            1,
            "длинный ответ порвал разговор, соединение поднялось заново"
        );
    }

    #[test]
    fn servfail_keeps_the_question() {
        let q = query(0xABCD, "example.org", None);
        let out = servfail(&q);
        assert_eq!(&out[0..2], &[0xAB, 0xCD]);
        assert_eq!(rcode(&out), 2);
        assert_eq!(out[2] & 0x80, 0x80, "ответ обязан быть помечен QR");
        assert_eq!(out[2] & 0x01, 0x01, "флаг RD запроса сохраняется");
        assert_eq!(&out[4..6], &1u16.to_be_bytes());
        assert_eq!(&out[6..12], &[0u8; 6]);
        assert_eq!(out.len(), question_end(&q).unwrap());
    }

    /// Мусор вместо запроса не становится ответом: на такое форвардер молчит.
    #[test]
    fn servfail_ignores_unparsable_query() {
        assert!(servfail(&[]).is_empty());
        assert!(servfail(&[0u8; 11]).is_empty());
        // QDCOUNT = 0: вопроса нет, отвечать не на что.
        let mut q = query(1, "example.org", None);
        q[4..6].copy_from_slice(&0u16.to_be_bytes());
        assert!(servfail(&q).is_empty());
        // Сжатое имя в вопросе это мусор, указателю там взяться неоткуда.
        let mut q = query(1, "example.org", None);
        q[12] = 0xC0;
        assert!(servfail(&q).is_empty());
        // Длина метки уводит за границу пакета.
        let mut q = query(1, "example.org", None);
        q[12] = 200;
        assert!(servfail(&q).is_empty());
    }

    #[test]
    fn udp_limit_comes_from_edns() {
        assert_eq!(udp_payload_limit(&query(1, "example.org", None)), MIN_UDP_PAYLOAD);
        assert_eq!(udp_payload_limit(&query(1, "example.org", Some(1232))), 1232);
        // Заявленное меньше классических 512 не сужает ответ, а заявленное
        // больше буфера приёма обрезается до него.
        assert_eq!(udp_payload_limit(&query(1, "example.org", Some(200))), MIN_UDP_PAYLOAD);
        assert_eq!(udp_payload_limit(&query(1, "example.org", Some(65535))), MAX_MSG);
    }

    #[test]
    fn truncation_keeps_question_and_sets_tc() {
        let q = query(0x0101, "long.example", None);
        let full = answer_a(&q, Ipv4Addr::new(10, 0, 0, 1));
        let short = truncated(&full);
        assert!(truncated_flag(&short), "переспрос по TCP заводится флагом TC");
        assert_eq!(&short[0..2], &q[0..2]);
        assert_eq!(&short[6..12], &[0u8; 6]);
        assert_eq!(short.len(), question_end(&q).unwrap());
    }
    async fn memory_forwarder(block_first_write: bool) -> (Forwarder, Arc<AtomicUsize>) {
        let (tx, rx) = mpsc::channel(16);
        let count = Arc::new(AtomicUsize::new(0));
        let connects = count.clone();
        let connect = move || {
            let n = connects.fetch_add(1, Ordering::SeqCst);
            let (client, mut server) = tokio::io::duplex(if block_first_write { 16 } else { 4096 });
            tokio::spawn(async move {
                if block_first_write && n == 0 {
                    let _server = server;
                    std::future::pending::<()>().await;
                    return;
                }
                let mut answered = false;
                loop {
                    let mut len = [0; 2];
                    if server.read_exact(&mut len).await.is_err() { return; }
                    let mut q = vec![0; u16::from_be_bytes(len) as usize];
                    if server.read_exact(&mut q).await.is_err() { return; }
                    if n == 0 && answered {
                        // Первый туннель после успешного ответа замирает,
                        // но поток остаётся открытым, как при blackhole WAN.
                        std::future::pending::<()>().await;
                    }
                    let answer = answer_a(&q, Ipv4Addr::new(1, 2, 3, 4));
                    server.write_all(&(answer.len() as u16).to_be_bytes()).await.unwrap();
                    server.write_all(&answer).await.unwrap();
                    answered = true;
                }
            });
            async move { Ok(client) }
        };
        let timeout = Duration::from_secs(5);
        tokio::spawn(upstream_loop(rx, connect, timeout));
        (Forwarder { jobs: tx, timeout }, count)
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_established_upstream_is_reconnected_after_query_deadline() {
        let (forwarder, connects) = memory_forwarder(false).await;
        assert_eq!(rcode(&forwarder.resolve(&query(1, "first.test", None)).await), 0);
        assert_eq!(rcode(&forwarder.resolve(&query(2, "blackhole.test", None)).await), 2);
        let answer = forwarder.resolve(&query(3, "recovered.test", None)).await;
        assert_eq!(rcode(&answer), 0,
            "после истечения запроса нужна новая гонка туннелей вместо вечного SERVFAIL");
        assert_eq!(connects.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_query_does_not_reconnect_open_upstream() {
        let (forwarder, connects) = memory_forwarder(false).await;
        assert_eq!(rcode(&forwarder.resolve(&query(1, "first.test", None)).await), 0);
        let second = forwarder.clone();
        let task = tokio::spawn(async move { second.resolve(&query(2, "cancelled.test", None)).await });
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;
        // Sweep удаляет отменённый запрос; его deadline не должен потом
        // выглядеть как недоступность апстрима.
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;
        let answer = forwarder.resolve(&query(3, "still-on-same.test", None)).await;
        assert_eq!(rcode(&answer), 2,
            "отмена не должна сама переключать ещё открытый апстрим");
        assert_eq!(connects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_answer_receiver_stops_stalled_reader() {
        let (reader, mut server) = tokio::io::duplex(1024);
        let (tx, rx) = mpsc::channel(1);
        let task = tokio::spawn(read_answers(reader, tx));
        drop(rx);
        tokio::time::timeout(Duration::from_secs(1), task).await
            .expect("reader не завершился после отключения receiver").unwrap();
        let mut byte = [0];
        assert_eq!(server.read(&mut byte).await.unwrap(), 0,
            "отменённый reader обязан закрыть старый туннельный поток");
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_upstream_write_expires_and_next_query_reconnects() {
        let (forwarder, connects) = memory_forwarder(true).await;
        let first = forwarder.resolve(&query(1, "blocked-write.test", None)).await;
        assert_eq!(rcode(&first), 2);
        let next = forwarder.resolve(&query(2, "recovered.test", None)).await;
        assert_eq!(rcode(&next), 0,
            "зависшая запись не должна навсегда остановить цикл DNS");
        assert_eq!(connects.load(Ordering::SeqCst), 2);
    }

    fn controlled_retry_forwarder(timeout: Duration) ->
        (Forwarder, mpsc::UnboundedReceiver<tokio::io::DuplexStream>, Arc<AtomicUsize>)
    {
        let (jobs, rx) = mpsc::channel(1024);
        let (servers, connections) = mpsc::unbounded_channel();
        let count = Arc::new(AtomicUsize::new(0));
        let connects = count.clone();
        tokio::spawn(upstream_loop(rx, move || {
            connects.fetch_add(1, Ordering::SeqCst);
            let (client, server) = tokio::io::duplex(4096);
            servers.send(server).unwrap();
            async move { Ok(client) }
        }, timeout));
        (Forwarder { jobs, timeout }, connections, count)
    }

    async fn retry_connection(connections: &mut mpsc::UnboundedReceiver<tokio::io::DuplexStream>)
        -> tokio::io::DuplexStream
    {
        tokio::time::timeout(Duration::from_secs(1), connections.recv()).await
            .expect("после раннего EOF не было повторного подключения").unwrap()
    }

    async fn read_retry_query(server: &mut tokio::io::DuplexStream) -> Vec<u8> {
        let mut len = [0; 2];
        server.read_exact(&mut len).await.unwrap();
        let mut query = vec![0; u16::from_be_bytes(len) as usize];
        server.read_exact(&mut query).await.unwrap();
        query
    }

    async fn reply_retry_query(server: &mut tokio::io::DuplexStream, q: &[u8], ip: Ipv4Addr) {
        let answer = answer_a(q, ip);
        server.write_all(&(answer.len() as u16).to_be_bytes()).await.unwrap();
        server.write_all(&answer).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn early_eof_retries_original_query_once_and_preserves_edns_client_id() {
        let (forwarder, mut connections, count) = controlled_retry_forwarder(Duration::from_secs(5));
        let original = query(0xbeef, "retry.test", Some(1232));
        let q = original.clone();
        let request = tokio::spawn(async move { forwarder.resolve(&q).await });
        let mut first = retry_connection(&mut connections).await;
        let accepted = read_retry_query(&mut first).await;
        assert_eq!(&accepted[2..], &original[2..]);
        drop(first);
        let mut second = retry_connection(&mut connections).await;
        let retried = read_retry_query(&mut second).await;
        assert_eq!(&retried[2..], &original[2..], "повтор потерял DNS flags/question/EDNS");
        reply_retry_query(&mut second, &retried, Ipv4Addr::new(1, 2, 3, 4)).await;
        let answer = request.await.unwrap();
        assert_eq!(rcode(&answer), 0);
        assert_eq!(&answer[0..2], &[0xbe, 0xef]);
        assert_eq!(first_a(&answer), Some(Ipv4Addr::new(1, 2, 3, 4)));
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn two_early_eofs_stop_after_exactly_two_attempts() {
        let (forwarder, mut connections, count) = controlled_retry_forwarder(Duration::from_secs(5));
        let client = forwarder.clone();
        let request = tokio::spawn(async move { client.resolve(&query(1, "twice.test", None)).await });
        for _ in 0..2 {
            let mut server = retry_connection(&mut connections).await;
            read_retry_query(&mut server).await;
            drop(server);
        }
        assert_eq!(rcode(&request.await.unwrap()), 2);
        assert!(tokio::time::timeout(Duration::from_secs(1), connections.recv()).await.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 2, "повтор зациклился при общем отказе апстрима");
    }

    #[tokio::test(start_paused = true)]
    async fn retry_connect_uses_original_deadline_and_is_cancelled_at_it() {
        struct Active(Arc<AtomicUsize>);
        impl Drop for Active {
            fn drop(&mut self) { self.0.fetch_sub(1, Ordering::SeqCst); }
        }
        let (jobs, rx) = mpsc::channel(16);
        let (servers, mut connections) = mpsc::unbounded_channel();
        let attempts = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let connecting = active.clone();
        tokio::spawn(upstream_loop(rx, move || {
            let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
            let active = connecting.clone();
            let servers = servers.clone();
            async move {
                if first {
                    let (client, server) = tokio::io::duplex(4096);
                    servers.send(server).unwrap();
                    Ok(client)
                } else {
                    active.fetch_add(1, Ordering::SeqCst);
                    let _guard = Active(active);
                    std::future::pending().await
                }
            }
        }, Duration::from_secs(5)));
        let forwarder = Forwarder { jobs, timeout: Duration::from_secs(5) };
        let started = tokio::time::Instant::now();
        let request = tokio::spawn(async move { forwarder.resolve(&query(1, "deadline.test", None)).await });
        let mut first = retry_connection(&mut connections).await;
        read_retry_query(&mut first).await;
        tokio::time::advance(Duration::from_secs(4)).await;
        drop(first);
        assert_eq!(rcode(&request.await.unwrap()), 2);
        tokio::task::yield_now().await;
        assert_eq!(tokio::time::Instant::now() - started, Duration::from_secs(5));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(active.load(Ordering::SeqCst), 0, "повтор продлил срок подключения");
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_query_is_not_retried_after_eof() {
        let (forwarder, mut connections, count) = controlled_retry_forwarder(Duration::from_secs(5));
        let client = forwarder.clone();
        let request = tokio::spawn(async move { client.resolve(&query(1, "cancel.test", None)).await });
        let mut first = retry_connection(&mut connections).await;
        read_retry_query(&mut first).await;
        request.abort();
        let _ = request.await;
        drop(first);
        assert!(tokio::time::timeout(Duration::from_secs(1), connections.recv()).await.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn update_notify_and_response_are_not_retried_after_eof() {
        for flags in [5 << 3, 4 << 3, 0x80] {
            let (forwarder, mut connections, count) = controlled_retry_forwarder(Duration::from_secs(5));
            let mut q = query(1, "non-query.test", None);
            q[2] = flags;
            let client = forwarder.clone();
        let request = tokio::spawn(async move { client.resolve(&q).await });
            let mut first = retry_connection(&mut connections).await;
            read_retry_query(&mut first).await;
            drop(first);
            assert_eq!(rcode(&request.await.unwrap()), 2);
            assert!(tokio::time::timeout(Duration::from_secs(1), connections.recv()).await.is_err());
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn multiple_pending_retries_keep_distinct_wire_ids_and_out_of_order_answers() {
        let (forwarder, mut connections, count) = controlled_retry_forwarder(Duration::from_secs(5));
        let originals: Vec<_> = (0..3).map(|n| query(0xbeef, &format!("{n}.retry.test"), Some(1232))).collect();
        let mut requests = Vec::new();
        for q in &originals {
            let q = q.clone();
            let forwarder = forwarder.clone();
            requests.push(tokio::spawn(async move { forwarder.resolve(&q).await }));
        }
        let mut first = retry_connection(&mut connections).await;
        for _ in 0..3 { read_retry_query(&mut first).await; }
        tokio::time::advance(Duration::from_secs(2)).await;
        drop(first);
        let mut second = retry_connection(&mut connections).await;
        let mut retried = Vec::new();
        for _ in 0..3 { retried.push(read_retry_query(&mut second).await); }
        let ids: std::collections::HashSet<_> = retried.iter().map(|q| &q[0..2]).collect();
        assert_eq!(ids.len(), 3, "одинаковые client ID столкнулись после повтора");
        for q in retried.iter().rev() {
            let index = originals.iter().position(|original| original[2..] == q[2..]).unwrap();
            reply_retry_query(&mut second, q, Ipv4Addr::new(1, 2, 3, index as u8)).await;
        }
        for (index, request) in requests.into_iter().enumerate() {
            let answer = request.await.unwrap();
            assert_eq!(rcode(&answer), 0);
            assert_eq!(&answer[0..2], &[0xbe, 0xef]);
            assert_eq!(first_a(&answer), Some(Ipv4Addr::new(1, 2, 3, index as u8)));
        }
        assert_eq!(count.load(Ordering::SeqCst), 2, "повторы открыли отдельное соединение на каждый запрос");
    }

    #[tokio::test(start_paused = true)]
    async fn eof_retry_queue_never_replays_more_than_256_outstanding_queries() {
        let (forwarder, mut connections, count) = controlled_retry_forwarder(Duration::from_secs(5));
        let mut requests = Vec::new();
        for n in 0..300 {
            let forwarder = forwarder.clone();
            requests.push(tokio::spawn(async move { forwarder.resolve(&query(n, "bound.test", None)).await }));
        }
        let mut first = retry_connection(&mut connections).await;
        for _ in 0..300 { read_retry_query(&mut first).await; }
        drop(first);
        let mut second = retry_connection(&mut connections).await;
        for _ in 0..256 {
            let q = read_retry_query(&mut second).await;
            reply_retry_query(&mut second, &q, Ipv4Addr::new(1, 2, 3, 4)).await;
        }
        let mut success = 0;
        for request in requests { success += usize::from(rcode(&request.await.unwrap()) == 0); }
        assert_eq!(success, 256);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        let mut extra = [0];
        assert!(tokio::time::timeout(Duration::from_secs(1), second.read(&mut extra)).await.is_err(),
            "повторили запрос сверх предела очереди");
    }

    struct FaultyDnsStream {
        inner: tokio::io::DuplexStream,
        read_error: Option<io::ErrorKind>,
        write_error: Option<io::ErrorKind>,
    }

    impl AsyncRead for FaultyDnsStream {
        fn poll_read(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<io::Result<()>>
        {
            if let Some(kind) = self.read_error.take() {
                return std::task::Poll::Ready(Err(io::Error::new(kind, "synthetic read fault")));
            }
            std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for FaultyDnsStream {
        fn poll_write(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>,
            buf: &[u8]) -> std::task::Poll<io::Result<usize>>
        {
            if let Some(kind) = self.write_error.take() {
                return std::task::Poll::Ready(Err(io::Error::new(kind, "synthetic write fault")));
            }
            std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
        }
        fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>)
            -> std::task::Poll<io::Result<()>>
        { std::pin::Pin::new(&mut self.inner).poll_flush(cx) }
        fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>)
            -> std::task::Poll<io::Result<()>>
        { std::pin::Pin::new(&mut self.inner).poll_shutdown(cx) }
    }

    fn faulty_retry_forwarder(read_error: Option<io::ErrorKind>, write_error: Option<io::ErrorKind>)
        -> (Forwarder, mpsc::UnboundedReceiver<tokio::io::DuplexStream>, Arc<AtomicUsize>)
    {
        let (jobs, rx) = mpsc::channel(16);
        let (servers, connections) = mpsc::unbounded_channel();
        let count = Arc::new(AtomicUsize::new(0));
        let connects = count.clone();
        tokio::spawn(upstream_loop(rx, move || {
            let first = connects.fetch_add(1, Ordering::SeqCst) == 0;
            let (inner, server) = tokio::io::duplex(4096);
            servers.send(server).unwrap();
            async move { Ok(FaultyDnsStream { inner,
                read_error: if first { read_error } else { None },
                write_error: if first { write_error } else { None },
            }) }
        }, Duration::from_secs(5)));
        (Forwarder { jobs, timeout: Duration::from_secs(5) }, connections, count)
    }

    #[tokio::test(start_paused = true)]
    async fn early_broken_pipe_write_retries_query_on_new_connection() {
        let (forwarder, mut connections, count) = faulty_retry_forwarder(None, Some(io::ErrorKind::BrokenPipe));
        let client = forwarder.clone();
        let request = tokio::spawn(async move { client.resolve(&query(0xcafe, "write-retry.test", None)).await });
        let _broken = retry_connection(&mut connections).await;
        let mut second = retry_connection(&mut connections).await;
        let q = read_retry_query(&mut second).await;
        reply_retry_query(&mut second, &q, Ipv4Addr::new(1, 2, 3, 4)).await;
        let answer = request.await.unwrap();
        assert_eq!(rcode(&answer), 0);
        assert_eq!(&answer[0..2], &[0xca, 0xfe]);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_tls_data_is_not_retried() {
        let (forwarder, mut connections, count) = faulty_retry_forwarder(Some(io::ErrorKind::InvalidData), None);
        let client = forwarder.clone();
        let request = tokio::spawn(async move { client.resolve(&query(1, "invalid.test", None)).await });
        let _broken = retry_connection(&mut connections).await;
        assert_eq!(rcode(&request.await.unwrap()), 2);
        assert!(tokio::time::timeout(Duration::from_secs(1), connections.recv()).await.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn expired_query_is_not_retried_after_late_eof() {
        let (forwarder, mut connections, count) = controlled_retry_forwarder(Duration::from_secs(5));
        let client = forwarder.clone();
        let request = tokio::spawn(async move { client.resolve(&query(1, "expired.test", None)).await });
        let mut first = retry_connection(&mut connections).await;
        read_retry_query(&mut first).await;
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(rcode(&request.await.unwrap()), 2);
        drop(first);
        assert!(tokio::time::timeout(Duration::from_secs(1), connections.recv()).await.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn queued_retries_do_not_starve_new_queries() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (forwarder, mut connections, _) = controlled_retry_forwarder(Duration::from_secs(5));
            let mut requests = Vec::new();
            for n in 0..8 {
                let client = forwarder.clone();
                requests.push(tokio::spawn(async move { client.resolve(&query(n, "old.test", None)).await }));
            }
            let mut first = retry_connection(&mut connections).await;
            for _ in 0..8 { read_retry_query(&mut first).await; }
            // Ставим новые запросы в канал до обработки обрыва, не давая
            // планировщику спрятать голодание за разным порядком spawn.
            let mut fresh = Vec::new();
            for n in 8..10 {
                let client = forwarder.clone();
                fresh.push(Box::pin(async move { client.resolve(&query(n, &format!("fresh{}.test", n - 8), None)).await }));
            }
            std::future::poll_fn(|cx| {
                for future in &mut fresh { assert!(future.as_mut().poll(cx).is_pending()); }
                std::task::Poll::Ready(())
            }).await;
            requests.extend(fresh.into_iter().map(tokio::spawn));
            drop(first);
            let mut second = retry_connection(&mut connections).await;
            let mut last_fresh = None;
            for index in 0..10 {
                let q = read_retry_query(&mut second).await;
                if q.windows(6).any(|bytes| bytes == b"fresh1") {
                    last_fresh = Some(index);
                }
                reply_retry_query(&mut second, &q, Ipv4Addr::new(1, 2, 3, 4)).await;
            }
            assert!(last_fresh.is_some_and(|index| index <= 3),
                "новые запросы ждали полного обхода очереди повторов: {last_fresh:?}");
            for request in requests { assert_eq!(rcode(&request.await.unwrap()), 0); }
        }).await.expect("новые запросы потерялись при разборе очереди повторов");
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_during_retry_connect_does_not_send_query_after_connect() {
        let (jobs, rx) = mpsc::channel(16);
        let (servers, mut connections) = mpsc::unbounded_channel();
        let attempts = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());
        let reconnect = gate.clone();
        tokio::spawn(upstream_loop(rx, move || {
            let first = attempts.fetch_add(1, Ordering::SeqCst) == 0;
            let (client, server) = tokio::io::duplex(4096);
            servers.send(server).unwrap();
            let gate = reconnect.clone();
            async move {
                if !first { gate.notified().await; }
                Ok(client)
            }
        }, Duration::from_secs(5)));
        let forwarder = Forwarder { jobs, timeout: Duration::from_secs(5) };
        let client = forwarder.clone();
        let request = tokio::spawn(async move { client.resolve(&query(1, "connect-cancel.test", None)).await });
        let mut first = retry_connection(&mut connections).await;
        read_retry_query(&mut first).await;
        drop(first);
        let mut second = retry_connection(&mut connections).await;
        request.abort();
        let _ = request.await;
        gate.notify_one();
        let mut byte = [0];
        assert!(tokio::time::timeout(Duration::from_secs(1), second.read(&mut byte)).await.is_err(),
            "после отмены клиента отправили повторный запрос");
    }

}
