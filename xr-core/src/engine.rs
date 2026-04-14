//! VPN Engine — main entry point for mobile/desktop clients.
//!
//! Architecture:
//! 1. DNS queries (UDP:53) → intercepted via Fake DNS
//! 2. TCP SYN → smoltcp socket (listen on unique ephemeral port)
//! 3. TCP Established → spawn relay task to xr-server or direct
//! 4. TCP data: smoltcp ↔ channels ↔ relay task

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};

use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::State as TcpState;
use tokio::sync::mpsc;
use tokio::time::Duration;

use xr_proto::config::{decode_key, RoutingConfig};
use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};
use xr_proto::protocol::Codec;
use xr_proto::routing::{Action, Router};

use crate::dns::FakeDns;
use crate::ip_stack::{IpStack, PacketQueue};
use crate::session::{relay_session_with_domain, ProtectSocketFn, SessionContext, TcpSessionKey};
use crate::state::{StateHandle, VpnState};
use crate::stats::Stats;

/// Ephemeral port counter for unique listen endpoints.
/// Each smoltcp socket listens on a unique port so multiple SYNs
/// to the same dst port (e.g. 443) don't conflict.
static EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(10000);

fn next_ephemeral_port() -> u16 {
    let port = EPHEMERAL_PORT.fetch_add(1, Ordering::Relaxed);
    if port >= 60000 {
        EPHEMERAL_PORT.store(10000, Ordering::Relaxed);
    }
    port
}

pub struct VpnConfig {
    pub server_address: String,
    pub server_port: u16,
    pub obfuscation_key: String,
    pub modifier: String,
    pub salt: u32,
    pub padding_min: u8,
    pub padding_max: u8,
    pub routing: RoutingConfig,
    pub geoip_path: Option<String>,
    pub on_server_down: String,
    /// DNS resolvers (host:port or IP) used for direct-connection name
    /// resolution via protected sockets. Typically supplied by the host
    /// platform (e.g. Android ConnectivityManager). Empty means use only
    /// the public fallback list.
    pub dns_resolvers: Vec<String>,
    /// Hub configuration for centralized presets.
    pub hub_url: Option<String>,
    pub hub_preset: Option<String>,
    pub hub_cache_dir: Option<String>,
}

pub struct VpnEngine {
    config: VpnConfig,
    state: StateHandle,
    stats: Stats,
    shutdown_tx: Option<tokio::sync::watch::Sender<bool>>,
}

impl VpnEngine {
    pub fn new(config: VpnConfig) -> Self {
        Self { config, state: StateHandle::new(), stats: Stats::new(), shutdown_tx: None }
    }
    pub fn state(&self) -> &StateHandle { &self.state }
    pub fn stats(&self) -> &Stats { &self.stats }

