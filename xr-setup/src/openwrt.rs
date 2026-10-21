//! Шаги, специфичные для OpenWRT: procd-сервис, dnsmasq на Quad9 и смена
//! раздаваемого SSID. Разбор вывода uci вынесен в чистые функции, сами шаги
//! зовут uci/init.d как есть.

use crate::actions::{cmd_ok, run_cmd};
use crate::steps::Step;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

fn uci_get(key: &str) -> Option<String> {
    let out = Command::new("uci").args(["-q", "get", key]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// procd-сервис xr-proxy: init-скрипт (вместе с fd-limit внутри), enable
/// и запуск. Замена скрипта под работающим сервисом перезапускает его.
pub struct ProcdService {
    pub init_path: PathBuf,
    pub content: String,
    /// Симлинк, который оставляет `enable` (START=99 в init-скрипте).
    pub rc_link: PathBuf,
}

impl ProcdService {
    fn init(&self) -> String {
        self.init_path.to_string_lossy().into_owned()
    }
}

impl Step for ProcdService {
    fn name(&self) -> String {
        "service:xr-proxy".into()
    }

    fn check(&self) -> Result<bool> {
        let same = std::fs::read_to_string(&self.init_path)
            .map(|cur| cur == self.content)
            .unwrap_or(false);
        Ok(same && self.rc_link.exists() && cmd_ok(&[&self.init(), "running"]))
    }

    fn apply(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&self.init_path, &self.content)
            .with_context(|| format!("запись {}", self.init_path.display()))?;
        std::fs::set_permissions(&self.init_path, std::fs::Permissions::from_mode(0o755))?;
        let init = self.init();
        run_cmd(&[&init, "enable"])?;
        // restart, а не start: если сервис уже бежал со старым init, он
        // должен перечитать и скрипт, и конфиг.
        run_cmd(&[&init, "restart"])
    }
}

/// Апстрим dnsmasq это локальный DNS-форвардер xr-client (XR-285): он уносит
/// резолв в туннель и говорит с публичным резолвером по DoT. Раньше тут стоял
/// Quad9 обычным UDP:53, и провайдер подменял ответ от него ровно так же, как
/// от своего: поддельный NXDOMAIN клал сайт до того, как перехвату было что
/// перехватывать.
pub struct DnsmasqForwarder {
    /// Порт форвардера на петле, тот же, что в секции `[dns]` конфига клиента.
    pub port: u16,
}

/// Единственный апстрим, который дозволен dnsmasq.
pub fn dnsmasq_upstream(port: u16) -> String {
    format!("127.0.0.1#{port}")
}

/// Стоит ли dnsmasq в целевом состоянии. Список апстримов обязан состоять
/// ровно из форвардера: соседний адрес в нём dnsmasq спрашивает наравне, и
/// часть запросов ушла бы из LAN открытым UDP, а какая именно, решал бы
/// случай. `noresolv` отрезает апстримы, которые роутеру раздал провайдер по
/// DHCP.
pub fn dnsmasq_state_ok(noresolv: bool, servers: &str, port: u16) -> bool {
    let listed: Vec<&str> = servers.split_whitespace().collect();
    noresolv && listed == [dnsmasq_upstream(port)]
}

impl Step for DnsmasqForwarder {
    fn name(&self) -> String {
        "dnsmasq:tunnel".into()
    }

    fn check(&self) -> Result<bool> {
        let noresolv = uci_get("dhcp.@dnsmasq[0].noresolv").as_deref() == Some("1");
        let servers = uci_get("dhcp.@dnsmasq[0].server").unwrap_or_default();
        Ok(dnsmasq_state_ok(noresolv, &servers, self.port))
    }

    fn apply(&self) -> Result<()> {
        run_cmd(&["uci", "set", "dhcp.@dnsmasq[0].noresolv=1"])?;
        // Список переписывается целиком: и апстримы провайдера, и Quad9 с
        // прежних раскладок это ровно то, от чего уходим.
        let _ = run_cmd(&["uci", "-q", "delete", "dhcp.@dnsmasq[0].server"]);
        run_cmd(&[
            "uci",
            "add_list",
            &format!("dhcp.@dnsmasq[0].server={}", dnsmasq_upstream(self.port)),
        ])?;
        run_cmd(&["uci", "commit", "dhcp"])?;
        run_cmd(&["/etc/init.d/dnsmasq", "restart"])
    }
}

/// Второй WAN-порт (XR-310): порт уходит из моста br-lan, под ним поднимается
/// интерфейс `wan2` с proto dhcp. Метрика выше основной, поэтому пока кабель
/// второго провайдера не воткнут, маршрутом по умолчанию остаётся первый
/// канал. `ipv6 = 0` и `peerdns = 0` не пускают в LAN чужие RA и DHCPv6 и не
/// дают второму провайдеру подменить резолвер. Интерфейс садится в зону wan,
/// и входящие с него форвард-правила не пускают.
pub struct SecondWan {
    /// Порт как его зовёт ядро (`lan4`), уходит из br-lan и становится
    /// устройством wan2.
    pub port: String,
    /// Метрика интерфейса wan2: выше метрики основного wan.
    pub metric: u32,
    /// Режим отката: снять wan2 и вернуть порт в мост.
    pub remove: bool,
}

/// Имя интерфейса второго канала. Оно же значение `interface` в `[[wan]]`
/// конфига клиента.
pub const SECOND_WAN_IFACE: &str = "wan2";

/// Префикс ключа секции моста br-lan в выводе `uci show network`: секция
/// бывает анонимной (`network.@device[0]`) и именованной (`network.br_lan`),
/// поэтому решает опция `name`, а не индекс.
pub fn find_br_lan_device(show_network: &str) -> Option<String> {
    show_network.lines().find_map(|l| {
        let l = l.trim();
        let (key, value) = l.split_once('=')?;
        (value.trim_matches('\'') == "br-lan" && key.ends_with(".name"))
            .then(|| key.trim_end_matches(".name").to_string())
    })
}

/// Префикс ключа wan-зоны файрвола: ищется по имени, а не по индексу,
/// раскладки зон на роутерах отличаются. Заголовок секции узнаётся по
/// неквоченому значению: тип секции uci печатает без кавычек, значения
/// опций в них.
pub fn find_wan_zone(show_firewall: &str) -> Option<String> {
    let mut current: Option<String> = None;
    for line in show_firewall.lines() {
        let line = line.trim();
        let (key, value) = match line.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        if !value.starts_with('\'') {
            current = Some(key.to_string());
            continue;
        }
        let value = value.trim_matches('\'');
        if let Some(sec) = &current {
            if key == format!("{sec}.name") && value == "wan" {
                return Some(sec.clone());
            }
        }
    }
    None
}

/// Значения list-поля из `uci show`: кавычки снимаются, каждая выдача uci
/// это отдельная строка вида `network.@device[0].ports='lan1'`.
fn uci_list_values(show: &str, field: &str) -> Vec<String> {
    let prefix = format!("{field}=");
    show.lines()
        .filter_map(|l| {
            let l = l.trim();
            l.starts_with(&prefix)
                .then(|| l[prefix.len()..].trim().trim_matches('\'').to_string())
        })
        .collect()
}

/// Одиночное значение опции интерфейса wan2 из `uci show network`, пустая
/// строка если опции нет.
fn wan2_option(show_network: &str, field: &str) -> String {
    show_network
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix(&format!("network.{SECOND_WAN_IFACE}.{field}="))
                .map(|v| v.trim_matches('\'').to_string())
        })
        .unwrap_or_default()
}

