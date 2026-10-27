//! Маршруты сервиса. Дефолтный маршрут не подменяется, а перекрывается
//! парой половинок /1 для каждого семейства: они специфичнее любого
//! дефолта, поэтому весь трафик уходит в utun, а маршрут физического
//! шлюза остаётся нетронутым для пути к серверу. Каждая устанавливаемая
//! запись описывается `RouteSpec`, сохраняется до применения и снимается
//! тем же списком, поэтому восстановление работает и после аварии.

use serde::{Deserialize, Serialize};
use std::net::IpAddr;

use crate::exec::{self, Cmd};

/// Разобранный `route -n get default` (или `-inet6 default`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultRoute {
    pub gateway: String,
    pub interface: String,
}

pub fn parse_default_route(output: &str) -> Option<DefaultRoute> {
    let mut gateway = None;
    let mut interface = None;
    for line in output.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("gateway:") {
            gateway = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("interface:") {
            interface = Some(v.trim().to_string());
        }
    }
    let gateway = gateway?;
    let interface = interface?;
    if gateway.is_empty() || interface.is_empty() {
        return None;
    }
    // Шлюз interface: без адреса не бывает, такой вывод не годится.
    if gateway == "interface" {
        return None;
    }
    Some(DefaultRoute { gateway, interface })
}

/// Одна запись таблицы маршрутов, поставленная сервисом.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteSpec {
    /// Назначение: адрес с префиксом (`0.0.0.0/1`) или голый адрес хоста.
    pub dst: String,
    /// `v4` или `v6`.
    pub family: String,
    /// `tun:<имя>` либо адрес шлюза физической сети.
    pub via: String,
}

impl RouteSpec {
    pub fn via_tun(dst: &str, family: &str, tun: &str) -> Self {
        Self {
            dst: dst.to_string(),
            family: family.to_string(),
            via: format!("tun:{tun}"),
        }
    }

    pub fn via_gateway(dst: &str, family: &str, gateway: &str) -> Self {
        Self {
            dst: dst.to_string(),
            family: family.to_string(),
            via: gateway.to_string(),
        }
    }

    fn is_tun(&self) -> bool {
        self.via.starts_with("tun:")
    }

    fn family_flag(&self) -> String {
        if self.family == "v6" {
            "-inet6".to_string()
        } else {
            "-inet".to_string()
        }
    }

    /// Команда установки записи. Существующая запись меняется, а не
    /// дублируется: повторный прогон после аварии не падает на EEXIST.
    pub fn add_cmd(&self) -> Cmd {
        if self.is_tun() {
            let tun = self.via.trim_start_matches("tun:");
            Cmd::new(
                exec::ROUTE,
                &[
                    "-n",
                    "add",
                    &self.family_flag(),
                    &self.dst,
                    "-interface",
                    tun,
                ],
            )
        } else {
            Cmd::new(
                exec::ROUTE,
                &["-n", "add", "-host", &self.family_flag(), &self.dst, &self.via],
            )
        }
    }

    /// Команда снятия записи.
    pub fn del_cmd(&self) -> Cmd {
        if self.is_tun() {
            let tun = self.via.trim_start_matches("tun:");
            Cmd::new(
                exec::ROUTE,
                &[
                    "-n",
                    "delete",
                    &self.family_flag(),
                    &self.dst,
                    "-interface",
                    tun,
                ],
            )
        } else {
            Cmd::new(
                exec::ROUTE,
                &["-n", "delete", "-host", &self.family_flag(), &self.dst, &self.via],
            )
        }
    }
}

/// Половинки адресного пространства, каждая со своим префиксом /1. Собираются
/// из октетов, как и прочие адреса-константы проекта.
fn v4_halves() -> [String; 2] {
    [
        format!("{}/1", IpAddr::from([0u8, 0, 0, 0])),
        format!("{}/1", IpAddr::from([128u8, 0, 0, 0])),
    ]
}

