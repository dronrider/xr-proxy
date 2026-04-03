//! Fake DNS — intercept DNS queries, return fake IPs, maintain domain↔IP mapping.
//!
//! When the VPN TUN intercepts a DNS query for "youtube.com", we return a fake IP
//! from the 198.18.0.0/15 range. Later, when a TCP SYN arrives for that fake IP,
//! we look up the original domain and apply routing rules.
//!
//! This is the standard approach used by Clash, Sing-box, Leaf, etc.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Reserved CIDR for fake IPs: 198.18.0.0/15 (198.18.0.0 - 198.19.255.255).
/// This range is designated for benchmarking (RFC 2544) and safe for local use.
const FAKE_IP_BASE: u32 = 0xC6120000; // 198.18.0.0
const FAKE_IP_MASK: u32 = 0xFFFE0000; // /15 → 131072 addresses

/// TTL for cached entries.
const DEFAULT_TTL: Duration = Duration::from_secs(3600); // 1 hour

struct Entry {
    domain: String,
    created: Instant,
}

/// Fake DNS resolver: bidirectional mapping between domains and fake IPs.
pub struct FakeDns {
    inner: Mutex<FakeDnsInner>,
}

struct FakeDnsInner {
    /// domain → fake IP
    domain_to_ip: HashMap<String, Ipv4Addr>,
    /// fake IP (as u32) → entry
    ip_to_entry: HashMap<u32, Entry>,
    /// Next IP to allocate (offset from FAKE_IP_BASE).
    next_offset: u32,
    /// TTL for entries.
    ttl: Duration,
}

impl FakeDns {
    pub fn new() -> Self {
        Self::with_ttl(DEFAULT_TTL)
    }

    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(FakeDnsInner {
                domain_to_ip: HashMap::new(),
                ip_to_entry: HashMap::new(),
                next_offset: 1, // skip .0.0
                ttl,
            }),
        }
    }

    /// Allocate (or return existing) fake IP for a domain.
    pub fn allocate(&self, domain: &str) -> Ipv4Addr {
        let domain_lower = domain.to_lowercase();
        let mut inner = self.inner.lock().unwrap();

        // Return existing if still valid.
        if let Some(&ip) = inner.domain_to_ip.get(&domain_lower) {
            let ip_u32 = u32::from(ip);
            if let Some(entry) = inner.ip_to_entry.get(&ip_u32) {
                if entry.created.elapsed() < inner.ttl {
                    return ip;
                }
            }
            // Expired — remove and re-allocate.
            inner.domain_to_ip.remove(&domain_lower);
        }

        // Allocate new fake IP.
        let ip = loop {
            let candidate = FAKE_IP_BASE | (inner.next_offset & !FAKE_IP_MASK);
            inner.next_offset = inner.next_offset.wrapping_add(1);

            // Wrap around within the /15 range.
            if inner.next_offset >= (!FAKE_IP_MASK) {
                inner.next_offset = 1;
                // Evict all expired entries on wrap.
                Self::evict_expired_inner(&mut inner);
            }

            // If this IP is free or expired, use it.
            match inner.ip_to_entry.get(&candidate) {
                Some(old) if old.created.elapsed() < inner.ttl => {
                    // Occupied and still valid — try next.
                    continue;
                }
                Some(_) => {
                    // Expired — evict.
                    if let Some(old) = inner.ip_to_entry.remove(&candidate) {
                        inner.domain_to_ip.remove(&old.domain);
                    }
                    break Ipv4Addr::from(candidate);
                }
                None => {
                    break Ipv4Addr::from(candidate);
                }
            }
        };

        let ip_u32 = u32::from(ip);
        inner.domain_to_ip.insert(domain_lower.clone(), ip);
        inner.ip_to_entry.insert(
            ip_u32,
            Entry {
                domain: domain_lower,
                created: Instant::now(),
            },
        );

        ip
    }

    /// Look up domain by fake IP. Returns None if not found or expired.
    pub fn lookup(&self, ip: Ipv4Addr) -> Option<String> {
        let ip_u32 = u32::from(ip);
        let inner = self.inner.lock().unwrap();

        inner.ip_to_entry.get(&ip_u32).and_then(|entry| {
            if entry.created.elapsed() < inner.ttl {
                Some(entry.domain.clone())
            } else {
                None
            }
        })
    }

    /// Check if an IP is in the fake range.
    pub fn is_fake_ip(ip: Ipv4Addr) -> bool {
        let ip_u32 = u32::from(ip);
        (ip_u32 & FAKE_IP_MASK) == FAKE_IP_BASE
    }

    /// Process a raw DNS query packet. Returns a DNS response with fake IP, or None.
    ///
    /// Supports only A record queries (type 1, class 1).
    pub fn handle_query(&self, query: &[u8]) -> Option<(Vec<u8>, Ipv4Addr)> {
        // Minimal DNS header: 12 bytes.
        if query.len() < 12 {
            return None;
        }

        let id = u16::from_be_bytes([query[0], query[1]]);
        let flags = u16::from_be_bytes([query[2], query[3]]);

        // Must be a standard query (QR=0, OPCODE=0).
        if flags & 0xF800 != 0 {
            return None;
        }

        let qdcount = u16::from_be_bytes([query[4], query[5]]);
        if qdcount == 0 {
            return None;
        }

        // Parse the first question.
        let (domain, qtype, qclass, qend) = parse_dns_question(&query[12..])?;

        // Only handle A records (type=1, class=IN=1).
        if qtype != 1 || qclass != 1 {
            return None;
        }

        let fake_ip = self.allocate(&domain);

        // Build DNS response.
        let response = build_dns_response(id, &query[12..qend + 12], fake_ip);
        Some((response, fake_ip))
    }

    fn evict_expired_inner(inner: &mut FakeDnsInner) {
        let ttl = inner.ttl;
        let expired_ips: Vec<u32> = inner
            .ip_to_entry
            .iter()
            .filter(|(_, e)| e.created.elapsed() >= ttl)
            .map(|(&ip, _)| ip)
            .collect();

        for ip in expired_ips {
            if let Some(entry) = inner.ip_to_entry.remove(&ip) {
                inner.domain_to_ip.remove(&entry.domain);
            }
        }
    }
}

