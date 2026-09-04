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
}
