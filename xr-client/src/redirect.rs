/// Generate and clean up nftables/iptables redirect rules.
///
/// On start: set up rules to redirect TCP traffic to the proxy.
/// On stop: clean up rules to restore normal traffic flow.
///
/// Uses full binary paths because procd/systemd may have minimal PATH.
use std::process::Command;

const NFT_TABLE: &str = "xr_proxy";
const IPT_CHAIN: &str = "XR_PROXY";

/// Common locations for nft on OpenWRT and regular Linux.
const NFT_PATHS: &[&str] = &["/usr/sbin/nft", "/sbin/nft", "/usr/bin/nft"];
/// Common locations for iptables.
const IPT_PATHS: &[&str] = &[
    "/usr/sbin/iptables",
    "/sbin/iptables",
    "/usr/bin/iptables",
];

/// Detect whether nftables or iptables is available.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FirewallBackend {
    Nftables,
    Iptables,
}

/// Find the full path to a binary by checking common locations.
fn find_binary(candidates: &[&str]) -> Option<String> {
    for path in candidates {
        if std::path::Path::new(path).exists() {
            return Some(path.to_string());
        }
    }
    // Fallback: try bare name (works if PATH is set correctly)
    if let Some(first) = candidates.first() {
        if let Some(bare_name) = first.rsplit('/').next() {
            if Command::new(bare_name)
                .arg("--version")
                .output()
                .is_ok()
            {
                return Some(bare_name.to_string());
            }
        }
    }
    None
}

fn find_nft() -> Option<String> {
    find_binary(NFT_PATHS)
}

fn find_iptables() -> Option<String> {
    find_binary(IPT_PATHS)
}

pub fn detect_backend() -> Option<FirewallBackend> {
    if find_nft().is_some() {
        Some(FirewallBackend::Nftables)
    } else if find_iptables().is_some() {
        Some(FirewallBackend::Iptables)
    } else {
        None
    }
}

/// Адрес сервера пула вместе с портом его туннеля.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerEndpoint {
    pub address: String,
    pub port: u16,
}

/// Set up redirect rules. Из перехвата выводится не сервер пула (LLD-10)
/// целиком, а его туннельный порт и всё, кроме 80 и 443: туннель не должен
/// зациклиться, а сайты на том же VPS обязаны уходить через прокси.
/// `bypass_ips` are source IPs that should not be redirected (game consoles, etc.)
pub fn setup_redirect(
    backend: FirewallBackend,
    listen_port: u16,
    servers: &[ServerEndpoint],
    bypass_ips: &[String],
    block_quic: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    for server in servers {
        if matches!(server.port, 80 | 443) {
            tracing::warn!(
                "tunnel port {} on {} is a web port: sites hosted on that VPS stay outside the tunnel",
                server.port,
                server.address
            );
        }
    }

    match backend {
        FirewallBackend::Nftables => setup_nftables(listen_port, servers, bypass_ips, block_quic),
        FirewallBackend::Iptables => setup_iptables(listen_port, servers, bypass_ips, block_quic),
    }
}

/// Remove redirect rules.
pub fn cleanup_redirect(backend: FirewallBackend) -> Result<(), Box<dyn std::error::Error>> {
    match backend {
        FirewallBackend::Nftables => cleanup_nftables(),
        FirewallBackend::Iptables => cleanup_iptables(),
    }
}

// ── nftables ─────────────────────────────────────────────────────────

fn setup_nftables(
    listen_port: u16,
    servers: &[ServerEndpoint],
    bypass_ips: &[String],
    block_quic: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let nft = find_nft().ok_or("nft binary not found")?;

    // Clean up any existing rules first
    let _ = cleanup_nftables();

    let ruleset = build_nft_ruleset(listen_port, servers, bypass_ips, block_quic);

    // Use sh -c with pipe to feed ruleset via stdin
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "echo '{}' | {} -f -",
            ruleset.replace('\'', "'\\''"),
            nft
        ))
        .status()?;

    if !status.success() {
        return Err(format!("nft command failed (binary: {})", nft).into());
    }

    tracing::info!("nftables redirect rules installed (table: {}, nft: {})", NFT_TABLE, nft);
    Ok(())
}

