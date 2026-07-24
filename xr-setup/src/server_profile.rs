//! Server-профиль (LLD-13 п. 2.1): чистый VPS приводится к работающему
//! xr-server, при --with-hub рядом встаёт xr-hub, и установка заканчивается
//! одноразовым инвайтом - швом с онбордингом LLD-04.

use crate::actions::{InstallBinary, SigningKey, Sysctl, SystemdUnit, WriteConfig};
use crate::arch::Arch;
use crate::fetch::BinSource;
use crate::hub_api::HubClient;
use crate::render::{
    render_hub_toml, render_server_toml, HubTomlParams, ServerTomlParams, HUB_UNIT, SERVER_UNIT,
    SYSCTL_CONF,
};
use crate::secrets;
use crate::steps::Step;
use anyhow::{Context, Result};
use base64::Engine;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const SERVER_BIN: &str = "/usr/local/bin/xr-server";
const HUB_BIN: &str = "/usr/local/bin/xr-hub";
const SERVER_CONF: &str = "/etc/xr-proxy/server.toml";
const HUB_CONF: &str = "/etc/xr-hub/config.toml";
const ADMIN_PASS_FILE: &str = "/etc/xr-hub/admin.pass";
const SIGNING_KEY_FILE: &str = "/var/lib/xr-hub/signing.key";
const SYSCTL_FILE: &str = "/etc/sysctl.d/99-xr-proxy.conf";
const SERVER_UNIT_NAME: &str = "xr-proxy-server";
const HUB_UNIT_NAME: &str = "xr-hub";

pub struct ServerOpts {
    pub with_hub: bool,
    pub hub_domain: Option<String>,
    pub key: Option<String>,
    pub server_addr: Option<String>,
    pub port: u16,
    pub source: Option<BinSource>,
    pub admin_pass: Option<String>,
    pub force: bool,
}

/// План хаба: свежий конфиг рендерится, существующий не трогается
/// (пароль в нём не восстановить из хеша).
pub struct HubPlan {
    pub fresh_conf: Option<String>,
    /// База локальных запросов к хабу (логин, минт инвайта).
    pub local_base: String,
    pub public_hub_url: String,
    pub admin_user: String,
    pub admin_pass: Option<String>,
    pub pass_generated: bool,
}

pub struct Resolved {
    pub arch: Arch,
    pub source: Option<Arc<BinSource>>,
    pub force: bool,
    pub server: ServerTomlParams,
    pub hub: Option<HubPlan>,
}

pub fn resolve(opts: ServerOpts) -> Result<Resolved> {
    let arch = crate::arch::detect()?;

    let server = match std::fs::read_to_string(SERVER_CONF) {
        Ok(text) if !opts.force => parse_server_conf(&text)
            .with_context(|| format!("разбор существующего {SERVER_CONF}"))?,
        _ => {
            let key = match &opts.key {
                Some(k) => validated_key(k)?,
                None => secrets::gen_obfuscation_key(),
            };
            ServerTomlParams {
                port: opts.port,
                key,
                salt: secrets::gen_salt(),
            }
        }
    };

    let hub = if opts.with_hub {
        Some(resolve_hub(&opts, &server)?)
    } else {
        None
    };

    Ok(Resolved {
        arch,
        source: opts.source.map(Arc::new),
        force: opts.force,
        server,
        hub,
    })
}

fn resolve_hub(opts: &ServerOpts, server: &ServerTomlParams) -> Result<HubPlan> {
    let domain = opts
        .hub_domain
        .as_deref()
        .context("--with-hub требует --hub-domain")?;

    if let Ok(text) = std::fs::read_to_string(HUB_CONF) {
        if !opts.force {
            let parsed: toml::Value = text.parse().context("разбор существующего конфига хаба")?;
            let bind = parsed
                .get("server")
                .and_then(|s| s.get("bind"))
                .and_then(|b| b.as_str())
                .unwrap_or("0.0.0.0:8080")
                .to_string();
            let admin_user = parsed
                .get("admin")
                .and_then(|a| a.get("users"))
                .and_then(|u| u.as_array())
                .and_then(|u| u.first())
                .and_then(|u| u.get("username"))
                .and_then(|n| n.as_str())
                .unwrap_or("admin")
                .to_string();
            let public_hub_url = parsed
                .get("invites")
                .and_then(|i| i.get("defaults"))
                .and_then(|d| d.get("hub_url"))
                .and_then(|u| u.as_str())
                .filter(|u| !u.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("https://{domain}"));
            let admin_pass = opts.admin_pass.clone().or_else(|| {
                std::fs::read_to_string(ADMIN_PASS_FILE)
                    .ok()
                    .map(|p| p.trim().to_string())
            });
            return Ok(HubPlan {
                fresh_conf: None,
                local_base: local_base_from_bind(&bind),
                public_hub_url,
                admin_user,
                admin_pass,
                pass_generated: false,
            });
        }
    }

    let pass_generated = opts.admin_pass.is_none();
    let admin_pass = opts.admin_pass.clone().unwrap_or_else(secrets::gen_password);
    let public_hub_url = format!("https://{domain}");
    let bind = "127.0.0.1:8080".to_string();
    let params = HubTomlParams {
        bind: bind.clone(),
        admin_user: "admin".into(),
        password_hash: secrets::hash_password(&admin_pass)?,
        signing_key_path: SIGNING_KEY_FILE.into(),
        server_addr: resolve_server_addr(opts)?,
        server_port: server.port,
        obfuscation_key: server.key.clone(),
        salt: server.salt,
        hub_url: public_hub_url.clone(),
    };
    Ok(HubPlan {
        fresh_conf: Some(render_hub_toml(&params)),
        local_base: local_base_from_bind(&bind),
        public_hub_url,
        admin_user: "admin".into(),
        admin_pass: Some(admin_pass),
        pass_generated,
    })
}

