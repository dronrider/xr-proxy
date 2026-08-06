//! Апгрейд до WebSocket насквозь и штатное закрытие до лимита сплайса
//! (LLD-38 п. 2.4).
//!
//! После `101` фронт перестаёт быть HTTP-посредником и просто гонит байты в обе
//! стороны. Содержимое кадров он не разбирает: ping/pong, фрагментация и
//! расширения это дело сторон, а соединение до агента уходит из пула насовсем
//! (п. 3.6), потому что HTTP на нём больше не живёт.
//!
//! Живёт такое соединение не дольше, чем ему позволяет relay: `splice_lifetime`
//! рубит сплайс жёстко и молча, и для живой ленты дашборда это неотличимо от
//! зависания. Поэтому фронт закрывает апгрейд сам и заранее, штатным закрытием
//! (`1001 going away` у WebSocket, обычный FIN у прочих апгрейдов): приложение
//! получает событие закрытия и переподключается, как оно это делает при потере
//! Wi-Fi.
//!
//! Единственное, что здесь всё же считается в кадрах, это их границы:
//! служебный `close`, всунутый в середину чужого кадра, приехал бы пиру мусором
//! вместо закрытия. Заголовки кадров дают длину, содержимое не читается.

use std::time::Duration;

use axum::http::{header, HeaderMap};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::Instant;

/// Штатное закрытие от сервера клиенту: `Close` с кодом 1001 «going away», без
/// маски (маскируется только то, что идёт от клиента).
pub const SERVER_CLOSE_GOING_AWAY: [u8; 4] = [0x88, 0x02, 0x03, 0xE9];

/// Запас перед потолком сплайса: закрываемся за минуту до него, как сказано в
/// дизайне. На коротком лимите (его уменьшают конфигом relay, когда проверяют
/// сценарий) минута съела бы весь лимит, поэтому запас не больше пятой части.
const CLOSE_MARGIN: Duration = Duration::from_secs(60);

/// Сколько ждём конца начатого кадра, прежде чем закрыть всё равно. Пир,
/// который тянет один кадр дольше этого, уже не в том состоянии, чтобы беречь
/// его кадровый поток.
const BOUNDARY_GRACE: Duration = Duration::from_secs(5);

/// Сколько живёт сокет после отправленного `close`: пиру нужно время дослать
/// свой ответный кадр и закрыться самому.
const LINGER: Duration = Duration::from_secs(3);

/// Какой протокол просит запрос. `None` значит обычный HTTP-запрос: апгрейд
/// объявляют оба заголовка сразу, одного `Upgrade` без `Connection` мало.
pub fn requested(headers: &HeaderMap) -> Option<String> {
    let connection = headers.get(header::CONNECTION)?.to_str().ok()?;
    let upgrade_asked = connection
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    if !upgrade_asked {
        return None;
    }
    let proto = headers.get(header::UPGRADE)?.to_str().ok()?.trim();
    (!proto.is_empty()).then(|| proto.to_string())
}

/// WebSocket ли это. Штатное закрытие у него своё (кадр `Close`), у прочих
/// апгрейдов закрытие это FIN.
pub fn is_websocket(proto: &str) -> bool {
    proto.eq_ignore_ascii_case("websocket")
}

/// Через сколько закрывать апгрейд штатно: потолок сплайса минус запас минус
/// возраст соединения (оно могло полежать в пуле и часть своего срока уже
/// прожить). `None` значит, что потолка нет и торопиться некуда.
pub fn close_after(splice_lifetime_secs: u64, age: Duration) -> Option<Duration> {
    if splice_lifetime_secs == 0 {
        return None;
    }
    let cap = Duration::from_secs(splice_lifetime_secs);
    let margin = CLOSE_MARGIN.min(cap / 5);
    let left = cap.saturating_sub(age).saturating_sub(margin);
    // Соединение, у которого запас уже вышел, закрываем не мгновенно: пусть
    // приложение хотя бы увидит открытый сокет и штатное закрытие после него.
    Some(left.max(Duration::from_millis(100)))
}