    pub fn start(&mut self, queue: PacketQueue, protect_socket: ProtectSocketFn) -> io::Result<()> {
        if self.shutdown_tx.is_some() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "already running"));
        }
        self.state.set(VpnState::Connecting);

        let key = decode_key(&self.config.obfuscation_key)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        let strategy = ModifierStrategy::from_str(&self.config.modifier)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unknown modifier"))?;
        let obfuscator = Obfuscator::new(key, self.config.salt, strategy);
        let codec = Codec::new(obfuscator, self.config.padding_min, self.config.padding_max);

        // Build router, optionally merging with hub preset.
        let router = if let (Some(hub_url), Some(preset_name), Some(cache_dir)) = (
            &self.config.hub_url,
            &self.config.hub_preset,
            &self.config.hub_cache_dir,
        ) {
            let mut cache = crate::presets::PresetCache::new(
                std::path::Path::new(cache_dir),
                hub_url,
                preset_name,
            );
            cache.load_from_disk();
            // Try short fetch (2s timeout) at startup.
            let rt = tokio::runtime::Handle::try_current();
            if let Ok(handle) = rt {
                let _ = handle.block_on(cache.fetch_if_stale(Duration::from_secs(2)));
            }
            if let Some(preset_rules) = cache.routing_config() {
                tracing::info!("merging preset '{}' with local overrides", preset_name);
                Router::from_merged(
                    &self.config.routing,
                    preset_rules,
                    self.config.geoip_path.as_deref(),
                )
            } else {
                tracing::warn!(
                    "preset '{}' unavailable, running with local overrides only",
                    preset_name
                );
                Router::new(&self.config.routing, self.config.geoip_path.as_deref())
            }
        } else {
            Router::new(&self.config.routing, self.config.geoip_path.as_deref())
        };
        let server_addr: SocketAddr = format!("{}:{}", self.config.server_address, self.config.server_port)
            .parse().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("{}", e)))?;
        let on_server_down = Action::from_str(&self.config.on_server_down);
        let fake_dns = Arc::new(FakeDns::new());

        // Build mux pool with protected socket factory.
        let mux_pool = {
            let addr = server_addr;
            let protect = protect_socket.clone();
            xr_proto::mux_pool::MuxPool::new(
                Arc::new(move || {
                    let protect = protect.clone();
                    let addr = addr;
                    Box::pin(async move {
                        crate::session::connect_protected_pub(addr, &protect).await
                    })
                }),
                codec.clone(),
            )
        };

        // Parse DNS resolvers from config strings.
        // Accept both "1.2.3.4" (assume :53) and "1.2.3.4:53". Bad entries
        // are logged and skipped — they shouldn't kill startup.
        let dns_resolvers: Vec<SocketAddr> = self.config.dns_resolvers.iter()
            .filter_map(|s| {
                let s = s.trim();
                if s.is_empty() { return None; }
                if let Ok(addr) = s.parse::<SocketAddr>() {
                    return Some(addr);
                }
                if let Ok(ip) = s.parse::<IpAddr>() {
                    return Some(SocketAddr::new(ip, 53));
                }
                tracing::warn!("ignoring malformed DNS resolver entry: {}", s);
                None
            })
            .collect();

        let ctx = Arc::new(SessionContext {
            router, codec, server_addr,
            fake_dns: fake_dns.clone(),
            stats: self.stats.clone(),
            on_server_down, protect_socket,
            mux_pool,
            dns_resolvers,
        });

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);
        let state = self.state.clone();
        let stats = self.stats.clone();

        tokio::spawn(async move {
            stats.mark_started();

            // Health check: establish the mux connection (TCP + MuxInit
            // handshake) before reporting Connected. This verifies end-to-end
            // protocol reachability AND pre-warms the pool — the first relay
            // stream will open instantly without a separate TCP connect.
            let mut health_shutdown = shutdown_rx.clone();
            let health_result = tokio::select! {
                r = ctx.mux_pool.warmup() => Some(r),
                _ = health_shutdown.changed() => None,
            };
            match health_result {
                None => {
                    // Shutdown requested during health check.
                    state.set(VpnState::Disconnected);
                    return;
                }
                Some(Err(e)) => {
                    let msg = format!("Сервер недоступен: {}", e);
                    stats.add_error(&msg);
                    state.set(VpnState::Error(msg));
                    return;
                }
                Some(Ok(())) => {
                    // Mux connection established — server is reachable.
                }
            }

            state.set(VpnState::Connected);
            if let Err(e) = run_event_loop(queue, ctx, fake_dns, shutdown_rx).await {
                state.set(VpnState::Error(e.to_string()));
            } else {
                state.set(VpnState::Disconnected);
            }
        });
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            self.state.set(VpnState::Disconnecting);
            let _ = tx.send(true);
        }
    }
    pub fn is_running(&self) -> bool { self.shutdown_tx.is_some() }
}

// ── Session ─────────────────────────────────────────────────────────

