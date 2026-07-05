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
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

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

/// Circuit-breaker cooldown. Once `open_stream` has tried *every* slot and they
/// all failed, the server is presumed down and the pool enters a cooldown: for
/// this long, new `open_stream` calls short-circuit to a fast error instead of
/// re-walking every dead slot. This is the fix for the fail-open P0 incident.
///
/// Why it matters: a *silent* server (TCP accepted by an unpaid-hosting stub,
/// but no mux handshake reply) makes each slot's `mux_handshake_client` block
/// the full 2s "init ack timeout". Across a 4-slot pool that is ~8s per logical
/// stream — and the handshake never completes, so the slot is never cached and
/// EVERY new connection pays the ~8s again. Android's connectivity probes
/// (`connectivitycheck.gstatic.com`, `*.google.com` → proxied) then time out,
/// the OS declares the Wi-Fi "no internet", and the user loses even the Direct
/// (non-proxied) path. With the breaker, the first failure arms the cooldown
/// and every subsequent connection fails open to Direct in microseconds.
///
/// Recovery is automatic: when the cooldown expires exactly one caller is
/// elected (via CAS) to re-probe the server for real while others keep
/// short-circuiting; a successful open clears the breaker, a failure re-arms it.
const SERVER_DOWN_COOLDOWN: Duration = Duration::from_secs(5);