pub fn plan(r: &Resolved) -> Vec<Box<dyn Step>> {
    let mut steps: Vec<Box<dyn Step>> = vec![
        Box::new(InstallBinary {
            file: format!("xr-server-{}", r.arch.dist_suffix()),
            dest: PathBuf::from(SERVER_BIN),
            source: r.source.clone(),
            restart_unit: Some(SERVER_UNIT_NAME.into()),
        }),
        Box::new(WriteConfig {
            label: "server".into(),
            path: PathBuf::from(SERVER_CONF),
            content: render_server_toml(&r.server),
            mode: 0o600,
            overwrite: r.force,
            restart_unit: Some(SERVER_UNIT_NAME.into()),
            extra: None,
        }),
        Box::new(Sysctl {
            path: PathBuf::from(SYSCTL_FILE),
            content: SYSCTL_CONF.into(),
        }),
        Box::new(SystemdUnit {
            unit: SERVER_UNIT_NAME.into(),
            content: SERVER_UNIT.into(),
        }),
    ];

    if let Some(hub) = &r.hub {
        steps.push(Box::new(InstallBinary {
            file: format!("xr-hub-{}", r.arch.dist_suffix()),
            dest: PathBuf::from(HUB_BIN),
            source: r.source.clone(),
            restart_unit: Some(HUB_UNIT_NAME.into()),
        }));
        steps.push(Box::new(SigningKey {
            path: PathBuf::from(SIGNING_KEY_FILE),
        }));
        if let Some(content) = &hub.fresh_conf {
            steps.push(Box::new(WriteConfig {
                label: "hub".into(),
                path: PathBuf::from(HUB_CONF),
                content: content.clone(),
                mode: 0o600,
                overwrite: r.force,
                restart_unit: Some(HUB_UNIT_NAME.into()),
                extra: hub
                    .admin_pass
                    .as_ref()
                    .map(|p| (PathBuf::from(ADMIN_PASS_FILE), format!("{p}\n"), 0o600)),
            }));
        }
        steps.push(Box::new(SystemdUnit {
            unit: HUB_UNIT_NAME.into(),
            content: HUB_UNIT.into(),
        }));
    }

    steps
}

/// Финал установки: инвайт через хаб (LLD-13 п. 3.5) либо, без хаба,
/// параметры подключения для инвайта на внешнем хабе.
pub fn finish(r: &Resolved) -> Result<()> {
    println!();
    let Some(hub) = &r.hub else {
        println!("Сервер работает: порт {}.", r.server.port);
        println!("Хаб не ставился (--with-hub не задан), инвайт выдать некому.");
        println!("Параметры для инвайта на своём хабе:");
        println!("  obfuscation_key = {}", r.server.key);
        println!("  salt = 0x{:08X}", r.server.salt);
        return Ok(());
    };

    let Some(pass) = &hub.admin_pass else {
        println!("Конфиг хаба уже существовал, пароль админа неизвестен: инвайт не выдан.");
        println!("Передай --admin-pass или положи пароль в {ADMIN_PASS_FILE} и запусти снова.");
        return Ok(());
    };

    let client = HubClient::new(&hub.local_base);
    client.wait_ready(Duration::from_secs(20))?;
    let session = client
        .login(&hub.admin_user, pass)
        .context("логин в свежепоставленный хаб")?;
    let invite = client.create_invite(&session, "xr-setup")?;

    println!("Готово. Одноразовый инвайт для приложения:");
    println!(
        "  {}",
        xr_proto::invite_url::build_https_url(&hub.public_hub_url, &invite.token)
    );
    println!(
        "  {}",
        xr_proto::invite_url::build_custom_url(&hub.public_hub_url, &invite.token)
    );
    println!("  истекает: {}", invite.expires_at);
    if hub.pass_generated {
        println!();
        println!("Пароль админа хаба (сгенерирован, лежит в {ADMIN_PASS_FILE}):");
        println!("  {} / {pass}", hub.admin_user);
    }
    println!();
    println!("Хаб слушает {}: наружу его выводит nginx с TLS,", hub.local_base);
    println!("это ручной шаг (docs/HUB-DEPLOY.md). Ссылка инвайта откроется");
    println!("у получателя после него.");
    Ok(())
}

