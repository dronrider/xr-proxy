//! UDP relay: путь UDP из TUN через туннельный порт на VPS и обратно.
//!
//! Модель повторяет роутерный `xr-client/src/udp_relay.rs`: своя таблица флоу
//! с NAT по туннельному порту, endpoint-independent маппинг, потолок
//! `max_flows` с вытеснением наименее свежего. Отличие одно: ответ уходит не
//! спуфящим сокетом, а собранным пакетом в TUN (`PacketQueue`), поэтому
//! `SpoofCache` ядру не нужен.
//!
//! Дорога пакета:
//! ```text
//! TUN -> tun_to_wire -> RelayPacket -> encode -> UDP-сокет к VPS
//! TUN <- wire_to_tun <- RelayPacket <- decode <- UDP-сокет от VPS
//! ```

use std::collections::HashMap;
use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::time::{Duration, Instant};

use xr_proto::obfuscation::Obfuscator;
use xr_proto::udp_relay::{self, RelayPacket, RelayType};

use crate::engine::{build_udp_response_ip, parse_ip_header, parse_udp_header};

/// Туннельный порт выдаётся на `(адрес, порт)` устройства и держится на всех
/// его адресатов сразу. Адресата в ключ не брать: иначе NAT на VPS становится
/// symmetric, а от типа NAT у Switch и Xbox зависит мультиплеер.
pub const TUNNEL_PORT_POOL: RangeInclusive<u16> = 40000..=65000;

/// Потолок таблицы флоу (XR-204). Отказ новому оставил бы потолок в руках
/// одного пишущего до конца flow_timeout.
pub const DEFAULT_MAX_FLOWS: usize = 1024;

/// Простой флоу возвращает туннельный порт в пул.
pub const DEFAULT_FLOW_TIMEOUT: Duration = Duration::from_secs(120);

/// Период keepalive: держит NAT-дыру к VPS, ответ сервер уходит написавшему.
pub const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(25);

/// Порт UDP relay на VPS (по умолчанию как у `xr-server`).
pub const DEFAULT_RELAY_PORT: u16 = 9999;

/// Буфер приёма downstream. Как у `xr-client` и `xr-server`: ответ крупнее
/// буфера обрезается и не декодируется.
pub const UDP_RECV_BUF: usize = 65536;

/// Порты, которые не уходят в реле: DNS перехватывает FakeDns, DHCP живёт
/// локальной сетью.
const EXCLUDE_DST_PORTS: [u16; 3] = [53, 67, 68];

struct UdpFlow {
    src_addr: SocketAddr,
    last_activity: Instant,
}

/// Таблица флоу с NAT по туннельному порту. Логика сокетов не касается, чтобы
/// гоняться юнитами без TUN и сети.
struct FlowTable {
    /// туннельный порт -> флоу
    flows: HashMap<u16, UdpFlow>,
    /// адрес устройства -> его туннельный порт
    ports: HashMap<SocketAddr, u16>,
    pool: RangeInclusive<u16>,
    max_flows: usize,
}

impl FlowTable {
    fn new(pool: RangeInclusive<u16>, max_flows: usize) -> Self {
        FlowTable {
            flows: HashMap::new(),
            ports: HashMap::new(),
            pool,
            max_flows,
        }
    }

    /// Туннельный порт устройства, заводя флоу на первом пакете. `None` значит,
    /// что свободных портов не осталось и пакет придётся отбросить.
    fn touch(&mut self, src_addr: SocketAddr, now: Instant) -> Option<u16> {
        if let Some(&port) = self.ports.get(&src_addr) {
            if let Some(flow) = self.flows.get_mut(&port) {
                flow.last_activity = now;
            }
            return Some(port);
        }

        if self.flows.len() >= self.max_flows {
            self.evict_stalest();
        }

        let port = match self.allocate(src_addr.port()) {
            Some(p) => p,
            None => {
                tracing::warn!(
                    "UDP relay: tunnel port pool exhausted ({} flows), dropping {}",
                    self.flows.len(),
                    src_addr
                );
                return None;
            }
        };
        if port != src_addr.port() {
            tracing::debug!("UDP relay: {} goes through tunnel port {}", src_addr, port);
        }

        self.ports.insert(src_addr, port);
        self.flows.insert(port, UdpFlow { src_addr, last_activity: now });
        Some(port)
    }

