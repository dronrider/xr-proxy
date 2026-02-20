/// Wire protocol: packet framing, commands, async read/write.
///
/// Packet layout:
/// ```text
/// [Nonce: 4 bytes] [Header: 4 bytes (obfuscated)] [Padding: 0-N bytes] [Payload (obfuscated)]
/// ```
///
/// Header (after deobfuscation):
///   - payload_len: u16 (big-endian)
///   - padding_len: u8
///   - command: u8
use crate::obfuscation::Obfuscator;
use rand::Rng;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

// ── Constants ────────────────────────────────────────────────────────

const NONCE_LEN: usize = 4;
const HEADER_LEN: usize = 4;
const MAX_PAYLOAD_LEN: usize = u16::MAX as usize;
/// Fixed magic embedded in header for basic validation (bits 5-7 of command byte).
const HEADER_MAGIC_MASK: u8 = 0xE0;
const HEADER_MAGIC: u8 = 0xA0;

// ── Commands ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Command {
    /// Client → Server: open connection to target.
    Connect = 1,
    /// Bidirectional: payload data.
    Data = 2,
    /// Bidirectional: close connection.
    Close = 3,
    /// Server → Client: connection established.
    ConnectAck = 4,
}

impl Command {
    fn from_byte(b: u8) -> Option<Self> {
        // Lower 5 bits = command, upper 3 bits = magic
        if b & HEADER_MAGIC_MASK != HEADER_MAGIC {
            return None;
        }
        match b & 0x1F {
            1 => Some(Self::Connect),
            2 => Some(Self::Data),
            3 => Some(Self::Close),
            4 => Some(Self::ConnectAck),
            _ => None,
        }
    }

    fn to_byte(self) -> u8 {
        HEADER_MAGIC | (self as u8)
    }
}

// ── Address types ────────────────────────────────────────────────────

/// Target address for Connect command.
#[derive(Debug, Clone)]
pub enum TargetAddr {
    Ip(SocketAddr),
    Domain(String, u16),
}

impl TargetAddr {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        match self {
            TargetAddr::Ip(SocketAddr::V4(addr)) => {
                buf.push(0x01); // IPv4
                buf.extend_from_slice(&addr.ip().octets());
                buf.extend_from_slice(&addr.port().to_be_bytes());
            }
            TargetAddr::Ip(SocketAddr::V6(addr)) => {
                buf.push(0x04); // IPv6
                buf.extend_from_slice(&addr.ip().octets());
                buf.extend_from_slice(&addr.port().to_be_bytes());
            }
            TargetAddr::Domain(domain, port) => {
                buf.push(0x03); // Domain
                let domain_bytes = domain.as_bytes();
                assert!(domain_bytes.len() <= 255);
                buf.push(domain_bytes.len() as u8);
                buf.extend_from_slice(domain_bytes);
                buf.extend_from_slice(&port.to_be_bytes());
            }
        }
        buf
    }

    pub fn decode(data: &[u8]) -> io::Result<(Self, usize)> {
        if data.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "empty address"));
        }
        match data[0] {
            0x01 => {
                // IPv4: 1 + 4 + 2 = 7 bytes
                if data.len() < 7 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "short IPv4"));
                }
                let ip = Ipv4Addr::new(data[1], data[2], data[3], data[4]);
                let port = u16::from_be_bytes([data[5], data[6]]);
                Ok((TargetAddr::Ip(SocketAddr::from((ip, port))), 7))
            }
            0x04 => {
                // IPv6: 1 + 16 + 2 = 19 bytes
                if data.len() < 19 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "short IPv6"));
                }
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&data[1..17]);
                let ip = Ipv6Addr::from(octets);
                let port = u16::from_be_bytes([data[17], data[18]]);
                Ok((TargetAddr::Ip(SocketAddr::from((ip, port))), 19))
            }
            0x03 => {
                // Domain: 1 + 1 + len + 2
                if data.len() < 2 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "short domain"));
                }
                let dlen = data[1] as usize;
                let total = 2 + dlen + 2;
                if data.len() < total {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "short domain data"));
                }
                let domain = String::from_utf8(data[2..2 + dlen].to_vec())
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid domain"))?;
                let port = u16::from_be_bytes([data[2 + dlen], data[3 + dlen]]);
                Ok((TargetAddr::Domain(domain, port), total))
            }
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "unknown addr type")),
        }
    }
}

// ── Frame ────────────────────────────────────────────────────────────

/// A decoded protocol frame.
#[derive(Debug)]
pub struct Frame {
    pub command: Command,
    pub payload: Vec<u8>,
}

// ── Codec ────────────────────────────────────────────────────────────

/// Stateful codec for reading/writing obfuscated frames.
///
/// Use one instance per TCP connection direction.
#[derive(Clone)]
pub struct Codec {
    obfuscator: Obfuscator,
    padding_min: u8,
    padding_max: u8,
}

impl Codec {
    pub fn new(obfuscator: Obfuscator, padding_min: u8, padding_max: u8) -> Self {
        Self {
            obfuscator,
            padding_min,
            padding_max,
        }
    }

