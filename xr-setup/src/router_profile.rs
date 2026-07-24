//! Router-профиль (LLD-13 п. 2.1): свежий OpenWRT приводится к раздающему
//! обход роутеру: xr-client с пулом серверов, procd с watchdog, dnsmasq на
//! Quad9, опционально enroll в реестр хаба (шов с LLD-17) и смена SSID
//! отложенным последним шагом.

use crate::actions::{InstallBinary, InstallScript, Restart, Sysctl, WriteConfig};
use crate::arch::Arch;
use crate::fetch::BinSource;
use crate::openwrt::{DnsmasqQuad9, ProcdService, WifiSsid};
use crate::render::{
    render_control_section, render_router_toml, RouterTomlParams, KILLSWITCH_CLEANUP,
    KILLSWITCH_SETUP, ROUTER_INIT, ROUTER_SYSCTL_CONF, ROUTER_WATCHDOG, UDP_TPROXY_CLEANUP,
    UDP_TPROXY_SETUP,
};
use crate::steps::Step;
use anyhow::{bail, Context, Result};
use base64::Engine;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const CLIENT_BIN: &str = "/usr/bin/xr-client";
const CLIENT_CONF: &str = "/etc/xr-proxy/config.toml";
const INIT_PATH: &str = "/etc/init.d/xr-proxy";
/// START=99 в init-скрипте.
const RC_LINK: &str = "/etc/rc.d/S99xr-proxy";
const SYSCTL_FILE: &str = "/etc/sysctl.d/99-xr-proxy.conf";

pub struct RouterOpts {
    /// addr:port в порядке приоритета (первый = primary).
    pub servers: Vec<String>,
    pub key: String,
    pub salt: u32,
    pub hub_url: Option<String>,
    pub preset: String,
    pub enroll_token: Option<String>,
    pub name: Option<String>,
    pub ssid: Option<String>,
    pub wifi_pass: Option<String>,
    pub source: Option<BinSource>,
    pub force: bool,
}

pub struct Resolved {
    pub arch: Arch,
    pub source: Option<Arc<BinSource>>,
    pub force: bool,
    pub config: String,
    pub enroll: Option<EnrollParams>,
    pub ssid: Option<(String, Option<String>)>,
}

pub struct EnrollParams {
    pub hub_url: String,
    pub token: String,
    pub name: String,
}

pub fn resolve(opts: RouterOpts) -> Result<Resolved> {
    let arch = crate::arch::detect()?;
    base64::engine::general_purpose::STANDARD
        .decode(&opts.key)
        .context("--key должен быть валидным base64")?;

    let params = RouterTomlParams {
        servers: opts
            .servers
            .iter()
            .map(|s| parse_server(s))
            .collect::<Result<_>>()?,
        key: opts.key,
        salt: opts.salt,
        hub: opts.hub_url.clone().map(|url| (url, opts.preset)),
    };
    let mut config = render_router_toml(&params);
    match std::fs::read_to_string(CLIENT_CONF) {
        Ok(_) if !opts.force => {
            println!(
                "  внимание: {CLIENT_CONF} уже существует и останется как есть (нужен --force)"
            );
        }
        // Перегенерация под --force не должна стирать выданную реестром
        // идентичность: повторный enroll одноразовым токеном невозможен.
        Ok(old) => {
            if let Some(control) = carry_control(&old) {
                config.push_str(&control);
            }
        }
        Err(_) => {}
    }

    let enroll = match (&opts.enroll_token, &opts.hub_url) {
        (Some(token), Some(url)) => Some(EnrollParams {
            hub_url: url.trim_end_matches('/').to_string(),
            token: token.clone(),
            name: match opts.name {
                Some(n) => n,
                None => hostname(),
            },
        }),
        (Some(_), None) => bail!("--enroll-token требует --hub-url"),
        _ => None,
    };

    Ok(Resolved {
        arch,
        source: opts.source.map(Arc::new),
        force: opts.force,
        config,
        enroll,
        ssid: opts.ssid.map(|s| (s, opts.wifi_pass)),
    })
}

/// Перенести `[control]` из старого конфига в новый как есть. Секция
/// выдаётся хабом при enroll, из флагов её не восстановить.
pub fn carry_control(old_config: &str) -> Option<String> {
    let v: toml::Value = old_config.parse().ok()?;
    let c = v.get("control")?;
    let field = |k: &str| c.get(k).and_then(|x| x.as_str()).map(str::to_string);
    Some(render_control_section(
        &field("hub_url")?,
        &field("router_id")?,
        &field("secret")?,
        &field("command_pubkey")?,
    ))
}

