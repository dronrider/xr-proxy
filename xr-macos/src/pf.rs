//! Kill-switch на PF. Правила грузятся в анкор `com.apple/xr.svc` под
//! wildcard-точкой `com.apple/*` из штатного `/etc/pf.conf`, файл правил
//! системы не правится. PF включается своей ссылкой (`pfctl -E`), ссылка
//! держит счётчик: остановка сервиса отпускает свою, чужие ссылки не
//! трогает. Порядок правил важен: сначала быстрые pass для туннеля, пути
//! к серверу и DHCP, затем общий block. Прямой DNS срезается по умолчанию,
//! политика как у роутера (XR-285).

use std::net::IpAddr;

use crate::exec::{self, Cmd};

pub const ANCHOR: &str = "com.apple/xr.svc";

/// Что разрешает kill-switch кроме туннеля: порты каждого сервера профиля.
/// У сервера два порта: туннельный TCP и UDP relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillSwitch {
    /// Имя нашего utun-интерфейса.
    pub tun: String,
    /// IP сервера и его порты.
    pub servers: Vec<(IpAddr, Vec<u16>)>,
    /// Разрешить прямые DNS-запросы резолверам сети (домены с действием
    /// direct). По умолчанию выключено.
    pub allow_direct_dns: bool,
    pub dns_servers: Vec<IpAddr>,
    /// Режим ожидания: адрес сервера ещё не разрешён, без DNS его не
    /// узнать. До первого разрешения правила пускают DNS наружу, иначе
    /// профиль с именем хоста вместо IP не поднялся бы никогда. Как только
    /// адрес стал известен, правила переезжают в строгие.
    pub pending: bool,
}

impl KillSwitch {
    pub fn render_rules(&self) -> String {
        let mut rules = String::new();
        rules.push_str("# xr-macos kill-switch (XR-309)\n");
        rules.push_str("pass out quick on lo0 all\n");
        // Пустое имя значит «туннель ещё не поднят»: правило на utun появится
        // после подъёма интерфейса, остальные правила работают уже сейчас.
        if !self.tun.is_empty() {
            rules.push_str(&format!("pass out quick on {} all\n", self.tun));
        }
        let mut servers = self.servers.clone();
        servers.sort();
        servers.dedup();
        for (ip, ports) in &servers {
            let mut ports = ports.clone();
            ports.sort_unstable();
            ports.dedup();
            for port in ports {
                for proto in ["tcp", "udp"] {
                    rules.push_str(&format!(
                        "pass out quick {} proto {} from any to {} port {}\n",
                        family_kw(*ip),
                        proto,
                        ip,
                        port
                    ));
                }
            }
        }
        // DHCP: обновление аренды адреса, без него смена Wi-Fi роняет сеть.
        rules.push_str("pass out quick proto udp from any port 68 to any port 67\n");
        rules.push_str("pass out quick inet6 proto udp from any port 546 to any port 547\n");
        // NDP и RA IPv6 живут на ICMPv6, без них физический интерфейс теряет
        // соседей; полезной нагрузки наружу ICMPv6 не несёт.
        rules.push_str("pass out quick proto icmp6 all\n");
        if self.pending {
            rules.push_str("pass out quick proto tcp from any to any port 53\n");
            rules.push_str("pass out quick proto udp from any to any port 53\n");
        }
        if self.allow_direct_dns {
            for dns in &self.dns_servers {
                for proto in ["tcp", "udp"] {
                    rules.push_str(&format!(
                        "pass out quick {} proto {} from any to {} port 53\n",
                        family_kw(*dns),
                        proto,
                        dns
                    ));
                }
            }
        }
        rules.push_str("block out quick all\n");
        rules
    }
}

fn family_kw(ip: IpAddr) -> &'static str {
    match ip {
        IpAddr::V4(_) => "inet",
        IpAddr::V6(_) => "inet6",
    }
}

pub fn enable_cmd() -> Cmd {
    Cmd::new(exec::PFCTL, &["-E"])
}

pub fn load_cmd(rules_path: &str) -> Cmd {
    Cmd::new(exec::PFCTL, &["-a", ANCHOR, "-f", rules_path])
}

pub fn flush_cmd() -> Cmd {
    Cmd::new(exec::PFCTL, &["-a", ANCHOR, "-F", "all"])
}