/// Команды uci для включения второго WAN. `lan_dev` и `wan_zone` это полные
/// префиксы секций из [`find_br_lan_device`] и [`find_wan_zone`]. Возврат
/// строками, а не вызовами, ради стенда: команды проверяются тестом до
/// того, как уедут на роутер.
pub fn second_wan_commands(lan_dev: &str, wan_zone: &str, port: &str, metric: u32) -> Vec<String> {
    let iface = SECOND_WAN_IFACE;
    vec![
        format!("uci del_list {lan_dev}.ports='{port}'"),
        format!("uci set network.{iface}=interface"),
        format!("uci set network.{iface}.proto='dhcp'"),
        format!("uci set network.{iface}.device='{port}'"),
        format!("uci set network.{iface}.metric='{metric}'"),
        format!("uci set network.{iface}.ipv6='0'"),
        format!("uci set network.{iface}.peerdns='0'"),
        format!("uci add_list {wan_zone}.network='{iface}'"),
    ]
}

/// Команды отката: снять wan2 и вернуть порт в мост br-lan.
pub fn second_wan_removal_commands(lan_dev: &str, wan_zone: &str, port: &str) -> Vec<String> {
    let iface = SECOND_WAN_IFACE;
    vec![
        format!("uci delete network.{iface}"),
        format!("uci del_list {wan_zone}.network='{iface}'"),
        format!("uci add_list {lan_dev}.ports='{port}'"),
    ]
}

