/// Routing engine: domain matching, GeoIP lookup, rule evaluation.
use std::net::IpAddr;
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

/// Compiled routing rule for fast matching.
#[derive(Debug)]
struct CompiledRule {
    action: Action,
    /// Exact domain matches (lowercase).
    exact_domains: Vec<String>,
    /// Wildcard suffixes: "*.google.com" stored as ".google.com".
    wildcard_suffixes: Vec<String>,
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

    let geoip_codes: Vec<String> = rule.geoip.iter().map(|s| s.to_uppercase()).collect();

    CompiledRule {
        action,
        exact_domains,
        wildcard_suffixes,
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
                    geoip: vec![],
                },
                RoutingRule {
                    action: "direct".into(),
                    domains: vec!["*.local".into()],
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
}
