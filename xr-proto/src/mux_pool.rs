//! Client-side multiplexed connection pool.
//!
//! Maintains N parallel TCP tunnels to the server (default 4) and
//! balances logical streams across them round-robin. The N>1 design
//! eliminates head-of-line blocking of a single TCP — a single packet
//! loss or slow-start no longer stalls every other stream.
//!
//! ```text
//! open_stream(target) ─→ [MuxPool] ─→ pick slot (RR) ─→ Multiplexer ─→ MuxStream
//! ```
//!
//! On per-slot failure (BrokenPipe / TimedOut) `open_stream` walks to
//! the next slot and tries there; failed slots are reconnected lazily on
//! the next call that lands on them.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::mux::{mux_handshake_client, mux_open_stream, Multiplexer, MuxStream};
use crate::protocol::{Codec, TargetAddr};

/// Factory for creating TCP connections to the server.
/// On Android, this wraps connect_protected(); on router, plain TcpStream::connect().
pub type ConnectFn = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = io::Result<TcpStream>> + Send>> + Send + Sync,
>;

/// Default pool size when caller passes 0 or no explicit value.
pub const DEFAULT_POOL_SIZE: usize = 4;

/// After this many consecutive ConnectAck timeouts on the same slot we treat
/// it as zombie (TCP alive but server-state lost — typical after a server
/// restart while client NAT hasn't sent RST yet) and force-reconnect.
/// 1-2 transient timeouts under heavy concurrent load are tolerated; a
/// genuinely dead slot will fail every attempt and trip this threshold fast.
const MAX_CONSECUTIVE_TIMEOUTS: u32 = 3;

/// Client-side connection pool over multiple parallel multiplexed tunnels.
pub struct MuxPool {
    connect_fn: ConnectFn,
    codec: Codec,
    slots: Vec<Mutex<Option<Arc<Multiplexer>>>>,
    /// Per-slot consecutive ConnectAck-timeout counter. Reset on any successful
    /// open_stream or on invalidation. Reaching `MAX_CONSECUTIVE_TIMEOUTS`
    /// invalidates the slot to recover from server-restart zombies.
    timeout_counters: Vec<AtomicU32>,
    next: AtomicUsize,
}

impl MuxPool {
    /// Create a new pool with `size` slots. `size == 0` falls back to
    /// `DEFAULT_POOL_SIZE` so callers can pass through config defaults
    /// without panicking on a misconfigured zero.
    pub fn new(connect_fn: ConnectFn, codec: Codec, size: usize) -> Arc<Self> {
        let size = if size == 0 { DEFAULT_POOL_SIZE } else { size };
        let mut slots = Vec::with_capacity(size);
        let mut timeout_counters = Vec::with_capacity(size);
        for _ in 0..size {
            slots.push(Mutex::new(None));
            timeout_counters.push(AtomicU32::new(0));
        }
        Arc::new(Self {
            connect_fn,
            codec,
            slots,
            timeout_counters,
            next: AtomicUsize::new(0),
        })
    }

    /// Number of slots in the pool.
    pub fn size(&self) -> usize {
        self.slots.len()
    }

