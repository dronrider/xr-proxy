//! Бюджет памяти движка под лимит Network Extension (XR-272, LLD-39).
//!
//! На iOS движок будет жить в процессе расширения `NEPacketTunnelProvider`, а
//! у того потолок 50 МБ на всё сразу: системные фреймворки, Swift-обвязка и
//! наше ядро. Здесь меряются две доли, из которых складывается наша часть:
//! постоянная цена поднятого движка с живым туннелем и цена одной TCP-сессии.
//! Мерка это RSS процесса теста, то есть числа с запасом (в них сидит и сам
//! харнесс), и они привязаны к машине; ценность в порядке величины и в том,
//! что порядок не уезжает незамеченным.
//!
//! Тест лежит отдельным файлом, потому что RSS меряется на весь процесс:
//! соседний тест в том же бинаре давал бы свои аллокации в наши дельты.
//!
//! Числа печатает `cargo test -p xr-core --test ios_memory_budget -- --nocapture`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

use xr_core::engine::{VpnConfig, VpnEngine, SESSION_RX_BUF, SESSION_TX_BUF};
use xr_core::ip_stack::{IpStack, PacketQueue};
use xr_core::state::VpnState;
use xr_proto::config::RoutingConfig;
use xr_proto::mux::{mux_handshake_server, Multiplexer};
use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};
use xr_proto::protocol::{Codec, Command};

/// Потолок процесса расширения на iOS 15+ (до iOS 15 было 15 МБ).
const NE_LIMIT: u64 = 50 * 1024 * 1024;

/// Постоянной цене движка отдаём половину потолка. Вторая половина остаётся
/// сессиям под нагрузкой, обвязке расширения и всплескам; вылезли за половину
/// на холостом туннеле - бюджет пора считать заново, а не двигать границу.
const IDLE_LIMIT: u64 = NE_LIMIT / 2;

/// Сколько сессий держим, когда меряем цену одной. Столько же открытых TCP
/// держит браузер с парой десятков вкладок, то есть это не синтетика.
const SESSIONS: usize = 256;

/// Потолок цены сессии: аллокация буферов smoltcp плюс запас на служебное
/// (запись в таблице сессий, каналы, задача relay).
const SESSION_LIMIT: u64 = 192 * 1024;

/// Сколько сессий отыгрываем прокачанными, когда меряем худший случай.
const DIRTY_SESSIONS: usize = 64;

const KEY: &[u8] = b"ios-memory-budget-key-1234567890";
const SALT: u32 = 0x1050_1050;

fn codec() -> Codec {
    let obfs = Obfuscator::new(KEY.to_vec(), SALT, ModifierStrategy::PositionalXorRotate);
    Codec::new(obfs, 16, 128)
}

/// RSS процесса в байтах. `ps` берётся вместо чтения /proc, потому что
/// харнесс гоняется и на macOS, где /proc нет.
fn rss_bytes() -> u64 {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .expect("для мерки памяти нужен ps в PATH");
    let text = String::from_utf8_lossy(&out.stdout);
    let kib: u64 = text
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("ps вернул не число: {:?}", text.trim()));
    kib * 1024
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Туннельный сервер на локалхосте: делает ровно то же, что VPS на приёме
/// соединения, но за туннелем никого нет. Пул движка прогревается
/// по-настоящему, поэтому в постоянную цену попадают живые mux-соединения со
/// своими буферами, а не только пустой рантайм.
async fn run_tunnel_server(listener: TcpListener) {
    loop {
        let Ok((mut conn, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let codec = codec();
            let mut buf = vec![0u8; 4096];
            let mut filled = 0;
            let init = loop {
                let Ok(n) = conn.read(&mut buf[filled..]).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                filled += n;
                match codec.decode_frame(&buf[..filled]) {
                    Ok(Some((frame, _))) => break frame,
                    Ok(None) => continue,
                    Err(_) => return,
                }
            };
            if init.command != Command::MuxInit {
                return;
            }
            let Ok(Some(caps)) = mux_handshake_server(&mut conn, &codec, &init).await else {
                return;
            };
            let mux = Multiplexer::new_server(conn, codec, caps);
            // Стримов не будет: харнесс меряет холостой туннель. Держим
            // соединение живым, пока жив мультиплексор, keepalive ему
            // отвечает собственный reader.
            let Some(mut new_streams) = mux.take_new_stream_rx().await else {
                return;
            };
            while new_streams.recv().await.is_some() {}
        });
    }
}

fn config(server_port: u16) -> VpnConfig {
    use base64::Engine as _;
    VpnConfig {
        server_address: "127.0.0.1".into(),
        server_port,
        servers: vec![],
        obfuscation_key: base64::engine::general_purpose::STANDARD.encode(KEY),
        modifier: "positional_xor_rotate".into(),
        salt: SALT,
        padding_min: 16,
        padding_max: 128,
        routing: RoutingConfig {
            default_action: "direct".into(),
            rules: vec![],
        },
        geoip_path: None,
        on_server_down: "direct".into(),
        dns_resolvers: vec![],
        hub_url: None,
        hub_preset: None,
        hub_cache_dir: None,
        hub_refresh_interval_secs: None,
        hub_trusted_public_key: None,
        system_resolver: None,
        // Столько же тоннелей, сколько поднимает приложение по умолчанию.
        mux_pool_size: 4,
    }
}