pub fn plan(r: &Resolved) -> Vec<Box<dyn Step>> {
    let restart = Some(Restart::Initd(PathBuf::from(INIT_PATH)));
    let mut steps: Vec<Box<dyn Step>> = vec![
        Box::new(InstallBinary {
            file: format!("xr-client-{}", r.arch.dist_suffix()),
            dest: PathBuf::from(CLIENT_BIN),
            source: r.source.clone(),
            restart: restart.clone(),
        }),
        Box::new(WriteConfig {
            label: "client".into(),
            path: PathBuf::from(CLIENT_CONF),
            content: r.config.clone(),
            mode: 0o600,
            overwrite: r.force,
            restart,
            extra: None,
        }),
        Box::new(InstallScript {
            label: "watchdog".into(),
            path: PathBuf::from("/usr/bin/xr-watchdog.sh"),
            content: ROUTER_WATCHDOG.into(),
        }),
        Box::new(InstallScript {
            label: "killswitch-setup".into(),
            path: PathBuf::from("/usr/bin/killswitch-setup.sh"),
            content: KILLSWITCH_SETUP.into(),
        }),
        Box::new(InstallScript {
            label: "killswitch-cleanup".into(),
            path: PathBuf::from("/usr/bin/killswitch-cleanup.sh"),
            content: KILLSWITCH_CLEANUP.into(),
        }),
        Box::new(InstallScript {
            label: "udp-tproxy-setup".into(),
            path: PathBuf::from("/usr/bin/udp-tproxy-setup.sh"),
            content: UDP_TPROXY_SETUP.into(),
        }),
        Box::new(InstallScript {
            label: "udp-tproxy-cleanup".into(),
            path: PathBuf::from("/usr/bin/udp-tproxy-cleanup.sh"),
            content: UDP_TPROXY_CLEANUP.into(),
        }),
        Box::new(Sysctl {
            path: PathBuf::from(SYSCTL_FILE),
            content: ROUTER_SYSCTL_CONF.into(),
        }),
        Box::new(ProcdService {
            init_path: PathBuf::from(INIT_PATH),
            content: ROUTER_INIT.into(),
            rc_link: PathBuf::from(RC_LINK),
        }),
    ];

    // Enroll после старта клиента (LLD-13 п. 2.1) и после dnsmasq: имя
    // хаба резолвится уже через Quad9, а не через резолверы провайдера.
    // SSID строго последним (п. 5.9): смена сети не должна оборвать ни
    // один шаг после себя.
    steps.push(Box::new(DnsmasqQuad9));
    if let Some(e) = &r.enroll {
        steps.push(Box::new(EnrollStep {
            config_path: PathBuf::from(CLIENT_CONF),
            hub_url: e.hub_url.clone(),
            token: e.token.clone(),
            name: e.name.clone(),
            arch: r.arch,
        }));
    }
    if let Some((ssid, pass)) = &r.ssid {
        steps.push(Box::new(WifiSsid {
            ssid: ssid.clone(),
            pass: pass.clone(),
        }));
    }
    steps
}

/// Финал: убедиться, что роутер реально раздаёт обход (LLD-13 п. 5.6):
/// процесс жив, nftables-перехват стоит, DNS отвечает.
pub fn finish(r: &Resolved) -> Result<()> {
    println!();
    wait_ok("процесс xr-client", &["pidof", "xr-client"])?;
    wait_ok("nftables-перехват (ip xr_proxy)", &["nft", "list", "table", "ip", "xr_proxy"])?;
    wait_ok("DNS через dnsmasq", &["nslookup", "openwrt.org", "127.0.0.1"])?;

    println!();
    println!("Роутер настроен, LAN-трафик идёт по правилам пресета.");
    if r.enroll.is_some() {
        println!("Роутер зарегистрирован на хабе, секция [control] в конфиге.");
    }
    if let Some((ssid, _)) = &r.ssid {
        println!("Wi-Fi переименуется в '{ssid}' через несколько секунд, переподключись к нему.");
    }
    Ok(())
}

