//! Fake DNS intercepts DNS queries, returns fake IPs and keeps a domain <-> IP map.
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

use xr_proto::protocol::MAX_DOMAIN_LEN;

use crate::stats::Stats;

/// Reserved CIDR for fake IPs: 198.18.0.0/15 (198.18.0.0 - 198.19.255.255).
/// This range is designated for benchmarking (RFC 2544) and safe for local use.
const FAKE_IP_BASE: u32 = 0xC6120000; // 198.18.0.0
const FAKE_IP_MASK: u32 = 0xFFFE0000; // /15 -> 131072 addresses

/// Сколько адресов раздаём: смещения от 1 до предпоследнего в /15. Нулевой это
/// адрес сети, последний широковещательный, оба остаются за бортом.
const FAKE_IP_POOL_SIZE: u32 = (!FAKE_IP_MASK) - 1; // 131070

/// TTL for cached entries.
const DEFAULT_TTL: Duration = Duration::from_secs(3600); // 1 hour

struct Entry {
    domain: String,
    created: Instant,
    /// Порядковый номер последнего обращения: по нему выбирается жертва, когда
    /// свободных адресов не осталось. Счётчик, а не время, чтобы порядок был
    /// строгим даже у записей, выданных в одну и ту же наносекунду.
    last_used: u64,
}

/// Fake DNS resolver: bidirectional mapping between domains and fake IPs.
pub struct FakeDns {
    inner: Mutex<FakeDnsInner>,
    /// Журнал приложения. Вытеснение живой записи это заметная деградация, и о
    /// ней надо сказать вслух; вне движка (тесты, разбор пакетов) журнала нет.
    stats: Option<Stats>,
}

struct FakeDnsInner {
    /// domain -> fake IP
    domain_to_ip: HashMap<String, Ipv4Addr>,
    /// fake IP (as u32) -> entry
    ip_to_entry: HashMap<u32, Entry>,
    /// Next IP to allocate (offset from FAKE_IP_BASE).
    next_offset: u32,
    /// Сколько адресов в пуле: раздаются смещения от 1 до `pool_size`.
    pool_size: u32,
    /// Монотонный счётчик обращений, из него берётся `Entry::last_used`.
    uses: u64,
    /// Пул уже отдавали под вытеснение и об этом сказано в журнал. Сбрасывается,
    /// как только очередной адрес нашёлся свободным: иначе поток запросов при
    /// забитом пуле пишет строку на каждый запрос и вымывает журнал.
    exhaustion_reported: bool,
    /// TTL for entries.
    ttl: Duration,
}

impl FakeDns {
    pub fn new() -> Self {
        Self::build(DEFAULT_TTL, FAKE_IP_POOL_SIZE, None)
    }

    /// Резолвер, который пишет о деградации в журнал приложения.
    pub fn with_stats(stats: Stats) -> Self {
        Self::build(DEFAULT_TTL, FAKE_IP_POOL_SIZE, Some(stats))
    }

    pub fn with_ttl(ttl: Duration) -> Self {
        Self::build(ttl, FAKE_IP_POOL_SIZE, None)
    }

    fn build(ttl: Duration, pool_size: u32, stats: Option<Stats>) -> Self {
        Self {
            inner: Mutex::new(FakeDnsInner {
                domain_to_ip: HashMap::new(),
                ip_to_entry: HashMap::new(),
                next_offset: 1, // skip .0.0
                pool_size: pool_size.clamp(1, FAKE_IP_POOL_SIZE),
                uses: 0,
                exhaustion_reported: false,
                ttl,
            }),
            stats,
        }
    }

