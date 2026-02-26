/// Routing engine: domain matching, IP range (CIDR) matching, GeoIP lookup.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use xr_proto::config::{RoutingConfig, RoutingRule};

/// Routing decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Proxy,
    Direct,
}

impl Action {
    pub fn from_str(s: &str) -> Self {
        match s {
            "proxy" => Action::Proxy,
            _ => Action::Direct,
        }
    }
}

/// Parsed CIDR range for fast matching.
#[derive(Debug)]
enum CidrRange {
    V4 { addr: u32, mask: u32 },
    V6 { addr: u128, mask: u128 },
}

impl CidrRange {
    fn parse(s: &str) -> Option<Self> {
        let (ip_str, prefix_str) = s.split_once('/')?;
        let prefix_len: u32 = prefix_str.parse().ok()?;

        if let Ok(ip) = ip_str.parse::<Ipv4Addr>() {
            if prefix_len > 32 {
                return None;
            }
            let mask = if prefix_len == 0 { 0 } else { !0u32 << (32 - prefix_len) };
            Some(CidrRange::V4 {
                addr: u32::from(ip) & mask,
                mask,
            })
        } else if let Ok(ip) = ip_str.parse::<Ipv6Addr>() {
            if prefix_len > 128 {
                return None;
            }
            let mask = if prefix_len == 0 { 0 } else { !0u128 << (128 - prefix_len) };
            Some(CidrRange::V6 {
                addr: u128::from(ip) & mask,
                mask,
            })
        } else {
            None
        }
    }

    fn contains(&self, ip: IpAddr) -> bool {
        match (self, ip) {
            (CidrRange::V4 { addr, mask }, IpAddr::V4(v4)) => {
                (u32::from(v4) & mask) == *addr
            }
            (CidrRange::V6 { addr, mask }, IpAddr::V6(v6)) => {
                (u128::from(v6) & mask) == *addr
            }
            _ => false,
        }
    }
}

/// Compiled routing rule for fast matching.
#[derive(Debug)]
struct CompiledRule {
    action: Action,
    /// Exact domain matches (lowercase).
    exact_domains: Vec<String>,
    /// Wildcard suffixes: "*.google.com" stored as ".google.com".
    wildcard_suffixes: Vec<String>,
    /// IP/CIDR ranges.
    ip_ranges: Vec<CidrRange>,
    /// GeoIP country codes (uppercase).
    geoip_codes: Vec<String>,
}

/// The routing engine. Created once from config, used for every connection.
pub struct Router {
    rules: Vec<CompiledRule>,
    default_action: Action,
    #[cfg(feature = "geoip")]
    geoip_reader: Option<maxminddb::Reader<Vec<u8>>>,
}

impl Router {
    pub fn new(config: &RoutingConfig, #[allow(unused)] geoip_path: Option<&str>) -> Self {
        let rules = config
            .rules
            .iter()
            .map(|r| compile_rule(r))
            .collect();

        let default_action = Action::from_str(&config.default_action);

        #[cfg(feature = "geoip")]
        let geoip_reader = geoip_path.and_then(|path| {
            match maxminddb::Reader::open_readfile(path) {
                Ok(reader) => {
                    tracing::info!("GeoIP database loaded: {}", path);
                    Some(reader)
                }
                Err(e) => {
                    tracing::warn!("Failed to load GeoIP database {}: {}", path, e);
                    None
                }
            }
        });

        Self {
            rules,
            default_action,
            #[cfg(feature = "geoip")]
            geoip_reader,
        }
    }

    /// Decide routing for a connection.
    ///
    /// `sni` is extracted from TLS ClientHello (may be None for non-TLS).
    /// `dest_ip` is the original destination IP from SO_ORIGINAL_DST.
    pub fn resolve(&self, sni: Option<&str>, dest_ip: IpAddr) -> Action {
        for rule in &self.rules {
            if self.matches_rule(rule, sni, dest_ip) {
                return rule.action;
            }
        }
        self.default_action
    }

    fn matches_rule(&self, rule: &CompiledRule, sni: Option<&str>, dest_ip: IpAddr) -> bool {
        // Check domain rules
        if let Some(hostname) = sni {
            let hostname_lower = hostname.to_lowercase();

            for exact in &rule.exact_domains {
                if hostname_lower == *exact {
                    return true;
                }
            }

            for suffix in &rule.wildcard_suffixes {
                if hostname_lower.ends_with(suffix.as_str()) || hostname_lower == suffix[1..] {
                    // "*.google.com" matches "mail.google.com" and "google.com"
                    return true;
                }
            }
        }

        // Check IP range rules (CIDR)
        for cidr in &rule.ip_ranges {
            if cidr.contains(dest_ip) {
                return true;
            }
        }

        // Check GeoIP rules
        if !rule.geoip_codes.is_empty() {
            if let Some(country) = self.lookup_country(dest_ip) {
                for code in &rule.geoip_codes {
                    if country.eq_ignore_ascii_case(code) {
                        return true;
                    }
                }
            }
        }

        false
    }

    fn lookup_country(&self, ip: IpAddr) -> Option<String> {
        #[cfg(feature = "geoip")]
        {
            if let Some(reader) = &self.geoip_reader {
                #[derive(serde::Deserialize)]
                struct Country {
                    country: Option<CountryInfo>,
                }
                #[derive(serde::Deserialize)]
                struct CountryInfo {
                    iso_code: Option<String>,
                }

                if let Ok(result) = reader.lookup::<Country>(ip) {
                    return result.country.and_then(|c| c.iso_code);
                }
            }
        }

        #[cfg(not(feature = "geoip"))]
        let _ = ip;

        None
    }
}