    /// Настоящий порт устройства достаётся тому, кто пришёл первым: NAT
    /// приставок и P2P держатся на нём. Остальным идёт первый свободный номер
    /// из пула, выдача детерминированная.
    fn allocate(&self, preferred: u16) -> Option<u16> {
        if preferred != 0 && !self.flows.contains_key(&preferred) {
            return Some(preferred);
        }
        self.pool.clone().find(|p| !self.flows.contains_key(p))
    }

    /// Снять наименее свежий флоу, вернув его туннельный порт в пул.
    fn evict_stalest(&mut self) {
        let victim = self
            .flows
            .iter()
            .min_by_key(|(_, flow)| flow.last_activity)
            .map(|(port, _)| *port);
        let port = match victim {
            Some(port) => port,
            None => return,
        };
        if let Some(flow) = self.flows.remove(&port) {
            self.ports.remove(&flow.src_addr);
            tracing::warn!(
                "UDP relay: flow table full ({} flows), evicted stalest {} (tunnel port {})",
                self.flows.len() + 1,
                flow.src_addr,
                port
            );
        }
    }

    /// Пакет от устройства, готовый к отправке в туннель.
    fn upstream_packet(
        &mut self,
        src_addr: SocketAddr,
        orig_dst: SocketAddr,
        payload: Vec<u8>,
        now: Instant,
    ) -> Option<RelayPacket> {
        let tunnel_port = self.touch(src_addr, now)?;
        Some(RelayPacket {
            relay_type: RelayType::Data,
            dst: orig_dst,
            src_port: tunnel_port,
            payload,
        })
    }

    /// Кому отдать ответ из туннеля и с какого адреса подать источник.
    /// `packet.src_port` это туннельный порт, он же ключ таблицы.
    fn downstream_target(
        &mut self,
        packet: &RelayPacket,
        now: Instant,
    ) -> Option<(SocketAddr, SocketAddr)> {
        let flow = self.flows.get_mut(&packet.src_port)?;
        flow.last_activity = now;
        Some((flow.src_addr, packet.dst))
    }

    /// Снимает протухшие флоу, возвращая их туннельные порты в пул.
    fn retire_expired(&mut self, timeout: Duration, now: Instant) -> usize {
        let expired: Vec<u16> = self
            .flows
            .iter()
            .filter(|(_, f)| now.duration_since(f.last_activity) >= timeout)
            .map(|(port, _)| *port)
            .collect();

        for port in &expired {
            if let Some(flow) = self.flows.remove(port) {
                self.ports.remove(&flow.src_addr);
            }
        }
        expired.len()
    }

    fn clear(&mut self) {
        self.flows.clear();
        self.ports.clear();
    }

    fn len(&self) -> usize {
        self.flows.len()
    }
}

/// Реле одного клиента: таблица флоу плюс обфускатор провода.
///
/// Чистая логика пакетов без сокетов: тестируется на PacketQueue и на
/// encode/decode, а сокет к VPS держит вызывающий (event loop или таск).
pub struct UdpRelay {
    flows: FlowTable,
    obfuscator: Obfuscator,
    flow_timeout: Duration,
}

impl UdpRelay {
    pub fn new(obfuscator: Obfuscator, max_flows: usize, flow_timeout: Duration) -> Self {
        UdpRelay {
            flows: FlowTable::new(TUNNEL_PORT_POOL, max_flows),
            obfuscator,
            flow_timeout,
        }
    }

    /// Пакет из TUN в провод. Не-UDP, исключённый порт и исчерпанный пул дают
    /// `None`; такой пакет не уезжает в туннель и в smoltcp не возвращается.
    pub fn tun_to_wire(&mut self, packet: &[u8], now: Instant) -> Option<Vec<u8>> {
        let (src_ip, dst_ip, proto, transport_off) = parse_ip_header(packet)?;
        if proto != 17 {
            return None;
        }
        let udp = &packet[transport_off..];
        let (src_port, dst_port, data_offset) = parse_udp_header(udp)?;
        if EXCLUDE_DST_PORTS.contains(&dst_port) {
            return None;
        }

        let src_addr = SocketAddr::new(src_ip, src_port);
        let dst_addr = SocketAddr::new(dst_ip, dst_port);
        let relay = self
            .flows
            .upstream_packet(src_addr, dst_addr, udp[data_offset..].to_vec(), now)?;
        Some(udp_relay::encode_relay_packet(&self.obfuscator, &relay))
    }