fn build_nft_ruleset(
    listen_port: u16,
    servers: &[ServerEndpoint],
    bypass_ips: &[String],
    block_quic: bool,
) -> String {
    // Build bypass rules for excluded source IPs
    let bypass_rules: String = bypass_ips
        .iter()
        .map(|ip| format!("        ip saddr {} return", ip))
        .collect::<Vec<_>>()
        .join("\n");

    let bypass_section = if bypass_rules.is_empty() {
        String::new()
    } else {
        format!("\n        # Bypass devices (game consoles, etc.)\n{}\n", bypass_rules)
    };

    // QUIC (UDP/443) от LAN дропаем, чтобы браузер fallback'нул на TCP/443
    // и попал под redirect выше. Без этого любой сайт с h3 в HTTPS-записи
    // DNS (alpn="h3") уходит по UDP напрямую мимо прокси — TCP-путь при
    // этом честно проксируется, и снаружи кажется, что «всё через прокси».
    let quic_section = if block_quic {
        r#"
    chain quic_block {
        type filter hook prerouting priority dstnat; policy accept;
        iifname "br-lan" udp dport 443 drop
    }
"#
    } else {
        ""
    };

    // Пул серверов (LLD-10): туннельный порт каждого VPS выпускаем мимо
    // перехвата, иначе телефон с персональным клиентом в той же LAN получил бы
    // туннель в туннеле. Второе правило выпускает всё, кроме 80 и 443: на VPS
    // висят ssh и служебные ручки, а catch-all redirect ниже утащил бы их в
    // прокси, где первым говорит сервер и соединение висело бы до таймаута.
    // Web-порты остаются под перехватом сознательно: сайты на том же VPS
    // уходили из LAN голыми, светя SNI провайдерскому фильтру.
    let server_returns: String = servers
        .iter()
        .flat_map(|s| {
            [
                format!("        ip daddr {} tcp dport {} return", s.address, s.port),
                format!("        ip daddr {} tcp dport != {{ 80, 443 }} return", s.address),
            ]
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"
table ip {table} {{
    chain prerouting {{
        type nat hook prerouting priority dstnat; policy accept;
{bypass_section}
{server_returns}
        ip daddr 10.0.0.0/8 return
        ip daddr 172.16.0.0/12 return
        ip daddr 192.168.0.0/16 return
        ip daddr 127.0.0.0/8 return
        # iifname "br-lan" — перехватываем только LAN-трафик. Это критично:
        # без iifname-фильтра catch-all redirect на :listen_port захватит и
        # WAN-входящий трафик, включая SSH 8822 на сам роутер и любые DNAT
        # (port-forward к LAN-устройствам). Управление роутером отвалится.
        # LAN-only — единственно безопасный способ ловить «всё остальное».
        # Исключаем служебные TCP (почта, SSH-22, DNS-TCP/DoT, STUN/SIP)
        # и сам listen_port, чтобы не зацикливаться.
        iifname "br-lan" tcp dport != {{ 22, 25, 53, 110, 143, 465, 587, 853, 993, 995, 3478, 5060, 5061, {listen_port} }} redirect to :{listen_port}
    }}
    chain input {{
        type filter hook input priority filter; policy accept;
        tcp dport {listen_port} ip saddr 10.0.0.0/8 accept
        tcp dport {listen_port} ip saddr 172.16.0.0/12 accept
        tcp dport {listen_port} ip saddr 192.168.0.0/16 accept
        tcp dport {listen_port} ip saddr 127.0.0.0/8 accept
        tcp dport {listen_port} drop
    }}
{quic_section}}}
"#,
        table = NFT_TABLE,
        server_returns = server_returns,
        listen_port = listen_port,
        bypass_section = bypass_section,
        quic_section = quic_section,
    )
}

fn cleanup_nftables() -> Result<(), Box<dyn std::error::Error>> {
    let nft = match find_nft() {
        Some(n) => n,
        None => return Ok(()), // no nft binary — nothing to clean
    };

    // Legacy: ручная таблица xr_quic_block с роутеров до того, как QUIC-блок
    // переехал в chain quic_block внутри xr_proxy. Подчищаем, чтобы не дублировать.
    let _ = Command::new(&nft)
        .args(["delete", "table", "ip", "xr_quic_block"])
        .status();

    let status = Command::new(&nft)
        .args(["delete", "table", "ip", NFT_TABLE])
        .status()?;
    if status.success() {
        tracing::info!("nftables rules cleaned up");
    }
    Ok(())
}

// ── iptables ─────────────────────────────────────────────────────────

fn setup_iptables(
    listen_port: u16,
    servers: &[ServerEndpoint],
    bypass_ips: &[String],
    block_quic: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = cleanup_iptables();

    for rule in build_ipt_rules(listen_port, servers, bypass_ips, block_quic) {
        let args: Vec<&str> = rule.iter().map(String::as_str).collect();
        run_ipt(&args)?;
    }

    let ipt = find_iptables().unwrap_or_else(|| "iptables".to_string());
    tracing::info!("iptables redirect rules installed (chain: {}, binary: {})", IPT_CHAIN, ipt);
    Ok(())
}

/// Аргументы вызовов iptables в порядке применения.
fn build_ipt_rules(
    listen_port: u16,
    servers: &[ServerEndpoint],
    bypass_ips: &[String],
    block_quic: bool,
) -> Vec<Vec<String>> {
    let rule = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<String>>();

    // Create custom chain
    let mut rules = vec![rule(&["-t", "nat", "-N", IPT_CHAIN])];

    // Bypass devices (game consoles, etc.)
    for ip in bypass_ips {
        rules.push(rule(&["-t", "nat", "-A", IPT_CHAIN, "-s", ip, "-j", "RETURN"]));
    }

    // Пул серверов (LLD-10): выпускаем только туннельный порт, см. разбор
    // в build_nft_ruleset. Второе правило («всё, кроме web-портов») здесь не
    // нужно: redirect ниже и так ловит только 80 и 443.
    for server in servers {
        let port = server.port.to_string();
        rules.push(rule(&[
            "-t", "nat", "-A", IPT_CHAIN,
            "-d", &server.address, "-p", "tcp", "--dport", &port,
            "-j", "RETURN",
        ]));
    }

    // Private destination ranges
    for net in ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "127.0.0.0/8"] {
        rules.push(rule(&["-t", "nat", "-A", IPT_CHAIN, "-d", net, "-j", "RETURN"]));
    }

    // Redirect HTTP/HTTPS
    let port_str = listen_port.to_string();
    rules.push(rule(&[
        "-t", "nat", "-A", IPT_CHAIN,
        "-p", "tcp", "-m", "multiport", "--dports", "80,443",
        "-j", "REDIRECT", "--to-ports", &port_str,
    ]));

    // Hook into PREROUTING
    rules.push(rule(&["-t", "nat", "-A", "PREROUTING", "-j", IPT_CHAIN]));

    // Drop QUIC so browsers fall back to interceptable TCP/443
    // (см. комментарий в build_nft_ruleset).
    if block_quic {
        rules.push(rule(&[
            "-t", "mangle", "-A", "PREROUTING",
            "-i", "br-lan", "-p", "udp", "--dport", "443", "-j", "DROP",
        ]));
    }

    rules
}

