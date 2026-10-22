use crate::actions::{cmd_ok, run_cmd};
use crate::render::{WAN_POOL_CLEANUP, WAN_POOL_INIT, WAN_POOL_SETUP, WAN_POOL_WATCH};
use crate::steps::Step;
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::process::Command;

const INIT: &str = "/etc/init.d/xr-wan-pool";
const CONFIG: &str = "/etc/xr-proxy/wan-pool.conf";
const FORWARDS: &str = "/etc/xr-proxy/wan-pool.forwardings";
const RC: &str = "/etc/rc.d/S98xr-wan-pool";
const CLIENT_INIT: &str = "/etc/init.d/xr-proxy";

fn client_reload_commands(running: bool) -> Vec<Vec<&'static str>> {
    if running {
        vec![vec![CLIENT_INIT, "restart"]]
    } else {
        Vec::new()
    }
}

fn reload_running_client() -> Result<()> {
    for argv in client_reload_commands(cmd_ok(&[CLIENT_INIT, "running"])) {
        run_cmd(&argv)?;
    }
    Ok(())
}

pub type Member = xr_proto::config::WanPoolMember;

#[derive(Clone, Debug)]
pub struct Pool {
    members: Vec<Member>,
    bypass_ips: Vec<String>,
}

pub fn parse(config: &str) -> Result<Option<Pool>> {
    let value: toml::Value = toml::from_str(config)?;
    let Some(client) = value.get("client") else {
        return Ok(None);
    };
    let Some(members) = client.get("wan_pool") else {
        return Ok(None);
    };
    let members: Vec<Member> = members.clone().try_into()?;
    if members.is_empty() {
        return Ok(None);
    }
    if members.len() > 8 {
        bail!("WAN-пул допускает не более 8 каналов");
    }
    xr_proto::config::validate_wan_pool(&members).map_err(anyhow::Error::msg)?;
    if members.iter().any(|m| m.interface == "br-lan") {
        bail!("LAN br-lan нельзя использовать как канал WAN-пула");
    }
    let bypass_ips: Vec<String> = client
        .get("wan_pool_bypass_ips")
        .cloned()
        .map(toml::Value::try_into)
        .transpose()?
        .unwrap_or_default();
    for value in &bypass_ips {
        if value.parse::<IpAddr>().is_err() && !xr_proto::routing::valid_cidr(value) {
            bail!("неверный IP/CIDR в wan_pool_bypass_ips: {value}");
        }
    }
    Ok(Some(Pool {
        members,
        bypass_ips,
    }))
}

impl Pool {
    pub fn config(&self) -> String {
        let members = self
            .members
            .iter()
            .map(|m| format!("{}:{}", m.interface, m.weight))
            .collect::<Vec<_>>()
            .join(" ");
        let bypass = self.bypass_ips.join(" ");
        format!("LAN='br-lan'\nMEMBERS='{members}'\nBYPASS_IPS='{bypass}'\n")
    }
}

pub fn carry(new: &str, old: &str) -> Result<String> {
    let old: toml::Value = toml::from_str(old)?;
    let Some(client) = old.get("client") else {
        return Ok(new.to_string());
    };
    let mut extra = String::new();
    for key in ["wan_pool", "wan_pool_bypass_ips"] {
        if let Some(value) = client.get(key) {
            extra.push_str(&format!("{key} = {value}\n"));
        }
    }
    Ok(new.replacen("[client]\n", &format!("[client]\n{extra}"), 1))
}

#[derive(Default)]
struct Section {
    kind: String,
    fields: BTreeMap<String, Vec<String>>,
}

fn sections(show: &str, package: &str) -> BTreeMap<String, Section> {
    let mut result: BTreeMap<String, Section> = BTreeMap::new();
    for line in show.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        let Some(rest) = key.strip_prefix(&format!("{package}.")) else {
            continue;
        };
        let (section, field) = rest
            .split_once('.')
            .map_or((rest, None), |(a, b)| (a, Some(b)));
        let entry = result.entry(section.to_string()).or_default();
        if let Some(field) = field {
            entry.fields.insert(
                field.to_string(),
                value
                    .split_whitespace()
                    .map(|v| v.trim_matches('\'').to_string())
                    .collect(),
            );
        } else {
            entry.kind = value.trim_matches('\'').to_string();
        }
    }
    result
}

fn field<'a>(section: &'a Section, name: &str) -> &'a [String] {
    section
        .fields
        .get(name)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

