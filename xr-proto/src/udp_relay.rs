/// UDP Relay protocol: packet framing for UDP-over-UDP tunneling.
///
/// Wire format (each UDP datagram):
/// ```text
/// [Nonce: 4 bytes] [Obfuscated data: RelayHeader + Payload]
/// ```
///
/// RelayHeader:
///   - type: u8 (DATA=1, KEEPALIVE=2, BIND_PORT=3)
///   - addr_type: u8 (IPv4=1, IPv6=4)
///   - dst_ip: 4 or 16 bytes
///   - dst_port: u16 BE
///   - src_port: u16 BE
///   - payload_len: u16 BE
///
/// Total overhead: 4 (nonce) + 12 (header v4) + payload = 16 bytes min for IPv4

use crate::obfuscation::Obfuscator;
use rand::Rng;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

/// Relay packet types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RelayType {
    /// UDP data packet with destination info.
    Data = 0x01,
    /// Keepalive (no payload).
    Keepalive = 0x02,
    /// Bind port request (client → server).
    BindPort = 0x03,
}

impl RelayType {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::Data),
            0x02 => Some(Self::Keepalive),
            0x03 => Some(Self::BindPort),
            _ => None,
        }
    }
}

/// Decoded relay packet.
#[derive(Debug)]
pub struct RelayPacket {
    pub relay_type: RelayType,
    pub dst: SocketAddr,
    pub src_port: u16,
    pub payload: Vec<u8>,
}

const NONCE_LEN: usize = 4;

/// Encode a relay packet into an obfuscated UDP datagram.
pub fn encode_relay_packet(
    obfuscator: &Obfuscator,
    packet: &RelayPacket,
) -> Vec<u8> {
    let mut rng = rand::thread_rng();
    let nonce: u32 = rng.gen();

    let mut body = Vec::with_capacity(32 + packet.payload.len());

    // Type
    body.push(packet.relay_type as u8);

    // Address
    match packet.dst {
        SocketAddr::V4(v4) => {
            body.push(0x01);
            body.extend_from_slice(&v4.ip().octets());
        }
        SocketAddr::V6(v6) => {
            body.push(0x04);
            body.extend_from_slice(&v6.ip().octets());
        }
    }

    // Dst port, src port, payload length
    body.extend_from_slice(&packet.dst.port().to_be_bytes());
    body.extend_from_slice(&packet.src_port.to_be_bytes());
    body.extend_from_slice(&(packet.payload.len() as u16).to_be_bytes());

    // Payload
    body.extend_from_slice(&packet.payload);

    // Obfuscate entire body
    obfuscator.apply(&mut body, nonce);

    // Prepend nonce
    let mut wire = Vec::with_capacity(NONCE_LEN + body.len());
    wire.extend_from_slice(&nonce.to_be_bytes());
    wire.extend_from_slice(&body);
    wire
}

/// Decode an obfuscated UDP datagram into a relay packet.
pub fn decode_relay_packet(
    obfuscator: &Obfuscator,
    data: &[u8],
) -> Option<RelayPacket> {
    if data.len() < NONCE_LEN + 2 {
        return None; // too short
    }

    let nonce = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let mut body = data[NONCE_LEN..].to_vec();
    obfuscator.apply(&mut body, nonce);

    if body.len() < 2 {
        return None;
    }

    let relay_type = RelayType::from_byte(body[0])?;
    let addr_type = body[1];

    // Keepalive has no further data needed
    if relay_type == RelayType::Keepalive {
        return Some(RelayPacket {
            relay_type,
            dst: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
            src_port: 0,
            payload: vec![],
        });
    }

    let (dst_ip_len, dst) = match addr_type {
        0x01 => {
            // IPv4: need 4 bytes IP + 2 port + 2 src_port + 2 payload_len = 10 more
            if body.len() < 2 + 4 + 2 + 2 + 2 {
                return None;
            }
            let ip = Ipv4Addr::new(body[2], body[3], body[4], body[5]);
            let port = u16::from_be_bytes([body[6], body[7]]);
            (4usize, SocketAddr::V4(SocketAddrV4::new(ip, port)))
        }
        0x04 => {
            // IPv6: need 16 bytes IP + 2 port + 2 src_port + 2 payload_len = 22 more
            if body.len() < 2 + 16 + 2 + 2 + 2 {
                return None;
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&body[2..18]);
            let ip = Ipv6Addr::from(octets);
            let port = u16::from_be_bytes([body[18], body[19]]);
            (16usize, SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)))
        }
        _ => return None,
    };

    let header_len = 2 + dst_ip_len + 2; // type + addr_type + ip + dst_port
    let src_port = u16::from_be_bytes([body[header_len], body[header_len + 1]]);
    let payload_len = u16::from_be_bytes([body[header_len + 2], body[header_len + 3]]) as usize;

    let payload_start = header_len + 4;
    if body.len() < payload_start + payload_len {
        return None;
    }

    let payload = body[payload_start..payload_start + payload_len].to_vec();

    Some(RelayPacket {
        relay_type,
        dst,
        src_port,
        payload,
    })
}