/// Подождать состояние с ретраями: клиенту после старта нужно несколько
/// секунд, чтобы поставить nftables-правила.
fn wait_ok(label: &str, argv: &[&str]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if crate::actions::cmd_ok(argv) {
            println!("  [v] {label}");
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("проверка не прошла: {label} (команда {})", argv.join(" "));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn hostname() -> String {
    std::process::Command::new("uname")
        .arg("-n")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "openwrt".into())
}

/// Разбор `addr:port` из --server; IPv6 в скобках, `[::1]:8443`.
pub fn parse_server(s: &str) -> Result<crate::render::RouterServer> {
    let (address, port) = if let Some(rest) = s.strip_prefix('[') {
        rest.split_once("]:")
            .with_context(|| format!("--server '{s}': ожидается [addr6]:port"))?
    } else {
        let (a, p) = s
            .rsplit_once(':')
            .with_context(|| format!("--server '{s}': ожидается addr:port"))?;
        if a.contains(':') {
            bail!("--server '{s}': IPv6 задаётся в скобках, [addr6]:port");
        }
        (a, p)
    };
    Ok(crate::render::RouterServer {
        address: address.to_string(),
        port: port
            .parse()
            .with_context(|| format!("--server '{s}': порт не число"))?,
    })
}

#[derive(Debug, Deserialize)]
pub struct EnrollResponse {
    pub router_id: String,
    pub secret: String,
    pub command_pubkey: String,
}

/// Регистрация роутера в реестре хаба одноразовым токеном (LLD-17 п. 2.1)
/// и запись `[control]` в конфиг. Хаб без реестра (до XR-025) ответит
/// ошибкой, и шаг честно упадёт; установка без --enroll-token его не имеет.
struct EnrollStep {
    config_path: PathBuf,
    hub_url: String,
    token: String,
    name: String,
    arch: Arch,
}

impl Step for EnrollStep {
    fn name(&self) -> String {
        "hub:enroll".into()
    }

    fn check(&self) -> Result<bool> {
        let Ok(text) = std::fs::read_to_string(&self.config_path) else {
            return Ok(false);
        };
        Ok(has_control_section(&text))
    }

    fn apply(&self) -> Result<()> {
        let resp: EnrollResponse = ureq::post(&format!("{}/api/v1/enroll", self.hub_url))
            .timeout(Duration::from_secs(15))
            .send_json(ureq::json!({
                "token": self.token,
                "name": self.name,
                "arch": self.arch.dist_suffix(),
                "version": env!("CARGO_PKG_VERSION"),
            }))
            .context("enroll на хабе (реестр роутеров, XR-025)")?
            .into_json()
            .context("разбор ответа enroll")?;

        let mut text = std::fs::read_to_string(&self.config_path)
            .with_context(|| format!("чтение {}", self.config_path.display()))?;
        text.push_str(&render_control_section(
            &self.hub_url,
            &resp.router_id,
            &resp.secret,
            &resp.command_pubkey,
        ));
        crate::actions::write_atomic(&self.config_path, text.as_bytes(), 0o600)?;
        Restart::Initd(PathBuf::from(INIT_PATH)).kick()
    }
}

/// Есть ли уже `[control]` в конфиге; TOML-разбором, а не поиском строки,
/// чтобы упоминание в комментарии не сошло за секцию.
pub fn has_control_section(config: &str) -> bool {
    config
        .parse::<toml::Value>()
        .map(|v| v.get("control").is_some())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::RouterServer;

    fn resolved(enroll: bool, ssid: bool) -> Resolved {
        Resolved {
            arch: Arch::Aarch64,
            source: None,
            force: false,
            config: render_router_toml(&RouterTomlParams {
                servers: vec![
                    RouterServer { address: "203.0.113.1".into(), port: 8443 },
                    RouterServer { address: "203.0.113.2".into(), port: 8443 },
                ],
                key: "QUJD".into(),
                salt: 0x1234_5678,
                hub: Some(("https://hub.test".into(), "russia".into())),
            }),
            enroll: enroll.then(|| EnrollParams {
                hub_url: "https://hub.test".into(),
                token: "tok".into(),
                name: "router-de".into(),
            }),
            ssid: ssid.then(|| ("HomeNet".to_string(), None)),
        }
    }

    fn names(r: &Resolved) -> Vec<String> {
        plan(r).iter().map(|s| s.name()).collect()
    }

    #[test]
    fn router_plan_base_steps_and_order() {
        assert_eq!(
            names(&resolved(false, false)),
            [
                "binary:xr-client",
                "config:client",
                "script:watchdog",
                "script:killswitch-setup",
                "script:killswitch-cleanup",
                "script:udp-tproxy-setup",
                "script:udp-tproxy-cleanup",
                "sysctl",
                "service:xr-proxy",
                "dnsmasq:quad9",
            ]
        );
    }

    #[test]
    fn enroll_goes_after_service_and_dns_and_ssid_is_last() {
        let names = names(&resolved(true, true));
        let pos = |n: &str| names.iter().position(|x| x == n).unwrap();
        assert!(pos("hub:enroll") > pos("service:xr-proxy"), "enroll после старта клиента");
        assert!(pos("hub:enroll") > pos("dnsmasq:quad9"), "имя хаба резолвится уже через Quad9");
        assert_eq!(names.last().unwrap(), "wifi:ssid", "смена SSID строго последняя");
    }

    #[test]
    fn rendered_config_parses_as_client_pool() {
        let r = resolved(false, false);
        let cfg: xr_proto::config::ClientConfig = toml::from_str(&r.config).unwrap();
        let entries = cfg.server_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].address, "203.0.113.1");
        assert_eq!(entries[0].priority, 0, "первый --server это primary");
        assert_eq!(entries[1].priority, 1);
        assert_eq!(cfg.obfuscation.key, "QUJD");
        assert_eq!(cfg.obfuscation.salt, 0x1234_5678);
        let hub = cfg.hub.expect("секция [hub]");
        assert_eq!(hub.url, "https://hub.test");
        assert_eq!(hub.preset, "russia");
        assert!(cfg.client.auto_redirect, "nftables ставит сам xr-client");
        assert!(cfg.client.block_quic);
    }

    #[test]
    fn config_without_hub_omits_section() {
        let cfg = render_router_toml(&RouterTomlParams {
            servers: vec![RouterServer { address: "203.0.113.1".into(), port: 8443 }],
            key: "QUJD".into(),
            salt: 1,
            hub: None,
        });
        let parsed: xr_proto::config::ClientConfig = toml::from_str(&cfg).unwrap();
        assert!(parsed.hub.is_none());
    }

    #[test]
    fn parses_server_addr_and_rejects_garbage() {
        let s = parse_server("203.0.113.1:8443").unwrap();
        assert_eq!((s.address.as_str(), s.port), ("203.0.113.1", 8443));
        assert!(parse_server("203.0.113.1").is_err());
        assert!(parse_server("host:port").is_err());
        let v6 = parse_server("[2001:db8::7]:8443").unwrap();
        assert_eq!((v6.address.as_str(), v6.port), ("2001:db8::7", 8443));
        assert!(parse_server("2001:db8::7:8443").is_err(), "голый IPv6 неоднозначен");
        assert!(parse_server("[2001:db8::7]").is_err());
    }

    #[test]
    fn force_carries_control_section_over() {
        let old = format!(
            "{}{}",
            render_router_toml(&RouterTomlParams {
                servers: vec![RouterServer { address: "203.0.113.9".into(), port: 8443 }],
                key: "QUJD".into(),
                salt: 7,
                hub: None,
            }),
            render_control_section("https://hub.test", "r1", "s3cr3t", "cGs=")
        );
        let carried = carry_control(&old).expect("секция должна переехать");
        assert!(has_control_section(&format!("[obfuscation]\nkey='k'\n{carried}")));
        assert!(carried.contains("router_id = \"r1\""));
        assert!(carried.contains("secret = \"s3cr3t\""));

        assert!(carry_control("[client]\nlisten_port = 1080\n").is_none());
        assert!(carry_control("[control]\nhub_url = \"x\"\n").is_none(), "неполная секция не переносится");
    }

    #[test]
    fn control_section_roundtrip() {
        let r = resolved(false, false);
        assert!(!has_control_section(&r.config));
        let with_control = format!(
            "{}{}",
            r.config,
            render_control_section("https://hub.test", "r1", "s3cr3t", "cGs=")
        );
        assert!(has_control_section(&with_control));
        // Клиент сегодняшнего формата обязан пережить незнакомую секцию.
        let cfg: xr_proto::config::ClientConfig = toml::from_str(&with_control).unwrap();
        assert!(cfg.server_entries().is_ok());
    }

    #[test]
    fn enroll_response_parses() {
        let resp: EnrollResponse = serde_json::from_str(
            r#"{"router_id":"r-1","secret":"s","command_pubkey":"cGs=","extra":1}"#,
        )
        .unwrap();
        assert_eq!(resp.router_id, "r-1");
    }
}