struct ActiveSession {
    smol_handle: SocketHandle,
    /// The real destination (from the SYN packet).
    real_dst: SocketAddr,
    /// Domain from Fake DNS (cached at SYN time, not at relay time).
    domain: Option<String>,
    /// Ephemeral port assigned to this session for smoltcp.
    eph_port: u16,
    /// Set when TCP reaches Established.
    relay: Option<RelayChannels>,
}

struct RelayChannels {
    to_relay: mpsc::Sender<Vec<u8>>,
    from_relay: mpsc::Receiver<Vec<u8>>,
}

// ── Event loop ──────────────────────────────────────────────────────

async fn run_event_loop(
    queue: PacketQueue,
    ctx: Arc<SessionContext>,
    fake_dns: Arc<FakeDns>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> io::Result<()> {
    let mut stack = IpStack::new(queue.clone());
    let waker = queue.notifier();

    // Key: (src_addr, dst_addr) from original SYN.
    // We rewrite the SYN dst_port → ephemeral port so smoltcp can handle
    // multiple connections to the same dst port (443).
    let mut sessions: HashMap<TcpSessionKey, ActiveSession> = HashMap::new();

    // Map: ephemeral_port → original TcpSessionKey.
    // Used to find which session a smoltcp socket belongs to.
    let mut port_to_key: HashMap<u16, TcpSessionKey> = HashMap::new();

    let mut stale_keys: Vec<TcpSessionKey> = Vec::new();

    loop {
        if *shutdown_rx.borrow() { return Ok(()); }

        // ── 1. Intercept DNS ────────────────────────────────────────
        let mut tcp_packets = Vec::new();
        while let Some(packet) = queue.pop_inbound_public() {
            if let Some(dns_response) = try_handle_dns(&packet, &fake_dns) {
                ctx.stats.add_dns_query();
                queue.push_outbound_public(dns_response);
            } else {
                tcp_packets.push(packet);
            }
        }

        // ── 2. Process TCP packets: rewrite dst port for SYNs ───────
        for mut pkt in tcp_packets {
            if pkt.len() < 20 { queue.push_inbound(pkt); continue; }
            if pkt[0] >> 4 != 4 { queue.push_inbound(pkt); continue; }
            let ihl = (pkt[0] & 0x0F) as usize * 4;
            let protocol = pkt[9];

            // Only rewrite TCP packets.
            if protocol == 6 && pkt.len() >= ihl + 14 {
                let src_port = u16::from_be_bytes([pkt[ihl], pkt[ihl+1]]);
                let dst_port = u16::from_be_bytes([pkt[ihl+2], pkt[ihl+3]]);
                let flags = pkt[ihl + 13];
                let is_syn = flags & 0x02 != 0 && flags & 0x10 == 0;

                let src_ip = Ipv4Addr::new(pkt[12], pkt[13], pkt[14], pkt[15]);
                let dst_ip = Ipv4Addr::new(pkt[16], pkt[17], pkt[18], pkt[19]);
                let orig_key = TcpSessionKey {
                    src_addr: SocketAddr::new(IpAddr::V4(src_ip), src_port),
                    dst_addr: SocketAddr::new(IpAddr::V4(dst_ip), dst_port),
                };

                if is_syn && !sessions.contains_key(&orig_key) {
                    // New connection: assign ephemeral port, create listen socket.
                    let eph_port = next_ephemeral_port();
                    let handle = stack.add_tcp_socket(65535, 65535);
                    let socket = stack.tcp_socket_mut(handle);

                    if socket.listen(eph_port).is_ok() {
                        // Cache domain NOW — FakeDns TTL might expire before Established.
                        let domain = if let IpAddr::V4(v4) = dst_ip.into() {
                            fake_dns.lookup(v4)
                        } else { None };

                        sessions.insert(orig_key, ActiveSession {
                            smol_handle: handle,
                            real_dst: orig_key.dst_addr,
                            domain,
                            eph_port,
                            relay: None,
                        });
                        port_to_key.insert(eph_port, orig_key);
                        ctx.stats.connection_opened();
                        ctx.stats.add_tcp_syn();

                        // Rewrite dst → smoltcp's IP:eph_port.
                        pkt[ihl+2] = (eph_port >> 8) as u8;
                        pkt[ihl+3] = eph_port as u8;
                        let smol_ip = crate::ip_stack::SMOL_IP;
                        let src_ip_inb = Ipv4Addr::new(pkt[12], pkt[13], pkt[14], pkt[15]);
                        pkt[16] = smol_ip.octets()[0];
                        pkt[17] = smol_ip.octets()[1];
                        pkt[18] = smol_ip.octets()[2];
                        pkt[19] = smol_ip.octets()[3];
                        pkt[10] = 0; pkt[11] = 0;
                        let ip_cksum = ipv4_checksum(&pkt[..ihl]);
                        pkt[10..12].copy_from_slice(&ip_cksum.to_be_bytes());
                        let smol_addr = Ipv4Addr::new(smol_ip.octets()[0], smol_ip.octets()[1], smol_ip.octets()[2], smol_ip.octets()[3]);
                        tcp_checksum_update(&mut pkt, ihl, &src_ip_inb, &smol_addr);
                    } else {
                        stack.remove_socket(handle);
                    }
                } else if let Some(session) = sessions.get(&orig_key) {
                    // Existing connection (ACK, data, FIN, etc.):
                    // Rewrite dst → smoltcp's IP:eph_port.
                    let ep = session.eph_port;
                    pkt[ihl+2] = (ep >> 8) as u8;
                    pkt[ihl+3] = ep as u8;
                    let src_ip_inb = Ipv4Addr::new(pkt[12], pkt[13], pkt[14], pkt[15]);
                    let smol_ip = crate::ip_stack::SMOL_IP;
                    pkt[16] = smol_ip.octets()[0];
                    pkt[17] = smol_ip.octets()[1];
                    pkt[18] = smol_ip.octets()[2];
                    pkt[19] = smol_ip.octets()[3];
                    pkt[10] = 0; pkt[11] = 0;
                    let ip_cksum = ipv4_checksum(&pkt[..ihl]);
                    pkt[10..12].copy_from_slice(&ip_cksum.to_be_bytes());
                    let smol_addr = Ipv4Addr::new(smol_ip.octets()[0], smol_ip.octets()[1], smol_ip.octets()[2], smol_ip.octets()[3]);
                    tcp_checksum_update(&mut pkt, ihl, &src_ip_inb, &smol_addr);
                }
            }

            queue.push_inbound(pkt);
        }

        // ── 3. Poll smoltcp ─────────────────────────────────────────
        for _ in 0..16 {
            if !stack.poll() { break; }
        }

        // ── 4. Check sessions: spawn relay, transfer data ───────────
        stale_keys.clear();

        for (key, session) in sessions.iter_mut() {
            let socket = stack.tcp_socket_mut(session.smol_handle);
            let tcp_state = socket.state();

            // Spawn relay when handshake completes.
            if tcp_state == TcpState::Established && session.relay.is_none() {
                let (to_relay_tx, to_relay_rx) = mpsc::channel(512);
                let (from_relay_tx, from_relay_rx) = mpsc::channel(512);
                session.relay = Some(RelayChannels {
                    to_relay: to_relay_tx,
                    from_relay: from_relay_rx,
                });

                let ctx_clone = ctx.clone();
                let real_dst = session.real_dst;
                let cached_domain = session.domain.clone();
                let key_clone = *key;
                // "ip-only" = сессия без восстановленного домена (IP-литерал или
                // просроченный fake DNS-кэш). "NO_DOMAIN" было слишком техническим.
                let domain_tag = cached_domain.as_deref().unwrap_or("ip-only").to_string();
                let relay_waker = waker.clone();
                tokio::spawn(async move {
                    let relay_key = TcpSessionKey {
                        src_addr: key_clone.src_addr,
                        dst_addr: real_dst,
                    };
                    match relay_session_with_domain(ctx_clone.clone(), relay_key, cached_domain, to_relay_rx, from_relay_tx, relay_waker).await {
                        Ok(()) => {}
                        Err(e) => {
                            let msg = format!("[{}] {}: {}", domain_tag, real_dst, e);
                            let err_text = e.to_string();

                            // Специальный случай: DoT-блок. Он работает по контракту
                            // Android (kind=ConnectionRefused заставляет Private DNS
                            // упасть в UDP fallback), случается на КАЖДОМ DNS-запросе
                            // и не является ни warning'ом, ни error'ом. Пишем только
                            // в tracing для отладки; в recent_errors не пускаем.
                            if err_text.contains("DoT blocked") {
                                tracing::debug!("{}", msg);
                                return;
                            }

                            // Классификация по io::ErrorKind:
                            //   InvalidInput → policy drop (fake IP, private IP) → WARN.
                            //   ConnectionReset / ConnectionAborted → удалённый сервер
                            //     закрыл соединение (RST) или ядро оборвало при смене
                            //     сети. Не наш косяк — это нормальное сетевое событие,
                            //     понижаем до WARN, чтобы реальные ошибки не тонули в
                            //     повторяющемся шуме (напр. Kaspersky-телеметрия в
                            //     цикле получает RST от своих же серверов).
                            //     Исключение: "mux open fail" всегда ERROR — тут
                            //     недоступен наш прокси-сервер, пользователю это важно.
                            //   Всё остальное (TimedOut на connect, Other, ...) → ERROR.
                            let is_mux_fail = err_text.contains("mux open fail");
                            match e.kind() {
                                std::io::ErrorKind::InvalidInput => {
                                    ctx_clone.stats.add_warn(&msg);
                                }
                                std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                                    if !is_mux_fail =>
                                {
                                    ctx_clone.stats.add_warn(&msg);
                                }
                                _ => {
                                    ctx_clone.stats.add_error(&msg);
                                }
                            }
                        }
                    }
                });
            }

            // Transfer data.
            if let Some(ref mut relay) = session.relay {
                // Detect dead relay: if receiver dropped, relay task died.
                // Abort socket so the app gets RST and retries with fresh DNS.
                if relay.to_relay.is_closed() {
                    socket.abort();
                    stale_keys.push(*key);
                    continue;
                }

                // smoltcp → relay (upload).
                while socket.can_recv() {
                    let mut buf = vec![0u8; 32768];
                    match socket.recv_slice(&mut buf) {
                        Ok(n) if n > 0 => {
                            buf.truncate(n);
                            ctx.stats.add_smol_recv(n as u64);
                            if relay.to_relay.try_send(buf).is_err() { break; }
                        }
                        _ => break,
                    }
                }

                // relay → smoltcp (download).
                while socket.can_send() {
                    match relay.from_relay.try_recv() {
                        Ok(data) => {
                            ctx.stats.add_smol_send(data.len() as u64);
                            let _ = socket.send_slice(&data);
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                            // Relay sender dropped — relay task finished.
                            socket.abort();
                            stale_keys.push(*key);
                            break;
                        }
                    }
                }
            }

            // Detect closed.
            if tcp_state == TcpState::Closed || tcp_state == TcpState::TimeWait {
                stale_keys.push(*key);
            }
        }

        // ── 5. Cleanup ──────────────────────────────────────────────
        for key in &stale_keys {
            if let Some(session) = sessions.remove(key) {
                port_to_key.remove(&session.eph_port);
                stack.remove_socket(session.smol_handle);
                ctx.stats.connection_closed();
            }
        }

        // ── 6. Poll again (process data written by relay → smoltcp) ─
        for _ in 0..16 {
            if !stack.poll() { break; }
        }

        // ── 7. Rewrite outbound packets: restore original dst ───────
        // smoltcp sends packets with src=SMOL_IP:eph_port.
        // We need to rewrite them to src=original_dst_ip:original_dst_port
        // so the TUN client sees the response from the expected address.
        while let Some(mut pkt) = queue.pop_smol_outbound() {
            if pkt.len() >= 40 && pkt[0] >> 4 == 4 && pkt[9] == 6 {
                let ihl = (pkt[0] & 0x0F) as usize * 4;
                if pkt.len() >= ihl + 4 {
                    let src_port = u16::from_be_bytes([pkt[ihl], pkt[ihl+1]]);
                    if let Some(orig_key) = port_to_key.get(&src_port) {
                        if let IpAddr::V4(orig_dst_ip) = orig_key.dst_addr.ip() {
                            pkt[12] = orig_dst_ip.octets()[0];
                            pkt[13] = orig_dst_ip.octets()[1];
                            pkt[14] = orig_dst_ip.octets()[2];
                            pkt[15] = orig_dst_ip.octets()[3];
                            let orig_port = orig_key.dst_addr.port();
                            pkt[ihl] = (orig_port >> 8) as u8;
                            pkt[ihl+1] = orig_port as u8;
                            pkt[10] = 0; pkt[11] = 0;
                            let ip_cksum = ipv4_checksum(&pkt[..ihl]);
                            pkt[10..12].copy_from_slice(&ip_cksum.to_be_bytes());
                            let dst_ip = Ipv4Addr::new(pkt[16], pkt[17], pkt[18], pkt[19]);
                            tcp_checksum_update(&mut pkt, ihl, &orig_dst_ip, &dst_ip);
                        }
                    }
                }
            }
            queue.push_outbound_public(pkt);
        }

        // ── 8. Debug ────────────────────────────────────────────────
        {
            let (mut established, mut syn_recv, mut listen, mut other) = (0u32, 0u32, 0u32, 0u32);
            for s in sessions.values() {
                match stack.tcp_socket(s.smol_handle).state() {
                    TcpState::Established => established += 1,
                    TcpState::SynReceived => syn_recv += 1,
                    TcpState::Listen => listen += 1,
                    _ => other += 1,
                }
            }
            if !sessions.is_empty() {
                ctx.stats.set_debug(format!(
                    "dev rx:{} tx:{} | L:{} SR:{} E:{} o:{} | s:{}",
                    stack.device.rx_count, stack.device.tx_count,
                    listen, syn_recv, established, other, sessions.len()
                ));
            }
        }

        // ── 9. Wait for new data or smoltcp timer ────────────────
        // If there's still pending data, loop immediately without waiting.
        if queue.has_inbound() || queue.has_outbound() {
            tokio::task::yield_now().await;
            continue;
        }

        // Sleep until woken by: TUN packet, relay data, smoltcp timer, or shutdown.
        let poll_delay = stack.poll_delay()
            .unwrap_or(Duration::from_millis(50))
            .min(Duration::from_millis(50));
        tokio::select! {
            _ = waker.notified() => {}
            _ = tokio::time::sleep(poll_delay) => {}
            _ = shutdown_rx.changed() => {}
        }
    }
}

// ── Packet helpers ──────────────────────────────────────────────────

fn try_handle_dns(packet: &[u8], fake_dns: &FakeDns) -> Option<Vec<u8>> {
    let (src_ip, dst_ip, protocol, ihl) = parse_ipv4_header(packet)?;
    if protocol != 17 { return None; }
    let udp = &packet[ihl..];
    let (src_port, dst_port, data_offset) = parse_udp_header(udp)?;
    if dst_port != 53 { return None; }
    let (dns_response, _) = fake_dns.handle_query(&udp[data_offset..])?;
    Some(build_udp_response(dst_ip, src_ip, dst_port, src_port, &dns_response))
}

pub fn parse_ipv4_header(p: &[u8]) -> Option<(Ipv4Addr, Ipv4Addr, u8, usize)> {
    if p.len() < 20 || p[0] >> 4 != 4 { return None; }
    let ihl = (p[0] & 0x0F) as usize * 4;
    if ihl < 20 || p.len() < ihl { return None; }
    Some((Ipv4Addr::new(p[12],p[13],p[14],p[15]), Ipv4Addr::new(p[16],p[17],p[18],p[19]), p[9], ihl))
}

pub fn parse_udp_header(p: &[u8]) -> Option<(u16, u16, usize)> {
    if p.len() < 8 { return None; }
    Some((u16::from_be_bytes([p[0],p[1]]), u16::from_be_bytes([p[2],p[3]]), 8))
}

pub fn parse_tcp_ports(p: &[u8]) -> Option<(u16, u16)> {
    if p.len() < 4 { return None; }
    Some((u16::from_be_bytes([p[0],p[1]]), u16::from_be_bytes([p[2],p[3]])))
}

pub fn build_udp_response(src_ip: Ipv4Addr, dst_ip: Ipv4Addr, src_port: u16, dst_port: u16, payload: &[u8]) -> Vec<u8> {
    let total = 20 + 8 + payload.len();
    let mut p = vec![0u8; total];
    p[0] = 0x45; p[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    p[8] = 64; p[9] = 17;
    p[12..16].copy_from_slice(&src_ip.octets()); p[16..20].copy_from_slice(&dst_ip.octets());
    let ck = ipv4_checksum(&p[..20]); p[10..12].copy_from_slice(&ck.to_be_bytes());
    p[20..22].copy_from_slice(&src_port.to_be_bytes()); p[22..24].copy_from_slice(&dst_port.to_be_bytes());
    p[24..26].copy_from_slice(&((8+payload.len()) as u16).to_be_bytes());
    p[28..].copy_from_slice(payload); p
}

/// Recalculate TCP checksum after NAT rewrite.
fn tcp_checksum_update(pkt: &mut [u8], ihl: usize, src_ip: &Ipv4Addr, dst_ip: &Ipv4Addr) {
    let tcp_len = pkt.len() - ihl;
    // Clear existing checksum.
    pkt[ihl + 16] = 0;
    pkt[ihl + 17] = 0;

    let mut sum = 0u32;
    // Pseudo-header: src IP, dst IP, zero, protocol(6), TCP length.
    for pair in src_ip.octets().chunks(2) {
        sum += u16::from_be_bytes([pair[0], pair[1]]) as u32;
    }
    for pair in dst_ip.octets().chunks(2) {
        sum += u16::from_be_bytes([pair[0], pair[1]]) as u32;
    }
    sum += 6u32; // protocol TCP
    sum += tcp_len as u32;

    // TCP segment.
    let tcp = &pkt[ihl..];
    for i in (0..tcp.len()).step_by(2) {
        let word = if i + 1 < tcp.len() {
            u16::from_be_bytes([tcp[i], tcp[i + 1]])
        } else {
            u16::from_be_bytes([tcp[i], 0])
        };
        sum += word as u32;
    }

    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    let cksum = !(sum as u16);
    pkt[ihl + 16] = (cksum >> 8) as u8;
    pkt[ihl + 17] = cksum as u8;
}

fn ipv4_checksum(h: &[u8]) -> u16 {
    let mut sum = 0u32;
    for i in (0..h.len()).step_by(2) {
        if i == 10 { continue; }
        sum += if i+1 < h.len() { u16::from_be_bytes([h[i],h[i+1]]) } else { u16::from_be_bytes([h[i],0]) } as u32;
    }
    while sum >> 16 != 0 { sum = (sum & 0xFFFF) + (sum >> 16); }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ipv4_header() {
        let mut pkt = vec![0u8; 40];
        pkt[0] = 0x45; pkt[9] = 6;
        pkt[12..16].copy_from_slice(&[10, 0, 0, 2]);
        pkt[16..20].copy_from_slice(&[93, 184, 216, 34]);
        let (src, dst, proto, ihl) = parse_ipv4_header(&pkt).unwrap();
        assert_eq!(src, Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(dst, Ipv4Addr::new(93, 184, 216, 34));
        assert_eq!(proto, 6); assert_eq!(ihl, 20);
    }

}
