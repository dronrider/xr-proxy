/// Routing engine: domain matching, IP range (CIDR) matching, GeoIP lookup.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use crate::config::{RoutingConfig, RoutingRule};
use crate::user_rule::{classify_pattern, normalize_pattern, RuleKind};

/// Routing decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Proxy,
    Direct,
    /// Соединение рвётся, наружу не выпускается. Для правил маршрутизации это
    /// явный блэкхол домена, для политики `on_server_down` это fail-closed:
    /// проксируемое либо идёт через прокси, либо не идёт вовсе, но никогда не
    /// утекает в Direct с реальным IP.
    Block,
}

impl Action {
    pub fn from_str(s: &str) -> Self {
        match s {
            "proxy" => Action::Proxy,
            "block" => Action::Block,
            _ => Action::Direct,
        }
    }

    /// Разбор политики `on_server_down`. Отличается от `from_str` тем, что
    /// дефолт здесь fail-closed: всё, кроме явного `"direct"`, это `Block`.
    /// Так опечатка (`"blok"`, `"Block"`, пустая строка) не открывает молча
    /// трафик в Direct. Раньше варианта `Block` не было вообще, и `"block"`
    /// проваливался в `Direct`, отчего реальный IP утекал при каждом отказе
    /// туннеля.
    pub fn on_server_down_from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "direct" => Action::Direct,
            _ => Action::Block,
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

