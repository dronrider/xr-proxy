//! Исходящие соединения с привязкой к каналу провайдера (XR-310).
//!
//! Сокет привязывается к интерфейсу через SO_BINDTODEVICE: имя интерфейса
//! переживает обновление DHCP-аренды второго роутера, не требует своих
//! `ip rule` и таблиц и не затрагивает fwmark 0x200 и таблицу 201 TPROXY.
//! Мёртвый канал даёт быстрый отказ (ENETUNREACH), основной маршрут ОС не
//! подставляется. `None` означает основной маршрут без привязки.

use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpStream;

/// Привязать сокет к интерфейсу канала. На не-Linux системах привязки нет:
/// клиент живёт на OpenWRT, а сборка и тесты идут и на других Unix.
fn bind_to_device(sock: &Socket, interface: &str) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        sock.bind_device(Some(interface.as_bytes()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = (sock, interface);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("привязка к интерфейсу '{interface}' поддержана только на Linux"),
        ))
    }
}

/// Открыть TCP-соединение с `addr` через выбранный канал: `Some(имя)` держит
/// сокет на интерфейсе канала, `None` отпускает на основной маршрут ОС.
/// Отвечает тем же контрактом, что `tunnel::connect_to_server`: таймаут на
/// всю попытку и `TcpStream` на выходе.
pub async fn connect_via(
    interface: Option<&str>,
    addr: SocketAddr,
    timeout: Duration,
) -> io::Result<TcpStream> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let sock = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    if let Some(name) = interface {
        bind_to_device(&sock, name)?;
    }
    // from_std_stream требует неблокирующий сокет; connect сам ждёт
    // EINPROGRESS и регистрирует дескриптор в реакторе tokio.
    sock.set_nonblocking(true)?;
    let connect = tokio::net::TcpSocket::from_std_stream(sock.into()).connect(addr);
    match tokio::time::timeout(timeout, connect).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("connect to {addr} timed out"),
        )),
    }
}

/// Соединение с сервером туннеля через канал: тот же контракт, что
/// `xr_proto::tunnel::connect_to_server`, плюс привязка к интерфейсу и
/// nodelay на mux-сокете.
pub async fn tunnel_connect(interface: Option<&str>, addr: SocketAddr) -> io::Result<TcpStream> {
    tunnel_connect_checked(interface, addr, interface_available).await
}

async fn tunnel_connect_checked(
    interface: Option<&str>,
    addr: SocketAddr,
    available: fn(&str) -> bool,
) -> io::Result<TcpStream> {
    if interface.is_some_and(|name| !available(name)) {
        return Err(io::Error::new(
            io::ErrorKind::NetworkUnreachable,
            "tunnel WAN has no usable IPv4 uplink",
        ));
    }
    let stream = connect_via(interface, addr, Duration::from_secs(2)).await?;
    // Nagle off на mux-сокете: контрольные кадры (Connect) крошечные и должны
    // уходить сразу, а не коалесцироваться за неотправленной upload-Data.
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn tunnel_rejects_unavailable_ipv4_before_binding_socket() {
        // carrier=1 не означает доступность IPv4: getifaddrs может уже не
        // вернуть адрес. На macOS подставляем такой ответ проверки, чтобы
        // результат не зависел от интерфейсов машины и SO_BINDTODEVICE.
        let err = tunnel_connect_checked(
            Some("wan-without-ipv4"),
            "127.0.0.1:1".parse().unwrap(),
            |_| false,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.kind(),
            io::ErrorKind::NetworkUnreachable,
            "без IPv4 нельзя запускать bind/connect или уходить основным маршрутом"
        );
    }

    #[tokio::test]
    async fn connect_without_interface_uses_default_route() {
        // Локальный слушатель отвечает без привязки: путь None обязан
        // работать так же, как обычный TcpStream::connect.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = connect_via(None, addr, Duration::from_secs(2))
            .await
            .unwrap();
        assert!(stream.peer_addr().unwrap() == addr);
    }

    #[tokio::test]
    async fn refused_connection_propagates_error() {
        // Порт без слушателя обязан давать ошибку соединения, а не успех:
        // слушатель поднимается и закрывается, чтобы занять заведомо
        // свободный порт.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let err = connect_via(None, addr, Duration::from_secs(2))
            .await
            .expect_err("порт без слушателя обязан отказывать");
        assert_ne!(err.kind(), io::ErrorKind::TimedOut, "{err}");
    }

    #[tokio::test]
    async fn bind_to_unknown_interface_fails_fast() {
        // Несуществующий интерфейс отказывает сразу: на Linux это ENODEV,
        // на прочих системах явный Unsupported. Оба лучше молчаливого ухода
        // через основной маршрут.
        let err = connect_via(
            Some("nosuchwan0"),
            "127.0.0.1:9".parse().unwrap(),
            Duration::from_secs(2),
        )
        .await
        .expect_err("несуществующий интерфейс обязан отказывать");
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::Unsupported | io::ErrorKind::NotFound
            ),
            "{err}"
        );
    }
}

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use xr_proto::config::WanPoolMember;
use xr_proto::mux::MuxStream;
use xr_proto::protocol::TargetAddr;
use xr_proto::server_pool::ServerPool;