    /// Провод от VPS в пакет TUN. Keepalive и чужой трафик дают `None` и на
    /// устройство не уходят.
    pub fn wire_to_tun(&mut self, data: &[u8], now: Instant) -> Option<Vec<u8>> {
        let packet = udp_relay::decode_relay_packet(&self.obfuscator, data)?;
        if packet.relay_type != RelayType::Data {
            return None;
        }

        let (device, spoof_src) = self.flows.downstream_target(&packet, now)?;
        Some(build_udp_response_ip(
            spoof_src.ip(),
            device.ip(),
            spoof_src.port(),
            device.port(),
            &packet.payload,
        ))
    }

    /// Keepalive для VPS: держит NAT-дыру, сервер отвечает написавшему.
    pub fn keepalive(&self) -> Vec<u8> {
        udp_relay::encode_keepalive(&self.obfuscator)
    }

    /// Чистка протухших флоу. Возвращает число снятых.
    pub fn retire_expired(&mut self, now: Instant) -> usize {
        let removed = self.flows.retire_expired(self.flow_timeout, now);
        if removed > 0 {
            tracing::debug!(
                "UDP relay: cleaned {} expired flows ({} active)",
                removed,
                self.flows.len()
            );
        }
        removed
    }

    /// Смена сети: флоу старого аплинка не живут, порты возвращаются в пул.
    pub fn clear(&mut self) {
        self.flows.clear();
    }

