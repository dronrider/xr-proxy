//! XR-299: чувствительная цель уходит через резидентский SOCKS5 и никогда
//! напрямую. Гоняет свежий бинарь xr-server целиком: конфиг с правилом
//! `outbound`, клиент со старым (не-mux) протоколом, фейковый резидент и
//! канарейка прямого пути. На старом коде сервер лезет к цели напрямую,
//! канарейка отвечает отказом соединения, и ответ через резидента не
//! приходит - тест падает, то есть ловит именно выпавшую гарантию.

use std::path::PathBuf;
use std::process::{Child, Command as ProcCommand, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use xr_proto::config::decode_key;
use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};
use xr_proto::protocol::{Codec, Command, Frame, TargetAddr};

/// Конфиг во временном файле, убирается за собой.
struct TempConfig {
    path: PathBuf,
}

impl TempConfig {
    fn new(tag: &str, body: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "xr-server-outbound-{tag}-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, body).expect("запись тестового конфига");
        Self { path }
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Дочерний процесс сервера: убивается на выходе из теста в любом исходе.
struct ServerGuard {
    child: Child,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Свободный TCP-порт: листенер берётся и сразу закрывается.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("свободный порт");
    l.local_addr().unwrap().port()
}

/// Канарейка прямого пути: принимает соединения и рвёт их. Прямой
/// connect сервера к ней выдаёт себя отсутствием ответа на эхо.
struct DirectCanary {
    addr: std::net::SocketAddr,
    accepts: Arc<AtomicUsize>,
}

impl DirectCanary {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("листенер канарейки");
        let addr = listener.local_addr().unwrap();
        let accepts = Arc::new(AtomicUsize::new(0));
        let counter = accepts.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });
        Self { addr, accepts }
    }
}

/// Запрос на фейковом резиденте.
struct Recorded {
    atyp: u8,
    host: Vec<u8>,
    port: u16,
}

/// Фейковый SOCKS5-резидент: диалог ведёт честно, на CONNECT отвечает
/// успехом и дальше эхоит байты, никуда не соединяясь. Ответ приходит
/// только если соединение действительно прошло через него.
struct FakeSocks5 {
    addr: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl FakeSocks5 {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("листенер резидента");
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let _ = serve(&mut stream, recorded).await;
                });
            }
        });
        Self { addr, requests }
    }
}

async fn serve(stream: &mut TcpStream, recorded: Arc<Mutex<Vec<Recorded>>>) -> std::io::Result<()> {
    let mut methods = [0u8; 2];
    stream.read_exact(&mut methods).await?;
    let mut offered = vec![0u8; methods[1] as usize];
    stream.read_exact(&mut offered).await?;
    // Аутентификация не требуется: нулевой метод, клиент её и не просит.
    stream.write_all(&[0x05, 0x00]).await?;

    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    let atyp = head[3];
    let (host, port) = match atyp {
        0x01 => {
            let mut ip = [0u8; 4];
            stream.read_exact(&mut ip).await?;
            let mut p = [0u8; 2];
            stream.read_exact(&mut p).await?;
            (ip.to_vec(), u16::from_be_bytes(p))
        }
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            let mut host = vec![0u8; len[0] as usize];
            stream.read_exact(&mut host).await?;
            let mut p = [0u8; 2];
            stream.read_exact(&mut p).await?;
            (host, u16::from_be_bytes(p))
        }
        other => panic!("неожиданный atyp в запросе: {other}"),
    };
    recorded.lock().unwrap().push(Recorded { atyp, host, port });
    stream
        .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;

    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        stream.write_all(&buf[..n]).await?;
    }
    Ok(())
}

/// Клиентская читалка кадров: держит непрочитанный хвост между вызовами.
struct FrameReader {
    buf: Vec<u8>,
    filled: usize,
}

impl FrameReader {
    fn new() -> Self {
        Self { buf: vec![0u8; 8192], filled: 0 }
    }

    async fn next(&mut self, sock: &mut TcpStream, codec: &Codec) -> Frame {
        loop {
            if let Some((frame, consumed)) = codec.decode_frame(&self.buf[..self.filled]).unwrap() {
                self.buf.copy_within(consumed..self.filled, 0);
                self.filled -= consumed;
                return frame;
            }
            if self.filled == self.buf.len() {
                self.buf.resize(self.buf.len() * 2, 0);
            }
            let n = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                sock.read(&mut self.buf[self.filled..]),
            )
            .await
            .expect("сервер обязан ответить кадром, а не молчать")
            .expect("сокет оборван без кадра");
            assert!(n > 0, "сервер закрыл сокет, не ответив кадром");
            self.filled += n;
        }
    }
}