pub fn forwarding_commands(
    network: &str,
    firewall: &str,
    devices: &[String],
) -> Result<Vec<Vec<String>>> {
    let network = sections(network, "network");
    let firewall = sections(firewall, "firewall");
    let mut commands = Vec::new();
    let mut targets = BTreeSet::new();
    for dev in devices {
        let interfaces: BTreeSet<&str> = network
            .iter()
            .filter(|(_, s)| {
                s.kind == "interface"
                    && ["device", "ifname"]
                        .iter()
                        .any(|k| field(s, k).contains(dev))
            })
            .map(|(name, _)| name.as_str())
            .collect();
        let zones: Vec<_> = firewall
            .values()
            .filter(|s| {
                s.kind == "zone"
                    && (field(s, "device").contains(dev)
                        || field(s, "network")
                            .iter()
                            .any(|iface| interfaces.contains(iface.as_str())))
            })
            .collect();
        if zones.len() != 1 {
            bail!("для {dev} нужна ровно одна firewall-зона");
        }
        let names = field(zones[0], "name");
        let Some(zone) = names.first() else {
            bail!("у firewall-зоны {dev} нет имени")
        };
        if zone == "lan"
            || !zone
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            bail!("недопустимая WAN-зона: {zone}");
        }
        targets.insert(zone.clone());
    }
    for (index, zone) in targets.iter().enumerate() {
        let allowed = firewall.values().any(|s| {
            s.kind == "forwarding"
                && field(s, "src") == ["lan"]
                && field(s, "dest") == [zone.as_str()]
                && field(s, "enabled") != ["0"]
        });
        if allowed {
            continue;
        }
        let name = format!("xr_wan_pool_{index}");
        if let Some(existing) = firewall.get(&name) {
            if existing.kind != "forwarding" || field(existing, "src") != ["lan"] {
                bail!("секция firewall.{name} уже занята");
            }
        }
        for assignment in [
            format!("firewall.{name}=forwarding"),
            format!("firewall.{name}.src=lan"),
            format!("firewall.{name}.dest={zone}"),
            format!("firewall.{name}.enabled=1"),
        ] {
            commands.push(vec!["uci".into(), "set".into(), assignment]);
        }
    }
    if !commands.is_empty() {
        commands.push(vec!["uci".into(), "commit".into(), "firewall".into()]);
        commands.push(vec!["/etc/init.d/firewall".into(), "reload".into()]);
    }
    Ok(commands)
}

fn show(package: &str) -> Result<String> {
    let out = Command::new("uci").args(["show", package]).output()?;
    if !out.status.success() {
        bail!("uci show {package} завершился ошибкой");
    }
    Ok(String::from_utf8(out.stdout)?)
}

pub struct WanPool {
    pub pool: Pool,
}

impl WanPool {
    fn files(&self) -> Vec<(&'static str, String, u32)> {
        vec![
            ("/usr/bin/wan-pool-setup.sh", WAN_POOL_SETUP.into(), 0o755),
            (
                "/usr/bin/wan-pool-cleanup.sh",
                WAN_POOL_CLEANUP.into(),
                0o755,
            ),
            ("/usr/bin/wan-pool-watch.sh", WAN_POOL_WATCH.into(), 0o755),
            (INIT, WAN_POOL_INIT.into(), 0o755),
            (CONFIG, self.pool.config(), 0o600),
        ]
    }
    fn commands(&self) -> Result<Vec<Vec<String>>> {
        forwarding_commands(
            &show("network")?,
            &show("firewall")?,
            &self
                .pool
                .members
                .iter()
                .map(|m| m.interface.clone())
                .collect::<Vec<_>>(),
        )
    }
}