    /// Allocate (or return existing) fake IP for a domain.
    pub fn allocate(&self, domain: &str) -> Ipv4Addr {
        let domain_lower = domain.to_lowercase();
        // Кого пришлось вытеснить ради этого домена. Строку журнала пишем уже
        // без лока: журнал ходит в файл, а на этом локе стоит весь движок.
        let mut evicted: Option<(String, Ipv4Addr)> = None;

        let ip = {
            let mut inner = self.inner.lock().unwrap();

            // Return existing if still valid.
            if let Some(&ip) = inner.domain_to_ip.get(&domain_lower) {
                let ip_u32 = u32::from(ip);
                let ttl = inner.ttl;
                inner.uses += 1;
                let stamp = inner.uses;
                if let Some(entry) = inner.ip_to_entry.get_mut(&ip_u32) {
                    if entry.created.elapsed() < ttl {
                        entry.last_used = stamp;
                        return ip;
                    }
                }
                // The entry expired, allocate a fresh address.
                inner.domain_to_ip.remove(&domain_lower);
            }

            // Проход по пулу ограничен его размером: дальше идти всё равно
            // некуда, а безоглядный цикл при полном пуле крутился вечно с
            // захваченным локом и вешал движок насмерть (XR-210).
            let mut free = None;
            for _ in 0..inner.pool_size {
                let candidate = FAKE_IP_BASE | inner.next_offset;
                inner.next_offset += 1;

                // Wrap around within the /15 range.
                if inner.next_offset > inner.pool_size {
                    inner.next_offset = 1;
                    // Evict all expired entries on wrap.
                    Self::evict_expired_inner(&mut inner);
                }

                // If this IP is free or expired, use it.
                match inner.ip_to_entry.get(&candidate) {
                    Some(old) if old.created.elapsed() < inner.ttl => {
                        // Occupied and still valid, so try the next one.
                        continue;
                    }
                    Some(_) => {
                        // Evict the expired entry.
                        Self::remove_entry_inner(&mut inner, candidate);
                        free = Some(candidate);
                        break;
                    }
                    None => {
                        free = Some(candidate);
                        break;
                    }
                }
            }

            let ip_u32 = match free {
                Some(candidate) => {
                    inner.exhaustion_reported = false;
                    candidate
                }
                None => {
                    // Свободных адресов нет, все записи живые. Отказать в
                    // резолве значит оставить приложение без имени вовсе, так
                    // что забираем адрес у того, к кому дольше всех не
                    // обращались: у него больше шансов оказаться забытым.
                    let victim = inner
                        .ip_to_entry
                        .iter()
                        .min_by_key(|(&ip, entry)| (entry.last_used, ip))
                        .map(|(&ip, _)| ip);
                    match victim {
                        Some(victim) => {
                            if let Some(old) = Self::remove_entry_inner(&mut inner, victim) {
                                if !inner.exhaustion_reported {
                                    inner.exhaustion_reported = true;
                                    evicted = Some((old.domain, Ipv4Addr::from(victim)));
                                }
                            }
                            victim
                        }
                        // Пул перебран целиком, значит таблица не пуста, и сюда
                        // не приходят. Отдаём первый адрес пула, а не роняем
                        // движок паникой: на телефоне это разрыв туннеля.
                        None => FAKE_IP_BASE | 1,
                    }
                }
            };

            let ip = Ipv4Addr::from(ip_u32);
            inner.uses += 1;
            let stamp = inner.uses;
            inner.domain_to_ip.insert(domain_lower.clone(), ip);
            inner.ip_to_entry.insert(
                ip_u32,
                Entry {
                    domain: domain_lower.clone(),
                    created: Instant::now(),
                    last_used: stamp,
                },
            );

            ip
        };

        if let Some((victim_domain, victim_ip)) = evicted {
            let msg = format!(
                "fake DNS: свободных адресов не осталось, {} ({}) вытеснен под {}, \
                 запрос приложения на этот адрес уйдёт не туда",
                victim_domain, victim_ip, domain_lower
            );
            tracing::warn!("{}", msg);
            if let Some(stats) = &self.stats {
                stats.add_warn(&msg);
            }
        }

        ip
    }

