//! Профиль подключения: тот же JSON, что получает приложение Android, и та
//! же дорога к конфигу движка (client_config xr-core). Сервис добавляет
//! политику mac-пути XR-300: срез QUIC включён, профиль тюнинга NE не
//! ставится, десктопу хватает дефолтных буферов.

use std::net::{IpAddr, ToSocketAddrs};

use xr_core::client_config::{build_config_json, parse_client_profile, parse_config, ClientProfile};
use xr_core::engine::VpnConfig;

/// Сервер профиля: адрес как в конфиге (может быть именем) и его порт.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

pub struct EngineSetup {
    pub config: VpnConfig,
    pub endpoints: Vec<Endpoint>,
}

/// Собрать конфиг движка из JSON профиля. `dns_resolvers` это срез системы
/// Mac, снятый до перевода DNS в туннель: резолверы Android из профиля на
/// Mac не работают.
pub fn build_engine_setup(
    profile_json: &str,
    dns_resolvers: &[String],
) -> Result<EngineSetup, String> {
    let mut profile: ClientProfile = parse_client_profile(profile_json)?;
    if !dns_resolvers.is_empty() {
        profile.dns_resolvers = dns_resolvers.to_vec();
    }

    let mut json: serde_json::Value =
        serde_json::from_str(&build_config_json(&profile)?)
            .map_err(|e| format!("конфиг не собран: {e}"))?;
    // Политика mac-пути (XR-300): QUIC срезается ядром, браузер уходит на TCP.
    json["block_quic"] = serde_json::Value::Bool(true);

    let config = parse_config(&json.to_string())?;
    let endpoints = profile_endpoints(&profile);

    Ok(EngineSetup { config, endpoints })
}

/// Список серверов профиля: пул `servers`, иначе одиночный легаси-адрес.
/// Пустой порт читается как дефолтный, записи без адреса выбрасываются.
pub fn profile_endpoints(profile: &ClientProfile) -> Vec<Endpoint> {
    const DEFAULT_SERVER_PORT: u16 = 8443;
    let mut out: Vec<Endpoint> = profile
        .servers
        .iter()
        .filter_map(|e| {
            let host = e.address.trim();
            if host.is_empty() {
                return None;
            }
            Some(Endpoint {
                host: host.to_string(),
                port: if e.port == 0 { DEFAULT_SERVER_PORT } else { e.port },
            })
        })
        .collect();
    if out.is_empty() {
        let legacy = profile.server_address.trim();
        if !legacy.is_empty() {
            out.push(Endpoint {
                host: legacy.to_string(),
                port: if profile.server_port == 0 {
                    DEFAULT_SERVER_PORT
                } else {
                    profile.server_port
                },
            });
        }
    }
    out
}