fn cleanup_iptables() -> Result<(), Box<dyn std::error::Error>> {
    // Remove from PREROUTING
    let _ = run_ipt(&["-t", "nat", "-D", "PREROUTING", "-j", IPT_CHAIN]);
    let _ = run_ipt(&[
        "-t", "mangle", "-D", "PREROUTING",
        "-i", "br-lan", "-p", "udp", "--dport", "443", "-j", "DROP",
    ]);
    // Flush and delete chain
    let _ = run_ipt(&["-t", "nat", "-F", IPT_CHAIN]);
    let _ = run_ipt(&["-t", "nat", "-X", IPT_CHAIN]);
    tracing::info!("iptables rules cleaned up");
    Ok(())
}

fn run_ipt(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let ipt = find_iptables().ok_or("iptables binary not found")?;
    let status = Command::new(&ipt).args(args).status()?;
    if !status.success() {
        return Err(format!("{} {:?} failed", ipt, args).into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> Vec<ServerEndpoint> {
        vec![
            ServerEndpoint { address: "203.0.113.10".into(), port: 8443 },
            ServerEndpoint { address: "198.51.100.7".into(), port: 9443 },
        ]
    }

    fn joined(rules: Vec<Vec<String>>) -> Vec<String> {
        rules.iter().map(|r| r.join(" ")).collect()
    }

    #[test]
    fn nft_server_exclusion_covers_tunnel_port_only() {
        let ruleset = build_nft_ruleset(1080, &pool(), &[], true);

        assert!(ruleset.contains("ip daddr 203.0.113.10 tcp dport 8443 return"));
        assert!(ruleset.contains("ip daddr 198.51.100.7 tcp dport 9443 return"));
        // Целиком адрес VPS выводить из перехвата нельзя: сайты, живущие на
        // той же машине, уходили бы из LAN мимо туннеля с открытым SNI.
        assert!(!ruleset.contains("ip daddr 203.0.113.10 return"));
        assert!(!ruleset.contains("ip daddr 198.51.100.7 return"));
    }

    #[test]
    fn nft_server_keeps_everything_but_web_ports_direct() {
        let ruleset = build_nft_ruleset(1080, &pool(), &[], true);

        // Под перехват у адреса сервера попадают только 80 и 443, всё
        // остальное (ssh, служебные ручки) идёт мимо прокси, как и раньше.
        assert!(ruleset.contains("ip daddr 203.0.113.10 tcp dport != { 80, 443 } return"));
        assert!(ruleset.contains("ip daddr 198.51.100.7 tcp dport != { 80, 443 } return"));
    }

    #[test]
    fn nft_ruleset_survives_empty_pool() {
        let ruleset = build_nft_ruleset(1080, &[], &[], false);

        assert!(!ruleset.contains("ip daddr  "));
        assert!(ruleset.contains("redirect to :1080"));
        assert!(ruleset.contains("ip daddr 127.0.0.0/8 return"));
    }

    #[test]
    fn nft_keeps_bypass_private_nets_and_quic_block() {
        let bypass = vec!["192.168.1.50".to_string()];
        let ruleset = build_nft_ruleset(1080, &pool(), &bypass, true);

        assert!(ruleset.contains("ip saddr 192.168.1.50 return"));
        assert!(ruleset.contains("ip daddr 192.168.0.0/16 return"));
        assert!(ruleset.contains("redirect to :1080"));
        assert!(ruleset.contains("udp dport 443 drop"));

        assert!(!build_nft_ruleset(1080, &pool(), &bypass, false).contains("udp dport 443 drop"));
    }

    #[test]
    fn ipt_server_exclusion_covers_tunnel_port_only() {
        let rules = joined(build_ipt_rules(1080, &pool(), &[], true));

        assert!(rules.contains(&format!(
            "-t nat -A {IPT_CHAIN} -d 203.0.113.10 -p tcp --dport 8443 -j RETURN"
        )));
        assert!(!rules.contains(&format!("-t nat -A {IPT_CHAIN} -d 203.0.113.10 -j RETURN")));
        assert!(!rules.contains(&format!("-t nat -A {IPT_CHAIN} -d 198.51.100.7 -j RETURN")));
    }

    #[test]
    fn ipt_keeps_bypass_private_nets_and_quic_block() {
        let bypass = vec!["192.168.1.50".to_string()];
        let rules = joined(build_ipt_rules(1080, &pool(), &bypass, true));

        assert!(rules.contains(&format!("-t nat -A {IPT_CHAIN} -s 192.168.1.50 -j RETURN")));
        assert!(rules.contains(&format!("-t nat -A {IPT_CHAIN} -d 192.168.0.0/16 -j RETURN")));
        assert!(rules.iter().any(|r| r.contains("REDIRECT --to-ports 1080")));

        let quic = "-t mangle -A PREROUTING -i br-lan -p udp --dport 443 -j DROP".to_string();
        assert!(rules.contains(&quic));
        assert!(!joined(build_ipt_rules(1080, &pool(), &bypass, false)).contains(&quic));
    }

    #[test]
    fn ipt_rules_keep_order_of_chain_setup() {
        let rules = joined(build_ipt_rules(1080, &pool(), &["192.168.1.50".to_string()], true));

        // Цепочка сначала создаётся, исключения идут до redirect, а хук в
        // PREROUTING ставится после всех правил цепочки, иначе часть трафика
        // проскочит мимо ещё не дописанных.
        assert_eq!(rules.first().unwrap(), &format!("-t nat -N {IPT_CHAIN}"));
        let hook = rules
            .iter()
            .position(|r| r == &format!("-t nat -A PREROUTING -j {IPT_CHAIN}"))
            .unwrap();
        let last_chain_rule = rules
            .iter()
            .rposition(|r| r.contains(&format!("-A {IPT_CHAIN}")))
            .unwrap();
        assert!(last_chain_rule < hook);

        let bypass = rules.iter().position(|r| r.contains("-s 192.168.1.50")).unwrap();
        let exclusion = rules.iter().position(|r| r.contains("-d 203.0.113.10")).unwrap();
        let redirect = rules.iter().position(|r| r.contains("REDIRECT")).unwrap();
        assert!(bypass < exclusion && exclusion < redirect);
    }
}
