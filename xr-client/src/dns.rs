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

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
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

struct Job {
    query: Vec<u8>,
    reply: oneshot::Sender<Vec<u8>>,
}

struct Pending {
    /// Идентификатор, с которым запрос пришёл от dnsmasq: на проводе к
    /// апстриму у него свой, потому что в одно соединение мультиплексируются
    /// разные спрашивающие.
    client_id: [u8; 2],
    reply: oneshot::Sender<Vec<u8>>,
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
        let (tx, rx) = oneshot::channel();
        let job = Job { query: query.to_vec(), reply: tx };
        // Очередь забита значит апстрим не отвечает, и ждать в ней места это
        // то самое молчание, вместо которого спрашивающему нужен отказ.
        if self.jobs.try_send(job).is_err() {
            return servfail(query);
        }
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(answer)) => answer,
            _ => servfail(query),
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
    Answer(Option<Vec<u8>>),
    Sweep,
    Stop,
}

async fn recv_answer(answers: &mut Option<mpsc::Receiver<Vec<u8>>>) -> Option<Vec<u8>> {
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
    let mut answers: Option<mpsc::Receiver<Vec<u8>>> = None;
    let mut last_complaint: Option<Instant> = None;
    let mut next_attempt: Option<Instant> = None;
    let mut sweep = tokio::time::interval(Duration::from_secs(1));

    loop {
        let event = tokio::select! {
            job = jobs.recv() => match job {
                Some(job) => Event::Job(job),
                None => Event::Stop,
            },
            answer = recv_answer(&mut answers) => Event::Answer(answer),
            _ = sweep.tick() => Event::Sweep,
        };

        match event {
            Event::Stop => break,
            Event::Sweep => {
                // Запись спрашивающего, который уже не ждёт (сработал таймаут
                // и ушёл SERVFAIL), держать незачем: без чистки таблица растёт
                // на каждом молчании апстрима.
                if let Some(c) = conn.as_mut() {
                    c.pending.retain(|_, p| !p.reply.is_closed());
                }
            }
            Event::Answer(Some(msg)) => {
                if msg.len() < 12 {
                    continue;
                }
                let wire_id = u16::from_be_bytes([msg[0], msg[1]]);
                if let Some(c) = conn.as_mut() {
                    if let Some(p) = c.pending.remove(&wire_id) {
                        let mut msg = msg;
                        msg[0] = p.client_id[0];
                        msg[1] = p.client_id[1];
                        let _ = p.reply.send(msg);
                    }
                }
            }
            Event::Answer(None) => {
                tracing::warn!("dns: upstream closed the connection, will reconnect on next query");
                // Ожидающие уходят вместе с таблицей: их oneshot закрывается,
                // и спрашивающий получает SERVFAIL, а не виснет до таймаута.
                conn = None;
                answers = None;
            }
            Event::Job(job) => {
                if job.query.len() < 12 {
                    continue;
                }
                // Спрашивающий уже ушёл по своему таймауту: тратить на него
                // попытку подключения значит задерживать тех, кто ещё ждёт.
                if job.reply.is_closed() {
                    continue;
                }
                if conn.is_none() {
                    if next_attempt.is_some_and(|t| Instant::now() < t) {
                        continue;
                    }
                    match tokio::time::timeout(timeout, connect()).await {
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
                            next_attempt = Some(Instant::now() + UPSTREAM_RETRY_PAUSE);
                            continue;
                        }
                        Err(_) => {
                            complain(&mut last_complaint, "connect timed out".to_string());
                            next_attempt = Some(Instant::now() + UPSTREAM_RETRY_PAUSE);
                            continue;
                        }
                    }
                }

                let c = conn.as_mut().expect("connection built above");
                let wire_id = c.next_free_id();
                let mut msg = job.query;
                let client_id = [msg[0], msg[1]];
                msg[0..2].copy_from_slice(&wire_id.to_be_bytes());

                let mut framed = Vec::with_capacity(msg.len() + 2);
                framed.extend_from_slice(&(msg.len() as u16).to_be_bytes());
                framed.extend_from_slice(&msg);
                if let Err(e) = c.writer.write_all(&framed).await {
                    tracing::warn!("dns: write to upstream failed: {e}");
                    conn = None;
                    answers = None;
                    continue;
                }
                c.pending.insert(wire_id, Pending { client_id, reply: job.reply });
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

async fn read_answers<R: AsyncRead + Unpin>(mut reader: R, tx: mpsc::Sender<Vec<u8>>) {
    loop {
        let mut len = [0u8; 2];
        if reader.read_exact(&mut len).await.is_err() {
            break;
        }
        // Длинный ответ дочитывается целиком, а не рвёт разговор: обрыв снёс
        // бы таблицу ожидающих, и SERVFAIL получили бы все, кто спрашивал
        // параллельно про другие имена, а само имя не резолвилось бы никогда.
        let n = u16::from_be_bytes(len) as usize;
        if n == 0 {
            break;
        }
        let mut msg = vec![0u8; n];
        if reader.read_exact(&mut msg).await.is_err() {
            break;
        }
        if tx.send(msg).await.is_err() {
            break;
        }
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
    async fn spawn_upstream(ip: Ipv4Addr, answers_per_conn: usize) -> (SocketAddr, Arc<AtomicUsize>) {
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
}