    pub fn len(&self) -> usize {
        self.flows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.flows.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{build_udp_response, parse_ipv4_header};
    use crate::ip_stack::PacketQueue;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use xr_proto::obfuscation::ModifierStrategy;

    fn test_obfuscator() -> Obfuscator {
        let key = b"test-key-32-bytes-long-enough!!!".to_vec();
        Obfuscator::new(key, 0xDEADBEEF, ModifierStrategy::PositionalXorRotate)
    }

    fn relay() -> UdpRelay {
        UdpRelay::new(test_obfuscator(), DEFAULT_MAX_FLOWS, DEFAULT_FLOW_TIMEOUT)
    }

    fn table() -> FlowTable {
        FlowTable::new(TUNNEL_PORT_POOL, usize::MAX)
    }

    fn v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), port)
    }

    fn v6(addr: Ipv6Addr, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V6(addr), port)
    }

    fn data(src_port: u16, dst: SocketAddr, payload: &[u8]) -> RelayPacket {
        RelayPacket {
            relay_type: RelayType::Data,
            dst,
            src_port,
            payload: payload.to_vec(),
        }
    }

    /// Собирает IPv4/UDP пакет, какой приходит из TUN.
    fn tun_udp(src: SocketAddr, dst: SocketAddr, payload: &[u8]) -> Vec<u8> {
        let (IpAddr::V4(src_ip), IpAddr::V4(dst_ip)) = (src.ip(), dst.ip()) else {
            panic!("tun_udp expects IPv4");
        };
        build_udp_response(src_ip, dst_ip, src.port(), dst.port(), payload)
    }

    /// Собирает IPv6/UDP пакет, какой приходит из TUN.
    fn tun_udp_v6(src: SocketAddr, dst: SocketAddr, payload: &[u8]) -> Vec<u8> {
        let (IpAddr::V6(src_ip), IpAddr::V6(dst_ip)) = (src.ip(), dst.ip()) else {
            panic!("tun_udp_v6 expects IPv6");
        };
        crate::engine::build_udp_response_v6(src_ip, dst_ip, src.port(), dst.port(), payload)
    }

    // -- Таблица флоу --

    /// Два устройства на одном порту (Xbox 3074) получают свои туннельные
    /// порты, ответы не пересекаются.
    #[test]
    fn two_devices_on_one_port_get_own_replies() {
        let mut t = table();
        let now = Instant::now();
        let server = v4(203, 0, 113, 5, 3074);
        let first = v4(192, 168, 1, 10, 3074);
        let second = v4(192, 168, 1, 11, 3074);

        let up_first = t.upstream_packet(first, server, b"a".to_vec(), now).unwrap();
        let up_second = t.upstream_packet(second, server, b"b".to_vec(), now).unwrap();
        assert_ne!(
            up_first.src_port, up_second.src_port,
            "двум устройствам нельзя отдавать один туннельный порт"
        );

        assert_eq!(
            t.downstream_target(&data(up_first.src_port, server, b"a-reply"), now),
            Some((first, server))
        );
        assert_eq!(
            t.downstream_target(&data(up_second.src_port, server, b"b-reply"), now),
            Some((second, server))
        );
    }

    /// Маппинг endpoint-independent: одно устройство уходит на VPS одним и тем
    /// же портом ко всем пирам.
    #[test]
    fn device_keeps_one_tunnel_port_for_all_peers() {
        let mut t = table();
        let now = Instant::now();
        let device = v4(192, 168, 1, 10, 3074);
        let matchmaking = v4(203, 0, 113, 5, 3074);
        let peer = v4(198, 51, 100, 7, 51820);

        let to_matchmaking = t.upstream_packet(device, matchmaking, b"a".to_vec(), now).unwrap();
        let to_peer = t.upstream_packet(device, peer, b"b".to_vec(), now).unwrap();

        assert_eq!(to_matchmaking.src_port, to_peer.src_port);
        assert_eq!(to_matchmaking.dst, matchmaking);
        assert_eq!(to_peer.dst, peer);
        assert_eq!(t.len(), 1, "устройству положен один флоу на все назначения");
    }

    #[test]
    fn first_device_keeps_its_real_port() {
        let mut t = table();
        let now = Instant::now();

        let up = t
            .upstream_packet(v4(192, 168, 1, 10, 3074), v4(203, 0, 113, 5, 3074), b"a".to_vec(), now)
            .unwrap();
        assert_eq!(up.src_port, 3074, "NAT приставок держится на настоящем порту");
    }

    #[test]
    fn substitute_port_comes_from_pool_head() {
        let now = Instant::now();

        let ports = || {
            let mut t = table();
            t.touch(v4(192, 168, 1, 10, 3074), now).unwrap();
            (
                t.touch(v4(192, 168, 1, 11, 3074), now).unwrap(),
                t.touch(v4(192, 168, 1, 12, 3074), now).unwrap(),
            )
        };

        assert_eq!(ports(), (40000, 40001));
        assert_eq!(ports(), (40000, 40001), "выдача обязана быть детерминированной");
    }

    #[test]
    fn expired_flow_returns_its_port_to_pool() {
        let mut t = table();
        let now = Instant::now();
        let timeout = Duration::from_secs(60);

        t.touch(v4(192, 168, 1, 10, 3074), now).unwrap();
        assert_eq!(t.touch(v4(192, 168, 1, 11, 3074), now), Some(40000));

        let later = now + timeout;
        assert_eq!(t.retire_expired(timeout, later), 2);
        assert_eq!(t.len(), 0);

        assert_eq!(t.touch(v4(192, 168, 1, 12, 3074), later), Some(3074));
        assert_eq!(t.touch(v4(192, 168, 1, 13, 3074), later), Some(40000));
    }

    /// Свежесть двигают пакеты в обе стороны: живой флоу переживает чистку,
    /// протухший сосед уходит и отдаёт порт.
    #[test]
    fn freshness_moves_on_both_directions() {
        let mut t = table();
        let now = Instant::now();
        let server = v4(203, 0, 113, 5, 3074);
        let live = v4(192, 168, 1, 10, 3074);
        let stale = v4(192, 168, 1, 11, 3074);
        let timeout = Duration::from_secs(60);

        let live_port = t.touch(live, now).unwrap();
        t.touch(stale, now).unwrap();

        // Ответ из туннеля двигает свежесть так же, как пакет в туннель.
        let later = now + Duration::from_secs(50);
        assert!(t.downstream_target(&data(live_port, server, b"pong"), later).is_some());
        assert_eq!(t.retire_expired(timeout, now + Duration::from_secs(70)), 1);
        assert_eq!(
            t.downstream_target(&data(live_port, server, b"pong"), now + Duration::from_secs(70)),
            Some((live, server))
        );
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn flood_beyond_limit_keeps_table_at_ceiling_and_returns_ports() {
        let mut t = FlowTable::new(TUNNEL_PORT_POOL, 3);
        let now = Instant::now();

        assert_eq!(t.touch(v4(192, 0, 2, 10, 3074), now).unwrap(), 3074);
        assert_eq!(t.touch(v4(192, 0, 2, 11, 3074), now + Duration::from_secs(1)).unwrap(), 40000);
        assert_eq!(t.touch(v4(192, 0, 2, 12, 3074), now + Duration::from_secs(2)).unwrap(), 40001);
        assert_eq!(t.len(), 3);

        assert_eq!(
            t.touch(v4(192, 0, 2, 13, 3074), now + Duration::from_secs(3)).unwrap(),
            3074,
            "порт вытесненного флоу обязан вернуться в пул"
        );
        assert_eq!(t.len(), 3, "поток сверх лимита не растит таблицу");
    }

    #[test]
    fn eviction_spares_living_flow() {
        let mut t = FlowTable::new(TUNNEL_PORT_POOL, 3);
        let now = Instant::now();
        let server = v4(203, 0, 113, 5, 3074);
        let device = v4(192, 0, 2, 10, 3074);

        let device_port = t.touch(device, now).unwrap();
        t.touch(v4(192, 0, 2, 11, 41000), now + Duration::from_secs(1)).unwrap();
        t.touch(v4(192, 0, 2, 12, 41001), now + Duration::from_secs(2)).unwrap();

        for i in 0u32..5 {
            let stamp = now + Duration::from_secs(3 + u64::from(i));
            let attacker = v4(192, 0, 2, 30, 50000 + i as u16);
            t.upstream_packet(device, server, b"ping".to_vec(), stamp).unwrap();
            let up = t.upstream_packet(attacker, server, b"x".to_vec(), stamp).unwrap();
            assert!(
                t.downstream_target(&data(up.src_port, server, b"y"), stamp).is_some(),
                "новый флоу сверх лимита получает запись, а не отказ"
            );
        }

        assert_eq!(t.len(), 3);
        assert_eq!(
            t.downstream_target(&data(device_port, server, b"z"), now + Duration::from_secs(10)),
            Some((device, server)),
            "живой флоу переживает напор сверх лимита"
        );
    }

    // -- Путь пакета --

    #[test]
    fn tun_to_wire_and_back_v4() {
        let mut r = relay();
        let now = Instant::now();
        let device = v4(192, 168, 1, 10, 57976);
        let server = v4(203, 0, 113, 5, 443);

        let wire = r
            .tun_to_wire(&tun_udp(device, server, b"hello"), now)
            .expect("UDP из TUN уезжает в провод");
        let back = r.wire_to_tun(&wire, now).expect("тот же пакет не возвращается в TUN");

        let (src_ip, dst_ip, proto, ihl) = parse_ipv4_header(&back).unwrap();
        assert_eq!(proto, 17);
        assert_eq!(src_ip, Ipv4Addr::new(203, 0, 113, 5), "источник ответа это сервер");
        assert_eq!(dst_ip, Ipv4Addr::new(192, 168, 1, 10), "адресат это устройство");
        let (src_port, dst_port, off) = parse_udp_header(&back[ihl..]).unwrap();
        assert_eq!((src_port, dst_port), (443, 57976));
        assert_eq!(&back[ihl..][off..], b"hello");
    }

    /// Тот же путь пакета для IPv6: адреса и порты переживают round-trip.
    #[test]
    fn tun_to_wire_and_back_v6() {
        let mut r = relay();
        let now = Instant::now();
        let device = v6(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 2), 57976);
        let server = v6(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1), 443);

        let wire = r
            .tun_to_wire(&tun_udp_v6(device, server, b"hello6"), now)
            .expect("UDP из TUN уезжает в провод");
        let back = r.wire_to_tun(&wire, now).expect("тот же пакет не возвращается в TUN");

        let (src_ip, dst_ip, proto, off) = crate::engine::parse_ip_header(&back).unwrap();
        assert_eq!(proto, 17);
        assert_eq!(src_ip, server.ip(), "источник ответа это сервер");
        assert_eq!(dst_ip, device.ip(), "адресат это устройство");
        let (src_port, dst_port, udp_off) = parse_udp_header(&back[off..]).unwrap();
        assert_eq!((src_port, dst_port), (443, 57976));
        assert_eq!(&back[off..][udp_off..], b"hello6");
    }

    /// Провод умеет адреса обоих семейств; round-trip через обфускатор обязан
    /// их сохранять.
    #[test]
    fn relay_packet_roundtrip_v4_and_v6_through_obfuscator() {
        let obfs = test_obfuscator();

        let targets = [
            v4(203, 0, 113, 5, 443),
            v6(Ipv6Addr::new(0x2001, 0xb28, 0xf23d, 0, 0, 0, 0, 1), 443),
        ];
        for dst in targets {
            let packet = data(40001, dst, b"payload");
            let wire = udp_relay::encode_relay_packet(&obfs, &packet);
            let decoded = udp_relay::decode_relay_packet(&obfs, &wire).unwrap();
            assert_eq!(decoded.relay_type, RelayType::Data);
            assert_eq!(decoded.dst, dst);
            assert_eq!(decoded.src_port, 40001);
            assert_eq!(decoded.payload, b"payload");
        }
    }

    #[test]
    fn keepalive_does_not_reach_the_device() {
        let mut r = relay();
        let now = Instant::now();

        let wire = r.keepalive();
        assert!(
            r.wire_to_tun(&wire, now).is_none(),
            "keepalive не уходит в TUN устройству"
        );
        assert!(r.is_empty(), "keepalive не заводит флоу");
    }

    #[test]
    fn excluded_dst_port_is_not_relayed() {
        let mut r = relay();
        let now = Instant::now();
        let device = v4(192, 168, 1, 10, 5000);

        for port in EXCLUDE_DST_PORTS {
            let wire = r.tun_to_wire(&tun_udp(device, v4(203, 0, 113, 5, port), b"x"), now);
            assert!(wire.is_none(), "порт {port} не должен уезжать в реле");
        }
        assert!(r.is_empty());
    }

    #[test]
    fn tcp_packet_is_not_relayed() {
        let mut r = relay();
        let mut pkt = tun_udp(v4(192, 168, 1, 10, 5000), v4(203, 0, 113, 5, 443), b"x");
        pkt[9] = 6; // TCP
        assert!(r.tun_to_wire(&pkt, Instant::now()).is_none());
    }

    /// Два флоу с общим портом и разными пирами не делят ответы: у каждого
    /// свой туннельный порт, ответ уходит владельцу потока.
    #[test]
    fn two_flows_on_one_port_with_different_peers_do_not_share_replies() {
        let mut r = relay();
        let now = Instant::now();
        let first = v4(192, 168, 1, 10, 3074);
        let second = v4(192, 168, 1, 11, 3074);
        let peer_a = v4(203, 0, 113, 5, 3074);
        let peer_b = v4(198, 51, 100, 7, 51820);

        let wire_a = r.tun_to_wire(&tun_udp(first, peer_a, b"a"), now).unwrap();
        let wire_b = r.tun_to_wire(&tun_udp(second, peer_b, b"b"), now).unwrap();
        assert_ne!(wire_a, wire_b, "разные флоу обязаны уехать разными пакетами");

        let decoded_a = udp_relay::decode_relay_packet(&test_obfuscator(), &wire_a).unwrap();
        let decoded_b = udp_relay::decode_relay_packet(&test_obfuscator(), &wire_b).unwrap();
        assert_ne!(decoded_a.src_port, decoded_b.src_port);

        let reply_a = udp_relay::encode_relay_packet(
            &test_obfuscator(),
            &data(decoded_a.src_port, peer_a, b"reply-a"),
        );
        let reply_b = udp_relay::encode_relay_packet(
            &test_obfuscator(),
            &data(decoded_b.src_port, peer_b, b"reply-b"),
        );

        let tun_a = r.wire_to_tun(&reply_a, now).unwrap();
        let tun_b = r.wire_to_tun(&reply_b, now).unwrap();
        assert_ne!(tun_a, tun_b);

        let (_, dst_a, _, ihl_a) = parse_ipv4_header(&tun_a).unwrap();
        let (_, dst_b, _, ihl_b) = parse_ipv4_header(&tun_b).unwrap();
        assert_eq!(dst_a, Ipv4Addr::new(192, 168, 1, 10), "ответ A уходит первому");
        assert_eq!(dst_b, Ipv4Addr::new(192, 168, 1, 11), "ответ B уходит второму");
        assert_eq!(&tun_a[ihl_a..][8..], b"reply-a");
        assert_eq!(&tun_b[ihl_b..][8..], b"reply-b");
    }

    // -- Стенд PacketQueue --

    /// UDP входит в TUN, уезжает в реле, ответ возвращается в TUN.
    #[test]
    fn packet_queue_stand_udp_round_trip() {
        let queue = PacketQueue::new();
        let mut r = relay();
        let now = Instant::now();
        let device = v4(192, 168, 1, 10, 57976);
        let server = v4(203, 0, 113, 5, 33334);

        // 1. Устройство шлёт UDP в TUN.
        queue.push_inbound(tun_udp(device, server, b"ping"));
        let inbound = queue.pop_inbound_public().expect("пакет из TUN");

        // 2. Реле уносит его в провод (как в UDP-сокет к VPS).
        let wire = r.tun_to_wire(&inbound, now).expect("UDP уезжает в реле");
        let decoded = udp_relay::decode_relay_packet(&test_obfuscator(), &wire).unwrap();
        assert_eq!(decoded.dst, server);
        assert_eq!(decoded.payload, b"ping");

        // 3. Ответ сервера приходит обратно по туннельному порту.
        let reply = udp_relay::encode_relay_packet(
            &test_obfuscator(),
            &data(decoded.src_port, server, b"pong"),
        );
        let tun_packet = r.wire_to_tun(&reply, now).expect("ответ возвращается в TUN");
        queue.push_outbound_public(tun_packet);

        // 4. Устройство забирает ответ из TUN.
        let outbound = queue.pop_outbound().expect("ответ в TUN");
        let (src_ip, dst_ip, proto, ihl) = parse_ipv4_header(&outbound).unwrap();
        assert_eq!(proto, 17);
        assert_eq!(src_ip, Ipv4Addr::new(203, 0, 113, 5));
        assert_eq!(dst_ip, Ipv4Addr::new(192, 168, 1, 10));
        let (src_port, dst_port, off) = parse_udp_header(&outbound[ihl..]).unwrap();
        assert_eq!((src_port, dst_port), (33334, 57976));
        assert_eq!(&outbound[ihl..][off..], b"pong");
    }

    #[test]
    fn retire_expired_on_relay_returns_ports() {
        let mut r = UdpRelay::new(
            test_obfuscator(),
            DEFAULT_MAX_FLOWS,
            Duration::from_secs(60),
        );
        let now = Instant::now();

        r.tun_to_wire(&tun_udp(v4(192, 168, 1, 10, 3074), v4(203, 0, 113, 5, 9), b"x"), now)
            .unwrap();
        assert_eq!(r.len(), 1);

        assert_eq!(r.retire_expired(now + Duration::from_secs(61)), 1);
        assert!(r.is_empty());

        // Номер снова свободен: следующий пакет получает настоящий порт.
        let wire = r
            .tun_to_wire(
                &tun_udp(v4(192, 168, 1, 10, 3074), v4(203, 0, 113, 5, 9), b"x"),
                now + Duration::from_secs(61),
            )
            .unwrap();
        let decoded = udp_relay::decode_relay_packet(&test_obfuscator(), &wire).unwrap();
        assert_eq!(decoded.src_port, 3074);
    }

    #[test]
    fn clear_drops_all_flows() {
        let mut r = relay();
        let now = Instant::now();

        r.tun_to_wire(&tun_udp(v4(192, 168, 1, 10, 3074), v4(203, 0, 113, 5, 9), b"x"), now)
            .unwrap();
        r.clear();
        assert!(r.is_empty());
    }
}
