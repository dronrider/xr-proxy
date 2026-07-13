//! Agent registry, byte counters and per-IP caps (LLD-23 §2.1, §2.6, §5.2).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::Mutex;
use xr_proto::mux::Multiplexer;

struct Registered {
    mux: Arc<Multiplexer>,
    generation: u64,
}

/// Maps a proven `agent_pubkey` to its live reverse-tunnel mux (LLD-23 §2.1).
/// Re-registration of the same key is last-writer-wins: the previous mux is shut
/// down (FIN to the zombie, the XR-086 lesson) and replaced. Deregistration is
/// generation-guarded, so a superseded connection's cleanup never evicts the
/// fresher registration that displaced it.
#[derive(Default)]
pub struct AgentRegistry {
    inner: Mutex<HashMap<String, Registered>>,
    next_gen: AtomicU64,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `mux` under `pubkey`, shutting down and replacing any prior mux.
    /// Returns the generation to hand back to [`deregister`](Self::deregister).
    pub async fn register(&self, pubkey: String, mux: Arc<Multiplexer>) -> u64 {
        let generation = self.next_gen.fetch_add(1, Ordering::Relaxed);
        let mut map = self.inner.lock().await;
        if let Some(old) = map.insert(pubkey, Registered { mux, generation }) {
            old.mux.shutdown();
        }
        generation
    }

    /// The live mux for `pubkey`, if any.
    pub async fn get(&self, pubkey: &str) -> Option<Arc<Multiplexer>> {
        self.inner.lock().await.get(pubkey).map(|r| r.mux.clone())
    }

    /// Remove `pubkey` only while it still holds the mux with `generation`, so a
    /// stale connection's teardown doesn't drop a newer registration.
    pub async fn deregister(&self, pubkey: &str, generation: u64) {
        let mut map = self.inner.lock().await;
        if map.get(pubkey).map(|r| r.generation) == Some(generation) {
            map.remove(pubkey);
        }
    }

    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }
}

/// Per-share transit byte totals (LLD-23 §2.6). No content, no share names beyond
/// the opaque id. Just "how much ciphertext moved", for ops visibility and the
/// metering hook (XR-075/073).
#[derive(Default)]
pub struct Counters {
    inner: StdMutex<HashMap<String, u64>>,
}

impl Counters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&self, share_id: &str, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut m = self.inner.lock().unwrap();
        *m.entry(share_id.to_string()).or_insert(0) += bytes;
    }

    /// Snapshot of `(share_id, total_bytes)` for periodic logging.
    pub fn snapshot(&self) -> Vec<(String, u64)> {
        let m = self.inner.lock().unwrap();
        m.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    #[cfg(test)]
    pub fn get(&self, share_id: &str) -> u64 {
        *self.inner.lock().unwrap().get(share_id).unwrap_or(&0)
    }
}

/// A live-registration budget per source IP (LLD-23 §5.2). A registrant holds a
/// [`IpCapGuard`] for the life of its connection; dropping it frees the slot.
pub struct IpCaps {
    max: usize,
    inner: StdMutex<HashMap<IpAddr, usize>>,
}

impl IpCaps {
    pub fn new(max: usize) -> Arc<Self> {
        Arc::new(Self {
            max,
            inner: StdMutex::new(HashMap::new()),
        })
    }

    /// Reserve one registration slot for `ip`, or `None` if the cap is reached.
    pub fn try_acquire(self: &Arc<Self>, ip: IpAddr) -> Option<IpCapGuard> {
        let mut m = self.inner.lock().unwrap();
        let count = m.entry(ip).or_insert(0);
        if *count >= self.max {
            return None;
        }
        *count += 1;
        Some(IpCapGuard {
            caps: self.clone(),
            ip,
        })
    }

    fn release(&self, ip: IpAddr) {
        let mut m = self.inner.lock().unwrap();
        if let Some(count) = m.get_mut(&ip) {
            *count -= 1;
            if *count == 0 {
                m.remove(&ip);
            }
        }
    }
}

/// RAII slot in an [`IpCaps`] budget; frees on drop.
pub struct IpCapGuard {
    caps: Arc<IpCaps>,
    ip: IpAddr,
}

impl Drop for IpCapGuard {
    fn drop(&mut self) {
        self.caps.release(self.ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};
    use xr_proto::protocol::Codec;

    fn dead_mux() -> Arc<Multiplexer> {
        // A mux over a closed duplex: reader hits EOF, so it becomes !alive. Fine
        // for registry bookkeeping tests, which don't move bytes.
        let key = b"test-key-32-bytes-long-enough!!!".to_vec();
        let codec = Codec::new(
            Obfuscator::new(key, 0xDEADBEEF, ModifierStrategy::PositionalXorRotate),
            0,
            0,
        );
        let (a, _b) = tokio::io::duplex(64);
        Multiplexer::new_server(a, codec, xr_proto::mux::MuxCaps::LOCAL)
    }

    #[tokio::test]
    async fn test_registry_evicts_and_generation_guards() {
        let reg = AgentRegistry::new();
        let first = dead_mux();
        let g1 = reg.register("agentA".into(), first.clone()).await;
        assert_eq!(reg.len().await, 1);

        // Re-register the same key: last-writer-wins, old mux is shut down.
        let second = dead_mux();
        let g2 = reg.register("agentA".into(), second.clone()).await;
        assert_ne!(g1, g2);
        assert_eq!(reg.len().await, 1);
        assert!(!first.is_alive(), "evicted mux must be shut down");

        // The stale connection (g1) deregistering must NOT drop the fresh one.
        reg.deregister("agentA", g1).await;
        assert_eq!(reg.len().await, 1, "generation guard protects the newer registration");

        // The current owner (g2) deregisters cleanly.
        reg.deregister("agentA", g2).await;
        assert_eq!(reg.len().await, 0);
    }

    #[test]
    fn test_ip_caps_budget() {
        let caps = IpCaps::new(2);
        let ip: IpAddr = Ipv4Addr::LOCALHOST.into();
        let g1 = caps.try_acquire(ip);
        let g2 = caps.try_acquire(ip);
        assert!(g1.is_some() && g2.is_some());
        assert!(caps.try_acquire(ip).is_none(), "third registration over cap is refused");
        drop(g1);
        assert!(caps.try_acquire(ip).is_some(), "freeing a slot admits a new one");
        drop(g2);
    }

    #[test]
    fn test_counters_accumulate_per_share() {
        let c = Counters::new();
        c.add("s1", 100);
        c.add("s1", 50);
        c.add("s2", 7);
        c.add("s2", 0); // ignored
        assert_eq!(c.get("s1"), 150);
        assert_eq!(c.get("s2"), 7);
        assert_eq!(c.snapshot().len(), 2);
    }
}
