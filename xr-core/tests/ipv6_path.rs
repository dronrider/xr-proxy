//! XR-302: IPv6 едет через туннель ядра наравне с IPv4. Тесты гоняют
//! публичный путь пакета: разбор заголовка из TUN, ответ AAAA того же домена
//! и UDP-ответ с контрольной суммой по псевдозаголовку IPv6. IPv4 на тех же
//! функциях обязан оставаться прежним.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use xr_core::dns::FakeDns;
use xr_core::engine::{build_udp_response_v6, parse_ip_header, parse_tcp_ports, parse_udp_header};

fn v6_client() -> Ipv6Addr {
    "2001:db8::10".parse().unwrap()
}

fn v6_target() -> Ipv6Addr {
    "2001:db8::20".parse().unwrap()
}

/// Пакет IPv6 с несущим next header и полезной нагрузкой после базового
/// заголовка.
fn ipv6_packet(next: u8, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0u8; 40 + payload.len()];
    p[0] = 0x60;
    p[4..6].copy_from_slice(&(payload.len() as u16).to_be_bytes());
    p[6] = next;
    p[7] = 64;
    p[8..24].copy_from_slice(&v6_client().octets());
    p[24..40].copy_from_slice(&v6_target().octets());
    p[40..].copy_from_slice(payload);
    p
}

/// TCP-сегмент IPv6: порты и флаги, payload за 20-байтовым заголовком.
fn tcp_v6(src_port: u16, dst_port: u16, flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut t = vec![0u8; 20 + payload.len()];
    t[0..2].copy_from_slice(&src_port.to_be_bytes());
    t[2..4].copy_from_slice(&dst_port.to_be_bytes());
    t[12] = 5 << 4;
    t[13] = flags;
    t[14..16].copy_from_slice(&0xFFFFu16.to_be_bytes());
    t[20..].copy_from_slice(payload);
    t
}