impl Step for WanPool {
    fn name(&self) -> String {
        "wan:pool".into()
    }
    fn check(&self) -> Result<bool> {
        let files_ok = self.files().iter().all(|(path, body, _)| {
            std::fs::read_to_string(path)
                .map(|s| s == *body)
                .unwrap_or(false)
        });
        Ok(files_ok
            && std::path::Path::new(RC).exists()
            && cmd_ok(&[INIT, "running"])
            && cmd_ok(&["nft", "list", "table", "ip", "xr_wan_pool"])
            && self.commands()?.is_empty())
    }
    fn apply(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let commands = self.commands()?;
        for (path, body, mode) in self.files() {
            if let Some(parent) = std::path::Path::new(path).parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, body)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        }
        let mut owned: BTreeSet<String> = std::fs::read_to_string(FORWARDS)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        for argv in &commands {
            if argv.len() == 3 && argv[0] == "uci" && argv[1] == "set" {
                if let Some(name) = argv[2]
                    .strip_prefix("firewall.")
                    .and_then(|s| s.strip_suffix("=forwarding"))
                {
                    owned.insert(name.to_string());
                }
            }
        }
        std::fs::write(FORWARDS, owned.into_iter().collect::<Vec<_>>().join("\n"))?;
        for argv in commands {
            run_cmd(&argv.iter().map(String::as_str).collect::<Vec<_>>())?;
        }
        run_cmd(&["/usr/bin/wan-pool-watch.sh", "--once"])?;
        run_cmd(&[INIT, "enable"])?;
        run_cmd(&[INIT, "restart"])?;
        self.verify()?;
        reload_running_client()
    }
    fn verify(&self) -> Result<()> {
        for _ in 0..20 {
            if self.check()? {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        bail!("WAN-пул не поднялся после установки")
    }
}

pub fn cleanup_forwarding_commands(firewall: &str, owned: &str) -> Result<Vec<Vec<String>>> {
    let firewall = sections(firewall, "firewall");
    let mut commands = Vec::new();
    for name in owned.lines() {
        let Some(index) = name.strip_prefix("xr_wan_pool_") else {
            bail!("чужая секция в списке WAN-пула")
        };
        if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
            bail!("неверное имя секции WAN-пула")
        }
        if let Some(section) = firewall.get(name) {
            if section.kind != "forwarding" || field(section, "src") != ["lan"] {
                bail!("секция firewall.{name} изменена вне WAN-пула")
            }
            commands.push(vec![
                "uci".into(),
                "delete".into(),
                format!("firewall.{name}"),
            ]);
        }
    }
    if !commands.is_empty() {
        commands.push(vec!["uci".into(), "commit".into(), "firewall".into()]);
        commands.push(vec!["/etc/init.d/firewall".into(), "reload".into()]);
    }
    Ok(commands)
}

pub struct WanPoolDisabled;

impl WanPoolDisabled {
    pub fn needed() -> bool {
        [INIT, CONFIG, FORWARDS]
            .iter()
            .any(|p| std::path::Path::new(p).exists())
    }
}

