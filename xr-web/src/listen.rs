//! Листенер фронта: HTTP/1.1 на локальном порту, при своём `[tls]` с
//! терминацией (LLD-38 п. 3.3).
//!
//! Сертификатами `xr-web` по умолчанию не занимается: он стоит за тем же
//! фронтом, что выводит наружу хаб, и wildcard живёт там. Блок `[tls]` это
//! установка без фронта, и провайдер там тот же ring, что у пиннинга агента:
//! второй крипто-бэкенд в одном процессе ссорится с ним за умолчание.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;

use crate::app::PeerAddr;
use crate::config::TlsConfig;

/// Принимать соединения, пока листенер жив. Каждое обслуживается своим таском:
/// упавшее соединение не трогает соседей.
pub async fn serve(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    router: Router,
) -> anyhow::Result<()> {
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("accept: {e}");
                return Err(e.into());
            }
        };
        sock.set_nodelay(true).ok();
        let router = router.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            match tls {
                Some(acceptor) => match acceptor.accept(sock).await {
                    Ok(stream) => serve_conn(stream, peer, router).await,
                    Err(e) => tracing::debug!("TLS-хендшейк с {peer} не состоялся: {e}"),
                },
                None => serve_conn(sock, peer, router).await,
            }
        });
    }
}

/// Одно соединение браузера. Адрес кладётся в запрос расширением: за фронтом
/// его называет заголовок, без фронта взять его больше неоткуда, а лимиту
/// попыток и логу входа адрес нужен.
pub async fn serve_conn<I>(io: I, peer: SocketAddr, router: Router)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let router = router.clone();
        async move {
            let mut req = req.map(axum::body::Body::new);
            req.extensions_mut().insert(PeerAddr(peer));
            router.oneshot(req).await
        }
    });
    // `with_upgrades` тут не ради фазы 2: апгрейд до WebSocket (фаза 3)
    // ложится сверху, не переписывая листенер.
    if let Err(e) = hyper::server::conn::http1::Builder::new()
        .serve_connection(hyper_util::rt::TokioIo::new(io), service)
        .with_upgrades()
        .await
    {
        tracing::debug!("соединение с {peer} закрылось: {e}");
    }
}

/// Собрать акцептор из пары «сертификат, ключ».
pub fn tls_acceptor(cfg: &TlsConfig) -> anyhow::Result<TlsAcceptor> {
    let certs = load_certs(&cfg.cert)?;
    let key = load_key(&cfg.key)?;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| anyhow::anyhow!("версии TLS: {e}"))?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|e| anyhow::anyhow!("сертификат и ключ не сошлись: {e}"))?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn load_certs(path: &std::path::Path) -> anyhow::Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let data = std::fs::read(path).with_context(|| format!("чтение {}", path.display()))?;
    let certs: Result<Vec<_>, _> = rustls_pemfile::certs(&mut data.as_slice()).collect();
    let certs = certs.with_context(|| format!("разбор {}", path.display()))?;
    if certs.is_empty() {
        anyhow::bail!("в {} нет ни одного сертификата", path.display());
    }
    Ok(certs)
}

fn load_key(path: &std::path::Path) -> anyhow::Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let data = std::fs::read(path).with_context(|| format!("чтение {}", path.display()))?;
    rustls_pemfile::private_key(&mut data.as_slice())
        .with_context(|| format!("разбор {}", path.display()))?
        .ok_or_else(|| anyhow::anyhow!("в {} нет приватного ключа", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Соединение целиком, как его увидит браузер: сырой HTTP/1.1 по duplex,
    /// без сети и без порта. Проверяется склейка листенера с роутером, ради
    /// которой отдельный тест и нужен: сам роутер тесты гоняют напрямую.
    #[tokio::test]
    async fn raw_http_request_reaches_the_router() {
        let state = crate::app::tests_support_state();
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let peer: SocketAddr = "198.51.100.9:40000".parse().unwrap();
        tokio::spawn(serve_conn(server, peer, crate::app::router(state)));

        client
            .write_all(b"GET /.xr-web/healthz HTTP/1.1\r\nHost: dash.web.example.com\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut answer = String::new();
        client.read_to_string(&mut answer).await.unwrap();
        assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
        assert!(answer.trim_end().ends_with("ok"), "{answer}");
    }

    #[tokio::test]
    async fn request_without_session_gets_the_login_page() {
        let state = crate::app::tests_support_state();
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let peer: SocketAddr = "198.51.100.9:40000".parse().unwrap();
        tokio::spawn(serve_conn(server, peer, crate::app::router(state)));

        client
            .write_all(b"GET /page HTTP/1.1\r\nHost: dash.web.example.com\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut answer = String::new();
        client.read_to_string(&mut answer).await.unwrap();
        assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
        assert!(answer.contains("/.xr-web/login"), "{answer}");
    }
}