fn interface_available(interface: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        if std::path::Path::new(&format!("/var/run/xr-wan-pool-watch/down.{interface}")).exists() {
            return false;
        }
        if std::fs::read_to_string(format!("/sys/class/net/{interface}/carrier"))
            .is_ok_and(|value| value.trim() == "0")
        {
            return false;
        }
        // ifdown может снять адрес раньше, чем пропадёт carrier.
        unsafe {
            let mut head = std::ptr::null_mut();
            if libc::getifaddrs(&mut head) != 0 {
                return false;
            }
            let mut current = head;
            let mut available = false;
            while !current.is_null() {
                let address = &*current;
                if !address.ifa_name.is_null()
                    && std::ffi::CStr::from_ptr(address.ifa_name).to_bytes() == interface.as_bytes()
                    && address.ifa_flags & libc::IFF_UP as u32 != 0
                    && !address.ifa_addr.is_null()
                    && (*address.ifa_addr).sa_family as i32 == libc::AF_INET
                {
                    available = true;
                    break;
                }
                current = address.ifa_next;
            }
            libc::freeifaddrs(head);
            available
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = interface;
        true
    }
}

pub struct WanPool {
    members: Vec<WanPoolMember>,
    selection: Mutex<Selection>,
    available: fn(&str) -> bool,
    pub pools: HashMap<String, Vec<Arc<ServerPool>>>,
}

struct Selection {
    next: u64,
    failed_until: Vec<Option<Instant>>,
}

impl WanPool {
    pub fn new(members: Vec<WanPoolMember>, pools: HashMap<String, Vec<Arc<ServerPool>>>) -> Self {
        Self {
            selection: Mutex::new(Selection {
                next: 0,
                failed_until: vec![None; members.len()],
            }),
            members,
            pools,
            available: interface_available,
        }
    }

    fn order_at(&self, now: Instant) -> Vec<usize> {
        let mut selection = self.selection.lock().unwrap();
        let healthy: Vec<usize> = (0..self.members.len())
            .filter(|&i| selection.failed_until[i].is_none_or(|until| now >= until))
            .collect();
        // Все каналы недоступны: всё равно пробуем их, без выхода через ОС.
        let candidates: Vec<usize> = if healthy.is_empty() {
            (0..self.members.len()).collect()
        } else {
            healthy
        };
        let total: u64 = candidates
            .iter()
            .map(|&i| self.members[i].weight as u64)
            .sum();
        if total == 0 {
            return vec![];
        }
        let mut ticket = selection.next % total;
        selection.next = selection.next.wrapping_add(1);
        let first = *candidates
            .iter()
            .find(|&&i| {
                if ticket < self.members[i].weight as u64 {
                    true
                } else {
                    ticket -= self.members[i].weight as u64;
                    false
                }
            })
            .unwrap();
        std::iter::once(first)
            .chain(candidates.into_iter().filter(|&i| i != first))
            .collect()
    }

    fn failed(&self, index: usize) {
        self.selection.lock().unwrap().failed_until[index] =
            Some(Instant::now() + Duration::from_secs(5));
    }

    fn restored(&self, index: usize) {
        self.selection.lock().unwrap().failed_until[index] = None;
    }

