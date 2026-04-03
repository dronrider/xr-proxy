//! IP stack: smoltcp-based userspace TCP/IP over TUN.
//!
//! The TUN device provides raw IP packets. smoltcp handles TCP state machines
//! (SYN/ACK, retransmission, etc.) and exposes socket-level read/write.
//!
//! Architecture:
//! ```text
//! TUN fd ←→ PacketQueue ←→ smoltcp::Interface ←→ TCP/UDP sockets
//! ```

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{HardwareAddress, IpCidr, Ipv4Address};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// MTU for the virtual TUN device.
pub const TUN_MTU: usize = 1500;

/// smoltcp interface IP — intentionally DIFFERENT from TUN IP (10.0.0.2).
pub const SMOL_IP: Ipv4Address = Ipv4Address::new(172, 16, 0, 1);
/// Gateway IP. MUST be smoltcp's own IP for any_ip routing check:
/// smoltcp requires routes.lookup(dst).gateway == one of ip_addrs.
pub const SMOL_GATEWAY: Ipv4Address = Ipv4Address::new(172, 16, 0, 1);

// ── Packet queue (TUN ↔ smoltcp bridge) ─────────────────────────────

/// Shared packet queue: the bridge between TUN I/O and smoltcp.
///
/// - `inbound`: packets read from TUN fd → fed into smoltcp.
/// - `outbound`: packets produced by smoltcp → written to TUN fd.
#[derive(Clone)]
pub struct PacketQueue {
    inner: Arc<Mutex<PacketQueueInner>>,
}

struct PacketQueueInner {
    inbound: VecDeque<Vec<u8>>,
    /// Packets from smoltcp device → engine (for NAT rewrite).
    smol_outbound: VecDeque<Vec<u8>>,
    /// Packets after NAT rewrite → TUN fd.
    tun_outbound: VecDeque<Vec<u8>>,
}

impl PacketQueue {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(PacketQueueInner {
                inbound: VecDeque::with_capacity(256),
                smol_outbound: VecDeque::with_capacity(256),
                tun_outbound: VecDeque::with_capacity(256),
            })),
        }
    }

    /// Push a packet from TUN into the stack (for smoltcp to process).
    pub fn push_inbound(&self, packet: Vec<u8>) {
        let mut inner = self.inner.lock().unwrap();
        // Drop oldest if queue is too large (backpressure).
        if inner.inbound.len() >= 4096 {
            inner.inbound.pop_front();
        }
        inner.inbound.push_back(packet);
    }

    /// Pop a packet for TUN fd (after NAT rewrite).
    pub fn pop_outbound(&self) -> Option<Vec<u8>> {
        self.inner.lock().unwrap().tun_outbound.pop_front()
    }

    /// Pop a packet from smoltcp device (before NAT rewrite).
    /// Called by engine event loop.
    pub fn pop_smol_outbound(&self) -> Option<Vec<u8>> {
        self.inner.lock().unwrap().smol_outbound.pop_front()
    }

    /// Pop a packet from the inbound queue (public, for DNS interception).
    pub fn pop_inbound_public(&self) -> Option<Vec<u8>> {
        self.inner.lock().unwrap().inbound.pop_front()
    }

    /// Push a packet to the TUN outbound queue (for DNS responses and NAT-rewritten packets).
    pub fn push_outbound_public(&self, packet: Vec<u8>) {
        let mut inner = self.inner.lock().unwrap();
        if inner.tun_outbound.len() >= 4096 {
            inner.tun_outbound.pop_front();
        }
        inner.tun_outbound.push_back(packet);
    }

    fn pop_inbound(&self) -> Option<Vec<u8>> {
        self.inner.lock().unwrap().inbound.pop_front()
    }

    fn push_outbound(&self, packet: Vec<u8>) {
        let mut inner = self.inner.lock().unwrap();
        if inner.smol_outbound.len() >= 4096 {
            inner.smol_outbound.pop_front();
        }
        inner.smol_outbound.push_back(packet);
    }

    /// Check if there's pending inbound data.
    pub fn has_inbound(&self) -> bool {
        !self.inner.lock().unwrap().inbound.is_empty()
    }

    /// Check if there's pending outbound data (for TUN).
    pub fn has_outbound(&self) -> bool {
        !self.inner.lock().unwrap().tun_outbound.is_empty()
    }
}

// ── smoltcp Device implementation ───────────────────────────────────

/// Virtual device backed by PacketQueue. Implements smoltcp's Device trait.
pub struct QueueDevice {
    queue: PacketQueue,
    pub rx_count: u64,
    pub tx_count: u64,
}

impl QueueDevice {
    pub fn new(queue: PacketQueue) -> Self {
        Self { queue, rx_count: 0, tx_count: 0 }
    }
}

impl Device for QueueDevice {
    type RxToken<'a> = QueueRxToken;
    type TxToken<'a> = QueueTxToken;

    fn receive(&mut self, _timestamp: SmolInstant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let packet = self.queue.pop_inbound()?;
        self.rx_count += 1;
        Some((
            QueueRxToken { packet },
            QueueTxToken {
                queue: self.queue.clone(),
            },
        ))
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        self.tx_count += 1;
        Some(QueueTxToken {
            queue: self.queue.clone(),
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = TUN_MTU;
        // Disable checksum verification/generation — we rewrite packets
        // (NAT with ephemeral ports) so checksums are invalid.
        // The TUN device and the real network stack handle checksums.
        caps.checksum = smoltcp::phy::ChecksumCapabilities::ignored();
        caps
    }
}

pub struct QueueRxToken {
    packet: Vec<u8>,
}

impl RxToken for QueueRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.packet)
    }
}