/// Parse a DNS question section. Returns (domain, qtype, qclass, bytes_consumed).
fn parse_dns_question(data: &[u8]) -> Option<(String, u16, u16, usize)> {
    let mut pos = 0;
    let mut parts = Vec::new();

    loop {
        if pos >= data.len() {
            return None;
        }
        let len = data[pos] as usize;
        if len == 0 {
            pos += 1;
            break;
        }
        if pos + 1 + len > data.len() {
            return None;
        }
        let label = std::str::from_utf8(&data[pos + 1..pos + 1 + len]).ok()?;
        parts.push(label.to_string());
        pos += 1 + len;
    }

    if pos + 4 > data.len() {
        return None;
    }

    let qtype = u16::from_be_bytes([data[pos], data[pos + 1]]);
    let qclass = u16::from_be_bytes([data[pos + 2], data[pos + 3]]);
    pos += 4;

    let domain = parts.join(".");
    Some((domain, qtype, qclass, pos))
}

/// Build a minimal DNS response with a single A record.
fn build_dns_response(id: u16, question_section: &[u8], ip: Ipv4Addr) -> Vec<u8> {
    let mut resp = Vec::with_capacity(12 + question_section.len() + 16);

    // Header.
    resp.extend_from_slice(&id.to_be_bytes());
    resp.extend_from_slice(&0x8180u16.to_be_bytes()); // QR=1, RD=1, RA=1
    resp.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT=1
    resp.extend_from_slice(&1u16.to_be_bytes()); // ANCOUNT=1
    resp.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT=0
    resp.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT=0

    // Question section (copy from query).
    resp.extend_from_slice(question_section);

    // Answer: pointer to domain in question (0xC00C = offset 12).
    resp.extend_from_slice(&0xC00Cu16.to_be_bytes()); // NAME pointer
    resp.extend_from_slice(&1u16.to_be_bytes()); // TYPE A
    resp.extend_from_slice(&1u16.to_be_bytes()); // CLASS IN
    resp.extend_from_slice(&60u32.to_be_bytes()); // TTL 60s
    resp.extend_from_slice(&4u16.to_be_bytes()); // RDLENGTH
    resp.extend_from_slice(&ip.octets()); // RDATA

    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocate_and_lookup() {
        let dns = FakeDns::new();

        let ip1 = dns.allocate("youtube.com");
        let ip2 = dns.allocate("google.com");
        let ip3 = dns.allocate("youtube.com"); // same domain

        assert_ne!(ip1, ip2);
        assert_eq!(ip1, ip3); // same domain → same IP
        assert!(FakeDns::is_fake_ip(ip1));
        assert!(FakeDns::is_fake_ip(ip2));

        assert_eq!(dns.lookup(ip1), Some("youtube.com".to_string()));
        assert_eq!(dns.lookup(ip2), Some("google.com".to_string()));
    }

    #[test]
    fn test_case_insensitive() {
        let dns = FakeDns::new();

        let ip1 = dns.allocate("YouTube.COM");
        let ip2 = dns.allocate("youtube.com");

        assert_eq!(ip1, ip2);
    }

    #[test]
    fn test_is_fake_ip() {
        assert!(FakeDns::is_fake_ip(Ipv4Addr::new(198, 18, 0, 1)));
        assert!(FakeDns::is_fake_ip(Ipv4Addr::new(198, 19, 255, 255)));
        assert!(!FakeDns::is_fake_ip(Ipv4Addr::new(198, 20, 0, 0)));
        assert!(!FakeDns::is_fake_ip(Ipv4Addr::new(8, 8, 8, 8)));
    }

    #[test]
    fn test_expired_entry() {
        let dns = FakeDns::with_ttl(Duration::from_millis(1));

        let ip1 = dns.allocate("example.com");
        std::thread::sleep(Duration::from_millis(5));

        // Expired — lookup fails.
        assert_eq!(dns.lookup(ip1), None);

        // Re-allocation may reuse or allocate new.
        let ip2 = dns.allocate("example.com");
        assert!(FakeDns::is_fake_ip(ip2));
        assert_eq!(dns.lookup(ip2), Some("example.com".to_string()));
    }

    #[test]
    fn test_handle_dns_query() {
        let dns = FakeDns::new();

        // Build a minimal DNS query for "example.com" type A.
        let query = build_test_dns_query("example.com");
        let (response, fake_ip) = dns.handle_query(&query).unwrap();

        assert!(FakeDns::is_fake_ip(fake_ip));
        assert_eq!(dns.lookup(fake_ip), Some("example.com".to_string()));

        // Verify response structure.
        assert!(response.len() >= 12);
        // QR=1 (response).
        assert_eq!(response[2] & 0x80, 0x80);
        // ANCOUNT=1.
        assert_eq!(u16::from_be_bytes([response[6], response[7]]), 1);
    }

    #[test]
    fn test_non_a_query_ignored() {
        let dns = FakeDns::new();

        // AAAA query (type 28) should be ignored.
        let mut query = build_test_dns_query("example.com");
        // Patch qtype to 28 (AAAA) — it's the 2 bytes after the question name.
        let name_end = 12 + "example".len() + 1 + "com".len() + 1 + 1; // header + labels + null
        query[name_end] = 0;
        query[name_end + 1] = 28;

        assert!(dns.handle_query(&query).is_none());
    }

    /// Build a DNS query packet for testing.
    fn build_test_dns_query(domain: &str) -> Vec<u8> {
        let mut pkt = Vec::new();

        // Header.
        pkt.extend_from_slice(&0x1234u16.to_be_bytes()); // ID
        pkt.extend_from_slice(&0x0100u16.to_be_bytes()); // Flags: RD=1
        pkt.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
        pkt.extend_from_slice(&0u16.to_be_bytes()); // ANCOUNT
        pkt.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
        pkt.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT

        // Question: domain name.
        for part in domain.split('.') {
            pkt.push(part.len() as u8);
            pkt.extend_from_slice(part.as_bytes());
        }
        pkt.push(0); // null terminator

        pkt.extend_from_slice(&1u16.to_be_bytes()); // QTYPE = A
        pkt.extend_from_slice(&1u16.to_be_bytes()); // QCLASS = IN

        pkt
    }
}