#[test]
fn engine_and_sessions_stay_within_the_network_extension_budget() {
    // Рантайм такой же, каким его заводит мост Android: четыре воркера.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("рантайм должен собраться");

    let base = rss_bytes();

    let listener = rt.block_on(async { TcpListener::bind("127.0.0.1:0").await.unwrap() });
    let port = listener.local_addr().unwrap().port();
    rt.spawn(run_tunnel_server(listener));

    let mut engine = VpnEngine::new(config(port));
    {
        let _guard = rt.enter();
        engine
            .start(PacketQueue::new(), Arc::new(|_fd: i32| true))
            .expect("движок должен подняться");
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    while engine.state().get() != VpnState::Connected {
        assert!(
            Instant::now() < deadline,
            "туннель не поднялся: {}",
            engine.state().get()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Прогрев пула идёт фоновыми задачами: даём им разложиться, иначе в
    // постоянную цену попадёт не весь пул.
    std::thread::sleep(Duration::from_millis(500));

    let idle = rss_bytes();
    let idle_cost = idle.saturating_sub(base);

    // Сессии заводим тем же вызовом, что и движок на SYN. Настоящий SYN сюда
    // не гоняем: чтобы smoltcp довёл рукопожатие, тесту пришлось бы держать
    // второй стек за устройство, а меряем мы цену буферов, а не путь пакета.
    let mut stack = IpStack::new(PacketQueue::new());
    let handles: Vec<_> = (0..SESSIONS)
        .map(|_| stack.add_tcp_socket(SESSION_RX_BUF, SESSION_TX_BUF))
        .collect();
    assert_eq!(handles.len(), SESSIONS);

    let loaded = rss_bytes();
    let session_cost = loaded.saturating_sub(idle) / SESSIONS as u64;

    // Худший случай это сессия, которая свои буферы реально прокачала:
    // страницы становятся грязными и до конца сессии такими и остаются, а
    // лимит расширения считается по грязным. Довести smoltcp до этого
    // состояния без живого рукопожатия нечем, поэтому цену грязных страниц
    // меряем тем же объёмом, что лежит в буферах сессии.
    let mut dirty = Vec::with_capacity(DIRTY_SESSIONS);
    for _ in 0..DIRTY_SESSIONS {
        let mut buf = vec![0u8; SESSION_RX_BUF + SESSION_TX_BUF];
        for page in buf.chunks_mut(4096) {
            page[0] = 1;
        }
        dirty.push(buf);
    }
    let dirty_rss = rss_bytes();
    let dirty_cost = dirty_rss.saturating_sub(loaded) / DIRTY_SESSIONS as u64;
    assert_eq!(dirty.len(), DIRTY_SESSIONS);

    println!("бюджет Network Extension: {:.1} МБ", mib(NE_LIMIT));
    println!("RSS харнесса до движка:   {:.1} МБ", mib(base));
    println!(
        "холостой туннель:         +{:.1} МБ (потолок {:.1})",
        mib(idle_cost),
        mib(IDLE_LIMIT)
    );
    println!(
        "{} сессий вхолостую:      +{:.1} МБ, то есть {:.0} КиБ на сессию (аллокация {} КиБ)",
        SESSIONS,
        mib(loaded.saturating_sub(idle)),
        session_cost as f64 / 1024.0,
        (SESSION_RX_BUF + SESSION_TX_BUF) / 1024
    );
    println!(
        "{} сессий прокачанных:     +{:.1} МБ, то есть {:.0} КиБ на сессию",
        DIRTY_SESSIONS,
        mib(dirty_rss.saturating_sub(loaded)),
        dirty_cost as f64 / 1024.0
    );
    println!(
        "потолок сессий в бюджете: {} штук прокачанных при холостом туннеле {:.1} МБ",
        (NE_LIMIT - idle_cost) / dirty_cost.max(1),
        mib(idle_cost)
    );

    // Аллокация на сессию проверяется отдельно от RSS: страницы буфера
    // ложатся в резидент лениво, и подними кто-нибудь размер буфера вчетверо,
    // мерка RSS этого может не заметить, а бюджет расширения заметит.
    assert!(
        (SESSION_RX_BUF + SESSION_TX_BUF) as u64 <= SESSION_LIMIT,
        "буферы сессии выросли до {} КиБ при потолке {} КиБ: пересчитай бюджет iOS",
        (SESSION_RX_BUF + SESSION_TX_BUF) / 1024,
        SESSION_LIMIT / 1024
    );
    assert!(
        idle_cost <= IDLE_LIMIT,
        "холостой движок стоит {:.1} МБ при потолке {:.1} МБ: в расширение iOS он не поместится",
        mib(idle_cost),
        mib(IDLE_LIMIT)
    );
    assert!(
        session_cost <= SESSION_LIMIT,
        "сессия стоит {:.0} КиБ при потолке {} КиБ",
        session_cost as f64 / 1024.0,
        SESSION_LIMIT / 1024
    );
    assert!(
        dirty_cost <= SESSION_LIMIT,
        "прокачанная сессия стоит {:.0} КиБ при потолке {} КиБ",
        dirty_cost as f64 / 1024.0,
        SESSION_LIMIT / 1024
    );
    // Обратная граница: страницы буфера обязаны попадать в резидент, когда по
    // ним пишут. Не попади они, мерка вхолостую выглядела бы так же дёшево, и
    // весь бюджет считался бы по ложному нулю.
    assert!(
        dirty_cost >= (SESSION_RX_BUF + SESSION_TX_BUF) as u64 / 2,
        "грязные страницы не легли в RSS ({:.0} КиБ на сессию): мерка врёт",
        dirty_cost as f64 / 1024.0
    );

    engine.stop();
}