/// Состояние сети в целевом виде с включённым вторым WAN: порт вне моста,
/// интерфейс wan2 живёт dhcp на этом порту с заданной метрикой без IPv6 и
/// чужих DNS, интерфейс числится в wan-зоне файрвола.
pub fn second_wan_state_ok(
    show_network: &str,
    show_firewall: &str,
    lan_dev: &str,
    wan_zone: &str,
    port: &str,
    metric: u32,
) -> bool {
    let bridge_ports = uci_list_values(show_network, &format!("{lan_dev}.ports"));
    let in_wan_zone = uci_list_values(show_firewall, &format!("{wan_zone}.network"))
        .iter()
        .any(|n| n == SECOND_WAN_IFACE);
    let iface_exists = show_network
        .lines()
        .any(|l| l.trim() == format!("network.{SECOND_WAN_IFACE}=interface"));
    iface_exists
        && !bridge_ports.iter().any(|p| p == port)
        && wan2_option(show_network, "proto") == "dhcp"
        && wan2_option(show_network, "device") == port
        && wan2_option(show_network, "metric") == metric.to_string()
        && wan2_option(show_network, "ipv6") == "0"
        && wan2_option(show_network, "peerdns") == "0"
        && in_wan_zone
}

/// Состояние после отката: wan2 нет, порт вернулся в мост.
pub fn second_wan_removed_ok(
    show_network: &str,
    show_firewall: &str,
    lan_dev: &str,
    wan_zone: &str,
    port: &str,
) -> bool {
    let iface_gone = !show_network
        .lines()
        .any(|l| l.trim().starts_with(&format!("network.{SECOND_WAN_IFACE}=")));
    let bridge_ports = uci_list_values(show_network, &format!("{lan_dev}.ports"));
    let out_of_wan_zone = !uci_list_values(show_firewall, &format!("{wan_zone}.network"))
        .iter()
        .any(|n| n == SECOND_WAN_IFACE);
    iface_gone && bridge_ports.iter().any(|p| p == port) && out_of_wan_zone
}