/// Сплайс апгрейда: байты в обе стороны, закрытие с одной стороны доезжает до
/// другой, а по подходе к потолку сплайса обе стороны получают штатное
/// закрытие.
pub async fn splice<B, A>(
    browser: B,
    agent: A,
    publication: String,
    proto: String,
    close_in: Option<Duration>,
) where
    B: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    A: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let ws = is_websocket(&proto);
    let deadline = close_in.map(|d| Instant::now() + d);
    let (browser_rx, browser_tx) = tokio::io::split(browser);
    let (agent_rx, agent_tx) = tokio::io::split(agent);

    let up = pump(browser_rx, agent_tx, deadline, ws.then(masked_close));
    let down = pump(
        agent_rx,
        browser_tx,
        deadline,
        ws.then(|| SERVER_CLOSE_GOING_AWAY.to_vec()),
    );
    let (up, down) = tokio::join!(up, down);

    // Одна строка на весь апгрейд, а не по строке на направление: обрыв важнее
    // штатного закрытия, а закрытие самим приложением это тишина.
    match (up, down) {
        (End::Broken(e), _) | (_, End::Broken(e)) => {
            tracing::warn!("апгрейд {publication} ({proto}): обрыв туннеля ({e})")
        }
        (End::Deadline, _) | (_, End::Deadline) => tracing::info!(
            "апгрейд {publication} ({proto}): штатное закрытие до потолка жизни сплайса"
        ),
        (End::Eof, End::Eof) => {
            tracing::debug!("апгрейд {publication} ({proto}): стороны закрылись сами")
        }
    }
}

/// Чем кончилось одно направление.
#[derive(Debug)]
enum End {
    /// Сторона закрылась сама, и закрытие доехало до другой.
    Eof,
    /// Закрыли мы, не дожидаясь потолка сплайса.
    Deadline,
    /// Туннель оборвался: relay срубил сплайс либо агент ушёл в
    /// переподключение.
    Broken(std::io::Error),
}

/// Одно направление сплайса. Копирует байты, а по своему сроку досылает
/// штатное закрытие и отпускает сокет, дав пиру время ответить.
async fn pump<R, W>(
    mut r: R,
    mut w: W,
    deadline: Option<Instant>,
    close_frame: Option<Vec<u8>>,
) -> End
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 16 * 1024];
    let mut frames = Framing::new(close_frame.is_some());
    let armed = deadline.is_some();
    // Далёкий срок это заглушка на случай, когда потолка нет: ветка всё равно
    // выключена условием.
    let sleep = tokio::time::sleep_until(deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400)));
    tokio::pin!(sleep);
    let mut closing = false;

    let end = loop {
        tokio::select! {
            biased;
            _ = &mut sleep, if armed => {
                if closing || frames.at_boundary() {
                    break End::Deadline;
                }
                // Кадр начат: дослать его целиком, иначе наш close уедет пиру
                // в середину чужого кадра и разберётся мусором.
                closing = true;
                sleep.as_mut().reset(Instant::now() + BOUNDARY_GRACE);
            }
            read = r.read(&mut buf) => match read {
                Ok(0) => break End::Eof,
                Ok(n) => {
                    if let Err(e) = w.write_all(&buf[..n]).await {
                        break End::Broken(e);
                    }
                    frames.feed(&buf[..n]);
                    if closing && frames.at_boundary() {
                        break End::Deadline;
                    }
                }
                Err(e) => break End::Broken(e),
            },
        }
    };

    if matches!(end, End::Deadline) {
        if let Some(frame) = close_frame {
            let _ = w.write_all(&frame).await;
            let _ = w.flush().await;
        }
    }
    let _ = w.shutdown().await;
    if matches!(end, End::Deadline) {
        // Ответный close пира копировать уже некуда, но сокет держим: без этого
        // пир упирается в закрытое соединение вместо штатного закрытия.
        let _ = tokio::time::timeout(LINGER, async {
            while let Ok(n) = r.read(&mut buf).await {
                if n == 0 {
                    break;
                }
            }
        })
        .await;
    }
    end
}

/// Штатное закрытие от клиента серверу: тот же код 1001, но маскированное, как
/// требует протокол от всего, что шлёт клиент. Фронт для агента и есть клиент.
fn masked_close() -> Vec<u8> {
    let mask: [u8; 4] = rand::random();
    vec![
        0x88,
        0x82,
        mask[0],
        mask[1],
        mask[2],
        mask[3],
        0x03 ^ mask[0],
        0xE9 ^ mask[1],
    ]
}

/// Где в потоке мы стоим: на границе кадров или посреди кадра. Заголовки дают
/// длину, содержимое не читается вовсе.
struct Framing {
    /// Выключено у не-WebSocket апгрейдов: границ там нет, закрытие это FIN.
    enabled: bool,
    state: State,
}

enum State {
    /// Копится заголовок кадра: до двух байт длина ещё неизвестна.
    Header(Vec<u8>),
    /// Осталось столько байт полезной нагрузки.
    Payload(u64),
}