    pub async fn connect(&self, addr: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
        let mut error = io::Error::new(io::ErrorKind::NotConnected, "WAN pool exhausted");
        for i in self.order_at(Instant::now()) {
            let interface = &self.members[i].interface;
            if !(self.available)(interface) {
                continue;
            }
            match connect_via(Some(interface), addr, timeout).await {
                Ok(stream) => {
                    self.restored(i);
                    tracing::info!("WAN pool direct {} via {}", addr, interface);
                    return Ok(stream);
                }
                Err(e) => {
                    // Refused/timeout могут относиться к цели, а не к провайдеру.
                    if matches!(
                        e.raw_os_error(),
                        Some(libc::ENETUNREACH | libc::ENODEV | libc::ENETDOWN)
                    ) {
                        self.failed(i);
                    }
                    error = e;
                }
            }
        }
        Err(error)
    }

    pub async fn open_stream(&self, key: &str, target: &TargetAddr) -> io::Result<MuxStream> {
        let pools = self
            .pools
            .get(key)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "WAN proxy pool missing"))?;
        let mut error = io::Error::new(io::ErrorKind::NotConnected, "WAN proxy pool exhausted");
        for i in self.order_at(Instant::now()) {
            if !(self.available)(&self.members[i].interface) {
                continue;
            }
            match pools[i].open_stream(target).await {
                Ok(stream) => {
                    self.restored(i);
                    tracing::info!(
                        "WAN pool proxy {:?} via {}",
                        target,
                        self.members[i].interface
                    );
                    return Ok(stream);
                }
                Err(e) if e.kind() == io::ErrorKind::InvalidInput => return Err(e),
                Err(e) => {
                    self.failed(i);
                    error = e;
                }
            }
        }
        Err(error)
    }
}

