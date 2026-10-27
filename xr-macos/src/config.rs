//! Конфиг сервиса macOS (TOML). Держит только обвязку Mac: серверы, ключи и
//! правила берёт профиль приложения из `profile_path`, тот же JSON, что
//! уезжает в Android по QR или с хаба.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    /// JSON-профиль приложения (ClientProfile xr-core).
    pub profile_path: String,
    /// Частота проверки сети сторожем, секунды.
    pub check_interval_secs: u64,
    /// Переводить системный DNS в туннель через networksetup.
    pub dns_apply: bool,
    /// Адрес DNS внутри туннеля, обслуживается FakeDns ядра.
    pub dns_server: Ipv4Addr,
    /// Разрешить прямые DNS-запросы к резолверам сети. Нужно, только если в
    /// правилах профиля есть домены с действием direct: их имена уйдут
    /// провайдеру в открытом виде. По умолчанию срезаются.
    pub allow_direct_dns: bool,
    /// Адрес Mac на utun, IPv4. Соседний адрес пары занимает `dns_server`.
    pub tun_address_v4: Ipv4Addr,
    /// Адрес Mac на utun, IPv6 (ULA, как у NE-приложения XR-300).
    pub tun_address_v6: Ipv6Addr,
    pub tun_mtu: u32,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            profile_path: "/etc/xr-proxy/profile.json".to_string(),
            check_interval_secs: 5,
            dns_apply: true,
            dns_server: Ipv4Addr::new(10, 0, 0, 1),
            allow_direct_dns: false,
            tun_address_v4: Ipv4Addr::new(10, 0, 0, 2),
            tun_address_v6: Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 2),
            tun_mtu: 1280,
        }
    }
}

#[derive(Debug, Default, serde::Deserialize)]
struct ServiceConfigRaw {
    profile_path: Option<String>,
    net: Option<NetRaw>,
    dnstun: Option<DnsRaw>,
    killswitch: Option<KillswitchRaw>,
    tun: Option<TunRaw>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct NetRaw {
    check_interval_secs: Option<u64>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct DnsRaw {
    apply: Option<bool>,
    dns_server: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct KillswitchRaw {
    allow_direct_dns: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct TunRaw {
    address_v4: Option<String>,
    address_v6: Option<String>,
    mtu: Option<u32>,
}

/// Ошибка конфигурации с названием поля, чтобы диагностика называла причину.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ConfigError {}

impl ServiceConfig {
    pub fn load(text: &str) -> Result<Self, ConfigError> {
        let raw: ServiceConfigRaw =
            toml::from_str(text).map_err(|e| ConfigError(format!("конфиг: {e}")))?;
        let mut cfg = Self::default();

        if let Some(p) = raw.profile_path {
            if p.trim().is_empty() {
                return Err(ConfigError("profile_path: путь не может быть пустым".into()));
            }
            cfg.profile_path = p;
        }
        if let Some(net) = raw.net {
            if let Some(secs) = net.check_interval_secs {
                if secs == 0 || secs > 300 {
                    return Err(ConfigError(format!(
                        "net.check_interval_secs: {secs}, ожидалось 1..300"
                    )));
                }
                cfg.check_interval_secs = secs;
            }
        }
        if let Some(dns) = raw.dnstun {
            if let Some(apply) = dns.apply {
                cfg.dns_apply = apply;
            }
            if let Some(server) = dns.dns_server {
                cfg.dns_server = server
                    .parse()
                    .map_err(|_| ConfigError(format!("dnstun.dns_server: {server}, ожидался IPv4")))?;
            }
        }
        if let Some(ks) = raw.killswitch {
            if let Some(allow) = ks.allow_direct_dns {
                cfg.allow_direct_dns = allow;
            }
        }
        if let Some(tun) = raw.tun {
            if let Some(addr) = tun.address_v4 {
                cfg.tun_address_v4 = addr
                    .parse()
                    .map_err(|_| ConfigError(format!("tun.address_v4: {addr}, ожидался IPv4")))?;
            }
            if let Some(addr) = tun.address_v6 {
                cfg.tun_address_v6 = addr
                    .parse()
                    .map_err(|_| ConfigError(format!("tun.address_v6: {addr}, ожидался IPv6")))?;
            }
            if let Some(mtu) = tun.mtu {
                if !(1280..=65535).contains(&mtu) {
                    return Err(ConfigError(format!(
                        "tun.mtu: {mtu}, ожидалось 1280..65535 (нижняя граница IPv6)"
                    )));
                }
                cfg.tun_mtu = mtu;
            }
        }

        if cfg.dns_server == cfg.tun_address_v4 {
            return Err(ConfigError(
                "dnstun.dns_server совпадает с tun.address_v4: DNS обязан сидеть на соседнем адресе"
                    .into(),
            ));
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_ne_convention() {
        let cfg = ServiceConfig::default();
        // Пара адресов как у NE-приложения XR-300: DNS на соседнем адресе.
        assert_eq!(cfg.dns_server.octets(), [10, 0, 0, 1]);
        assert_eq!(cfg.tun_address_v4.octets(), [10, 0, 0, 2]);
        assert_eq!(
            cfg.tun_address_v6,
            Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 2)
        );
        assert_eq!(cfg.tun_mtu, 1280);
        assert!(cfg.dns_apply);
        assert!(!cfg.allow_direct_dns);
    }

    #[test]
    fn empty_text_gives_defaults() {
        let cfg = ServiceConfig::load("").unwrap();
        assert_eq!(cfg, ServiceConfig::default());
    }

    #[test]
    fn overrides_parse_from_toml() {
        let text = r#"
profile_path = "/etc/xr/test-profile.json"

[net]
check_interval_secs = 10

[dnstun]
apply = false
dns_server = "192.0.2.53"

[killswitch]
allow_direct_dns = true

[tun]
address_v4 = "198.51.100.2"
address_v6 = "fd01::2"
mtu = 1400
"#;
        let cfg = ServiceConfig::load(text).unwrap();
        assert_eq!(cfg.profile_path, "/etc/xr/test-profile.json");
        assert_eq!(cfg.check_interval_secs, 10);
        assert!(!cfg.dns_apply);
        assert_eq!(cfg.dns_server.to_string(), "192.0.2.53");
        assert!(cfg.allow_direct_dns);
        assert_eq!(cfg.tun_address_v4.to_string(), "198.51.100.2");
        assert_eq!(cfg.tun_address_v6.to_string(), "fd01::2");
        assert_eq!(cfg.tun_mtu, 1400);
    }

    #[test]
    fn bad_values_are_named() {
        assert_eq!(
            ServiceConfig::load("[net]\ncheck_interval_secs = 0").unwrap_err().0,
            "net.check_interval_secs: 0, ожидалось 1..300"
        );
        assert!(ServiceConfig::load("[tun]\nmtu = 1000")
            .unwrap_err()
            .0
            .contains("tun.mtu"));
        assert!(ServiceConfig::load("[tun]\naddress_v4 = \"not-an-ip\"")
            .unwrap_err()
            .0
            .contains("tun.address_v4"));
        assert!(ServiceConfig::load("dnstun = 5").is_err());
    }

    #[test]
    fn dns_equal_to_tun_address_rejected() {
        let text = "[tun]\naddress_v4 = \"198.51.100.2\"\n[dnstun]\ndns_server = \"198.51.100.2\"";
        let err = ServiceConfig::load(text).unwrap_err();
        assert!(err.0.contains("соседнем адресе"));
    }
}