impl Framing {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            state: State::Header(Vec::new()),
        }
    }

    /// Стоим ли на границе кадров.
    fn at_boundary(&self) -> bool {
        if !self.enabled {
            return true;
        }
        matches!(&self.state, State::Header(buf) if buf.is_empty())
    }

    fn feed(&mut self, mut bytes: &[u8]) {
        if !self.enabled {
            return;
        }
        while !bytes.is_empty() {
            match &mut self.state {
                State::Payload(left) => {
                    let take = (*left).min(bytes.len() as u64) as usize;
                    *left -= take as u64;
                    bytes = &bytes[take..];
                    if *left == 0 {
                        self.state = State::Header(Vec::new());
                    }
                }
                State::Header(buf) => {
                    buf.push(bytes[0]);
                    bytes = &bytes[1..];
                    if let Some(len) = header_len(buf) {
                        if buf.len() == len {
                            let payload = payload_len(buf);
                            self.state = if payload == 0 {
                                State::Header(Vec::new())
                            } else {
                                State::Payload(payload)
                            };
                        }
                    }
                }
            }
        }
    }
}

/// Полная длина заголовка кадра, когда её уже видно (нужны первые два байта).
fn header_len(buf: &[u8]) -> Option<usize> {
    if buf.len() < 2 {
        return None;
    }
    let masked = buf[1] & 0x80 != 0;
    let code = buf[1] & 0x7F;
    let extended = match code {
        126 => 2,
        127 => 8,
        _ => 0,
    };
    Some(2 + extended + if masked { 4 } else { 0 })
}