fn validated_key(key: &str) -> Result<String> {
    base64::engine::general_purpose::STANDARD
        .decode(key)
        .context("--key должен быть валидным base64")?;
    Ok(key.to_string())
}

/// Разбор существующего server.toml: ключ и salt не перегенерируются
/// (LLD-13 п. 5.1), инвайты продолжают нести рабочие значения.
pub fn parse_server_conf(text: &str) -> Result<ServerTomlParams> {
    let v: toml::Value = text.parse().context("не TOML")?;
    let obf = v.get("obfuscation").context("нет секции [obfuscation]")?;
    let key = obf
        .get("key")
        .and_then(|k| k.as_str())
        .context("нет obfuscation.key")?
        .to_string();
    let salt = obf
        .get("salt")
        .and_then(|s| s.as_integer())
        .context("нет obfuscation.salt")? as u32;
    let port = v
        .get("server")
        .and_then(|s| s.get("port"))
        .and_then(|p| p.as_integer())
        .unwrap_or(8443) as u16;
    Ok(ServerTomlParams { port, key, salt })
}

fn local_base_from_bind(bind: &str) -> String {
    let port = bind.rsplit(':').next().unwrap_or("8080");
    format!("http://127.0.0.1:{port}")
}

fn resolve_server_addr(opts: &ServerOpts) -> Result<String> {
    if let Some(addr) = &opts.server_addr {
        return Ok(addr.clone());
    }
    let out = std::process::Command::new("hostname")
        .arg("-I")
        .output()
        .context("hostname -I")?;
    pick_public_ip(&String::from_utf8_lossy(&out.stdout)).context(
        "не нашёл публичный адрес этого VPS, задай --server-addr (IP или домен для инвайтов)",
    )
}

/// Первый публичный адрес из вывода `hostname -I`; приватные, loopback и
/// link-local не годятся, до них клиенты снаружи не достучатся.
pub fn pick_public_ip(hostname_i: &str) -> Option<String> {
    use std::net::IpAddr;
    hostname_i
        .split_whitespace()
        .filter_map(|s| s.parse::<IpAddr>().ok())
        .find(|ip| match ip {
            IpAddr::V4(v4) => {
                !v4.is_private() && !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified()
            }
            IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_unspecified(),
        })
        .map(|ip| ip.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(with_hub: bool, force: bool) -> Resolved {
        let server = ServerTomlParams {
            port: 8443,
            key: "QUJD".into(),
            salt: 0x1234_5678,
        };
        let hub = with_hub.then(|| HubPlan {
            fresh_conf: Some("bind".into()),
            local_base: "http://127.0.0.1:8080".into(),
            public_hub_url: "https://hub.test".into(),
            admin_user: "admin".into(),
            admin_pass: Some("pass".into()),
            pass_generated: true,
        });
        Resolved {
            arch: Arch::X86_64,
            source: None,
            force,
            server,
            hub,
        }
    }

    fn names(r: &Resolved) -> Vec<String> {
        plan(r).iter().map(|s| s.name()).collect()
    }

    #[test]
    fn server_plan_without_hub() {
        assert_eq!(
            names(&resolved(false, false)),
            ["binary:xr-server", "config:server", "sysctl", "service:xr-proxy-server"]
        );
    }

    #[test]
    fn server_plan_with_hub_appends_hub_steps() {
        assert_eq!(
            names(&resolved(true, false)),
            [
                "binary:xr-server",
                "config:server",
                "sysctl",
                "service:xr-proxy-server",
                "binary:xr-hub",
                "hub:signing-key",
                "config:hub",
                "service:xr-hub"
            ]
        );
    }

    #[test]
    fn existing_hub_conf_is_not_rewritten() {
        let mut r = resolved(true, false);
        r.hub.as_mut().unwrap().fresh_conf = None;
        assert!(
            !names(&r).contains(&"config:hub".to_string()),
            "существующий конфиг хаба не трогаем: хеш пароля не восстановить"
        );
    }

    #[test]
    fn parse_roundtrips_with_render() {
        let params = ServerTomlParams {
            port: 9000,
            key: "a2V5LWJ5dGVz".into(),
            salt: 0xDEADBEEF,
        };
        let parsed = parse_server_conf(&render_server_toml(&params)).unwrap();
        assert_eq!(parsed.port, 9000);
        assert_eq!(parsed.key, params.key);
        assert_eq!(parsed.salt, 0xDEADBEEF);
    }

    #[test]
    fn picks_first_public_ip() {
        assert_eq!(
            pick_public_ip("127.0.0.1 192.168.1.5 203.0.113.7 10.0.0.2\n").as_deref(),
            Some("203.0.113.7")
        );
        assert_eq!(pick_public_ip("127.0.0.1 10.0.0.2"), None);
        assert_eq!(pick_public_ip(""), None);
    }

    #[test]
    fn local_base_follows_bind_port() {
        assert_eq!(local_base_from_bind("127.0.0.1:8080"), "http://127.0.0.1:8080");
        assert_eq!(local_base_from_bind("0.0.0.0:9090"), "http://127.0.0.1:9090");
    }
}
