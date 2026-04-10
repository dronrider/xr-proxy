//! Client-side multiplexed connection pool.
//!
//! Manages a single persistent TCP connection to the server with
//! automatic reconnection.
//!
//! ```text
//! open_stream(target) ─→ [MuxPool] ─→ get/create Multiplexer ─→ MuxStream
//! ```

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::future::Future;

use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::mux::{mux_handshake_client, mux_open_stream, Multiplexer, MuxStream};
use crate::protocol::{Codec, TargetAddr};

/// Factory for creating TCP connections to the server.
/// On Android, this wraps connect_protected(); on router, plain TcpStream::connect().
pub type ConnectFn = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = io::Result<TcpStream>> + Send>> + Send + Sync,
>;

/// Client-side connection pool over a multiplexed tunnel.
pub struct MuxPool {
    connect_fn: ConnectFn,
    codec: Codec,
    current: Mutex<Option<Arc<Multiplexer>>>,
}

impl MuxPool {
    pub fn new(connect_fn: ConnectFn, codec: Codec) -> Arc<Self> {
        Arc::new(Self {
            connect_fn,
            codec,
            current: Mutex::new(None),
        })
    }

    /// Open a new logical stream to the target through the multiplexed connection.
    ///
    /// - If no connection exists, establishes one (TCP + MuxInit handshake).
    /// - If the connection is dead, reconnects automatically.
    pub async fn open_stream(&self, target: &TargetAddr) -> io::Result<MuxStream> {
        let mux = self.get_or_connect().await?;
        match mux_open_stream(&mux, target).await {
            Ok(stream) => Ok(stream),
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe
                    || e.kind() == io::ErrorKind::TimedOut => {
                // Connection died or stale — reconnect and retry once.
                tracing::debug!("mux stream failed ({}), reconnecting", e);
                self.invalidate().await;
                let mux = self.get_or_connect().await?;
                mux_open_stream(&mux, target).await
            }
            Err(e) => Err(e),
        }
    }

    /// Get existing multiplexer or create a new connection.
    async fn get_or_connect(&self) -> io::Result<Arc<Multiplexer>> {
        let mut guard = self.current.lock().await;

        // Return existing if alive.
        if let Some(ref mux) = *guard {
            if mux.is_alive() {
                return Ok(mux.clone());
            }
        }

        // Connect.
        let mut stream = (self.connect_fn)().await?;

        // MuxInit handshake.
        match mux_handshake_client(&mut stream, &self.codec).await {
            Ok(true) => {
                let mux = Multiplexer::new_client(stream, self.codec.clone());
                *guard = Some(mux.clone());
                tracing::info!("mux connection established");
                Ok(mux)
            }
            Ok(false) => {
                Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "server rejected mux handshake",
                ))
            }
            Err(e) => Err(e),
        }
    }

    /// Invalidate the current connection (e.g., after it died).
    async fn invalidate(&self) {
        let mut guard = self.current.lock().await;
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obfuscation::{ModifierStrategy, Obfuscator};

    fn test_codec() -> Codec {
        let key = b"test-key-32-bytes-long-enough!!!".to_vec();
        let obfs = Obfuscator::new(key, 0xDEADBEEF, ModifierStrategy::PositionalXorRotate);
        Codec::new(obfs, 0, 0)
    }

    #[tokio::test]
    async fn test_pool_connect_error_propagates() {
        let codec = test_codec();

        let connect_fn: ConnectFn = Arc::new(move || {
            Box::pin(async move {
                Err(io::Error::new(io::ErrorKind::ConnectionRefused, "test"))
            })
        });

        let pool = MuxPool::new(connect_fn, codec);
        let err = pool.open_stream(&TargetAddr::Domain("test.com".to_string(), 443)).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }
}
