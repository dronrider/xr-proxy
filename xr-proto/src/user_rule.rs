//! Пользовательские правила маршрутизации (LLD-05, XR-047).
//!
//! Одно правило это пара «паттерн -> действие», где паттерн либо домен
//! (точный или с подстановкой `*.`), либо CIDR-диапазон. Список правил
//! упорядочен, срабатывает первое совпадение; при сборке `Router` правила
//! кладутся перед пресетом хаба, поэтому пользовательское всегда выигрывает.
//! Валидация паттерна живёт здесь одна на всех: Android дёргает её через
//! JNI (`nativeClassifyPattern`), движок при разборе конфига.

use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, Ipv6Addr};

use crate::config::{RoutingConfig, RoutingRule};

/// Распознанный тип паттерна.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    /// Точный домен: `github.com`.
    Domain,
    /// Домен с подстановкой: `*.github.com` (матчит и сам `github.com`);
    /// одиночная `*` матчит любой SNI.
    Wildcard,
    /// IPv4-диапазон: `10.0.0.0/8` или одиночный `1.2.3.4` (нормализуется в /32).
    CidrV4,
    /// IPv6-диапазон: `2001:db8::/48` или одиночный адрес (нормализуется в /128).
    CidrV6,
}

impl RuleKind {
    /// Строковый тег для JNI-ответа и логов.
    pub fn as_str(&self) -> &'static str {
        match self {
            RuleKind::Domain => "domain",
            RuleKind::Wildcard => "wildcard",
            RuleKind::CidrV4 => "cidr4",
            RuleKind::CidrV6 => "cidr6",
        }
    }
}

/// Почему паттерн не распознан. Текст показывается пользователю под полем
/// ввода, поэтому он на русском и без технических деталей.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulePatternError(pub String);

impl std::fmt::Display for RulePatternError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RulePatternError {}

/// Одно пользовательское правило. `action` держим строкой ("proxy"/"direct"),
/// как в `RoutingRule`: разбор в `Action` происходит при компиляции `Router`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRule {
    pub action: String,
    pub pattern: String,
}

/// Нормализация перед классификацией и сохранением: обрезка пробелов и
/// нижний регистр. Одиночный IP дополнительно приводится к CIDR-виду
/// (`1.2.3.4` -> `1.2.3.4/32`), чтобы паттерн совпадал с тем, что реально
/// компилируется в правило.
pub fn normalize_pattern(raw: &str) -> String {
    let s = raw.trim().to_lowercase();
    if s.parse::<Ipv4Addr>().is_ok() {
        return format!("{}/32", s);
    }
    if s.parse::<Ipv6Addr>().is_ok() {
        return format!("{}/128", s);
    }
    s
}

/// Классифицирует нормализованный паттерн. Принимает то, что вернула
/// [`normalize_pattern`]; сырой пользовательский ввод сначала прогонять
/// через неё.
pub fn classify_pattern(pattern: &str) -> Result<RuleKind, RulePatternError> {
    let s = pattern.trim();
    if s.is_empty() {
        return Err(RulePatternError("Введите домен или IP-диапазон".into()));
    }
    if s.contains("://") {
        // URL вместо домена — частая ошибка вставки из буфера.
        return Err(RulePatternError("Введите домен без схемы (https://) и пути".into()));
    }
    if s.starts_with('[') || s.ends_with(']') {
        return Err(RulePatternError("Введите IPv6 без квадратных скобок".into()));
    }

    // CIDR: ip/prefix.
    if let Some((ip, prefix)) = s.split_once('/') {
        let prefix: u32 = prefix
            .parse()
            .map_err(|_| RulePatternError("Некорректный формат".into()))?;
        if ip.parse::<Ipv4Addr>().is_ok() {
            if prefix > 32 {
                return Err(RulePatternError("Префикс IPv4 не больше /32".into()));
            }
            return Ok(RuleKind::CidrV4);
        }
        if ip.parse::<Ipv6Addr>().is_ok() {
            if prefix > 128 {
                return Err(RulePatternError("Префикс IPv6 не больше /128".into()));
            }
            return Ok(RuleKind::CidrV6);
        }
        return Err(RulePatternError("Некорректный формат".into()));
    }

    // Одиночная `*` — «любой домен» (эквивалент proxy_all по SNI).
    if s == "*" {
        return Ok(RuleKind::Wildcard);
    }

    // Домен с подстановкой: `*.rest`, где rest — валидный домен.
    if let Some(rest) = s.strip_prefix("*.") {
        return if is_valid_domain(rest) {
            Ok(RuleKind::Wildcard)
        } else {
            Err(RulePatternError("Некорректный формат".into()))
        };
    }

    if s.contains('*') {
        return Err(RulePatternError(
            "Подстановка допустима только в начале: *.example.com".into(),
        ));
    }

    if is_valid_domain(s) {
        return Ok(RuleKind::Domain);
    }
    Err(RulePatternError("Некорректный формат".into()))
}

/// Домен: ASCII-метки из букв/цифр/дефисов через точки, минимум две метки
/// (одиночное слово вроде `ffff` — почти всегда опечатка, не маршрут).
/// Кириллические домены нужно вводить в punycode.
fn is_valid_domain(s: &str) -> bool {
    if !s.contains('.') || s.len() > 253 {
        return false;
    }
    // Голые IP сюда не доходят (normalize_pattern превращает их в CIDR),
    // но защитимся: «домен» из одних цифр и точек это битый IP, не имя.
    if s.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return false;
    }
    s.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// Собирает `RoutingConfig` из пользовательских правил: каждое правило