pub fn rules_cmd() -> Cmd {
    Cmd::new(exec::PFCTL, &["-a", ANCHOR, "-s", "rules"])
}

pub fn release_cmd(token: &str) -> Cmd {
    Cmd::new(exec::PFCTL, &["-X", token])
}

/// Вытащить ссылку включения из вывода `pfctl -E`. Отсутствие строки Token
/// не ошибка: PF уже включён кем-то другим, отпускать нечего.
pub fn parse_token(output: &str) -> Option<String> {
    for line in output.lines() {
        if let Some(rest) = line.trim().strip_prefix("Token") {
            let value = rest.trim_start_matches([':', ' ']).trim();
            if !value.is_empty() && value.chars().all(|c| c.is_ascii_digit()) {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> KillSwitch {
        KillSwitch {
            tun: "utun9".into(),
            servers: vec![
                ("203.0.113.10".parse().unwrap(), vec![8443, 9999]),
                ("2001:db8::10".parse().unwrap(), vec![8443, 9999]),
            ],
            allow_direct_dns: false,
            dns_servers: Vec::new(),
            pending: false,
        }
    }

    #[test]
    fn rules_allow_tunnel_server_and_dhcp_then_block_all() {
        let text = sample().render_rules();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "# xr-macos kill-switch (XR-309)");
        assert!(lines.contains(&"pass out quick on lo0 all"));
        assert!(lines.contains(&"pass out quick on utun9 all"));
        // Без поднятого туннеля правило на utun не рендерится вовсе.
        let mut bare = sample();
        bare.tun = String::new();
        assert!(!bare.render_rules().contains("utun9"));
        assert!(lines.contains(&"pass out quick inet proto tcp from any to 203.0.113.10 port 8443"));
        assert!(lines.contains(&"pass out quick inet proto udp from any to 203.0.113.10 port 9999"));
        assert!(lines.contains(&"pass out quick inet6 proto tcp from any to 2001:db8::10 port 8443"));
        assert!(lines.contains(&"pass out quick proto udp from any port 68 to any port 67"));
        assert!(lines.contains(&"pass out quick proto icmp6 all"));
        // Общий block обязан стоять последним: всё, что не разрешено выше,
        // прямого выхода не получает.
        assert_eq!(lines.last().copied().unwrap(), "block out quick all");
        assert!(!text.contains("port 53"), "прямой DNS срезан по умолчанию");
    }

    #[test]
    fn direct_dns_allowed_by_knob_only() {
        let mut ks = sample();
        ks.allow_direct_dns = true;
        ks.dns_servers = vec!["192.0.2.1".parse().unwrap()];
        let text = ks.render_rules();
        assert!(text.contains("pass out quick inet proto udp from any to 192.0.2.1 port 53"));
        assert_eq!(text.lines().last().unwrap(), "block out quick all");
    }

    #[test]
    fn pending_mode_allows_plain_dns_until_server_resolved() {
        let mut ks = sample();
        ks.servers.clear();
        ks.pending = true;
        let text = ks.render_rules();
        assert!(text.contains("pass out quick proto udp from any to any port 53"));
        assert_eq!(text.lines().last().unwrap(), "block out quick all");
        // В строгом режиме строки порта 53 нет вовсе.
        assert!(!sample().render_rules().contains("port 53"));
    }

    #[test]
    fn rules_are_deterministic() {
        let mut ks = sample();
        ks.servers.reverse();
        assert_eq!(ks.render_rules(), sample().render_rules());
    }

    #[test]
    fn token_parses_from_pfctl_enable_output() {
        let out = format!(
            "No ALTQ support in kernel\nALTQ related functions disabled\nPF enabled\nToken : {}\n",
            2967317173964597489u64
        );
        assert_eq!(parse_token(&out), Some("2967317173964597489".to_string()));
        assert_eq!(parse_token("PF enabled\n"), None);
        assert_eq!(parse_token(""), None);
    }

    #[test]
    fn cmds_use_absolute_pfctl() {
        assert_eq!(enable_cmd().program, "/sbin/pfctl");
        assert_eq!(load_cmd("/tmp/x").args, vec!["-a", ANCHOR, "-f", "/tmp/x"]);
        assert_eq!(flush_cmd().args, vec!["-a", ANCHOR, "-F", "all"]);
        assert_eq!(release_cmd("42").args, vec!["-X", "42"]);
    }
}
