//! Системный DNS Mac. Без Network Extension резолвер меняется через
//! `networksetup`: каждому включённому сервису ставится адрес DNS внутри
//! туннеля, его обслуживает FakeDns ядра, а провайдерский резолвер не
//! спрашивается вовсе (как у роутера, XR-285). Исходные значения каждого
//! сервиса сохраняются до правки и восстанавливаются при остановке и
//! удалении; пустой список у `networksetup` это DNS от DHCP.

use serde::{Deserialize, Serialize};

use crate::exec::{self, Cmd};

/// Снимок DNS одного сетевого сервиса.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceDns {
    pub service: String,
    /// Пустой список означает «от DHCP».
    pub servers: Vec<String>,
}

/// Разобрать `networksetup -listallnetworkservices`. Заголовок про звёздочку
/// пропускается, отключённые сервисы (первый символ `*`) не трогаем: у
/// выключенного интерфейса DNS не спрашивают.
pub fn parse_services(output: &str) -> Vec<String> {
    output
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| !l.starts_with('*'))
        .map(str::to_string)
        .collect()
}

/// Разобрать `networksetup -getdnsservers <сервис>`.
pub fn parse_dns(output: &str) -> Vec<String> {
    if output.contains("There aren't any DNS Servers") {
        return Vec::new();
    }
    output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Команда чтения DNS сервиса.
pub fn get_dns_cmd(service: &str) -> Cmd {
    Cmd::new(exec::NETWORKSETUP, &["-getdnsservers", service])
}

/// Команда установки списка DNS. Пустой список ставит «Empty», то есть
/// возвращает сервис к DNS от DHCP.
pub fn set_dns_cmd(service: &str, servers: &[String]) -> Cmd {
    let mut args: Vec<String> = vec!["-setdnsservers".into(), service.into()];
    if servers.is_empty() {
        args.push("Empty".into());
    } else {
        args.extend(servers.iter().cloned());
    }
    Cmd { program: exec::NETWORKSETUP.into(), args }
}

/// Подсеть адресов туннеля (адрес Mac и DNS в ней). Префикс без четвёртого
/// октета: конкретные адреса собирает конфиг, здесь важна сама подсеть.
const TUNNEL_PREFIX: &str = "10.0.0.";

/// Резолверы для движка из снимка системы: цикл, link-local и адрес DNS
/// самого туннеля выбрасываются, повторов нет. Движок спрашивает их по
/// прямому пути (через защищённые сокеты) только для доменов с действием
/// direct.
pub fn usable_for_engine(snapshot: &[ServiceDns]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for svc in snapshot {
        for server in &svc.servers {
            let s = server.trim();
            if s.is_empty()
                || s.starts_with("127.")
                || s.starts_with("169.254.")
                || s.starts_with("fe80:")
                || s.starts_with(TUNNEL_PREFIX)
            {
                continue;
            }
            if !out.iter().any(|seen| seen == s) {
                out.push(s.to_string());
            }
        }
    }
    out
}

/// Выбросить из снимка DNS туннеля: сервис, чей единственный резолвер до
/// старта уже указывал в туннель (прошлый прогон не оставил снимка),
/// исходного значения не имеет, и восстановление вернёт ему DNS от DHCP.
/// Смешанные списки не трогаются: у них есть живые адреса. Возврат говорит,
/// нашлись ли такие записи.
pub fn strip_stale_tunnel_dns(snapshot: &mut [ServiceDns], tunnel_dns: &str) -> bool {
    let mut stale = false;
    for svc in snapshot {
        if svc.servers.len() == 1 && svc.servers[0] == tunnel_dns {
            svc.servers.clear();
            stale = true;
        }
    }
    stale
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVICES: &str = "An asterisk (*) denotes that a network service is disabled.
Dell Universal Dock D6000
Thunderbolt Bridge
Wi-Fi
Сервер Outline
*Ethernet USB
";

    #[test]
    fn parse_services_skips_header_and_disabled() {
        let services = parse_services(SERVICES);
        assert_eq!(
            services,
            vec![
                "Dell Universal Dock D6000",
                "Thunderbolt Bridge",
                "Wi-Fi",
                "Сервер Outline",
            ]
        );
    }

    #[test]
    fn parse_dns_reads_dhcp_marker_and_list() {
        assert_eq!(parse_dns("There aren't any DNS Servers set on Wi-Fi.\n"), Vec::<String>::new());
        assert_eq!(
            parse_dns("192.0.2.1\n192.0.2.2\n"),
            vec!["192.0.2.1", "192.0.2.2"]
        );
    }

    #[test]
    fn set_dns_cmd_passes_servers_or_empty() {
        assert_eq!(
            set_dns_cmd("Wi-Fi", &["198.51.100.53".into()]).args,
            vec!["-setdnsservers", "Wi-Fi", "198.51.100.53"]
        );
        assert_eq!(
            set_dns_cmd("Wi-Fi", &[]).args,
            vec!["-setdnsservers", "Wi-Fi", "Empty"]
        );
    }

    #[test]
    fn get_dns_cmd_names_service() {
        assert_eq!(
            get_dns_cmd("Сервер Outline").args,
            vec!["-getdnsservers", "Сервер Outline"]
        );
    }

    #[test]
    fn usable_for_engine_filters_own_and_local() {
        let snapshot = vec![
            ServiceDns {
                service: "Wi-Fi".into(),
                servers: vec![
                    format!("{TUNNEL_PREFIX}53"), // наш DNS в туннеле из повторного прогона
                    "192.0.2.1".into(),
                    "127.0.0.1".into(),
                    "192.0.2.1".into(), // повтор
                ],
            },
            ServiceDns {
                service: "Thunderbolt Bridge".into(),
                servers: vec!["fe80::1".into(), "198.51.100.53".into()],
            },
        ];
        assert_eq!(
            usable_for_engine(&snapshot),
            vec!["192.0.2.1".to_string(), "198.51.100.53".to_string()]
        );
    }

    #[test]
    fn stale_tunnel_dns_in_snapshot_becomes_dhcp() {
        let tunnel_dns = format!("{TUNNEL_PREFIX}1");
        let mut snapshot = vec![
            ServiceDns {
                service: "Wi-Fi".into(),
                // Единственный адрес равен DNS туннеля: исходное неизвестно.
                servers: vec![tunnel_dns.clone()],
            },
            ServiceDns {
                service: "Thunderbolt Bridge".into(),
                servers: vec!["192.0.2.1".into(), "192.0.2.2".into()],
            },
            ServiceDns {
                service: "Сервер Outline".into(),
                servers: Vec::new(),
            },
        ];
        assert!(strip_stale_tunnel_dns(&mut snapshot, &tunnel_dns));
        // Мёртвый адрес туннеля превращается в «от DHCP» ...
        assert_eq!(snapshot[0].servers, Vec::<String>::new());
        // ... посторонние списки не трогаются.
        assert_eq!(
            snapshot[1].servers,
            vec!["192.0.2.1".to_string(), "192.0.2.2".to_string()]
        );
        assert_eq!(snapshot[2].servers, Vec::<String>::new());
        // Смешанный список с адресом туннеля остаётся как есть.
        let mut mixed = vec![ServiceDns {
            service: "Wi-Fi".into(),
            servers: vec![tunnel_dns.clone(), "192.0.2.1".into()],
        }];
        assert!(!strip_stale_tunnel_dns(&mut mixed, &tunnel_dns));
        assert_eq!(mixed[0].servers.len(), 2);
        // Сценарий ревью: снимок с DNS туннеля, затем остановка. Откат
        // рендерит команду из сохранённого снимка, ей достаётся Empty.
        let cmd = set_dns_cmd(&snapshot[0].service, &snapshot[0].servers);
        assert_eq!(cmd.args, vec!["-setdnsservers", "Wi-Fi", "Empty"]);
        // Чистый снимок повторной правки не требует.
        assert!(!strip_stale_tunnel_dns(&mut snapshot, &tunnel_dns));
    }

    #[test]
    fn snapshot_roundtrips_through_json() {
        let snapshot = vec![ServiceDns {
            service: "Wi-Fi".into(),
            servers: Vec::new(),
        }];
        let text = serde_json::to_string(&snapshot).unwrap();
        let back: Vec<ServiceDns> = serde_json::from_str(&text).unwrap();
        assert_eq!(snapshot, back);
    }
}