/// Контрольная сумма транспорта поверх псевдозаголовка IPv6 (RFC 8200).
/// Реализация своя, не копия кода под теста.
fn ipv6_pseudo_checksum(src: &Ipv6Addr, dst: &Ipv6Addr, next: u8, segment: &[u8]) -> u16 {
    let mut sum = 0u32;
    for part in [src.octets(), dst.octets()] {
        for pair in part.chunks(2) {
            sum += u16::from_be_bytes([pair[0], pair[1]]) as u32;
        }
    }
    let len = segment.len() as u32;
    sum += (len >> 16) as u32;
    sum += (len & 0xFFFF) as u32;
    sum += next as u32;
    for i in (0..segment.len()).step_by(2) {
        let word = if i + 1 < segment.len() {
            u16::from_be_bytes([segment[i], segment[i + 1]])
        } else {
            u16::from_be_bytes([segment[i], 0])
        };
        sum += word as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Разбор IPv6 из TUN отдаёт адреса, транспорт и смещение за заголовком.
#[test]
fn parse_ip_header_reads_ipv6_tcp() {
    let tcp = tcp_v6(40000, 443, 0x02, &[]);
    let pkt = ipv6_packet(6, &tcp);
    let (src, dst, proto, off) = parse_ip_header(&pkt).expect("IPv6-пакет обязан разбираться");
    assert_eq!(src, IpAddr::V6(v6_client()));
    assert_eq!(dst, IpAddr::V6(v6_target()));
    assert_eq!(proto, 6);
    assert_eq!(off, 40);
    assert_eq!(parse_tcp_ports(&pkt[off..]), Some((40000, 443)));
}

/// Не-первый фрагмент IPv6 не считается транспортом: без сессии и NAT.
#[test]
fn parse_ip_header_rejects_non_first_ipv6_fragment() {
    let mut frag = vec![0u8; 8 + 20];
    frag[0] = 6; // next = TCP (мусор за фрагментом)
    let off_flags = 136u16 << 3;
    frag[2..4].copy_from_slice(&off_flags.to_be_bytes());
    let pkt = ipv6_packet(44, &frag);
    assert!(
        parse_ip_header(&pkt).is_none(),
        "фрагмент с offset=136 уходит в стек как есть"
    );
}

/// Первый фрагмент (offset=0) несёт транспорт и разбирается дальше.
#[test]
fn parse_ip_header_accepts_first_ipv6_fragment() {
    let tcp = tcp_v6(40000, 443, 0x02, &[]);
    let mut frag = vec![0u8; 8];
    frag[0] = 6;
    frag[2..4].copy_from_slice(&0x0001u16.to_be_bytes()); // offset=0, M=1
    let pkt = ipv6_packet(44, &[frag, tcp].concat());
    let (_, _, proto, off) = parse_ip_header(&pkt).expect("первый фрагмент обязан разбираться");
    assert_eq!(proto, 6);
    assert_eq!(off, 48);
}

/// UDP-ответ поверх IPv6 несёт валидную сумму по псевдозаголовку.
#[test]
fn build_udp_response_v6_checksum_matches_rfc() {
    let src = v6_target();
    let dst = v6_client();
    let payload = b"hello-v6";
    let pkt = build_udp_response_v6(src, dst, 53, 40000, payload);
    let (sport, dport, udp_off) = parse_udp_header(&pkt[40..]).expect("UDP-заголовок");
    assert_eq!((sport, dport), (53, 40000));
    assert_eq!(udp_off, 8);

    let segment = &pkt[40..];
    let got = u16::from_be_bytes([pkt[46], pkt[47]]);
    let mut zeroed = segment.to_vec();
    zeroed[6] = 0;
    zeroed[7] = 0;
    let want = ipv6_pseudo_checksum(&src, &dst, 17, &zeroed);
    let want = if want == 0 { 0xFFFF } else { want };
    assert_eq!(got, want, "сумма в пакете обязана сходиться с эталоном");
}

/// AAAA отвечает fake-адресом того же домена, что и A, и оба вспоминаются.
#[test]
fn fake_dns_answers_aaaa_and_a_for_same_domain() {
    let dns = FakeDns::new();
    let aaaa_query = build_dns_query("example.com", 28);
    let (resp, aaaa_ip) = dns.handle_query(&aaaa_query).expect("AAAA-ответ");
    assert!(aaaa_ip.is_ipv6(), "AAAA обязан отдать IPv6");
    assert!(FakeDns::is_fake_v6(match aaaa_ip {
        IpAddr::V6(v6) => v6,
        _ => unreachable!(),
    }));
    assert_eq!(resp[4..6], [0, 1], "ANCOUNT=1");
    // Ответ: указатель на имя (2) + TYPE (2) + CLASS (2) + TTL (4) + RDLENGTH (2) + RDATA (16).
    assert_eq!(
        resp[resp.len() - 26..resp.len() - 24],
        [0, 28],
        "TYPE AAAA в ответе"
    );

    let a_query = build_dns_query("example.com", 1);
    let (_, a_ip) = dns.handle_query(&a_query).expect("A-ответ");
    assert!(a_ip.is_ipv4());
    let aaaa_v6 = match aaaa_ip {
        IpAddr::V6(v6) => v6,
        _ => unreachable!(),
    };
    let a_v4 = match a_ip {
        IpAddr::V4(v4) => v4,
        _ => unreachable!(),
    };
    assert_eq!(
        FakeDns::to_fake_v6(a_v4),
        aaaa_v6,
        "A и AAAA одного домена берутся из одного пула"
    );
    assert_eq!(dns.lookup_ip(aaaa_ip).as_deref(), Some("example.com"));
}

/// IPv4 на том же разборе не меняется: соседний трафик не ломается.
#[test]
fn parse_ip_header_still_reads_ipv4() {
    let mut p = vec![0u8; 20 + 20];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&40u16.to_be_bytes());
    p[8] = 64;
    p[9] = 6;
    p[12..16].copy_from_slice(&Ipv4Addr::new(10, 0, 0, 2).octets());
    p[16..20].copy_from_slice(&Ipv4Addr::new(10, 0, 0, 1).octets());
    let tcp = tcp_v6(12345, 80, 0x02, &[]);
    p[20..].copy_from_slice(&tcp);
    let (src, dst, proto, off) = parse_ip_header(&p).expect("IPv4-пакет обязан разбираться");
    assert_eq!(src, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)));
    assert_eq!(dst, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
    assert_eq!(proto, 6);
    assert_eq!(off, 20);
}

/// DNS-запрос нужного типа: заголовок, одно имя, qtype и class IN.
fn build_dns_query(domain: &str, qtype: u16) -> Vec<u8> {
    let mut pkt = vec![0u8; 12];
    pkt[0] = 0x12;
    pkt[1] = 0x34;
    pkt[2] = 0x01;
    pkt[5] = 1;
    for label in domain.split('.') {
        pkt.push(label.len() as u8);
        pkt.extend_from_slice(label.as_bytes());
    }
    pkt.push(0);
    pkt.extend_from_slice(&qtype.to_be_bytes());
    pkt.extend_from_slice(&1u16.to_be_bytes());
    pkt
}