#[tokio::test]
async fn sensitive_target_exits_through_resident_and_never_directly() {
    let socks = FakeSocks5::start().await;
    let canary = DirectCanary::start().await;
    let server_port = free_port();

    // Ключ 32 байта в base64: клиент и сервер раскодируют его одним и тем же
    // decode_key, обфускация совпадает байт в байт.
    let key = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=";
    let config_body = format!(
        r#"
[server]
listen = "127.0.0.1"
port = {server_port}

[obfuscation]
key = "{key}"
modifier = "positional_xor_rotate"
salt = 42

[routing]
default_action = "direct"

[[routing.rules]]
action = "proxy"
ip_ranges = ["127.0.0.1/32"]
outbound = "resident"

[outbound.resident]
type = "socks5"
address = "127.0.0.1"
port = {socks_port}
"#,
        socks_port = socks.addr.port(),
    );
    let cfg = TempConfig::new("resident", &config_body);

    let mut child = ProcCommand::new(env!("CARGO_BIN_EXE_xr-server"))
        .arg("-c")
        .arg(&cfg.path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("запуск xr-server");
    let stderr_pipe = child.stderr.take();
    let mut guard = ServerGuard { child };
    // Дождаться, пока листенер встанет: проба закрывается сразу, хендшейк
    // на ней сервер обрывает без вреда тесту.
    let mut ready = false;
    for _ in 0..100 {
        match tokio::time::timeout(
            std::time::Duration::from_millis(200),
            TcpStream::connect(("127.0.0.1", server_port)),
        )
        .await
        {
            Ok(Ok(probe)) => {
                drop(probe);
                ready = true;
                break;
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
    if !ready {
        let _ = guard.child.kill();
        let _ = guard.child.wait();
        let mut log = String::new();
        if let Some(mut pipe) = stderr_pipe {
            let _ = std::io::Read::read_to_string(&mut pipe, &mut log);
        }
        panic!("сервер не поднялся за 5 секунд: {log}");
    }

    // Старый клиентский протокол: обфусцированный Connect, без mux и без
    // каких-либо знаний про outbound.
    let raw_key = decode_key(key).expect("валидный base64 ключа");
    let obfs = Obfuscator::new(raw_key, 42, ModifierStrategy::PositionalXorRotate);
    let codec = Codec::new(obfs, 0, 0);

    let mut client = TcpStream::connect(("127.0.0.1", server_port))
        .await
        .expect("клиентский коннект к серверу");
    let payload = TargetAddr::Ip(canary.addr).encode().expect("кодирование цели");
    client
        .write_all(&codec.encode_frame(Command::Connect, &payload).unwrap())
        .await
        .expect("отправка Connect");

    let mut reader = FrameReader::new();
    let ack = reader.next(&mut client, &codec).await;
    assert_eq!(ack.command, Command::ConnectAck, "сервер отвечает ConnectAck");

    client
        .write_all(&codec.encode_frame(Command::Data, b"ping").unwrap())
        .await
        .expect("отправка данных");

    // Ответ обязан вернуться через резидента: эхо даёт только он, канарейка
    // соединение рвёт. На старом коде придёт Close или EOF - тест упадёт.
    let echo = reader.next(&mut client, &codec).await;
    assert_eq!(
        echo.command,
        Command::Data,
        "данные возвращаются кадром Data, получен {:?}",
        echo.command
    );
    assert_eq!(echo.payload, b"ping", "эхо пришло через резидента");

    let requests = socks.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "резидент обязан получить запрос на цель");
    assert_eq!(requests[0].atyp, 0x01, "IP-цель уходит адресом");
    let octets = [127u8, 0, 0, 1];
    assert_eq!(requests[0].host, octets, "адрес цели дошёл до резидента");
    assert_eq!(requests[0].port, canary.addr.port(), "порт цели дошёл до резидента");
    assert_eq!(
        canary.accepts.load(Ordering::SeqCst),
        0,
        "прямого соединения с целью быть не должно"
    );
}