/// становится отдельным `RoutingRule` с одним паттерном, порядок сохраняется.
/// Невалидные паттерны пропускаются с WARN (fail-soft: битое правило не
/// должно валить старт туннеля), действие кроме "proxy"/"direct"/"block"
/// разберёт `Action::from_str` (неизвестное уходит в Direct).
pub fn to_routing_config(rules: &[UserRule], default_action: &str) -> RoutingConfig {
    let compiled = rules
        .iter()
        .filter_map(|r| {
            let pattern = normalize_pattern(&r.pattern);
            match classify_pattern(&pattern) {
                Ok(RuleKind::Domain) | Ok(RuleKind::Wildcard) => Some(RoutingRule {
                    action: r.action.clone(),
                    domains: vec![pattern],
                    ip_ranges: vec![],
                    geoip: vec![],
                }),
                Ok(RuleKind::CidrV4) | Ok(RuleKind::CidrV6) => Some(RoutingRule {
                    action: r.action.clone(),
                    domains: vec![],
                    ip_ranges: vec![pattern],
                    geoip: vec![],
                }),
                Err(e) => {
                    tracing::warn!("skipping invalid user rule '{}': {}", r.pattern, e);
                    None
                }
            }
        })
        .collect();
    RoutingConfig {
        default_action: default_action.to_string(),
        rules: compiled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::{Action, Router};

    #[test]
    fn classify_domains() {
        assert_eq!(classify_pattern("github.com"), Ok(RuleKind::Domain));
        assert_eq!(classify_pattern("youtu.be"), Ok(RuleKind::Domain));
        assert_eq!(classify_pattern("api-v2.my-site.co.uk"), Ok(RuleKind::Domain));
    }

    #[test]
    fn classify_wildcards() {
        assert_eq!(classify_pattern("*.github.com"), Ok(RuleKind::Wildcard));
        assert_eq!(classify_pattern("*"), Ok(RuleKind::Wildcard));
    }

    #[test]
    fn classify_cidr() {
        assert_eq!(classify_pattern("10.0.0.0/8"), Ok(RuleKind::CidrV4));
        assert_eq!(classify_pattern("91.108.56.0/22"), Ok(RuleKind::CidrV4));
        assert_eq!(classify_pattern("2001:b28:f23d::/48"), Ok(RuleKind::CidrV6));
    }

    #[test]
    fn classify_rejects_garbage() {
        for bad in [
            "", "ffff", "*.*", "https://github.com", "github.com/path*",
            "10.0.0.0/33", "2001:db8::/129", "[2001:db8::]/48", "1.2.3/8",
            "foo.*", "тест.рф", "a..b",
        ] {
            assert!(
                classify_pattern(bad).is_err(),
                "'{}' must be rejected",
                bad
            );
        }
    }

    #[test]
    fn normalize_lowercases_and_trims() {
        assert_eq!(normalize_pattern("  GitHub.COM "), "github.com");
        assert_eq!(normalize_pattern("*.YouTube.com"), "*.youtube.com");
    }

    #[test]
    fn normalize_bare_ip_becomes_cidr() {
        assert_eq!(normalize_pattern("1.2.3.4"), "1.2.3.4/32");
        assert_eq!(normalize_pattern("2001:db8::1"), "2001:db8::1/128");
        assert_eq!(classify_pattern(&normalize_pattern("1.2.3.4")), Ok(RuleKind::CidrV4));
    }

    #[test]
    fn to_routing_config_keeps_order_and_splits_kinds() {
        let rules = vec![
            UserRule { action: "direct".into(), pattern: "YouTube.com".into() },
            UserRule { action: "proxy".into(), pattern: "*.github.corp".into() },
            UserRule { action: "proxy".into(), pattern: "10.0.0.0/8".into() },
        ];
        let cfg = to_routing_config(&rules, "direct");
        assert_eq!(cfg.default_action, "direct");
        assert_eq!(cfg.rules.len(), 3);
        assert_eq!(cfg.rules[0].domains, vec!["youtube.com"]);
        assert_eq!(cfg.rules[0].action, "direct");
        assert_eq!(cfg.rules[1].domains, vec!["*.github.corp"]);
        assert_eq!(cfg.rules[2].ip_ranges, vec!["10.0.0.0/8"]);
        assert!(cfg.rules[2].domains.is_empty());
    }

    #[test]
    fn to_routing_config_skips_invalid_fail_soft() {
        let rules = vec![
            UserRule { action: "proxy".into(), pattern: "ffff".into() },
            UserRule { action: "proxy".into(), pattern: "github.com".into() },
        ];
        let cfg = to_routing_config(&rules, "direct");
        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0].domains, vec!["github.com"]);
    }

    /// Сквозной сценарий LLD-05 §5.5: пользовательское правило перекрывает
    /// пресет, потому что стоит раньше в merged-роутере.
    #[test]
    fn user_rule_overrides_preset_in_merged_router() {
        let overrides = to_routing_config(
            &[UserRule { action: "direct".into(), pattern: "youtube.com".into() }],
            "direct",
        );
        let preset = RoutingConfig {
            default_action: "direct".into(),
            rules: vec![RoutingRule {
                action: "proxy".into(),
                domains: vec!["youtube.com".into(), "*.youtube.com".into()],
                ip_ranges: vec![],
                geoip: vec![],
            }],
        };
        let router = Router::from_merged(&overrides, &preset, None);
        let ip = "1.2.3.4".parse().unwrap();
        assert_eq!(router.resolve(Some("youtube.com"), ip), Action::Direct);
        assert_eq!(router.resolve(Some("www.youtube.com"), ip), Action::Proxy);
    }
}