/// Разрешить адреса серверов в IP. Литералы идут как есть, имена спрашивает
/// системный резолвер; его надо позвать до взвода kill-switch, иначе прямые
/// DNS-запросы режутся собственными правилами.
pub fn resolve_endpoints(endpoints: &[Endpoint]) -> Result<Vec<(IpAddr, u16)>, String> {
    let mut out = Vec::new();
    for ep in endpoints {
        let host = ep.host.trim_start_matches('[').trim_end_matches(']');
        let sock = format!("{}:{}", host, ep.port);
        let addrs: Vec<std::net::SocketAddr> = sock
            .to_socket_addrs()
            .map_err(|e| format!("сервер {}: {}", ep.host, e))?
            .collect();
        if addrs.is_empty() {
            return Err(format!("сервер {}: адрес не разрешается", ep.host));
        }
        for a in addrs {
            out.push((a.ip(), ep.port));
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Проверка профиля без старта: ключ разбирается, сервер есть. Причина
/// отказа возвращается текстом, `xr-macos validate` печатает её как есть.
pub fn validate_profile(profile_json: &str) -> Result<(), String> {
    let setup = match build_engine_setup(profile_json, &[]) {
        Ok(setup) => setup,
        // Причина сборки конфига без сервера приходит terse-строкой общего
        // конвейера, человеку она ничего не говорит.
        Err(e) if e == "no_server" => {
            return Err("профиль без сервера: задайте server_address или servers".into())
        }
        Err(e) => return Err(e),
    };
    if setup.endpoints.is_empty() {
        return Err("профиль без сервера: задайте server_address или servers".into());
    }
    xr_proto::config::decode_key(&setup.config.obfuscation_key)
        .map(|_| ())
        .map_err(|e| format!("ключ обфускации: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = r#"{
        "server_address": "203.0.113.10",
        "server_port": 8443,
        "obfuscation_key": "a2V5",
        "modifier": "positional_xor_rotate",
        "salt": 7,
        "servers": [
            {"name": "VPS RU", "address": "203.0.113.10", "port": 8443},
            {"name": "VPS DE", "address": "198.51.100.20", "port": 8443}
        ]
    }"#;

    #[test]
    fn setup_builds_engine_config_with_pool() {
        let setup = build_engine_setup(PROFILE, &["192.0.2.1".into()]).unwrap();
        assert_eq!(setup.config.server_address, "203.0.113.10");
        assert_eq!(setup.config.server_port, 8443);
        assert_eq!(setup.config.servers.len(), 2);
        assert_eq!(setup.config.dns_resolvers, vec!["192.0.2.1".to_string()]);
        assert!(setup.config.block_quic, "mac-путь срезает QUIC");
    }

    #[test]
    fn endpoints_follow_pool_order() {
        let profile = parse_client_profile(PROFILE).unwrap();
        let eps = profile_endpoints(&profile);
        assert_eq!(eps.len(), 2);
        assert_eq!(eps[0], Endpoint { host: "203.0.113.10".into(), port: 8443 });
        assert_eq!(eps[1], Endpoint { host: "198.51.100.20".into(), port: 8443 });
    }

    #[test]
    fn legacy_single_server_and_default_port() {
        let profile = parse_client_profile(
            r#"{"server_address": "203.0.113.10", "obfuscation_key": "a2V5"}"#,
        )
        .unwrap();
        let eps = profile_endpoints(&profile);
        assert_eq!(eps, vec![Endpoint { host: "203.0.113.10".into(), port: 8443 }]);
    }

    #[test]
    fn resolve_endpoints_takes_ip_literals_offline() {
        let eps = vec![
            Endpoint { host: "203.0.113.10".into(), port: 8443 },
            Endpoint { host: "198.51.100.20".into(), port: 8443 },
        ];
        let ips = resolve_endpoints(&eps).unwrap();
        assert_eq!(ips.len(), 2);
        // Порядок после сортировки не важен, важен состав.
        let hosts: Vec<String> = ips.iter().map(|(ip, _)| ip.to_string()).collect();
        assert!(hosts.contains(&"203.0.113.10".to_string()));
        assert!(hosts.contains(&"198.51.100.20".to_string()));
    }

    #[test]
    fn resolve_reports_unresolvable_host() {
        let eps = vec![Endpoint { host: "no-such-host.invalid".into(), port: 8443 }];
        assert!(resolve_endpoints(&eps).unwrap_err().contains("no-such-host"));
    }

    #[test]
    fn validate_rejects_profile_without_key() {
        let err = validate_profile(r#"{"server_address": "203.0.113.10"}"#).unwrap_err();
        assert!(err.contains("ключ"), "причина названа: {err}");
    }

    #[test]
    fn validate_rejects_profile_without_server() {
        let err = validate_profile(r#"{"obfuscation_key": "a2V5"}"#).unwrap_err();
        assert!(err.contains("без сервера"));
    }

    #[test]
    fn validate_accepts_working_profile() {
        validate_profile(PROFILE).unwrap();
    }
}