    /// Create a router by merging override rules (higher priority) with preset
    /// rules (fallback). `default_action` is taken from `overrides`.
    pub fn from_merged(
        overrides: &RoutingConfig,
        preset: &RoutingConfig,
        #[allow(unused)] geoip_path: Option<&str>,
    ) -> Self {
        let rules: Vec<CompiledRule> = overrides
            .rules
            .iter()
            .chain(preset.rules.iter())
            .map(|r| compile_rule(r))
            .collect();

        let default_action = Action::from_str(&overrides.default_action);

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
    /// `dest_ip` is the original destination IP.
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
                // "*.google.com" хранится как ".google.com" и матчит как
                // "mail.google.com", так и сам "google.com"; одиночная "*" это
                // пустой суффикс, он матчит любой SNI. Срез по байтам здесь
                // недопустим: домен приходит из чужого конфига, и на
                // многобайтовом символе он роняет процесс.
                if hostname_lower.ends_with(suffix.as_str()) {
                    return true;
                }
                if let Some(base) = suffix.strip_prefix('.') {
                    if hostname_lower == base {
                        return true;
                    }
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

    // Домены из пресета хаба и из TOML проходят ту же проверку, что и
    // пользовательские правила (`user_rule::classify_pattern`): битое
    // отбраковывается с WARN, как невалидный CIDR ниже. Fail-soft, потому что
    // одна опечатка в общем пресете не должна лишать весь парк маршрутизации.
    for domain in &rule.domains {
        let d = normalize_pattern(domain);
        match classify_pattern(&d) {
            Ok(RuleKind::Wildcard) => {
                // "*.google.com" -> ".google.com", одиночная "*" -> ""
                wildcard_suffixes.push(d.strip_prefix('*').unwrap_or_default().to_string());
            }
            Ok(RuleKind::Domain) => exact_domains.push(d),
            Ok(RuleKind::CidrV4) | Ok(RuleKind::CidrV6) => {
                tracing::warn!("IP range in domains list, use ip_ranges instead: {}", domain)
            }
            // Текст ошибки от classify_pattern написан для экрана «Правила» на
            // Android, в лог клиента его не тащим: соседние записи английские.
            Err(_) => tracing::warn!("Invalid domain in config: {}", domain),
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
    use crate::config::{RoutingConfig, RoutingRule};

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
                    domains: vec!["*.corp.local".into()],
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
    fn test_from_str_recognizes_block() {
        // Регресс XR-081: раньше варианта Block не было, и "block"
        // проваливался в Direct, отчего on_server_down=block молча утекал.
        assert_eq!(Action::from_str("proxy"), Action::Proxy);
        assert_eq!(Action::from_str("direct"), Action::Direct);
        assert_eq!(Action::from_str("block"), Action::Block);
        assert_eq!(Action::from_str("whatever"), Action::Direct);
    }

    #[test]
    fn test_on_server_down_is_fail_closed() {
        // Явный direct открывает Direct-фолбэк, всё остальное (block,
        // опечатка, пустая строка, иной регистр) это Block, то есть
        // проксируемое рвётся, а не утекает мимо прокси.
        assert_eq!(Action::on_server_down_from_str("direct"), Action::Direct);
        assert_eq!(Action::on_server_down_from_str("block"), Action::Block);
        assert_eq!(Action::on_server_down_from_str("Block"), Action::Block);
        assert_eq!(Action::on_server_down_from_str("  block  "), Action::Block);
        assert_eq!(Action::on_server_down_from_str("blok"), Action::Block);
        assert_eq!(Action::on_server_down_from_str(""), Action::Block);
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
    fn test_wildcard_suffix_matching_survives_multibyte() {
        // Регресс XR-206: матчинг резал суффикс по байтам (`suffix[1..]`) в
        // расчёте на ведущую точку, и на кириллице срез попадал внутрь
        // символа, роняя процесс на первом же SNI. Матчинг не должен
        // полагаться на то, что домены уже отфильтрованы компиляцией.
        let router = Router::new(
            &RoutingConfig { default_action: "direct".into(), rules: vec![] },
            None,
        );
        let rule = CompiledRule {
            action: Action::Proxy,
            exact_domains: vec![],
            wildcard_suffixes: vec!["яндекс.рф".into()],
            ip_ranges: vec![],
            geoip_codes: vec![],
        };
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        assert!(router.matches_rule(&rule, Some("почта.яндекс.рф"), ip));
        assert!(!router.matches_rule(&rule, Some("google.com"), ip));
        assert!(!router.matches_rule(&rule, None, ip));
    }

    #[test]
    fn test_empty_wildcard_suffix_matches_any_sni() {
        // Одиночная "*" компилируется в пустой суффикс: любой SNI её матчит,
        // а соединение без SNI нет.
        let router = Router::new(
            &RoutingConfig { default_action: "direct".into(), rules: vec![] },
            None,
        );
        let rule = CompiledRule {
            action: Action::Proxy,
            exact_domains: vec![],
            wildcard_suffixes: vec!["".into()],
            ip_ranges: vec![],
            geoip_codes: vec![],
        };
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        assert!(router.matches_rule(&rule, Some("example.com"), ip));
        assert!(!router.matches_rule(&rule, None, ip));
    }

    #[test]
    fn test_bare_star_rule_matches_any_sni() {
        // Правило `domains = ["*"]` это proxy_all по SNI: компиляция даёт
        // пустой суффикс, соединение без SNI под него не подпадает.
        let config = RoutingConfig {
            default_action: "direct".into(),
            rules: vec![RoutingRule {
                action: "proxy".into(),
                domains: vec!["*".into()],
                ip_ranges: vec![],
                geoip: vec![],
            }],
        };
        let router = Router::new(&config, None);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        assert_eq!(router.resolve(Some("example.com"), ip), Action::Proxy);
        assert_eq!(router.resolve(Some("почта.яндекс.рф"), ip), Action::Proxy);
        assert_eq!(router.resolve(None, ip), Action::Direct);
    }

    #[test]
    fn test_broken_preset_domain_does_not_kill_the_rest() {
        // Опечатка в пресете хаба или в TOML отбраковывается с WARN, соседние
        // правила продолжают работать, процесс живёт.
        let config = RoutingConfig {
            default_action: "direct".into(),
            rules: vec![RoutingRule {
                action: "proxy".into(),
                domains: vec![
                    "*яндекс.рф".into(),
                    "ffff".into(),
                    "https://youtube.com".into(),
                    "*.google.com".into(),
                ],
                ip_ranges: vec![],
                geoip: vec![],
            }],
        };
        let router = Router::new(&config, None);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        assert_eq!(router.resolve(Some("mail.google.com"), ip), Action::Proxy);
        assert_eq!(router.resolve(Some("яндекс.рф"), ip), Action::Direct);
        assert_eq!(router.resolve(Some("ffff"), ip), Action::Direct);
    }

    #[test]
    fn test_ip_range_in_domains_is_skipped() {
        // Диапазону место в ip_ranges: точным доменом он не станет, но и
        // остальные домены правила из-за него не пропадут.
        let config = RoutingConfig {
            default_action: "direct".into(),
            rules: vec![RoutingRule {
                action: "proxy".into(),
                domains: vec!["10.0.0.0/8".into(), "1.2.3.4".into(), "youtube.com".into()],
                ip_ranges: vec![],
                geoip: vec![],
            }],
        };
        let router = Router::new(&config, None);
        assert_eq!(router.resolve(None, "10.1.2.3".parse().unwrap()), Action::Direct);
        assert_eq!(router.resolve(Some("10.0.0.0/8"), "8.8.8.8".parse().unwrap()), Action::Direct);
        assert_eq!(router.resolve(Some("youtube.com"), "8.8.8.8".parse().unwrap()), Action::Proxy);
    }

    #[test]
    fn test_domain_is_normalized_before_compile() {
        let config = RoutingConfig {
            default_action: "direct".into(),
            rules: vec![RoutingRule {
                action: "proxy".into(),
                domains: vec![" *.GitHub.com ".into(), "  YouTube.com".into()],
                ip_ranges: vec![],
                geoip: vec![],
            }],
        };
        let router = Router::new(&config, None);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        assert_eq!(router.resolve(Some("api.github.com"), ip), Action::Proxy);
        assert_eq!(router.resolve(Some("github.com"), ip), Action::Proxy);
        assert_eq!(router.resolve(Some("youtube.com"), ip), Action::Proxy);
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

    #[test]
    fn test_from_merged_override_takes_priority() {
        let overrides = RoutingConfig {
            default_action: "direct".into(),
            rules: vec![RoutingRule {
                action: "direct".into(),
                domains: vec!["github.corp.internal".into()],
                ip_ranges: vec![],
                geoip: vec![],
            }],
        };
        let preset = RoutingConfig {
            default_action: "proxy".into(), // ignored — overrides wins
            rules: vec![RoutingRule {
                action: "proxy".into(),
                domains: vec!["*.github.com".into()],
                ip_ranges: vec![],
                geoip: vec![],
            }],
        };
        let router = Router::from_merged(&overrides, &preset, None);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();

        // Override rule matches first → direct
        assert_eq!(router.resolve(Some("github.corp.internal"), ip), Action::Direct);
        // Preset rule provides fallback → proxy
        assert_eq!(router.resolve(Some("api.github.com"), ip), Action::Proxy);
        // Neither → default_action from overrides (direct)
        assert_eq!(router.resolve(Some("example.com"), ip), Action::Direct);
    }

    #[test]
    fn test_from_merged_empty_override() {
        let overrides = RoutingConfig {
            default_action: "direct".into(),
            rules: vec![],
        };
        let preset = RoutingConfig {
            default_action: "proxy".into(),
            rules: vec![RoutingRule {
                action: "proxy".into(),
                domains: vec!["youtube.com".into()],
                ip_ranges: vec![],
                geoip: vec![],
            }],
        };
        let router = Router::from_merged(&overrides, &preset, None);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();

        // Preset rule works
        assert_eq!(router.resolve(Some("youtube.com"), ip), Action::Proxy);
        // default_action from overrides
        assert_eq!(router.resolve(Some("example.com"), ip), Action::Direct);
    }
}