/// Длина полезной нагрузки из готового заголовка.
fn payload_len(buf: &[u8]) -> u64 {
    match buf[1] & 0x7F {
        126 => u16::from_be_bytes([buf[2], buf[3]]) as u64,
        127 => u64::from_be_bytes([
            buf[2], buf[3], buf[4], buf[5], buf[6], buf[7], buf[8], buf[9],
        ]),
        code => code as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    /// Сколько тест ждёт байтов, прежде чем считать сплайс сломанным. Всё, что
    /// он ждёт от другой стороны, ждётся с этим потолком: сплайс, разучившийся
    /// пробрасывать конец потока, иначе подвешивает прогон вместо падения, и
    /// регрессия выглядит не краснотой, а зависшим `cargo test`.
    const TEST_WAIT: Duration = Duration::from_secs(5);

    async fn within<F: std::future::Future>(what: &str, f: F) -> F::Output {
        match tokio::time::timeout(TEST_WAIT, f).await {
            Ok(v) => v,
            Err(_) => panic!("{what}: не дождались за {} с", TEST_WAIT.as_secs()),
        }
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn upgrade_is_seen_only_when_both_headers_ask_for_it() {
        assert_eq!(
            requested(&headers(&[("connection", "keep-alive, Upgrade"), ("upgrade", "websocket")])).as_deref(),
            Some("websocket")
        );
        // Один лишь Upgrade это не апгрейд: так его объявляет протокол, и
        // приняв половину, фронт пообещал бы браузеру сплайс, которого не будет.
        assert_eq!(requested(&headers(&[("upgrade", "websocket")])), None);
        assert_eq!(requested(&headers(&[("connection", "upgrade")])), None);
        assert_eq!(requested(&headers(&[("connection", "close")])), None);
        assert!(is_websocket("WebSocket"));
        assert!(!is_websocket("h2c"));
    }

    #[test]
    fn closing_time_leaves_a_margin_and_counts_the_age() {
        // Час на relay: закрываемся за минуту до него.
        assert_eq!(
            close_after(3600, Duration::ZERO),
            Some(Duration::from_secs(3540))
        );
        // Соединение взято из пула и часть срока уже прожило: запас считается
        // от рождения соединения, а не от начала апгрейда.
        assert_eq!(
            close_after(3600, Duration::from_secs(100)),
            Some(Duration::from_secs(3440))
        );
        // Короткий лимит проверки: минута съела бы его целиком, поэтому запас
        // это пятая часть.
        assert_eq!(close_after(30, Duration::ZERO), Some(Duration::from_secs(24)));
        // Срок уже вышел: закрытие не мгновенное, но и не отложенное.
        assert_eq!(
            close_after(30, Duration::from_secs(300)),
            Some(Duration::from_millis(100))
        );
        // Потолка нет: закрывать по времени нечего.
        assert_eq!(close_after(0, Duration::ZERO), None);
    }

    #[test]
    fn frame_boundaries_are_tracked_by_headers_only() {
        let mut f = Framing::new(true);
        assert!(f.at_boundary(), "пустой поток стоит на границе");

        // Короткий кадр от сервера: два байта заголовка, три байта нагрузки.
        f.feed(&[0x81, 0x03]);
        assert!(!f.at_boundary());
        f.feed(b"ab");
        assert!(!f.at_boundary(), "нагрузка ещё не дочитана");
        f.feed(b"c");
        assert!(f.at_boundary());

        // Кадр от клиента: маска добавляет четыре байта заголовка.
        f.feed(&[0x81, 0x82, 1, 2, 3, 4]);
        assert!(!f.at_boundary());
        f.feed(&[0x10, 0x20]);
        assert!(f.at_boundary());

        // Расширенная длина 126: два байта на длину.
        f.feed(&[0x82, 126, 0x00, 0x04]);
        assert!(!f.at_boundary());
        f.feed(&[1, 2, 3, 4]);
        assert!(f.at_boundary());

        // Пустой кадр (тот же close без тела) границы не сдвигает.
        f.feed(&[0x88, 0x00]);
        assert!(f.at_boundary());

        // Заголовок, приехавший по байту, тоже считается.
        let mut f = Framing::new(true);
        for b in [0x81u8, 127, 0, 0, 0, 0, 0, 0, 0, 2] {
            f.feed(&[b]);
            assert!(!f.at_boundary(), "заголовок ещё не кончился");
        }
        f.feed(&[7, 7]);
        assert!(f.at_boundary());

        // Не-WebSocket апгрейд: границ нет, закрывать можно когда угодно.
        let mut off = Framing::new(false);
        off.feed(&[0x81, 0x05]);
        assert!(off.at_boundary());
    }

    #[test]
    fn client_close_is_masked_and_says_going_away() {
        let frame = masked_close();
        assert_eq!(frame.len(), 8);
        assert_eq!(frame[0], 0x88, "opcode close");
        assert_eq!(frame[1], 0x82, "маска обязательна и длина два байта");
        let mask = &frame[2..6];
        assert_eq!(frame[6] ^ mask[0], 0x03);
        assert_eq!(frame[7] ^ mask[1], 0xE9);
        assert_ne!(masked_close()[2..6], frame[2..6], "маска не постоянная");
    }

    /// Сплайс на паре duplex: байты в обе стороны, закрытие с одной доезжает до
    /// другой. Настоящие апгрейды поверх этого гоняет `app.rs`, тут проверяется
    /// сам сплайс.
    #[tokio::test]
    async fn bytes_go_both_ways_and_a_close_reaches_the_other_side() {
        let (mut browser, browser_side) = tokio::io::duplex(4096);
        let (mut agent, agent_side) = tokio::io::duplex(4096);
        tokio::spawn(splice(
            browser_side,
            agent_side,
            "dash".into(),
            "websocket".into(),
            None,
        ));

        browser.write_all(b"\x81\x03abc").await.unwrap();
        let mut got = [0u8; 5];
        within("кадр браузера до агента", agent.read_exact(&mut got)).await.unwrap();
        assert_eq!(&got, b"\x81\x03abc");

        agent.write_all(b"\x81\x03xyz").await.unwrap();
        within("кадр агента до браузера", browser.read_exact(&mut got)).await.unwrap();
        assert_eq!(&got, b"\x81\x03xyz");

        // Браузер ушёл: агент обязан увидеть конец, а не ждать вечно. Ждём с
        // потолком именно поэтому: без проброса конца это ожидание вечное.
        drop(browser);
        let mut rest = Vec::new();
        within("конец потока до агента", agent.read_to_end(&mut rest)).await.unwrap();
        assert!(rest.is_empty());
    }

    #[tokio::test]
    async fn deadline_closes_at_a_frame_boundary() {
        let (mut browser, browser_side) = tokio::io::duplex(4096);
        let (mut agent, agent_side) = tokio::io::duplex(4096);
        tokio::spawn(splice(
            browser_side,
            agent_side,
            "dash".into(),
            "websocket".into(),
            Some(Duration::from_millis(150)),
        ));

        // Кадр начат, но не дописан: срок наступает ровно посреди него.
        agent.write_all(b"\x81\x05ab").await.unwrap();
        let mut head = [0u8; 4];
        within("начало кадра", browser.read_exact(&mut head)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        agent.write_all(b"cde").await.unwrap();

        // Сначала доезжает хвост начатого кадра, и только потом наш close:
        // всунутый в середину, он приехал бы браузеру мусором.
        let mut tail = [0u8; 3];
        within("хвост начатого кадра", browser.read_exact(&mut tail)).await.unwrap();
        assert_eq!(&tail, b"cde");
        let mut close = [0u8; 4];
        within("закрытие браузеру", browser.read_exact(&mut close)).await.unwrap();
        assert_eq!(close, SERVER_CLOSE_GOING_AWAY);

        // Агент получает своё закрытие тем же сроком, маскированное.
        let mut theirs = [0u8; 8];
        within("закрытие агенту", agent.read_exact(&mut theirs)).await.unwrap();
        assert_eq!(theirs[0], 0x88);
        assert_eq!(theirs[6] ^ theirs[2], 0x03);
        assert_eq!(theirs[7] ^ theirs[3], 0xE9);
    }
}