/// Build a keepalive datagram.
pub fn encode_keepalive(obfuscator: &Obfuscator) -> Vec<u8> {
    let packet = RelayPacket {
        relay_type: RelayType::Keepalive,
        dst: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)),
        src_port: 0,
        payload: vec![],
    };
    encode_relay_packet(obfuscator, &packet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obfuscation::{ModifierStrategy, Obfuscator};

    fn test_obfuscator() -> Obfuscator {
        let key = b"test-key-32-bytes-long-enough!!!".to_vec();
        Obfuscator::new(key, 0xDEADBEEF, ModifierStrategy::PositionalXorRotate)
    }

    #[test]
    fn test_data_roundtrip_v4() {
        let obfs = test_obfuscator();
        let packet = RelayPacket {
            relay_type: RelayType::Data,
            dst: "82.20.200.183:63243".parse().unwrap(),
            src_port: 57976,
            payload: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };

        let wire = encode_relay_packet(&obfs, &packet);
        let decoded = decode_relay_packet(&obfs, &wire).unwrap();

        assert_eq!(decoded.relay_type, RelayType::Data);
        assert_eq!(decoded.dst, "82.20.200.183:63243".parse::<SocketAddr>().unwrap());
        assert_eq!(decoded.src_port, 57976);
        assert_eq!(decoded.payload, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn test_data_roundtrip_v6() {
        let obfs = test_obfuscator();
        let packet = RelayPacket {
            relay_type: RelayType::Data,
            dst: "[2001:b28:f23d::1]:443".parse().unwrap(),
            src_port: 12345,
            payload: b"hello v6".to_vec(),
        };

        let wire = encode_relay_packet(&obfs, &packet);
        let decoded = decode_relay_packet(&obfs, &wire).unwrap();

        assert_eq!(decoded.relay_type, RelayType::Data);
        assert_eq!(decoded.dst, "[2001:b28:f23d::1]:443".parse::<SocketAddr>().unwrap());
        assert_eq!(decoded.src_port, 12345);
        assert_eq!(decoded.payload, b"hello v6");
    }

    #[test]
    fn test_keepalive_roundtrip() {
        let obfs = test_obfuscator();
        let wire = encode_keepalive(&obfs);
        let decoded = decode_relay_packet(&obfs, &wire).unwrap();
        assert_eq!(decoded.relay_type, RelayType::Keepalive);
    }

    #[test]
    fn test_wrong_key_fails() {
        let obfs1 = test_obfuscator();
        let obfs2 = Obfuscator::new(
            b"wrong-key-32-bytes-long-enough!!".to_vec(),
            0xDEADBEEF,
            ModifierStrategy::PositionalXorRotate,
        );

        let packet = RelayPacket {
            relay_type: RelayType::Data,
            dst: "1.2.3.4:5678".parse().unwrap(),
            src_port: 9999,
            payload: b"secret".to_vec(),
        };
        let wire = encode_relay_packet(&obfs1, &packet);
        // Wrong key should produce garbage — either None or wrong data
        let result = decode_relay_packet(&obfs2, &wire);
        match result {
            None => {} // good
            Some(d) => {
                // If by chance it parsed, data should be wrong
                assert!(d.payload != b"secret" || d.src_port != 9999);
            }
        }
    }

    #[test]
    fn test_empty_payload() {
        let obfs = test_obfuscator();
        let packet = RelayPacket {
            relay_type: RelayType::Data,
            dst: "1.2.3.4:80".parse().unwrap(),
            src_port: 1234,
            payload: vec![],
        };

        let wire = encode_relay_packet(&obfs, &packet);
        let decoded = decode_relay_packet(&obfs, &wire).unwrap();
        assert_eq!(decoded.payload.len(), 0);
        assert_eq!(decoded.src_port, 1234);
    }

    #[test]
    fn test_large_payload() {
        let obfs = test_obfuscator();
        let payload = vec![0xAB; 1400]; // typical MTU-limited UDP
        let packet = RelayPacket {
            relay_type: RelayType::Data,
            dst: "10.0.0.1:9999".parse().unwrap(),
            src_port: 55555,
            payload: payload.clone(),
        };

        let wire = encode_relay_packet(&obfs, &packet);
        let decoded = decode_relay_packet(&obfs, &wire).unwrap();
        assert_eq!(decoded.payload, payload);
    }
}