#[cfg(test)]
mod pool_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncReadExt;
    use xr_proto::mux::{mux_handshake_server, Multiplexer};
    use xr_proto::mux_pool::MuxPool;
    use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};
    use xr_proto::protocol::{Codec, Command};
    use xr_proto::server_pool::{PoolProfile, PoolServer};

    fn test_pool(
        members: Vec<WanPoolMember>,
        pools: HashMap<String, Vec<Arc<ServerPool>>>,
    ) -> WanPool {
        let mut pool = WanPool::new(members, pools);
        pool.available = |_| true;
        pool
    }

    fn members() -> Vec<WanPoolMember> {
        vec![
            WanPoolMember {
                interface: "wan".into(),
                weight: 1,
            },
            WanPoolMember {
                interface: "lan1".into(),
                weight: 3,
            },
        ]
    }

    #[test]
    fn weighted_new_connections_and_recovery() {
        let pool = test_pool(members(), HashMap::new());
        let now = Instant::now();
        let mut counts = [0; 2];
        for _ in 0..400 {
            counts[pool.order_at(now)[0]] += 1;
        }
        assert_eq!(counts, [100, 300]);
        pool.failed(1);
        for _ in 0..40 {
            assert_eq!(pool.order_at(now), vec![0]);
        }
        let later = Instant::now() + Duration::from_secs(6);
        counts = [0; 2];
        for _ in 0..400 {
            counts[pool.order_at(later)[0]] += 1;
        }
        assert_eq!(counts, [100, 300]);
        pool.failed(0);
        assert!(
            !pool.order_at(now).is_empty(),
            "all-down still probes configured WANs"
        );
    }

    fn codec() -> Codec {
        Codec::new(
            Obfuscator::new(vec![42; 32], 12, ModifierStrategy::PositionalXorRotate),
            0,
            0,
        )
    }

    async fn serving_pool(counter: Arc<AtomicUsize>, failed: bool) -> Arc<ServerPool> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = counter.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let served = served.clone();
                tokio::spawn(async move {
                    let codec = codec();
                    let mut bytes = vec![0; 4096];
                    let mut filled = 0;
                    let init = loop {
                        let Ok(n) = stream.read(&mut bytes[filled..]).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        filled += n;
                        match codec.decode_frame(&bytes[..filled]) {
                            Ok(Some((frame, _))) => break frame,
                            Ok(None) => continue,
                            Err(_) => return,
                        }
                    };
                    let Ok(Some(caps)) = mux_handshake_server(&mut stream, &codec, &init).await
                    else {
                        return;
                    };
                    let mux = Multiplexer::new_server(stream, codec, caps);
                    let Some(mut rx) = mux.take_new_stream_rx().await else {
                        return;
                    };
                    while let Some(ns) = rx.recv().await {
                        served.fetch_add(1, Ordering::Relaxed);
                        let _ = mux
                            .send_frame(ns.stream_id, Command::ConnectAck, vec![0])
                            .await;
                        let _ = mux.register_stream(ns.stream_id).await;
                    }
                });
            }
        });
        let connect = Arc::new(move || {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::Relaxed);
                if failed {
                    return Err(io::Error::new(io::ErrorKind::NotConnected, "WAN down"));
                }
                TcpStream::connect(addr).await
            })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = io::Result<TcpStream>> + Send>,
                >
        });
        ServerPool::new(
            vec![PoolServer {
                name: "test".into(),
                addr: addr.to_string(),
                pool: MuxPool::new(connect, codec(), 1),
            }],
            PoolProfile::mobile(),
            None,
        )
    }

    #[tokio::test]
    async fn proxy_streams_balance_even_with_one_tunnel_per_wan_and_fail_over() {
        let a = Arc::new(AtomicUsize::new(0));
        let b = Arc::new(AtomicUsize::new(0));
        let pa = serving_pool(a.clone(), false).await;
        let pb = serving_pool(b.clone(), false).await;
        let pool = test_pool(members(), HashMap::from([("global".into(), vec![pa, pb])]));
        let target = TargetAddr::Domain("example.com".into(), 443);
        for _ in 0..40 {
            let stream = pool.open_stream("global", &target).await.unwrap();
            assert!(stream.is_alive());
        }
        // Один connect и 10/30 Connect-кадров на каждом канале.
        assert_eq!(a.load(Ordering::Relaxed), 11);
        assert_eq!(b.load(Ordering::Relaxed), 31);
        let bad = serving_pool(a.clone(), true).await;
        let good = serving_pool(b.clone(), false).await;
        let pool = test_pool(
            members(),
            HashMap::from([("global".into(), vec![bad, good])]),
        );
        assert!(pool
            .open_stream("global", &target)
            .await
            .unwrap()
            .is_alive());
        assert_eq!(pool.order_at(Instant::now()), vec![1]);
    }

    #[tokio::test]
    async fn known_down_wan_is_skipped_without_touching_server_health() {
        let down_calls = Arc::new(AtomicUsize::new(0));
        let live_calls = Arc::new(AtomicUsize::new(0));
        let down = serving_pool(down_calls.clone(), true).await;
        let live = serving_pool(live_calls.clone(), false).await;
        let mut pool = test_pool(
            members(),
            HashMap::from([("global".into(), vec![down, live])]),
        );
        pool.available = |interface| interface != "wan";
        let target = TargetAddr::Domain("example.com".into(), 443);
        for _ in 0..4 {
            assert!(tokio::time::timeout(
                Duration::from_millis(500),
                pool.open_stream("global", &target)
            )
            .await
            .unwrap()
            .unwrap()
            .is_alive());
        }
        assert_eq!(down_calls.load(Ordering::Relaxed), 0);
        assert_eq!(live_calls.load(Ordering::Relaxed), 5);
        assert!(pool
            .selection
            .lock()
            .unwrap()
            .failed_until
            .iter()
            .all(Option::is_none));
    }

    #[tokio::test]
    async fn known_down_direct_wan_is_skipped_before_any_socket_attempt() {
        let mut pool = test_pool(
            vec![WanPoolMember {
                interface: "nosuchwan0".into(),
                weight: 1,
            }],
            HashMap::new(),
        );
        pool.available = |_| false;
        let error = pool
            .connect("127.0.0.1:9".parse().unwrap(), Duration::from_secs(30))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
        assert!(pool.selection.lock().unwrap().failed_until[0].is_none());
    }

    #[tokio::test]
    async fn all_wan_proxy_failures_return_error_without_direct_connection() {
        let counter = Arc::new(AtomicUsize::new(0));
        let a = serving_pool(counter.clone(), true).await;
        let b = serving_pool(counter.clone(), true).await;
        let pool = test_pool(members(), HashMap::from([("global".into(), vec![a, b])]));
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        assert!(pool
            .open_stream("global", &TargetAddr::Ip(target.local_addr().unwrap()))
            .await
            .is_err());
        assert!(counter.load(Ordering::Relaxed) >= 2);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), target.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn bound_direct_pool_never_uses_unbound_default_route() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pool = test_pool(
            vec![WanPoolMember {
                interface: "nosuchwan0".into(),
                weight: 1,
            }],
            HashMap::new(),
        );
        assert!(pool
            .connect(target.local_addr().unwrap(), Duration::from_millis(100))
            .await
            .is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(30), target.accept())
                .await
                .is_err()
        );
    }
}