pub struct QueueTxToken {
    queue: PacketQueue,
}

impl TxToken for QueueTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let result = f(&mut buf);
        self.queue.push_outbound(buf);
        result
    }
}

// ── IP Stack ────────────────────────────────────────────────────────

/// The smoltcp-based IP stack.
pub struct IpStack {
    pub iface: Interface,
    pub device: QueueDevice,
    pub sockets: SocketSet<'static>,
    pub queue: PacketQueue,
}

impl IpStack {
    /// Create a new IP stack with the given packet queue.
    pub fn new(queue: PacketQueue) -> Self {
        let mut device = QueueDevice::new(queue.clone());

        let config = Config::new(HardwareAddress::Ip);
        let mut iface = Interface::new(config, &mut device, SmolInstant::now());

        // Configure interface with a DIFFERENT IP than the TUN (10.0.0.2).
        // With any_ip=true, smoltcp considers ALL IPs as local.
        // If the smoltcp IP == TUN IP, smoltcp treats SYN-ACK as loopback
        // and never sends it through the device → TCP handshake never completes.
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(SMOL_IP.into(), 16))
                .ok();
        });

        // Accept packets for ANY destination IP.
        iface.set_any_ip(true);

        // Default route.
        iface.routes_mut().add_default_ipv4_route(SMOL_GATEWAY).ok();

        let sockets = SocketSet::new(Vec::new());

        Self {
            iface,
            device,
            sockets,
            queue,
        }
    }

    /// Poll the interface — process queued packets, advance TCP state machines.
    /// Returns true if any socket made progress.
    pub fn poll(&mut self) -> bool {
        let timestamp = SmolInstant::now();
        matches!(
            self.iface.poll(timestamp, &mut self.device, &mut self.sockets),
            smoltcp::iface::PollResult::SocketStateChanged
        )
    }

    /// Get the next poll delay (for sleep/timeout in the event loop).
    pub fn poll_delay(&mut self) -> Option<std::time::Duration> {
        let timestamp = SmolInstant::now();
        self.iface
            .poll_delay(timestamp, &self.sockets)
            .map(|d| std::time::Duration::from_millis(d.total_millis()))
    }

    /// Add a TCP socket to the stack. Returns its handle.
    pub fn add_tcp_socket(
        &mut self,
        rx_buf_size: usize,
        tx_buf_size: usize,
    ) -> SocketHandle {
        let rx_buf = smoltcp::socket::tcp::SocketBuffer::new(vec![0u8; rx_buf_size]);
        let tx_buf = smoltcp::socket::tcp::SocketBuffer::new(vec![0u8; tx_buf_size]);
        let socket = smoltcp::socket::tcp::Socket::new(rx_buf, tx_buf);
        self.sockets.add(socket)
    }

    /// Add a UDP socket to the stack. Returns its handle.
    pub fn add_udp_socket(
        &mut self,
        rx_meta_count: usize,
        rx_payload_size: usize,
        tx_meta_count: usize,
        tx_payload_size: usize,
    ) -> SocketHandle {
        let rx_buf = smoltcp::socket::udp::PacketBuffer::new(
            vec![smoltcp::socket::udp::PacketMetadata::EMPTY; rx_meta_count],
            vec![0u8; rx_payload_size],
        );
        let tx_buf = smoltcp::socket::udp::PacketBuffer::new(
            vec![smoltcp::socket::udp::PacketMetadata::EMPTY; tx_meta_count],
            vec![0u8; tx_payload_size],
        );
        let socket = smoltcp::socket::udp::Socket::new(rx_buf, tx_buf);
        self.sockets.add(socket)
    }

    /// Get a reference to a TCP socket by handle.
    pub fn tcp_socket(&self, handle: SocketHandle) -> &smoltcp::socket::tcp::Socket<'static> {
        self.sockets.get::<smoltcp::socket::tcp::Socket>(handle)
    }

    /// Get a mutable reference to a TCP socket by handle.
    pub fn tcp_socket_mut(
        &mut self,
        handle: SocketHandle,
    ) -> &mut smoltcp::socket::tcp::Socket<'static> {
        self.sockets.get_mut::<smoltcp::socket::tcp::Socket>(handle)
    }

    /// Remove a socket from the stack.
    pub fn remove_socket(&mut self, handle: SocketHandle) {
        self.sockets.remove(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packet_queue() {
        let queue = PacketQueue::new();

        assert!(!queue.has_inbound());
        assert!(!queue.has_outbound());

        queue.push_inbound(vec![1, 2, 3]);
        assert!(queue.has_inbound());

        let pkt = queue.pop_inbound().unwrap();
        assert_eq!(pkt, vec![1, 2, 3]);
        assert!(!queue.has_inbound());
    }

    #[test]
    fn test_ip_stack_creation() {
        let queue = PacketQueue::new();
        let _stack = IpStack::new(queue);
    }

    #[test]
    fn test_backpressure() {
        let queue = PacketQueue::new();
        // Push more than 4096 packets — oldest should be dropped.
        for i in 0..5000u16 {
            queue.push_inbound(i.to_be_bytes().to_vec());
        }
        // Queue should have at most 4096 entries.
        let mut count = 0;
        while queue.pop_inbound().is_some() {
            count += 1;
        }
        assert!(count <= 4096);
    }
}