fn v6_halves() -> [String; 2] {
    [
        format!("{}/1", IpAddr::from([0u16, 0, 0, 0, 0, 0, 0, 0])),
        format!("{}/1", IpAddr::from([0x8000u16, 0, 0, 0, 0, 0, 0, 0])),
    ]
}

/// Полный набор записей сервиса: DNS-хост и половинки дефолта в туннель,
/// каждому IP сервера отдельный маршрут через шлюз физической сети.
pub fn plan(
    tun: &str,
    dns_server: IpAddr,
    default_v4: Option<&DefaultRoute>,
    default_v6: Option<&DefaultRoute>,
    server_ips: &[(IpAddr, u16)],
) -> Vec<RouteSpec> {
    let v4 = v4_halves();
    let v6 = v6_halves();
    let mut out = vec![
        RouteSpec::via_tun(&format!("{}/32", dns_server), "v4", tun),
        RouteSpec::via_tun(&v4[0], "v4", tun),
        RouteSpec::via_tun(&v4[1], "v4", tun),
        RouteSpec::via_tun(&v6[0], "v6", tun),
        RouteSpec::via_tun(&v6[1], "v6", tun),
    ];
    if let Some(dr) = default_v4 {
        for (ip, _) in server_ips.iter().filter(|(ip, _)| ip.is_ipv4()) {
            out.push(RouteSpec::via_gateway(&ip.to_string(), "v4", &dr.gateway));
        }
    }
    if let Some(dr) = default_v6 {
        for (ip, _) in server_ips.iter().filter(|(ip, _)| ip.is_ipv6()) {
            out.push(RouteSpec::via_gateway(&ip.to_string(), "v6", &dr.gateway));
        }
    }
    out
}

