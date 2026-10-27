//! Проверка соседнего VPN перед стартом (XR-309, DoD 6). Одновременный
//! запуск с чужим VPN не поддержан: kill-switch и маршруты /1 конфликтуют
//! с его перехватом, поэтому запуск отклоняется с названной причиной до
//! изменения сети. Ищутся два признака: подключённый VPN-сервис в
//! `scutil --nc list` и чужой дефолтный маршрут через utun.

use crate::exec::{self, Cmd};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VpnConflict {
    pub name: String,
    pub reason: String,
}

/// Разобрать `scutil --nc list`. Возвращает имена сервисов с состоянием
/// Connected.
pub fn parse_connected_vpn(output: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in output.lines() {
        if !line.contains("(Connected)") {
            continue;
        }
        // Строка: "* (Connected)   <uuid> VPN (bundle) \"Имя\" [VPN:proto]".
        let name = line
            .rsplit('"')
            .nth(1)
            .map(str::trim)
            .filter(|n| !n.is_empty());
        if let Some(name) = name {
            out.push(name.to_string());
        }
    }
    out
}

/// Найти чужой дефолтный маршрут через utun по выводу
/// `netstat -rn -f inet`. Наш собственный интерфейс исключается.
pub fn parse_foreign_utun_default(output: &str, own_utun: &str) -> Option<String> {
    for line in output.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("default") {
            continue;
        }
        // default <gateway> <flags> <netif>: интерфейс четвёртый.
        let netif = fields.nth(2);
        if let Some(iface) = netif {
            if iface.starts_with("utun") && iface != own_utun {
                return Some(iface.to_string());
            }
        }
    }
    None
}

/// Сводная проверка. `own_utun` может быть пустым, если наш интерфейс ещё
/// не поднят.
pub fn check(nc_list: &str, netstat_inet: &str, own_utun: &str) -> Option<VpnConflict> {
    if let Some(name) = parse_connected_vpn(nc_list).first() {
        return Some(VpnConflict {
            name: name.clone(),
            reason: format!(
                "подключён другой VPN ({name}): остановите его или выберите другую сеть"
            ),
        });
    }
    if let Some(iface) = parse_foreign_utun_default(netstat_inet, own_utun) {
        return Some(VpnConflict {
            name: iface.clone(),
            reason: format!(
                "маршрут по умолчанию занят интерфейсом {iface} чужого VPN"
            ),
        });
    }
    None
}

pub fn nc_list_cmd() -> Cmd {
    Cmd::new(exec::SCUTIL, &["--nc", "list"])
}

pub fn netstat_cmd() -> Cmd {
    Cmd::new(exec::NETSTAT, &["-rn", "-f", "inet"])
}

#[cfg(test)]
mod tests {
    use super::*;

    const NC_LIST: &str = "Available network connection services in the current set (*=enabled):
* (Disconnected)   291DD91D-6829-499A-8C65-34D78234FF5E VPN (org.outline.macos.client) \"Сервер Outline\" [VPN:org.outline.macos.client]
* (Connected)      7A993D42-11B6-4A63-8ACD-6B7E861F0D48 VPN (com.apple.ipsec) \"Работа\" [VPN:com.apple.ipsec]
";

    const NETSTAT: &str = "Routing tables

Internet:
Destination        Gateway            Flags               Netif Expire
default            192.0.2.1          UGScg                 en0
127                127.0.0.1          UCS                   lo0
";

    #[test]
    fn connected_vpn_is_named() {
        assert_eq!(parse_connected_vpn(NC_LIST), vec!["Работа".to_string()]);
        assert!(parse_connected_vpn(NC_LIST.lines().next().unwrap()).is_empty());
    }

    #[test]
    fn foreign_utun_default_is_detected() {
        let with_utun = NETSTAT.replace("en0", "utun4");
        assert_eq!(
            parse_foreign_utun_default(&with_utun, "utun9"),
            Some("utun4".to_string())
        );
        // Свой интерфейс не считается конфликтом.
        assert_eq!(parse_foreign_utun_default(&with_utun, "utun4"), None);
        assert_eq!(parse_foreign_utun_default(NETSTAT, ""), None);
    }

    #[test]
    fn check_names_the_conflict_with_reason() {
        let conflict = check(NC_LIST, NETSTAT, "").unwrap();
        assert_eq!(conflict.name, "Работа");
        assert!(conflict.reason.contains("другой VPN"));
        assert!(conflict.reason.contains("Работа"));

        let iface_conflict = check("", &NETSTAT.replace("en0", "utun4"), "").unwrap();
        assert_eq!(iface_conflict.name, "utun4");
        assert!(iface_conflict.reason.contains("чужого VPN"));
    }

    #[test]
    fn clean_network_passes() {
        let disconnected_only = NC_LIST.lines().next().unwrap().to_string()
            + NC_LIST.lines().nth(1).unwrap();
        assert!(check(&disconnected_only, NETSTAT, "").is_none());
    }
}
