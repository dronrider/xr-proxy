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

/// Разбирается ли CIDR-диапазон из конфига тем же разбором, который встраивает
/// его в правило `compile_rule`. Хаб зовёт это для `ip_ranges` пресета на
/// входе (XR-308): битый диапазон клиент выкидывает с WARN, поэтому хаб
/// отказывает ему сразу, вместе с доменами.
pub fn valid_cidr(s: &str) -> bool {
    CidrRange::parse(s).is_some()
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
    /// Имя исходящего выхода сервера (XR-299): у цели, попавшей в правило,
    /// соединение обязано уходить через него. У клиентских правил None.
    outbound: Option<String>,
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

    /// Имя исходящего выхода для цели (XR-299): первое правило с полем
    /// `outbound`, которое матчит домен `sni` либо адрес `dest_ip`.
    ///
    /// Правила без поля пропускаются, даже если они матчат раньше: широкий
    /// прокси-пресет соседней группы не должен снимать с чувствительного
    /// домена требование уходить через резидента. Доменьная цель проверяется
    /// только доменами правила, IP-цель только его диапазонами: чтобы сверить
    /// домен с `ip_ranges`, его пришлось бы резолвить на стороне вызывающего,
    /// а серверу это утекает DNS с адресом хостинга.
    pub fn resolve_outbound(&self, sni: Option<&str>, dest_ip: Option<IpAddr>) -> Option<&str> {
        for rule in &self.rules {
            let Some(outbound) = rule.outbound.as_deref() else {
                continue;
            };
            let domain_hit = sni.is_some_and(|host| self.matches_domain(rule, host));
            let ip_hit = dest_ip.is_some_and(|ip| self.matches_ip(rule, ip));
            if domain_hit || ip_hit {
                return Some(outbound);
            }
        }
        None
    }

    fn matches_rule(&self, rule: &CompiledRule, sni: Option<&str>, dest_ip: IpAddr) -> bool {
        self.matches_domain(rule, sni.unwrap_or(""))
            || self.matches_ip(rule, dest_ip)
    }

    /// Матч по домену. Пустая строка (`sni = None`) ничего не совпадает:
    /// точный список и суффиксы привязаны к реальному хостнейму, а пустой
    /// домен не бывает.
    fn matches_domain(&self, rule: &CompiledRule, hostname: &str) -> bool {
        if hostname.is_empty() {
            return false;
        }
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

        false
    }

    /// Матч по адресу назначения: CIDR-диапазоны и GeoIP.
    fn matches_ip(&self, rule: &CompiledRule, dest_ip: IpAddr) -> bool {
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
        outbound: rule.outbound.clone(),
    }
}