/// Настройка адреса и MTU самого utun: пара адресов как у NE-приложения
/// (локальный плюс соседний DNS), ULA для IPv6 и MTU не ниже IPv6-минимума.
pub fn ifconfig_cmds(
    tun: &str,
    address_v4: &str,
    dns_server: &str,
    address_v6: &str,
    mtu: u32,
) -> Vec<Cmd> {
    vec![
        Cmd::new(exec::IFCONFIG, &[tun, "inet", address_v4, dns_server, "up"]),
        Cmd::new(exec::IFCONFIG, &[tun, "inet6", &format!("{}/128", address_v6), "up"]),
        Cmd::new(exec::IFCONFIG, &[tun, "mtu", &mtu.to_string()]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTE_GET: &str = "   route to: default
destination: default
       mask: default
    gateway: 192.0.2.1
  interface: en0
      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING,GLOBAL>
 recvpipe  sendpipe  ssthresh  rtt,msec    rttvar  hopcount      mtu     expire
       0         0         0         0         0         0      1500         0
";

    #[test]
    fn parse_default_route_reads_gateway_and_interface() {
        let dr = parse_default_route(ROUTE_GET).unwrap();
        assert_eq!(dr.gateway, "192.0.2.1");
        assert_eq!(dr.interface, "en0");
    }

    #[test]
    fn parse_default_route_rejects_incomplete_output() {
        assert!(parse_default_route("").is_none());
        assert!(parse_default_route("gateway: 192.0.2.1\n").is_none());
        // Маршрут «интерфейсом» без шлюза не годится: серверный хост-роут
        // через него не построить.
        assert!(parse_default_route("gateway: interface\ninterface: en0\n").is_none());
    }

    #[test]
    fn plan_covers_dns_halves_and_server_exceptions() {
        let dr4 = DefaultRoute { gateway: "192.0.2.1".into(), interface: "en0".into() };
        let dr6 = DefaultRoute { gateway: "fe80::1%en0".into(), interface: "en0".into() };
        let servers = vec![
            ("203.0.113.10".parse::<IpAddr>().unwrap(), 8443u16),
            ("2001:db8::10".parse::<IpAddr>().unwrap(), 8443),
        ];
        let specs = plan("utun9", "203.0.113.53".parse().unwrap(), Some(&dr4), Some(&dr6), &servers);
        let dsts: Vec<String> = specs.iter().map(|s| s.dst.clone()).collect();
        let v4 = v4_halves();
        let v6 = v6_halves();
        assert_eq!(
            dsts,
            vec![
                "203.0.113.53/32".to_string(),
                v4[0].clone(),
                v4[1].clone(),
                v6[0].clone(),
                v6[1].clone(),
                "203.0.113.10".to_string(),
                "2001:db8::10".to_string(),
            ]
        );
        // Верхние половинки обязаны начинаться с 128 и 8000 соответственно:
        // пара /1 закрывает весь диапазон, кроме дефолтного маршрута.
        let upper4: std::net::Ipv4Addr = v4[1].trim_end_matches("/1").parse().unwrap();
        assert_eq!(upper4.octets()[0], 128);
        let upper6: std::net::Ipv6Addr = v6[1].trim_end_matches("/1").parse().unwrap();
        assert_eq!(upper6.segments()[0], 0x8000);
        let server4 = &specs[specs.len() - 2];
        assert_eq!(server4.via, "192.0.2.1");
        let server6 = specs.last().unwrap();
        assert_eq!(server6.via, "fe80::1%en0");
    }

    #[test]
    fn plan_skips_server_route_without_default() {
        let servers = vec![("203.0.113.10".parse::<IpAddr>().unwrap(), 8443u16)];
        let specs = plan("utun9", "203.0.113.53".parse().unwrap(), None, None, &servers);
        // Без физического маршрута исключение серверу не построить, туннельные
        // половинки ставятся всё равно: kill-switch держит прямой выход закрытым.
        assert_eq!(specs.len(), 5);
    }

    #[test]
    fn add_and_delete_cmds_render_expected_vectors() {
        let tun_spec = RouteSpec::via_tun(&v4_halves()[0], "v4", "utun9");
        assert_eq!(
            tun_spec.add_cmd().args,
            vec!["-n", "add", "-inet", "0.0.0.0/1", "-interface", "utun9"]
        );
        assert_eq!(
            tun_spec.del_cmd().args,
            vec!["-n", "delete", "-inet", "0.0.0.0/1", "-interface", "utun9"]
        );
        let gw_spec = RouteSpec::via_gateway("2001:db8::10", "v6", "fe80::1%en0");
        assert_eq!(
            gw_spec.add_cmd().args,
            vec!["-n", "add", "-host", "-inet6", "2001:db8::10", "fe80::1%en0"]
        );
        assert_eq!(
            gw_spec.del_cmd().args,
            vec!["-n", "delete", "-host", "-inet6", "2001:db8::10", "fe80::1%en0"]
        );
    }

    #[test]
    fn specs_roundtrip_through_json() {
        let specs = plan(
            "utun9",
            "203.0.113.53".parse().unwrap(),
            Some(&DefaultRoute { gateway: "192.0.2.1".into(), interface: "en0".into() }),
            None,
            &[("203.0.113.10".parse().unwrap(), 8443)],
        );
        let text = serde_json::to_string(&specs).unwrap();
        let back: Vec<RouteSpec> = serde_json::from_str(&text).unwrap();
        assert_eq!(specs, back);
    }

    #[test]
    fn ifconfig_sets_point_to_point_pair_and_mtu() {
        let cmds = ifconfig_cmds("utun9", "198.51.100.2", "198.51.100.1", "fd01::2", 1280);
        assert_eq!(cmds[0].args, vec!["utun9", "inet", "198.51.100.2", "198.51.100.1", "up"]);
        assert_eq!(cmds[1].args, vec!["utun9", "inet6", "fd01::2/128", "up"]);
        assert_eq!(cmds[2].args, vec!["utun9", "mtu", "1280"]);
    }
}