fn uci_show(name: &str) -> Result<String> {
    let out = Command::new("uci")
        .args(["show", name])
        .output()
        .with_context(|| format!("uci show {name}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Разобранные куски uci, нужные шагу второго WAN.
struct SecondWanSections {
    show_network: String,
    show_firewall: String,
    lan_dev: String,
    wan_zone: String,
}

impl SecondWan {
    fn sections(&self) -> Result<SecondWanSections> {
        let show_network = uci_show("network")?;
        let show_firewall = uci_show("firewall")?;
        let lan_dev = find_br_lan_device(&show_network)
            .ok_or_else(|| anyhow::anyhow!("в конфиге network нет моста br-lan"))?;
        let wan_zone = find_wan_zone(&show_firewall)
            .ok_or_else(|| anyhow::anyhow!("в конфиге firewall нет зоны wan"))?;
        Ok(SecondWanSections { show_network, show_firewall, lan_dev, wan_zone })
    }
}

impl Step for SecondWan {
    fn name(&self) -> String {
        if self.remove {
            "wan:second-remove".into()
        } else {
            "wan:second".into()
        }
    }

    fn check(&self) -> Result<bool> {
        let s = self.sections()?;
        Ok(if self.remove {
            second_wan_removed_ok(&s.show_network, &s.show_firewall, &s.lan_dev, &s.wan_zone, &self.port)
        } else {
            second_wan_state_ok(
                &s.show_network,
                &s.show_firewall,
                &s.lan_dev,
                &s.wan_zone,
                &self.port,
                self.metric,
            )
        })
    }

    fn apply(&self) -> Result<()> {
        let s = self.sections()?;
        let commands = if self.remove {
            second_wan_removal_commands(&s.lan_dev, &s.wan_zone, &self.port)
        } else {
            second_wan_commands(&s.lan_dev, &s.wan_zone, &self.port, self.metric)
        };
        for command in &commands {
            let argv: Vec<&str> = command.split_whitespace().collect();
            run_cmd(&argv)?;
        }
        run_cmd(&["uci", "commit", "network"])?;
        run_cmd(&["uci", "commit", "firewall"])?;
        // reload, а не restart: меняются только затронутые интерфейсы, сессия
        // настройки по Wi-Fi или соседнему порту не рвётся.
        run_cmd(&["/etc/init.d/network", "reload"])
    }
}

/// Секции wifi-iface, которым меняем SSID: точки доступа, но не sta-аплинки
/// (репитер, ходящий клиентом в чужую сеть, трогать нельзя).
pub fn ap_sections(uci_show_wireless: &str) -> Vec<String> {
    let mut sections = Vec::new();
    for line in uci_show_wireless.lines() {
        if let Some(name) = line.strip_suffix("=wifi-iface") {
            sections.push(name.trim().to_string());
        }
    }
    sections.retain(|s| {
        !uci_show_wireless
            .lines()
            .any(|l| l.trim() == format!("{s}.mode='sta'"))
    });
    sections
}

/// Смена раздаваемого SSID (LLD-13 п. 5.9). Всегда последний шаг плана:
/// `wifi reload` рвёт Wi-Fi-сессию, через которую роутер настраивают,
/// поэтому uci-значения коммитятся сразу, а перезагрузка радио уходит в
/// фон с задержкой, чтобы установщик успел договорить.
pub struct WifiSsid {
    pub ssid: String,
    pub pass: Option<String>,
}

impl WifiSsid {
    fn sections(&self) -> Result<Vec<String>> {
        let out = Command::new("uci")
            .args(["show", "wireless"])
            .output()
            .context("uci show wireless")?;
        if !out.status.success() {
            anyhow::bail!("на роутере нет конфига wireless (uci show wireless)");
        }
        let sections = ap_sections(&String::from_utf8_lossy(&out.stdout));
        if sections.is_empty() {
            anyhow::bail!("не нашёл ни одной точки доступа в конфиге wireless");
        }
        Ok(sections)
    }
}

impl Step for WifiSsid {
    fn name(&self) -> String {
        "wifi:ssid".into()
    }

    fn check(&self) -> Result<bool> {
        let done = self.sections()?.iter().all(|s| {
            let ssid_ok = uci_get(&format!("{s}.ssid")).as_deref() == Some(self.ssid.as_str());
            let pass_ok = self
                .pass
                .as_ref()
                .is_none_or(|p| uci_get(&format!("{s}.key")).as_deref() == Some(p.as_str()));
            ssid_ok && pass_ok
        });
        Ok(done)
    }

    fn apply(&self) -> Result<()> {
        for s in self.sections()? {
            run_cmd(&["uci", "set", &format!("{s}.ssid={}", self.ssid)])?;
            if let Some(pass) = &self.pass {
                run_cmd(&["uci", "set", &format!("{s}.encryption=psk2")])?;
                run_cmd(&["uci", "set", &format!("{s}.key={pass}")])?;
            }
        }
        run_cmd(&["uci", "commit", "wireless"])?;
        Command::new("sh")
            .args(["-c", "sleep 10; wifi reload"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("отложенный wifi reload")?;
        println!("      SSID применится через ~10 секунд, сеть переименуется в '{}'", self.ssid);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ap_sections_take_access_points_and_skip_sta_uplink() {
        let uci = "\
wireless.radio0=wifi-device
wireless.radio0.channel='36'
wireless.default_radio0=wifi-iface
wireless.default_radio0.ssid='RIDERS'
wireless.default_radio0.mode='ap'
wireless.default_radio1=wifi-iface
wireless.default_radio1.ssid='RIDERS'
wireless.wwan=wifi-iface
wireless.wwan.mode='sta'
wireless.wwan.ssid='UPSTREAM'
";
        assert_eq!(
            ap_sections(uci),
            ["wireless.default_radio0", "wireless.default_radio1"],
            "iface без mode считается точкой доступа, sta-аплинк не трогаем"
        );
    }

    #[test]
    fn ap_sections_empty_on_no_wifi() {
        assert!(ap_sections("").is_empty());
    }

    #[test]
    fn dnsmasq_state_wants_only_the_forwarder() {
        assert!(dnsmasq_state_ok(true, "127.0.0.1#5353", 5353));
        assert!(!dnsmasq_state_ok(false, "127.0.0.1#5353", 5353), "без noresolv едут апстримы провайдера");
        assert!(!dnsmasq_state_ok(true, "", 5353));
        assert!(!dnsmasq_state_ok(true, "9.9.9.9 149.112.112.112", 5353), "прежняя раскладка на Quad9 это не целевое состояние");
        assert!(
            !dnsmasq_state_ok(true, "127.0.0.1#5353 9.9.9.9", 5353),
            "соседний открытый апстрим dnsmasq спрашивает наравне, и часть запросов уходит голыми"
        );
        assert!(!dnsmasq_state_ok(true, "127.0.0.1#5300", 5353), "порт форвардера обязан совпадать с конфигом клиента");
    }

    const SHOW_NETWORK: &str = "\
network.loopback=interface
network.loopback.device='lo'
network.globals=globals
network.@device[0]=device
network.@device[0].name='br-lan'
network.@device[0].type='bridge'
network.@device[0].ports='lan1'
network.@device[0].ports='lan2'
network.@device[0].ports='lan3'
network.@device[0].ports='lan4'
network.lan=interface
network.lan.device='br-lan'
network.wan=interface
network.wan.device='wan'
network.wan.proto='dhcp'
";

    const SHOW_FIREWALL: &str = "\
firewall.@zone[0]=zone
firewall.@zone[0].name='lan'
firewall.@zone[0].network='lan'
firewall.@zone[1]=zone
firewall.@zone[1].name='wan'
firewall.@zone[1].network='wan'
firewall.@zone[1].network='wan6'
firewall.@zone[1].input='REJECT'
";

    #[test]
    fn br_lan_and_wan_zone_found_by_name() {
        assert_eq!(
            find_br_lan_device(SHOW_NETWORK).as_deref(),
            Some("network.@device[0]")
        );
        assert_eq!(find_wan_zone(SHOW_FIREWALL).as_deref(), Some("firewall.@zone[1]"));
        // Именованные секции тоже находятся.
        assert_eq!(
            find_br_lan_device("network.br_lan=device\nnetwork.br_lan.name='br-lan'\n").as_deref(),
            Some("network.br_lan")
        );
        assert!(find_wan_zone("firewall.@zone[0]=zone\nfirewall.@zone[0].name='lan'\n").is_none());
    }

    #[test]
    fn second_wan_commands_free_port_and_bound_interface() {
        let commands = second_wan_commands("network.@device[0]", "firewall.@zone[1]", "lan4", 40);
        assert_eq!(
            commands[0],
            "uci del_list network.@device[0].ports='lan4'",
            "порт уходит из моста"
        );
        assert!(commands.contains(&"uci set network.wan2.proto='dhcp'".to_string()));
        assert!(commands.contains(&"uci set network.wan2.device='lan4'".to_string()));
        assert!(commands.contains(&"uci set network.wan2.metric='40'".to_string()));
        assert!(commands.contains(&"uci set network.wan2.ipv6='0'".to_string()), "чужие RA не должны попадать в LAN");
        assert!(commands.contains(&"uci set network.wan2.peerdns='0'".to_string()), "второй провайдер не должен подменять резолвер");
        assert!(commands.contains(&"uci add_list firewall.@zone[1].network='wan2'".to_string()));
    }

    #[test]
    fn second_wan_removal_returns_port_to_bridge() {
        let commands = second_wan_removal_commands("network.@device[0]", "firewall.@zone[1]", "lan4");
        assert!(commands.contains(&"uci delete network.wan2".to_string()));
        assert!(commands.contains(&"uci del_list firewall.@zone[1].network='wan2'".to_string()));
        assert!(commands.contains(&"uci add_list network.@device[0].ports='lan4'".to_string()));
    }

    #[test]
    fn second_wan_state_demands_everything() {
        let applied_network = "\
network.@device[0]=device
network.@device[0].name='br-lan'
network.@device[0].ports='lan1'
network.@device[0].ports='lan2'
network.@device[0].ports='lan3'
network.wan2=interface
network.wan2.proto='dhcp'
network.wan2.device='lan4'
network.wan2.metric='40'
network.wan2.ipv6='0'
network.wan2.peerdns='0'
";
        let applied_firewall = "\
firewall.@zone[1]=zone
firewall.@zone[1].name='wan'
firewall.@zone[1].network='wan'
firewall.@zone[1].network='wan6'
firewall.@zone[1].network='wan2'
";
        assert!(second_wan_state_ok(
            applied_network,
            applied_firewall,
            "network.@device[0]",
            "firewall.@zone[1]",
            "lan4",
            40
        ));
        // Порт всё ещё в мосту.
        assert!(
            !second_wan_state_ok(SHOW_NETWORK, applied_firewall, "network.@device[0]", "firewall.@zone[1]", "lan4", 40),
            "порт в мосту это не целевое состояние"
        );
        // wan2 живёт на другом порту.
        let wrong_port = applied_network.replace("network.wan2.device='lan4'", "network.wan2.device='lan3'");
        assert!(!second_wan_state_ok(&wrong_port, applied_firewall, "network.@device[0]", "firewall.@zone[1]", "lan4", 40));
        // Метрика чужая.
        let wrong_metric = applied_network.replace("'40'", "'41'");
        assert!(!second_wan_state_ok(&wrong_metric, applied_firewall, "network.@device[0]", "firewall.@zone[1]", "lan4", 40));
        // IPv6 на wan2 открыт.
        let with_v6 = applied_network.replace("network.wan2.ipv6='0'", "network.wan2.ipv6='1'");
        assert!(!second_wan_state_ok(&with_v6, applied_firewall, "network.@device[0]", "firewall.@zone[1]", "lan4", 40));
        // Интерфейс вне wan-зоны.
        assert!(!second_wan_state_ok(applied_network, SHOW_FIREWALL, "network.@device[0]", "firewall.@zone[1]", "lan4", 40));
        // Удаление сошлось: wan2 нет, порт в мосту.
        assert!(second_wan_removed_ok(SHOW_NETWORK, SHOW_FIREWALL, "network.@device[0]", "firewall.@zone[1]", "lan4"));
        assert!(!second_wan_removed_ok(applied_network, applied_firewall, "network.@device[0]", "firewall.@zone[1]", "lan4"));
    }
}