    /// Look up domain by fake IP. Returns None if not found or expired.
    pub fn lookup(&self, ip: Ipv4Addr) -> Option<String> {
        let ip_u32 = u32::from(ip);
        let mut inner = self.inner.lock().unwrap();

        let ttl = inner.ttl;
        inner.uses += 1;
        let stamp = inner.uses;
        inner.ip_to_entry.get_mut(&ip_u32).and_then(|entry| {
            if entry.created.elapsed() < ttl {
                // Обращение к записи двигает её в конец очереди на вытеснение.
                entry.last_used = stamp;
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
            Self::remove_entry_inner(inner, ip);
        }
    }

    /// Снять запись по адресу вместе с обратной ссылкой. Ссылку убираем только
    /// когда она всё ещё указывает на этот адрес: протухший домен мог успеть
    /// получить новый, и отбирать у него живую запись не за что.
    fn remove_entry_inner(inner: &mut FakeDnsInner, ip: u32) -> Option<Entry> {
        let entry = inner.ip_to_entry.remove(&ip)?;
        if inner.domain_to_ip.get(&entry.domain) == Some(&Ipv4Addr::from(ip)) {
            inner.domain_to_ip.remove(&entry.domain);
        }
        Some(entry)
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

    // Отдельная метка не длиннее 255 байт, но их количество ничем не ограничено,
    // а собранное имя уезжает в Connect, где длина домена это один байт. Так что
    // предел тот же, что у сниффера SNI: приложение на телефоне запросом с сотней
    // меток не должно валить движок (XR-205). Настоящих имён это не задевает,
    // DNS даёт максимум 253 символа.
    let domain = parts.join(".");
    if domain.len() > MAX_DOMAIN_LEN {
        tracing::warn!(
            "fake DNS: имя вопроса {} байт, больше предела домена",
            domain.len()
        );
        return None;
    }
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
    use std::collections::HashSet;

    #[test]
    fn test_allocate_and_lookup() {
        let dns = FakeDns::new();

        let ip1 = dns.allocate("youtube.com");
        let ip2 = dns.allocate("google.com");
        let ip3 = dns.allocate("youtube.com"); // same domain

        assert_ne!(ip1, ip2);
        assert_eq!(ip1, ip3); // same domain -> same IP
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

    /// Меток в вопросе может быть сколько угодно, а собранное имя уезжает в
    /// Connect с однобайтовой длиной. Слишком длинное имя fake DNS не берёт,
    /// иначе приложение таким запросом роняло движок (XR-205).
    #[test]
    fn test_overlong_query_name_ignored() {
        let dns = FakeDns::new();

        // Сотня меток по три символа даёт имя далеко за пределом домена.
        let long = vec!["abc"; 100].join(".");
        assert!(long.len() > MAX_DOMAIN_LEN);
        assert!(dns.handle_query(&build_test_dns_query(&long)).is_none());

        // Имя ровно на границе (64 метки по три символа это ровно 255 байт)
        // ещё обслуживается.
        let edge = vec!["abc"; 64].join(".");
        assert_eq!(edge.len(), MAX_DOMAIN_LEN);
        let (_, fake_ip) = dns.handle_query(&build_test_dns_query(&edge)).unwrap();
        assert_eq!(dns.lookup(fake_ip), Some(edge));
    }

    /// Пул на четыре адреса, занятый живыми записями. Раньше подбор кандидата
    /// в таком пуле крутился вечно с захваченным локом, и движок вставал
    /// намертво до перезапуска VPN (XR-210). Теперь проход ограничен размером
    /// пула, а адрес забирается у самой давней записи.
    #[test]
    fn full_pool_evicts_instead_of_spinning() {
        let dns = tiny_pool(4);

        let taken: Vec<_> = ["a.com", "b.com", "c.com", "d.com"]
            .iter()
            .map(|d| dns.allocate(d))
            .collect();
        assert_eq!(taken.iter().collect::<HashSet<_>>().len(), 4);

        let fifth = dns.allocate("e.com");

        assert!(FakeDns::is_fake_ip(fifth));
        assert_eq!(dns.lookup(fifth), Some("e.com".to_string()));
        // Забрали адрес у самой давней записи, у неё же снялась обратная ссылка.
        assert_eq!(fifth, taken[0]);
        assert_eq!(dns.lookup(taken[1]), Some("b.com".to_string()));
    }

    /// Жертва выбирается по последнему обращению, а не по времени выдачи:
    /// адрес, который только что спрашивали при TCP-SYN, скорее всего живёт в
    /// чьём-то соединении, и отбирать его нельзя.
    #[test]
    fn eviction_takes_the_least_recently_used() {
        let dns = tiny_pool(3);

        let a = dns.allocate("a.com");
        let b = dns.allocate("b.com");
        let c = dns.allocate("c.com");
        assert_eq!(dns.lookup(a), Some("a.com".to_string()));

        let d = dns.allocate("d.com");

        assert_eq!(d, b);
        assert_eq!(dns.lookup(a), Some("a.com".to_string()));
        assert_eq!(dns.lookup(c), Some("c.com".to_string()));
    }

    /// Повторный запрос того же имени это тоже обращение: домен, который
    /// приложение спрашивает снова и снова, из пула вылетать не должен.
    #[test]
    fn repeated_query_saves_entry_from_eviction() {
        let dns = tiny_pool(3);

        let a = dns.allocate("a.com");
        let b = dns.allocate("b.com");
        dns.allocate("c.com");
        assert_eq!(dns.allocate("a.com"), a);

        assert_eq!(dns.allocate("d.com"), b);
        assert_eq!(dns.lookup(a), Some("a.com".to_string()));
    }

    /// Вытесненное имя спрашивают снова: оно обязано получить свежий адрес, а
    /// не увести обратную ссылку у того, кто занял старый.
    #[test]
    fn evicted_domain_gets_a_fresh_address() {
        let dns = tiny_pool(2);

        let a = dns.allocate("a.com");
        let b = dns.allocate("b.com");
        assert_eq!(dns.allocate("c.com"), a);

        let a_again = dns.allocate("a.com");

        assert_eq!(a_again, b);
        assert_eq!(dns.lookup(a), Some("c.com".to_string()));
        assert_eq!(dns.lookup(b), Some("a.com".to_string()));
    }

    /// Вырожденный пул из одного адреса: вызов всё равно завершается.
    #[test]
    fn pool_of_one_address_still_answers() {
        let dns = tiny_pool(1);

        let a = dns.allocate("a.com");
        let b = dns.allocate("b.com");

        assert_eq!(a, b);
        assert_eq!(dns.lookup(b), Some("b.com".to_string()));
    }

    /// Молча деградировать нельзя: об исчерпании пула сказано в журнал. Но
    /// сказано один раз на всю полосу вытеснений, иначе поток запросов вымоет
    /// журнал строкой на каждый запрос.
    #[test]
    fn exhaustion_is_reported_once() {
        let stats = Stats::new();
        let dns = FakeDns::build(DEFAULT_TTL, 2, Some(stats.clone()));

        dns.allocate("a.com");
        dns.allocate("b.com");
        assert!(stats.recent_errors().is_empty());

        dns.allocate("c.com");
        dns.allocate("d.com");
        dns.allocate("e.com");

        let warns: Vec<_> = stats
            .recent_errors()
            .into_iter()
            .filter(|line| line.contains("fake DNS: свободных адресов не осталось"))
            .collect();
        assert_eq!(warns.len(), 1, "журнал: {:?}", stats.recent_errors());
        assert!(warns[0].contains("WARN"));
        assert!(warns[0].contains("a.com"));
    }

    /// Пул с заданным размером: перебрать 131070 адресов в тесте нечем, а
    /// поведение при исчерпании от размера не зависит.
    fn tiny_pool(size: u32) -> FakeDns {
        FakeDns::build(DEFAULT_TTL, size, None)
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