impl Step for WanPoolDisabled {
    fn name(&self) -> String {
        "wan:pool-disable".into()
    }
    fn check(&self) -> Result<bool> {
        Ok(!std::path::Path::new(RC).exists()
            && !std::path::Path::new(CONFIG).exists()
            && !std::path::Path::new(FORWARDS).exists()
            && (!std::path::Path::new(INIT).exists() || !cmd_ok(&[INIT, "running"])))
    }
    fn apply(&self) -> Result<()> {
        let owned = std::fs::read_to_string(FORWARDS).unwrap_or_default();
        let commands = if owned.is_empty() {
            Vec::new()
        } else {
            cleanup_forwarding_commands(&show("firewall")?, &owned)?
        };
        if std::path::Path::new(INIT).exists() {
            run_cmd(&[INIT, "stop"])?;
            run_cmd(&[INIT, "disable"])?;
        } else if std::path::Path::new("/usr/bin/wan-pool-cleanup.sh").exists() {
            run_cmd(&["/usr/bin/wan-pool-cleanup.sh"])?;
        }
        for argv in commands {
            run_cmd(&argv.iter().map(String::as_str).collect::<Vec<_>>())?;
        }
        for file in [CONFIG, FORWARDS] {
            match std::fs::remove_file(file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        reload_running_client()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const CLIENT: &str = "[client]\nwan_pool=[{interface='wan',weight=1},{interface='lan1',weight=3}]\nwan_pool_bypass_ips=['192.168.1.42']\n";
    #[test]
    fn configuration_is_validated_before_shell_rendering() {
        assert_eq!(
            parse(CLIENT).unwrap().unwrap().config(),
            "LAN='br-lan'\nMEMBERS='wan:1 lan1:3'\nBYPASS_IPS='192.168.1.42'\n"
        );
        assert!(parse("[client]\n").unwrap().is_none());
        for broken in [
            CLIENT.replace("lan1", "x';id"),
            CLIENT.replace("weight=3", "weight=0"),
            CLIENT.replace("192.168.1.42", "$(id)"),
            CLIENT.replace("lan1", "wan"),
        ] {
            assert!(parse(&broken).is_err());
        }
    }
    #[test]
    fn force_preserves_both_pool_fields_and_other_config() {
        let new = "[client]\nlisten_port=1080\n[routing]\ndefault_action='proxy'\n";
        let merged = carry(new, CLIENT).unwrap();
        assert_eq!(
            parse(&merged).unwrap().unwrap().config(),
            parse(CLIENT).unwrap().unwrap().config()
        );
        let v: toml::Value = toml::from_str(&merged).unwrap();
        assert_eq!(v["client"]["listen_port"].as_integer(), Some(1080));
        assert_eq!(v["routing"]["default_action"].as_str(), Some("proxy"));
    }

    #[test]
    fn pool_validation_matches_core_and_keeps_ip_cidr_exceptions() {
        let config = "[client]\nwan_pool=[{interface='eth0:1',weight=4294967295}]\nwan_pool_bypass_ips=['192.168.1.0/24','2001:db8::/64','::1']\n";
        let parsed = parse(config).unwrap().unwrap();
        xr_proto::config::validate_wan_pool(&parsed.members).unwrap();
        assert_eq!(parsed.config(), "LAN='br-lan'\nMEMBERS='eth0:1:4294967295'\nBYPASS_IPS='192.168.1.0/24 2001:db8::/64 ::1'\n");
        for invalid in [
            "192.168.1.0/33",
            "2001:db8::/129",
            "2001:db8:::",
            "192.168.1.999",
        ] {
            assert!(parse(&config.replace("192.168.1.0/24", invalid)).is_err());
        }
        assert!(parse(&config.replace("weight=4294967295", "weight=0")).is_err());
        assert!(parse(&config.replace(
            "interface='eth0:1',weight=4294967295",
            "interface='wan',weight=4294967295},{interface='wan2',weight=1"
        ))
        .is_err());
    }
    #[test]
    fn forwarding_uses_device_to_interface_to_zone_and_leaves_input_untouched() {
        let network = "network.wan=interface\nnetwork.wan.device='wan'\nnetwork.wan2=interface\nnetwork.wan2.device='lan1'\n";
        let firewall = "firewall.a=zone\nfirewall.a.name='wan'\nfirewall.a.network='wan'\nfirewall.b=zone\nfirewall.b.name='wan2'\nfirewall.b.network='wan2'\nfirewall.f=forwarding\nfirewall.f.src='lan'\nfirewall.f.dest='wan'\n";
        let commands =
            forwarding_commands(network, firewall, &["wan".into(), "lan1".into()]).unwrap();
        assert_eq!(commands.len(), 6);
        assert!(commands
            .iter()
            .any(|c| c.iter().any(|v| v.ends_with(".dest=wan2"))));
        assert!(!commands
            .iter()
            .flatten()
            .any(|v| v.contains("input") || v.contains("masq") || v.contains("network.")));
        let allowed = format!(
            "{firewall}firewall.f2=forwarding\nfirewall.f2.src='lan'\nfirewall.f2.dest='wan2'\n"
        );
        assert!(
            forwarding_commands(network, &allowed, &["wan".into(), "lan1".into()])
                .unwrap()
                .is_empty()
        );
        assert!(forwarding_commands(network, firewall, &["missing".into()]).is_err());
    }

    #[test]
    fn removal_deletes_only_forwardings_recorded_by_the_installer() {
        let firewall = "firewall.xr_wan_pool_1=forwarding\nfirewall.xr_wan_pool_1.src='lan'\nfirewall.xr_wan_pool_1.dest='wan2'\nfirewall.user=forwarding\nfirewall.user.src='lan'\nfirewall.user.dest='wan2'\nfirewall.wan2=zone\nfirewall.wan2.input='REJECT'\n";
        let commands = cleanup_forwarding_commands(firewall, "xr_wan_pool_1\n").unwrap();
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0], ["uci", "delete", "firewall.xr_wan_pool_1"]);
        assert!(!commands
            .iter()
            .flatten()
            .any(|s| s.contains("firewall.user") || s.contains("firewall.wan2")));
        assert!(cleanup_forwarding_commands(firewall, "user\n").is_err());
        assert!(cleanup_forwarding_commands(
            &firewall.replace(".src='lan'", ".src='wan'"),
            "xr_wan_pool_1\n"
        )
        .is_err());
    }

    #[test]
    fn changed_pool_reloads_running_client_and_leaves_initial_start_to_service_step() {
        assert_eq!(
            client_reload_commands(true),
            vec![vec!["/etc/init.d/xr-proxy", "restart"]]
        );
        assert!(client_reload_commands(false).is_empty());
    }
}
