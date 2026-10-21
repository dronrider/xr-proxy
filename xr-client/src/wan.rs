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
    let domain = if addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
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
pub async fn tunnel_connect(
    interface: Option<&str>,
    addr: SocketAddr,
) -> io::Result<TcpStream> {
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
    async fn connect_without_interface_uses_default_route() {
        // Локальный слушатель отвечает без привязки: путь None обязан
        // работать так же, как обычный TcpStream::connect.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = connect_via(None, addr, Duration::from_secs(2)).await.unwrap();
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
            matches!(err.kind(), io::ErrorKind::Unsupported | io::ErrorKind::NotFound),
            "{err}"
        );
    }
}