    /// Pre-establish all mux connections concurrently. Used by the engine
    /// health check — verifies protocol reachability and pre-warms the pool
    /// so the first relay stream opens instantly. Returns Ok if at least
    /// one slot established; failed slots will reconnect lazily on demand.
    pub async fn warmup(&self) -> io::Result<()> {
        let mut futs: Vec<Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>>> = (0..self
            .slots
            .len())
            .map(|idx| {
                let f: Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> =
                    Box::pin(async move { self.acquire_or_connect(idx).await.map(|_| ()) });
                f
            })
            .collect();

        let mut last_err: Option<io::Error> = None;
        let mut any_ok = false;

        // Drive all slot connects concurrently in this single task — no
        // tokio::spawn (would require Arc<Self> receiver), no extra crate.
        // Each Pending waker is registered on the latest poll, so when any
        // inner future is woken the parent re-polls all remaining ones.
        std::future::poll_fn(|cx: &mut Context<'_>| -> Poll<()> {
            let mut i = 0;
            while i < futs.len() {
                match futs[i].as_mut().poll(cx) {
                    Poll::Ready(Ok(())) => {
                        any_ok = true;
                        drop(futs.swap_remove(i));
                    }
                    Poll::Ready(Err(e)) => {
                        last_err = Some(e);
                        drop(futs.swap_remove(i));
                    }
                    Poll::Pending => i += 1,
                }
            }
            if futs.is_empty() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;

        if any_ok {
            Ok(())
        } else {
            Err(last_err
                .unwrap_or_else(|| io::Error::new(io::ErrorKind::Other, "no mux slots")))
        }
    }

    /// Open a new logical stream through one of the pool's tunnels.
    ///
    /// Round-robin: each call advances `next` by one and starts probing
    /// from `next % size`. If the chosen slot is dead, the call
    /// reconnects it; if the reconnect or the Connect handshake fails,
    /// the next slot is tried. After walking all slots, the last error
    /// is returned.
    pub async fn open_stream(&self, target: &TargetAddr) -> io::Result<MuxStream> {
        let n = self.slots.len();
        let start = self.next.fetch_add(1, Ordering::Relaxed) % n;
        let mut last_err: Option<io::Error> = None;

        for i in 0..n {
            let idx = (start + i) % n;
            let mux = match self.acquire_or_connect(idx).await {
                Ok(m) => m,
                Err(e) => {
                    tracing::debug!("mux slot {} unavailable ({}), trying next", idx, e);
                    last_err = Some(e);
                    continue;
                }
            };

            match mux_open_stream(&mux, target).await {
                Ok(stream) => {
                    // Successful open — reset the timeout counter so a future
                    // burst doesn't accumulate on top of past transient failures.
                    self.timeout_counters[idx].store(0, Ordering::Relaxed);
                    return Ok(stream);
                }
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    // True death — reader/writer of this Multiplexer have
                    // already exited (that's how BrokenPipe propagates), so
                    // the TCP is closed. Drop the slot reference and let
                    // the next call reconnect.
                    tracing::debug!(
                        "mux slot {} broken pipe ({}), invalidating",
                        idx,
                        e
                    );
                    self.invalidate_slot(idx).await;
                    last_err = Some(e);
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                    // ConnectAck timeout: usually transient overload (busy
                    // mux, slow server) → failover without invalidation to
                    // avoid orphaning live reader/writer tasks (TCP kept
                    // open until MUX_MAX_LIFETIME = 4h, server max_connections
                    // semaphore fills up — saw 171 ghost ESTAB in prod).
                    //
                    // BUT after enough consecutive timeouts on the same slot
                    // it's almost certainly a zombie: TCP still ESTAB on the
                    // client side (NAT hasn't sent RST), server-side state
                    // lost (server restart). At that point invalidating
                    // *is* the right call — the dead Multiplexer's reader
                    // will exit on next mux keepalive failure anyway.
                    let prev = self.timeout_counters[idx].fetch_add(1, Ordering::Relaxed);
                    if prev + 1 >= MAX_CONSECUTIVE_TIMEOUTS {
                        tracing::warn!(
                            "mux slot {} hit {} consecutive connect-ack timeouts, invalidating (zombie?)",
                            idx,
                            prev + 1
                        );
                        self.invalidate_slot(idx).await;
                    } else {
                        tracing::debug!(
                            "mux slot {} connect-ack timeout ({}), failover without invalidation [counter={}]",
                            idx,
                            e,
                            prev + 1
                        );
                    }
                    last_err = Some(e);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        Err(last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::Other, "no mux slots")))
    }

    /// Return the slot's live mux, reconnecting if necessary. The slot
    /// lock is held across the connect/handshake to coalesce concurrent
    /// callers landing on the same dead slot.
    async fn acquire_or_connect(&self, idx: usize) -> io::Result<Arc<Multiplexer>> {
        let mut guard = self.slots[idx].lock().await;
        if let Some(ref mux) = *guard {
            if mux.is_alive() {
                return Ok(mux.clone());
            }
        }

        let mut stream = (self.connect_fn)().await?;
        match mux_handshake_client(&mut stream, &self.codec).await {
            Ok(true) => {
                let mux = Multiplexer::new_client(stream, self.codec.clone());
                *guard = Some(mux.clone());
                tracing::info!("mux slot {} connection established", idx);
                Ok(mux)
            }
            Ok(false) => Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "server rejected mux handshake",
            )),
            Err(e) => Err(e),
        }
    }

    async fn invalidate_slot(&self, idx: usize) {
        let mut guard = self.slots[idx].lock().await;
        // Take the old Multiplexer out and explicitly shutdown it so the
        // underlying TCP closes promptly. Otherwise the orphaned reader/
        // writer tasks keep the socket ESTABLISHED on the server until
        // MUX_MAX_LIFETIME (4h), producing ghost sessions (we saw the
        // server accumulate 16+ ESTAB per router this way).
        if let Some(old) = guard.take() {
            old.shutdown();
        }
        self.timeout_counters[idx].store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obfuscation::{ModifierStrategy, Obfuscator};
    use std::sync::atomic::AtomicU32;

    fn test_codec() -> Codec {
        let key = b"test-key-32-bytes-long-enough!!!".to_vec();
        let obfs = Obfuscator::new(key, 0xDEADBEEF, ModifierStrategy::PositionalXorRotate);
        Codec::new(obfs, 0, 0)
    }

    fn always_failing_connect() -> ConnectFn {
        Arc::new(move || {
            Box::pin(async move {
                Err(io::Error::new(io::ErrorKind::ConnectionRefused, "test"))
            })
        })
    }

    #[tokio::test]
    async fn test_pool_size_zero_uses_default() {
        let pool = MuxPool::new(always_failing_connect(), test_codec(), 0);
        assert_eq!(pool.size(), DEFAULT_POOL_SIZE);
    }

    #[tokio::test]
    async fn test_pool_size_honors_explicit_value() {
        let pool = MuxPool::new(always_failing_connect(), test_codec(), 7);
        assert_eq!(pool.size(), 7);
    }

    #[tokio::test]
    async fn test_pool_connect_error_propagates() {
        let pool = MuxPool::new(always_failing_connect(), test_codec(), 1);
        let err = pool
            .open_stream(&TargetAddr::Domain("test.com".to_string(), 443))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    /// open_stream must walk every slot before giving up, so that a
    /// transient failure on one tunnel doesn't immediately propagate
    /// when another could have served the request.
    #[tokio::test]
    async fn test_open_stream_tries_all_slots_on_failure() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        let connect_fn: ConnectFn = Arc::new(move || {
            let counter = counter_clone.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::Relaxed);
                Err(io::Error::new(io::ErrorKind::ConnectionRefused, "test"))
            })
        });

        let pool = MuxPool::new(connect_fn, test_codec(), 3);
        let _err = pool
            .open_stream(&TargetAddr::Domain("test.com".to_string(), 443))
            .await
            .unwrap_err();
        assert_eq!(
            counter.load(Ordering::Relaxed),
            3,
            "open_stream should attempt every slot before failing"
        );
    }

    /// Direct check on the consecutive-timeout counter contract:
    /// reaching MAX_CONSECUTIVE_TIMEOUTS must invalidate, success must reset.
    /// We exercise the counter directly — full mux mocking would require
    /// faking ConnectAck timeouts on a live Multiplexer, which is invasive
    /// for what is fundamentally a per-slot atomic.
    #[tokio::test]
    async fn test_consecutive_timeout_counter() {
        let pool = MuxPool::new(always_failing_connect(), test_codec(), 2);
        // Initially zero.
        assert_eq!(pool.timeout_counters[0].load(Ordering::Relaxed), 0);
        // Bump up to threshold-1 — slot must NOT be considered zombie yet.
        for _ in 0..(MAX_CONSECUTIVE_TIMEOUTS - 1) {
            pool.timeout_counters[0].fetch_add(1, Ordering::Relaxed);
        }
        assert!(pool.timeout_counters[0].load(Ordering::Relaxed) < MAX_CONSECUTIVE_TIMEOUTS);
        // One more push and invalidate_slot must reset it.
        pool.timeout_counters[0].fetch_add(1, Ordering::Relaxed);
        assert_eq!(pool.timeout_counters[0].load(Ordering::Relaxed), MAX_CONSECUTIVE_TIMEOUTS);
        pool.invalidate_slot(0).await;
        assert_eq!(
            pool.timeout_counters[0].load(Ordering::Relaxed),
            0,
            "invalidate_slot must reset the counter"
        );
        // Other slot is independent.
        assert_eq!(pool.timeout_counters[1].load(Ordering::Relaxed), 0);
    }

    /// warmup must drive all slot connects concurrently — so when every
    /// connect_fn fails, the total number of TCP attempts equals the
    /// pool size (and the call returns the last error).
    #[tokio::test]
    async fn test_warmup_attempts_all_slots() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        let connect_fn: ConnectFn = Arc::new(move || {
            let counter = counter_clone.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::Relaxed);
                Err(io::Error::new(io::ErrorKind::ConnectionRefused, "test"))
            })
        });

        let pool = MuxPool::new(connect_fn, test_codec(), 4);
        let err = pool.warmup().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(counter.load(Ordering::Relaxed), 4);
    }
}