    /// Encode a frame into wire bytes.
    pub fn encode_frame(&self, command: Command, payload: &[u8]) -> io::Result<Vec<u8>> {
        if payload.len() > MAX_PAYLOAD_LEN {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "payload too large"));
        }

        let mut rng = rand::thread_rng();
        let nonce: u32 = rng.gen();
        let padding_len = if self.padding_max > self.padding_min {
            rng.gen_range(self.padding_min..=self.padding_max)
        } else {
            self.padding_min
        };

        // Build header: [payload_len: u16 BE] [padding_len: u8] [command: u8]
        let payload_len = payload.len() as u16;
        let mut header = [0u8; HEADER_LEN];
        header[0..2].copy_from_slice(&payload_len.to_be_bytes());
        header[2] = padding_len;
        header[3] = command.to_byte();

        // Obfuscate header
        self.obfuscator.apply(&mut header, nonce);

        // Generate random padding
        let mut padding = vec![0u8; padding_len as usize];
        rng.fill(&mut padding[..]);

        // Obfuscate payload
        let mut obfs_payload = payload.to_vec();
        // Use offset = nonce + HEADER_LEN + padding_len to vary key position
        let payload_offset = nonce.wrapping_add((HEADER_LEN + padding_len as usize) as u32);
        self.obfuscator.apply(&mut obfs_payload, payload_offset);

        // Assemble wire bytes: nonce + header + padding + payload
        let total = NONCE_LEN + HEADER_LEN + padding_len as usize + payload.len();
        let mut wire = Vec::with_capacity(total);
        wire.extend_from_slice(&nonce.to_be_bytes());
        wire.extend_from_slice(&header);
        wire.extend_from_slice(&padding);
        wire.extend_from_slice(&obfs_payload);

        Ok(wire)
    }

    /// Try to decode a frame from a buffer. Returns the frame and number of
    /// bytes consumed, or None if the buffer doesn't contain a complete frame.
    pub fn decode_frame(&self, buf: &[u8]) -> io::Result<Option<(Frame, usize)>> {
        if buf.len() < NONCE_LEN + HEADER_LEN {
            return Ok(None); // need more data
        }

        // Read nonce
        let nonce = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);

        // Deobfuscate header
        let mut header = [0u8; HEADER_LEN];
        header.copy_from_slice(&buf[NONCE_LEN..NONCE_LEN + HEADER_LEN]);
        self.obfuscator.apply(&mut header, nonce);

        // Parse header
        let payload_len = u16::from_be_bytes([header[0], header[1]]) as usize;
        let padding_len = header[2] as usize;
        let command = Command::from_byte(header[3]).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid command/magic — wrong key?",
            )
        })?;

        // Sanity check
        if payload_len > MAX_PAYLOAD_LEN || padding_len > 255 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad frame header"));
        }

        let total_len = NONCE_LEN + HEADER_LEN + padding_len + payload_len;
        if buf.len() < total_len {
            return Ok(None); // need more data
        }

        // Skip padding, deobfuscate payload
        let payload_start = NONCE_LEN + HEADER_LEN + padding_len;
        let mut payload = buf[payload_start..payload_start + payload_len].to_vec();
        let payload_offset = nonce.wrapping_add((HEADER_LEN + padding_len) as u32);
        self.obfuscator.apply(&mut payload, payload_offset);

        Ok(Some((Frame { command, payload }, total_len)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obfuscation::{ModifierStrategy, Obfuscator};

    fn test_codec() -> Codec {
        let key = b"test-key-32-bytes-long-enough!!!".to_vec();
        let obfs = Obfuscator::new(key, 0xDEADBEEF, ModifierStrategy::PositionalXorRotate);
        Codec::new(obfs, 8, 32)
    }

    #[test]
    fn test_frame_roundtrip() {
        let codec = test_codec();
        let payload = b"Hello from xr-proxy!";
        let wire = codec.encode_frame(Command::Data, payload).unwrap();

        let (frame, consumed) = codec.decode_frame(&wire).unwrap().unwrap();
        assert_eq!(frame.command, Command::Data);
        assert_eq!(frame.payload, payload);
        assert_eq!(consumed, wire.len());
    }

    #[test]
    fn test_connect_addr_roundtrip() {
        let addr = TargetAddr::Domain("www.google.com".to_string(), 443);
        let encoded = addr.encode();
        let (decoded, len) = TargetAddr::decode(&encoded).unwrap();
        assert_eq!(len, encoded.len());
        match decoded {
            TargetAddr::Domain(d, p) => {
                assert_eq!(d, "www.google.com");
                assert_eq!(p, 443);
            }
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn test_wrong_key_fails() {
        let key1 = b"correct-key-1234567890abcdef".to_vec();
        let key2 = b"wrong---key-1234567890abcdef".to_vec();
        let obfs1 = Obfuscator::new(key1, 0x11, ModifierStrategy::PositionalXorRotate);
        let obfs2 = Obfuscator::new(key2, 0x11, ModifierStrategy::PositionalXorRotate);

        let codec1 = Codec::new(obfs1, 0, 0);
        let codec2 = Codec::new(obfs2, 0, 0);

        // Try many frames — with wrong key, most should fail magic check
        let mut errors = 0;
        for i in 0..100 {
            let payload = format!("secret payload number {}", i);
            let wire = codec1.encode_frame(Command::Data, payload.as_bytes()).unwrap();
            match codec2.decode_frame(&wire) {
                Err(_) => errors += 1,
                Ok(Some((frame, _))) => {
                    // Even if magic check passes by chance, payload should be garbage
                    if frame.payload != payload.as_bytes() {
                        errors += 1;
                    }
                }
                Ok(None) => errors += 1,
            }
        }
        // With random magic collisions (~1.5%), at least 90% should fail
        assert!(errors > 90, "wrong key should fail for most frames, got {} errors out of 100", errors);
    }

    #[test]
    fn test_partial_buffer() {
        let codec = test_codec();
        let wire = codec.encode_frame(Command::Data, b"test").unwrap();

        // Feed partial data
        let half = &wire[..wire.len() / 2];
        assert!(codec.decode_frame(half).unwrap().is_none());
    }
}