fn compile_rule(rule: &RoutingRule) -> CompiledRule {
    let action = Action::from_str(&rule.action);
    let mut exact_domains = Vec::new();
    let mut wildcard_suffixes = Vec::new();

    for domain in &rule.domains {
        let d = domain.to_lowercase();
        if let Some(suffix) = d.strip_prefix('*') {
            // "*.google.com" → ".google.com"
            wildcard_suffixes.push(suffix.to_string());
        } else {
            exact_domains.push(d);
        }
    }

    let mut ip_ranges = Vec::new();
    for cidr_str in &rule.ip_ranges {
        match CidrRange::parse(cidr_str) {
            Some(cidr) => ip_ranges.push(cidr),
            None => tracing::warn!("Invalid CIDR range in config: {}", cidr_str),
        }
    }

    let geoip_codes: Vec<String> = rule.geoip.iter().map(|s| s.to_uppercase()).collect();

    CompiledRule {
        action,
        exact_domains,
        wildcard_suffixes,
        ip_ranges,
        geoip_codes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xr_proto::config::{RoutingConfig, RoutingRule};

    fn make_config() -> RoutingConfig {
        RoutingConfig {
            default_action: "direct".into(),
            rules: vec![
                RoutingRule {
                    action: "proxy".into(),
                    domains: vec![
                        "youtube.com".into(),
                        "*.youtube.com".into(),
                        "*.google.com".into(),
                    ],
                    ip_ranges: vec![],
                    geoip: vec![],
                },
                RoutingRule {
                    action: "direct".into(),
                    domains: vec!["*.local".into()],
                    ip_ranges: vec![],
                    geoip: vec![],
                },
            ],
        }
    }

    #[test]
    fn test_exact_match() {
        let router = Router::new(&make_config(), None);
        let ip: IpAddr = "93.184.216.34".parse().unwrap();
        assert_eq!(router.resolve(Some("youtube.com"), ip), Action::Proxy);
    }

    #[test]
    fn test_wildcard_match() {
        let router = Router::new(&make_config(), None);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        assert_eq!(router.resolve(Some("mail.google.com"), ip), Action::Proxy);
        assert_eq!(router.resolve(Some("www.youtube.com"), ip), Action::Proxy);
    }

    #[test]
    fn test_wildcard_also_matches_base() {
        let router = Router::new(&make_config(), None);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        // "*.google.com" should also match "google.com"
        assert_eq!(router.resolve(Some("google.com"), ip), Action::Proxy);
    }

    #[test]
    fn test_no_match_uses_default() {
        let router = Router::new(&make_config(), None);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        assert_eq!(router.resolve(Some("example.com"), ip), Action::Direct);
    }

    #[test]
    fn test_no_sni_uses_default() {
        let router = Router::new(&make_config(), None);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        assert_eq!(router.resolve(None, ip), Action::Direct);
    }

    #[test]
    fn test_cidr_v4_match() {
        let config = RoutingConfig {
            default_action: "direct".into(),
            rules: vec![RoutingRule {
                action: "proxy".into(),
                domains: vec![],
                ip_ranges: vec![
                    "91.108.56.0/22".into(),
                    "149.154.160.0/20".into(),
                ],
                geoip: vec![],
            }],
        };
        let router = Router::new(&config, None);
        // Inside 91.108.56.0/22 (91.108.56.0 - 91.108.59.255)
        assert_eq!(router.resolve(None, "91.108.57.3".parse().unwrap()), Action::Proxy);
        // Inside 149.154.160.0/20 (149.154.160.0 - 149.154.175.255)
        assert_eq!(router.resolve(None, "149.154.167.50".parse().unwrap()), Action::Proxy);
        // Outside
        assert_eq!(router.resolve(None, "8.8.8.8".parse().unwrap()), Action::Direct);
    }

    #[test]
    fn test_cidr_v6_match() {
        let config = RoutingConfig {
            default_action: "direct".into(),
            rules: vec![RoutingRule {
                action: "proxy".into(),
                domains: vec![],
                ip_ranges: vec!["2001:b28:f23d::/48".into()],
                geoip: vec![],
            }],
        };
        let router = Router::new(&config, None);
        assert_eq!(
            router.resolve(None, "2001:b28:f23d::1".parse().unwrap()),
            Action::Proxy
        );
        assert_eq!(
            router.resolve(None, "2001:b28:f23e::1".parse().unwrap()),
            Action::Direct
        );
    }

    #[test]
    fn test_cidr_and_domain_combined() {
        let config = RoutingConfig {
            default_action: "direct".into(),
            rules: vec![RoutingRule {
                action: "proxy".into(),
                domains: vec!["*.telegram.org".into()],
                ip_ranges: vec!["91.108.56.0/22".into()],
                geoip: vec![],
            }],
        };
        let router = Router::new(&config, None);
        // Match by domain
        assert_eq!(router.resolve(Some("web.telegram.org"), "1.2.3.4".parse().unwrap()), Action::Proxy);
        // Match by IP (no SNI — typical for Telegram)
        assert_eq!(router.resolve(None, "91.108.56.1".parse().unwrap()), Action::Proxy);
        // Neither
        assert_eq!(router.resolve(Some("example.com"), "8.8.8.8".parse().unwrap()), Action::Direct);
    }
}