/// Exclusivity window the elected re-probe caller reserves while it walks the
/// slots. This MUST exceed the worst-case walk against a dead server, otherwise
/// a second prober gets elected mid-walk: the cooldown (5s) is shorter than a
/// full 4-slot walk against a dropped/silent server (~8s, each slot paying its
/// connect/handshake timeout), so without a longer lease probers stack up,
/// contend on the per-slot connect locks, and individual walks balloon to 25s+.
/// 15s comfortably covers the common dead-server walk while keeping
/// post-recovery latency to ~cooldown + one walk.
const PROBE_LEASE: Duration = Duration::from_secs(15);

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
    /// Circuit-breaker deadline: `0` means healthy; any other value is a
    /// "server presumed down until this many millis since `created`" instant.
    /// While `now < down_until_ms`, `open_stream` short-circuits to a fast
    /// error so the caller fails open to Direct without re-probing a dead VPS.
    down_until_ms: AtomicU64,
    /// Monotonic base for `down_until_ms`. `Instant` can't be stored in an
    /// atomic, so we keep a fixed origin and measure elapsed millis against it.
    created: Instant,
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
            down_until_ms: AtomicU64::new(0),
            created: Instant::now(),
        })
    }

    /// Number of slots in the pool.
    pub fn size(&self) -> usize {
        self.slots.len()
    }

    /// Millis elapsed since pool construction (monotonic, never panics).
    fn now_ms(&self) -> u64 {
        self.created.elapsed().as_millis() as u64
    }

    /// Whether the circuit breaker currently considers the server down.
    /// Exposed for health/monitoring surfaces (e.g. status indicator).
    pub fn is_server_down(&self) -> bool {
        let until = self.down_until_ms.load(Ordering::Relaxed);
        until != 0 && self.now_ms() < until
    }

    /// Arm the breaker: server presumed down for `SERVER_DOWN_COOLDOWN`.
    fn arm_breaker(&self) {
        let until = self.now_ms() + SERVER_DOWN_COOLDOWN.as_millis() as u64;
        self.down_until_ms.store(until, Ordering::Relaxed);
    }

    /// Clear the breaker after a successful open — server is reachable again.
    fn clear_breaker(&self) {
        self.down_until_ms.store(0, Ordering::Relaxed);
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
                    Box::pin(async move { self.acquire_or_connect(idx, false).await.map(|_| ()) });
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
        // Circuit-breaker gate. When the server is presumed down we avoid
        // re-walking every dead slot (which costs ~8s on a silent server) and
        // instead fail fast so the caller falls open to Direct immediately.
        //
        // At cooldown expiry exactly ONE caller is elected to re-probe: the CAS
        // both detects the expiry and extends the deadline, so concurrent
        // callers keep short-circuiting while the elected prober does the slow
        // walk. A success clears the breaker; a failure re-arms it below.
        let mut am_prober = false;
        let breaker = self.down_until_ms.load(Ordering::Relaxed);
        if breaker != 0 {
            if self.now_ms() < breaker {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "mux pool: server in fail-open cooldown",
                ));
            }
            // Cooldown expired — try to win the probe election. Reserve a lease
            // that outlasts the whole walk so no second prober is elected while
            // we are still probing (that would stack probers and stall them).
            let extended = self.now_ms() + PROBE_LEASE.as_millis() as u64;
            if self
                .down_until_ms
                .compare_exchange(breaker, extended, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
            {
                // Another caller became the prober — short-circuit.
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "mux pool: server in fail-open cooldown",
                ));
            }
            // We are the sole prober; fall through to a real slot walk.
            am_prober = true;
        }

        let n = self.slots.len();
        let start = self.next.fetch_add(1, Ordering::Relaxed) % n;
        let mut last_err: Option<io::Error> = None;

        for i in 0..n {
            // If a *concurrent* caller has armed the breaker while we were
            // grinding through dead slots, stop and fail open immediately
            // rather than walking the remaining slots under mutex contention.
            // Without this, a reconnect storm against a freshly-dead server
            // (e.g. a watchdog restart) lets in-flight connections queue on the
            // per-slot connect locks and stall for many multiples of one walk.
            // The elected prober is exempt: it must finish its walk to decide
            // whether the server has recovered.
            if !am_prober && self.is_server_down() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "mux pool: server in fail-open cooldown",
                ));
            }
            let idx = (start + i) % n;
            let mux = match self.acquire_or_connect(idx, !am_prober).await {
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
                    // Server reachable again — release the fail-open breaker.
                    self.clear_breaker();
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

        // Every slot failed — arm the breaker so the next callers fail open to
        // Direct instantly instead of each paying the full slot-walk latency.
        self.arm_breaker();
        Err(last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::Other, "no mux slots")))
    }

    /// Return the slot's live mux, reconnecting if necessary. The slot
    /// lock is held across the connect/handshake to coalesce concurrent
    /// callers landing on the same dead slot.
    ///
    /// `bail_if_down`: when true, after acquiring the lock we re-check the
    /// circuit breaker and fail fast instead of spending a ~2s connect on a
    /// known-dead server. This is what drains a reconnect storm: the lock is
    /// held across the connect, so callers queued behind a dead slot would
    /// otherwise each still pay a full connect timeout even after the breaker
    /// armed. The prober passes `false` — it MUST connect to test recovery.
    async fn acquire_or_connect(
        &self,
        idx: usize,
        bail_if_down: bool,
    ) -> io::Result<Arc<Multiplexer>> {
        let mut guard = self.slots[idx].lock().await;
        if let Some(ref mux) = *guard {
            if mux.is_alive() {
                return Ok(mux.clone());
            }
        }

        // No live mux on this slot. If the breaker armed while we waited for the
        // lock, don't burn a connect timeout — fail open immediately.
        if bail_if_down && self.is_server_down() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "mux pool: server in fail-open cooldown",
            ));
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

    /// Lightweight health probe for `ServerPool` (LLD-10): ensure slot 0 has a
    /// live mux, connecting it if needed; no user stream is opened. Cheap on a
    /// healthy warm slot (one `is_alive` check under the slot lock). Bypasses
    /// the breaker gate on purpose (an active prober MUST test the real path)
    /// and clears the breaker on success so the next `open_stream` doesn't
    /// short-circuit on a stale cooldown.
    pub async fn probe(&self) -> io::Result<()> {
        self.acquire_or_connect(0, false).await?;
        self.clear_breaker();
        Ok(())
    }

    /// Force every slot to reconnect and clear the fail-open breaker.
    ///
    /// Called when the host signals that the underlying network changed
    /// (Android LTE↔Wi-Fi): the live mux TCP sockets are still bound to the
    /// now-dead interface, so we drop them eagerly instead of waiting for the
    /// slow consecutive-timeout detector (`MAX_CONSECUTIVE_TIMEOUTS`) to trip.
    /// `protect(fd)` binds a *new* socket to the current default network, so
    /// the reconnect on the next `open_stream` (or the explicit `warmup` the
    /// engine kicks off) lands on the new uplink.
    ///
    /// Clearing the breaker matters: without it the first post-switch
    /// `open_stream` would short-circuit on a stale `down_until_ms` cooldown
    /// instead of actually probing the recovered path.
    pub async fn recycle(&self) {
        for idx in 0..self.slots.len() {
            self.invalidate_slot(idx).await;
        }
        self.clear_breaker();
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

    /// The fail-open circuit breaker is the P0 fix: once a full slot walk
    /// fails (server down), the breaker arms and subsequent `open_stream`
    /// calls short-circuit WITHOUT touching `connect_fn` — so the caller falls
    /// open to Direct in microseconds instead of re-walking every dead slot
    /// (~8s on a silent server). This is what keeps a dead VPS from making
    /// Android declare the whole Wi-Fi "no internet".
    #[tokio::test]
    async fn test_breaker_short_circuits_after_full_failure() {
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
        let target = TargetAddr::Domain("test.com".to_string(), 443);

        // First call walks all 3 slots, fails, and arms the breaker.
        let _ = pool.open_stream(&target).await.unwrap_err();
        assert_eq!(counter.load(Ordering::Relaxed), 3);
        assert!(pool.is_server_down(), "breaker must arm after a full failed walk");

        // Second call within cooldown must short-circuit: connect_fn untouched.
        let err = pool.open_stream(&target).await.unwrap_err();
        assert_eq!(
            counter.load(Ordering::Relaxed),
            3,
            "breaker must short-circuit without re-attempting any slot"
        );
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }

    /// A pool that has never failed must NOT report the server down, and the
    /// first `open_stream` must actually attempt the slots (no spurious
    /// breaker engaged from construction).
    #[tokio::test]
    async fn test_breaker_starts_disarmed() {
        let pool = MuxPool::new(always_failing_connect(), test_codec(), 2);
        assert!(!pool.is_server_down(), "a fresh pool must not be marked down");
    }

    /// Self-healing: once the cooldown elapses, exactly one caller must be
    /// elected to re-probe the server (so a recovered VPS is picked up without
    /// a restart). We expire the deadline directly instead of sleeping the full
    /// cooldown to keep the test fast and deterministic.
    #[tokio::test]
    async fn test_breaker_reprobes_after_cooldown_expiry() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        let connect_fn: ConnectFn = Arc::new(move || {
            let counter = counter_clone.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::Relaxed);
                Err(io::Error::new(io::ErrorKind::ConnectionRefused, "test"))
            })
        });
        let pool = MuxPool::new(connect_fn, test_codec(), 2);
        let target = TargetAddr::Domain("x.com".to_string(), 443);

        // Full failed walk arms the breaker (2 attempts on a 2-slot pool).
        let _ = pool.open_stream(&target).await.unwrap_err();
        assert_eq!(counter.load(Ordering::Relaxed), 2);
        assert!(pool.is_server_down());

        // Within cooldown: short-circuit, no new connect attempts.
        let _ = pool.open_stream(&target).await.unwrap_err();
        assert_eq!(counter.load(Ordering::Relaxed), 2);

        // Expire the cooldown — the next caller must be elected prober and
        // re-walk every slot (a real recovery attempt).
        pool.down_until_ms.store(pool.now_ms(), Ordering::Relaxed);
        let _ = pool.open_stream(&target).await.unwrap_err();
        assert_eq!(
            counter.load(Ordering::Relaxed),
            4,
            "after cooldown expiry exactly one caller must re-probe all slots"
        );
    }

    /// recycle() must clear the fail-open breaker so the first `open_stream`
    /// after a network switch re-probes the new uplink instead of
    /// short-circuiting on the stale cooldown. (Slots hold no live mux in this
    /// failing-connect setup, so the observable contract is the breaker reset.)
    #[tokio::test]
    async fn test_recycle_clears_breaker() {
        let pool = MuxPool::new(always_failing_connect(), test_codec(), 2);
        let target = TargetAddr::Domain("x.com".to_string(), 443);

        // Full failed walk arms the breaker.
        let _ = pool.open_stream(&target).await.unwrap_err();
        assert!(pool.is_server_down(), "breaker must arm after a full failed walk");

        // Recycle on network change must disarm it.
        pool.recycle().await;
        assert!(
            !pool.is_server_down(),
            "recycle must clear the breaker so the next open_stream re-probes"
        );
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
