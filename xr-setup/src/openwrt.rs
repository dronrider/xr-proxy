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

/// Апстримы DNS роутера: dnsmasq переводится на Quad9 мимо резолверов
/// провайдера, как настроен живой флот.
pub const QUAD9: [&str; 2] = ["9.9.9.9", "149.112.112.112"];

pub struct DnsmasqQuad9;

impl Step for DnsmasqQuad9 {
    fn name(&self) -> String {
        "dnsmasq:quad9".into()
    }

    fn check(&self) -> Result<bool> {
        let noresolv = uci_get("dhcp.@dnsmasq[0].noresolv").as_deref() == Some("1");
        let servers = uci_get("dhcp.@dnsmasq[0].server").unwrap_or_default();
        Ok(noresolv && QUAD9.iter().all(|ip| servers.split_whitespace().any(|s| s == *ip)))
    }

    fn apply(&self) -> Result<()> {
        run_cmd(&["uci", "set", "dhcp.@dnsmasq[0].noresolv=1"])?;
        // Список переписывается целиком: старые апстримы провайдера в нём
        // и есть то, от чего уходим.
        let _ = run_cmd(&["uci", "-q", "delete", "dhcp.@dnsmasq[0].server"]);
        for ip in QUAD9 {
            run_cmd(&["uci", "add_list", &format!("dhcp.@dnsmasq[0].server={ip}")])?;
        }
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
}