/// Почему правило с полем `outbound` не годится для сервера, либо `None`
/// (XR-299). Гарантия обхода прямого выхода держится на том, что правило
/// реально матчит, а `compile_rule` битые записи выкидывает с WARN и
/// продолжает: опечатка в домене молча выпадала бы из гарантии, и об этом
/// узнали бы только по адресу хостинга в ответе. Поэтому здесь fail-closed
/// вместо fail-soft, в отличие от общих пресетов.
pub fn outbound_rule_reject_reason(rule: &RoutingRule) -> Option<String> {
    if rule.domains.is_empty() && rule.ip_ranges.is_empty() {
        return Some("no domains or ip_ranges: nothing to match".into());
    }
    for domain in &rule.domains {
        let d = normalize_pattern(domain);
        match classify_pattern(&d) {
            Ok(RuleKind::Domain) | Ok(RuleKind::Wildcard) => {}
            Ok(RuleKind::CidrV4) | Ok(RuleKind::CidrV6) => {
                return Some(format!("IP range in domains, use ip_ranges: {domain}"));
            }
            Err(_) => return Some(format!("domain does not parse: {domain}")),
        }
    }
    for cidr in &rule.ip_ranges {
        if CidrRange::parse(cidr).is_none() {
            return Some(format!("broken CIDR range: {cidr}"));
        }
    }
    None
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
                    name: None,
                    action: "proxy".into(),
                    domains: vec![
                        "youtube.com".into(),
                        "*.youtube.com".into(),
                        "*.google.com".into(),
                    ],
                    ip_ranges: vec![],
                    geoip: vec![],
                    outbound: None,
                },
                RoutingRule {
                    name: None,
                    action: "direct".into(),
                    domains: vec!["*.corp.local".into()],
                    ip_ranges: vec![],
                    geoip: vec![],
                    outbound: None,
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
            outbound: None,
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
            outbound: None,
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
                name: None,
                action: "proxy".into(),
                domains: vec!["*".into()],
                ip_ranges: vec![],
                geoip: vec![],
                outbound: None,
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
                name: None,
                action: "proxy".into(),
                domains: vec![
                    "*яндекс.рф".into(),
                    "ffff".into(),
                    "https://youtube.com".into(),
                    "*.google.com".into(),
                ],
                ip_ranges: vec![],
                geoip: vec![],
                outbound: None,
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
                name: None,
                action: "proxy".into(),
                domains: vec!["10.0.0.0/8".into(), "1.2.3.4".into(), "youtube.com".into()],
                ip_ranges: vec![],
                geoip: vec![],
                outbound: None,
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
                name: None,
                action: "proxy".into(),
                domains: vec![" *.GitHub.com ".into(), "  YouTube.com".into()],
                ip_ranges: vec![],
                geoip: vec![],
                outbound: None,
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
                name: None,
                action: "proxy".into(),
                domains: vec![],
                ip_ranges: vec![
                    "91.108.56.0/22".into(),
                    "149.154.160.0/20".into(),
                ],
                geoip: vec![],
                outbound: None,
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
                name: None,
                action: "proxy".into(),
                domains: vec![],
                ip_ranges: vec!["2001:b28:f23d::/48".into()],
                geoip: vec![],
                outbound: None,
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
                name: None,
                action: "proxy".into(),
                domains: vec!["*.telegram.org".into()],
                ip_ranges: vec!["91.108.56.0/22".into()],
                geoip: vec![],
                outbound: None,
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
                name: None,
                action: "direct".into(),
                domains: vec!["github.corp.internal".into()],
                ip_ranges: vec![],
                geoip: vec![],
                outbound: None,
            }],
        };
        let preset = RoutingConfig {
            default_action: "proxy".into(), // ignored — overrides wins
            rules: vec![RoutingRule {
                name: None,
                action: "proxy".into(),
                domains: vec!["*.github.com".into()],
                ip_ranges: vec![],
                geoip: vec![],
                outbound: None,
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
                name: None,
                action: "proxy".into(),
                domains: vec!["youtube.com".into()],
                ip_ranges: vec![],
                geoip: vec![],
                outbound: None,
            }],
        };
        let router = Router::from_merged(&overrides, &preset, None);
        let ip: IpAddr = "1.2.3.4".parse().unwrap();

        // Preset rule works
        assert_eq!(router.resolve(Some("youtube.com"), ip), Action::Proxy);
        // default_action from overrides
        assert_eq!(router.resolve(Some("example.com"), ip), Action::Direct);
    }

    /// Правило XR-299 с полем outbound: домен уезжает именем, цель в
    /// ip_ranges (в боевом конфиге так лежит подсеть инференса Claude)
    /// уезжает адресом, чужая цель выхода не касается. В тесте подсеть
    /// документационная: сами диапазоны это данные конфига, а не кода.
    #[test]
    fn test_resolve_outbound_matches_domain_and_ip_targets() {
        let config = RoutingConfig {
            default_action: "direct".into(),
            rules: vec![RoutingRule {
                name: Some("Claude".into()),
                action: "proxy".into(),
                outbound: Some("resident".into()),
                domains: vec!["claude.ai".into(), "*.anthropic.com".into()],
                ip_ranges: vec!["192.0.2.0/24".into()],
                geoip: vec![],
            }],
        };
        let router = Router::new(&config, None);

        assert_eq!(router.resolve_outbound(Some("claude.ai"), None), Some("resident"));
        assert_eq!(router.resolve_outbound(Some("api.anthropic.com"), None), Some("resident"));
        assert_eq!(router.resolve_outbound(Some("CLAUDE.AI"), None), Some("resident"));
        let infer: IpAddr = "192.0.2.5".parse().unwrap();
        assert_eq!(router.resolve_outbound(None, Some(infer)), Some("resident"));

        let foreign: IpAddr = "198.51.100.7".parse().unwrap();
        assert_eq!(router.resolve_outbound(None, Some(foreign)), None);
        assert_eq!(router.resolve_outbound(Some("example.com"), None), None);
        assert_eq!(router.resolve_outbound(None, None), None);
    }

    /// Широкое правило соседней группы матчит раньше, но outbound-правила
    /// пропускаются только по полю: чувствительный домен обязан остаться под
    /// резидентом, какой бы прокси-пресет ни стоял впереди.
    #[test]
    fn test_resolve_outbound_ignores_rules_without_outbound() {
        let config = RoutingConfig {
            default_action: "direct".into(),
            rules: vec![
                RoutingRule {
                    name: Some("Общий прокси".into()),
                    action: "proxy".into(),
                    outbound: None,
                    domains: vec!["*.ai".into()],
                    ip_ranges: vec![],
                    geoip: vec![],
                },
                RoutingRule {
                    name: Some("AI".into()),
                    action: "proxy".into(),
                    outbound: Some("resident".into()),
                    domains: vec!["chat.example.ai".into()],
                    ip_ranges: vec![],
                    geoip: vec![],
                },
            ],
        };
        let router = Router::new(&config, None);

        assert_eq!(
            router.resolve_outbound(Some("chat.example.ai"), None),
            Some("resident"),
            "правило без outbound раньше по порядку не снимает требование резидента"
        );
        assert_eq!(router.resolve_outbound(Some("other.ai"), None), None);
    }

    /// Доменьная цель сверяется только с доменами правила: ip_ranges для неё
    /// неприменимы, иначе серверу пришлось бы резолвить домен у себя.
    #[test]
    fn test_resolve_outbound_domain_target_skips_ip_ranges() {
        let config = RoutingConfig {
            default_action: "direct".into(),
            rules: vec![RoutingRule {
                name: None,
                action: "proxy".into(),
                outbound: Some("resident".into()),
                domains: vec![],
                ip_ranges: vec!["192.0.2.0/24".into()],
                geoip: vec![],
            }],
        };
        let router = Router::new(&config, None);

        assert_eq!(router.resolve_outbound(Some("claude.ai"), None), None);
        let infer: IpAddr = "192.0.2.5".parse().unwrap();
        assert_eq!(router.resolve_outbound(None, Some(infer)), Some("resident"));
    }

    #[test]
    fn test_valid_cidr() {
        for good in ["192.0.2.0/24", "192.0.2.1/32", "192.0.2.0/0", "2001:db8::/48", "2001:db8::/32"] {
            assert!(valid_cidr(good), "expected valid: {good}");
        }
        // Без префикса, с префиксом за границей семейства и с мусором в адресе.
        for bad in ["192.0.2.0", "192.0.2.0/33", "2001:db8::/129", "banana/8", "192.0.2.0/8/8"] {
            assert!(!valid_cidr(bad), "expected invalid: {bad}");
        }
    }

    #[test]
    fn test_outbound_rule_reject_reason() {
        let good = RoutingRule {
            name: Some("AI".into()),
            action: "proxy".into(),
            outbound: Some("resident".into()),
            domains: vec!["claude.ai".into(), "*.anthropic.com".into()],
            ip_ranges: vec!["192.0.2.0/24".into()],
            geoip: vec![],
        };
        assert_eq!(outbound_rule_reject_reason(&good), None);

        let empty = RoutingRule {
            domains: vec![],
            ip_ranges: vec![],
            ..good.clone()
        };
        assert!(
            outbound_rule_reject_reason(&empty).unwrap().contains("nothing to match"),
            "правило без содержимого не матчит никогда"
        );

        let broken_domain = RoutingRule {
            domains: vec!["claude".into()],
            ip_ranges: vec![],
            ..good.clone()
        };
        assert!(
            outbound_rule_reject_reason(&broken_domain)
                .unwrap()
                .contains("does not parse"),
            "одна метка выпала бы из compile_rule с WARN"
        );

        let cidr_in_domains = RoutingRule {
            domains: vec!["192.0.2.0/24".into()],
            ip_ranges: vec![],
            ..good.clone()
        };
        assert!(
            outbound_rule_reject_reason(&cidr_in_domains)
                .unwrap()
                .contains("use ip_ranges")
        );

        let broken_cidr = RoutingRule {
            domains: vec![],
            ip_ranges: vec!["192.0.2.0/99".into()],
            ..good.clone()
        };
        assert!(
            outbound_rule_reject_reason(&broken_cidr).unwrap().contains("broken CIDR")
        );
    }
}
