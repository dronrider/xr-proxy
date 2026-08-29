//! Server-профиль (LLD-13 п. 2.1): чистый VPS приводится к работающему
//! xr-server, при --with-hub рядом встаёт xr-hub, и установка заканчивается
//! одноразовым инвайтом - швом с онбордингом LLD-04.

use crate::actions::{
    AppendBlock, HubBackup, InstallBinary, Restart, ServiceAlert, SigningKey, Sysctl, SystemdUnit,
    WriteConfig,
};
use crate::arch::Arch;
use crate::fetch::BinSource;
use crate::hub_api::HubClient;
use crate::render::{
    render_hub_toml, render_hub_web_block, render_server_toml, render_web_toml, HubTomlParams,
    ServerTomlParams, WebTomlParams, HUB_BACKUP, HUB_BACKUP_CRON, HUB_BACKUP_ENV, HUB_UNIT,
    SERVER_UNIT, SERVICE_ALERT, SERVICE_ALERT_CRON, SYSCTL_CONF, WEB_UNIT,
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
const HUB_BACKUP_BIN: &str = "/usr/local/bin/xr-hub-backup.sh";
const HUB_BACKUP_CRON_FILE: &str = "/etc/cron.d/xr-hub-backup";
const HUB_BACKUP_ENV_FILE: &str = "/etc/xr-hub/backup.env";
const SERVICE_ALERT_BIN: &str = "/usr/local/bin/xr-service-alert.sh";
const SERVICE_ALERT_CRON_FILE: &str = "/etc/cron.d/xr-service-alert";
/// Тот же env, что у cert-alert.sh: токен один на все алерты машины.
const ALERT_ENV_FILE: &str = "/etc/xr-proxy/alert.env";
const WEB_BIN: &str = "/usr/local/bin/xr-web";
const WEB_CONF: &str = "/etc/xr-web/config.toml";
const WEB_UNIT_NAME: &str = "xr-web";
/// Локальный порт браузерного входа: наружу его выводит тот же фронт, что и
/// хаб, разводя их по `Host`.
const WEB_BIND: &str = "127.0.0.1:8090";
const SYSCTL_FILE: &str = "/etc/sysctl.d/99-xr-proxy.conf";
const SERVER_UNIT_NAME: &str = "xr-proxy-server";
const HUB_UNIT_NAME: &str = "xr-hub";

pub struct ServerOpts {
    pub with_hub: bool,
    pub hub_domain: Option<String>,
    /// Поставить рядом браузерный вход (LLD-38 фаза 2).
    pub with_web: bool,
    /// Домен публикаций: `<имя>.<домен>`. В коде его нет, он приходит сюда.
    pub web_domain: Option<String>,
    /// База хаба для фронта, если хаб живёт не на этой машине.
    pub hub_url: Option<String>,
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
    /// В существующем конфиге включён hub-native TLS: локальный минт
    /// инвайта по plain HTTP туда не пройдёт.
    pub native_tls: bool,
}

/// План браузерного входа: домен, база хаба и общий секрет, который обязан
/// совпасть с хабовым. Секрет генерируется один раз и кладётся в оба конфига,
/// поэтому руками его переносить не приходится (LLD-38 п. 2.5).
pub struct WebPlan {
    pub domain: String,
    pub hub_url: String,
    pub shared_secret: String,
    /// В конфигах фронта и хаба уже стоят разные секреты: сам установщик
    /// такое не чинит (перезаписывать чужие конфиги он не вправе), но и молчать
    /// об этом нельзя.
    pub secret_conflict: bool,
    /// Конфиг хаба лежит на этой машине: блок `[web]` дописывается сюда.
    pub hub_conf_here: bool,
}

pub struct Resolved {
    pub arch: Arch,
    pub source: Option<Arc<BinSource>>,
    pub force: bool,
    pub server: ServerTomlParams,
    pub hub: Option<HubPlan>,
    pub web: Option<WebPlan>,
}

pub fn resolve(opts: ServerOpts) -> Result<Resolved> {
    let arch = crate::arch::detect()?;

    let server = match std::fs::read_to_string(SERVER_CONF) {
        Ok(text) if !opts.force => {
            let parsed = parse_server_conf(&text)
                .with_context(|| format!("разбор существующего {SERVER_CONF}"))?;
            if opts.key.as_deref().is_some_and(|k| k != parsed.key) {
                println!("  внимание: {SERVER_CONF} уже существует, --key проигнорирован (нужен --force)");
            }
            if opts.port != 8443 && opts.port != parsed.port {
                println!("  внимание: {SERVER_CONF} уже существует, --port проигнорирован (нужен --force)");
            }
            parsed
        }
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

    let web = if opts.with_web {
        Some(resolve_web(&opts, hub.as_ref())?)
    } else {
        None
    };

    Ok(Resolved {
        arch,
        source: opts.source.map(Arc::new),
        force: opts.force,
        server,
        hub,
        web,
    })
}

fn resolve_web(opts: &ServerOpts, hub: Option<&HubPlan>) -> Result<WebPlan> {
    let domain = opts
        .web_domain
        .as_deref()
        .context("--with-web требует --web-domain")?
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    if domain.is_empty() {
        anyhow::bail!("--web-domain пуст: публикация живёт на <имя>.<домен>");
    }

    let hub_url = match (&opts.hub_url, hub) {
        (Some(url), _) => url.clone(),
        // Хаб на этой же машине: фронт ходит в него локально, TLS и фронт на
        // этом пути ни к чему.
        (None, Some(plan)) => plan.local_base.clone(),
        (None, None) => anyhow::bail!("--with-web без --with-hub требует --hub-url"),
    };

    let in_web = shared_secret_of(&std::fs::read_to_string(WEB_CONF).unwrap_or_default());
    let in_hub = shared_secret_of(&std::fs::read_to_string(HUB_CONF).unwrap_or_default());
    let secret_conflict = match (&in_web, &in_hub) {
        (Some(a), Some(b)) => a != b,
        _ => false,
    };
    let shared_secret = in_web
        .or(in_hub)
        .unwrap_or_else(secrets::gen_shared_secret);

    Ok(WebPlan {
        domain,
        hub_url,
        shared_secret,
        secret_conflict,
        hub_conf_here: std::path::Path::new(HUB_CONF).exists(),
    })
}

/// Секрет из блока `[web]` конфига (хаба или фронта), если он там уже стоит.
pub fn shared_secret_of(text: &str) -> Option<String> {
    let parsed: toml::Value = text.parse().ok()?;
    let secret = parsed
        .get("web")?
        .get("shared_secret")?
        .as_str()?
        .trim()
        .to_string();
    (!secret.is_empty()).then_some(secret)
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
                native_tls: parsed.get("tls").is_some(),
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
        native_tls: false,
    })
}

pub fn plan(r: &Resolved) -> Vec<Box<dyn Step>> {
    let mut steps: Vec<Box<dyn Step>> = vec![
        Box::new(InstallBinary {
            file: format!("xr-server-{}", r.arch.dist_suffix()),
            dest: PathBuf::from(SERVER_BIN),
            source: r.source.clone(),
            restart: Some(Restart::Unit(SERVER_UNIT_NAME.into())),
        }),
        Box::new(WriteConfig {
            label: "server".into(),
            path: PathBuf::from(SERVER_CONF),
            content: render_server_toml(&r.server),
            mode: 0o600,
            overwrite: r.force,
            restart: Some(Restart::Unit(SERVER_UNIT_NAME.into())),
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
        Box::new(ServiceAlert {
            script: PathBuf::from(SERVICE_ALERT_BIN),
            script_content: SERVICE_ALERT.into(),
            cron: PathBuf::from(SERVICE_ALERT_CRON_FILE),
            cron_content: SERVICE_ALERT_CRON.into(),
        }),
    ];

    if let Some(hub) = &r.hub {
        steps.push(Box::new(InstallBinary {
            file: format!("xr-hub-{}", r.arch.dist_suffix()),
            dest: PathBuf::from(HUB_BIN),
            source: r.source.clone(),
            restart: Some(Restart::Unit(HUB_UNIT_NAME.into())),
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
                restart: Some(Restart::Unit(HUB_UNIT_NAME.into())),
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
        steps.push(Box::new(HubBackup {
            script: PathBuf::from(HUB_BACKUP_BIN),
            script_content: HUB_BACKUP.into(),
            cron: PathBuf::from(HUB_BACKUP_CRON_FILE),
            cron_content: HUB_BACKUP_CRON.into(),
            env: PathBuf::from(HUB_BACKUP_ENV_FILE),
            env_template: HUB_BACKUP_ENV.into(),
        }));
    }

    if let Some(web) = &r.web {
        steps.push(Box::new(InstallBinary {
            file: format!("xr-web-{}", r.arch.dist_suffix()),
            dest: PathBuf::from(WEB_BIN),
            source: r.source.clone(),
            restart: Some(Restart::Unit(WEB_UNIT_NAME.into())),
        }));
        steps.push(Box::new(WriteConfig {
            label: "web".into(),
            path: PathBuf::from(WEB_CONF),
            content: render_web_toml(&WebTomlParams {
                bind: WEB_BIND.into(),
                domain: web.domain.clone(),
                hub_url: web.hub_url.clone(),
                shared_secret: web.shared_secret.clone(),
            }),
            mode: 0o600,
            overwrite: r.force,
            restart: Some(Restart::Unit(WEB_UNIT_NAME.into())),
            extra: None,
        }));
        // Секрет и домен нужны обеим сторонам. Конфиг хаба целиком не
        // перерисовывается (хеш пароля из него не восстановить), поэтому блок
        // дописывается в конец, и только если его там ещё нет.
        if web.hub_conf_here {
            steps.push(Box::new(AppendBlock {
                label: "hub-web".into(),
                path: PathBuf::from(HUB_CONF),
                marker: "[web]".into(),
                block: render_hub_web_block(&web.domain, &web.shared_secret),
                restart: Some(Restart::Unit(HUB_UNIT_NAME.into())),
            }));
        }
        steps.push(Box::new(SystemdUnit {
            unit: WEB_UNIT_NAME.into(),
            content: WEB_UNIT.into(),
        }));
    }

    steps
}

/// Что установщик не может сделать сам за браузерный вход. Каждая находка
/// называет точную команду: молчаливо неработающий вход неотличим от рабочего,
/// пока в него не постучится браузер.
pub fn web_findings(web: &WebPlan) -> Vec<String> {
    let mut out = Vec::new();
    if web.secret_conflict {
        out.push(format!(
            "Находка: в {WEB_CONF} и {HUB_CONF} стоят разные [web] shared_secret.\n  \
             Служебные ручки хаба ответят фронту 401, и вход отдаст 502.\n  \
             Приведи их к одному значению и перезапусти оба:\n  \
             systemctl restart xr-hub xr-web"
        ));
    }
    if !web.hub_conf_here {
        out.push(format!(
            "Находка: хаб не на этой машине, блок [web] дописать некуда.\n  \
             Добавь в его конфиг и перезапусти хаб:{}",
            render_hub_web_block(&web.domain, &web.shared_secret)
                .replace('\n', "\n  ")
        ));
    }
    out.push(format!(
        "Осталось вне установщика: DNS-запись *.{domain} на этот VPS, \
         wildcard-сертификат и правило фронта (*.{domain} -> {bind}).\n  \
         Подробности в docs/HUB-DEPLOY.md, раздел 3. Проверка после этого:\n  \
         curl -s https://<публикация>.{domain}/.xr-web/healthz   # ждём ok",
        domain = web.domain,
        bind = WEB_BIND,
    ));
    out
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
        print_web_findings(r);
        print_alert_env_finding();
        return Ok(());
    };

    let outcome = finish_hub(hub);
    print_web_findings(r);
    if let Some(finding) =
        backup_env_finding(std::fs::read_to_string(HUB_BACKUP_ENV_FILE).ok().as_deref())
    {
        println!();
        println!("{finding}");
    }
    print_alert_env_finding();
    outcome
}

/// Незаполненный alert.env: сторож crash-loop ходит по cron, но слать алерт
/// некуда, и тишину такого сторожа не отличить от здоровых сервисов. Файл
/// общий с cert-alert.sh, на живых VPS он обычно уже стоит.
fn print_alert_env_finding() {
    if let Some(finding) =
        alert_env_finding(std::fs::read_to_string(ALERT_ENV_FILE).ok().as_deref())
    {
        println!();
        println!("{finding}");
    }
}

pub fn alert_env_finding(env: Option<&str>) -> Option<String> {
    let filled = env.is_some_and(|text| {
        ["TG_TOKEN", "TG_CHAT"]
            .iter()
            .all(|key| env_value_set(text, key))
    });
    if filled {
        return None;
    }
    Some(format!(
        "Находка: сторож crash-loop сервисов ходит по cron, но Telegram не настроен.\n  \
         Заполни {ALERT_ENV_FILE} (TG_TOKEN и TG_CHAT, тот же файл, что у cert-alert) и проверь:\n  \
         {SERVICE_ALERT_BIN}"
    ))
}

fn print_web_findings(r: &Resolved) {
    let Some(web) = &r.web else { return };
    println!();
    println!("Браузерный вход: публикации открываются на https://<имя>.{}", web.domain);
    for finding in web_findings(web) {
        println!();
        println!("{finding}");
    }
}

/// Незаполненный backup.env: бэкап хаба собирается по cron, но никуда не
/// уезжает, а тишину в этом случае не отличить от рабочей отправки. Отправка
/// требует обе переменные, поэтому одного адреса мало: иначе установщик
/// молчит, а cron каждый день шлёт алерт.
pub fn backup_env_finding(env: Option<&str>) -> Option<String> {
    let filled = env.is_some_and(|text| {
        ["BACKUP_HOST", "BACKUP_KEY"]
            .iter()
            .all(|key| env_value_set(text, key))
    });
    if filled {
        return None;
    }
    Some(format!(
        "Находка: бэкап состояния хаба собирается по cron, но приёмник не задан.\n  \
         Заполни {HUB_BACKUP_ENV_FILE} (BACKUP_HOST и BACKUP_KEY) и проверь отправку:\n  \
         {HUB_BACKUP_BIN}"
    ))
}

/// Переменная env-файла задана непустым значением. Закомментированная
/// строка не в счёт: болванка установщика состоит как раз из них.
fn env_value_set(text: &str, key: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        !line.starts_with('#')
            && line
                .strip_prefix(key)
                .and_then(|rest| rest.trim_start().strip_prefix('='))
                .is_some_and(|value| !value.trim().is_empty())
    })
}

fn finish_hub(hub: &HubPlan) -> Result<()> {
    if hub.native_tls {
        println!("В конфиге хаба включён TLS: минт инвайта по локальному HTTP не пройдёт.");
        println!("Создай инвайт в админке хаба ({}).", hub.public_hub_url);
        return Ok(());
    }

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
            IpAddr::V6(v6) => {
                !v6.is_loopback()
                    && !v6.is_unspecified()
                    // link-local fe80::/10 и unique-local fc00::/7 снаружи
                    // недостижимы так же, как приватные v4.
                    && (v6.segments()[0] & 0xffc0) != 0xfe80
                    && (v6.segments()[0] & 0xfe00) != 0xfc00
            }
        })
        .map(|ip| ip.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web_plan(hub_conf_here: bool, secret_conflict: bool) -> WebPlan {
        WebPlan {
            domain: "web.example.com".into(),
            hub_url: "http://127.0.0.1:8080".into(),
            shared_secret: "s3cret".into(),
            secret_conflict,
            hub_conf_here,
        }
    }

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
            native_tls: false,
        });
        Resolved {
            arch: Arch::X86_64,
            source: None,
            force,
            server,
            hub,
            web: None,
        }
    }

    fn names(r: &Resolved) -> Vec<String> {
        plan(r).iter().map(|s| s.name()).collect()
    }

    #[test]
    fn server_plan_without_hub() {
        assert_eq!(
            names(&resolved(false, false)),
            [
                "binary:xr-server",
                "config:server",
                "sysctl",
                "service:xr-proxy-server",
                "server:service-alert"
            ]
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
                "server:service-alert",
                "binary:xr-hub",
                "hub:signing-key",
                "config:hub",
                "service:xr-hub",
                "hub:backup"
            ]
        );
    }

    #[test]
    fn backup_finding_fires_until_the_receiver_is_set() {
        for empty in [
            None,
            Some(HUB_BACKUP_ENV),
            Some("#BACKUP_HOST=203.0.113.7\n#BACKUP_KEY=/root/.ssh/xr-hub-backup\n"),
            Some("BACKUP_HOST=\nBACKUP_KEY=/root/.ssh/xr-hub-backup\n"),
            // Отправка требует обе переменные: с одним адресом установщик
            // молчал бы, а cron слал алерт про незаполненный env.
            Some("BACKUP_HOST=203.0.113.7\n"),
            Some("BACKUP_HOST=203.0.113.7\nBACKUP_KEY=\n"),
            Some("BACKUP_HOST=203.0.113.7\n#BACKUP_KEY=/root/.ssh/xr-hub-backup\n"),
        ] {
            let finding = backup_env_finding(empty).unwrap_or_else(|| {
                panic!("незаполненный env обязан стать находкой: {empty:?}")
            });
            assert!(finding.contains(HUB_BACKUP_ENV_FILE));
            assert!(finding.contains(HUB_BACKUP_BIN), "в находке нужна точная команда");
        }
        assert!(
            backup_env_finding(Some(
                "# приёмник\nBACKUP_HOST = 203.0.113.7\nBACKUP_KEY=/root/.ssh/xr-hub-backup\n"
            ))
            .is_none(),
            "заполненный env находкой не считается"
        );
    }

    #[test]
    fn alert_finding_fires_until_telegram_is_set() {
        for empty in [
            None,
            Some("#TG_TOKEN=123:ABC\n#TG_CHAT=42\n"),
            Some("TG_TOKEN=123:ABC\n"),
        ] {
            let finding = alert_env_finding(empty)
                .unwrap_or_else(|| panic!("ненастроенный Telegram обязан стать находкой: {empty:?}"));
            assert!(finding.contains(ALERT_ENV_FILE));
            assert!(finding.contains(SERVICE_ALERT_BIN), "в находке нужна точная команда");
        }
        assert!(
            alert_env_finding(Some("TG_TOKEN=123:ABC\nTG_CHAT=42\n")).is_none(),
            "заполненный env находкой не считается"
        );
    }

    #[test]
    fn server_plan_with_web_appends_front_steps() {
        let mut r = resolved(true, false);
        r.web = Some(web_plan(true, false));
        assert_eq!(
            names(&r),
            [
                "binary:xr-server",
                "config:server",
                "sysctl",
                "service:xr-proxy-server",
                "server:service-alert",
                "binary:xr-hub",
                "hub:signing-key",
                "config:hub",
                "service:xr-hub",
                "hub:backup",
                "binary:xr-web",
                "config:web",
                "config:hub-web",
                "service:xr-web"
            ]
        );

        // Хаб на другой машине: дописывать блок [web] некуда, и шага нет.
        r.web = Some(web_plan(false, false));
        assert!(
            !names(&r).contains(&"config:hub-web".to_string()),
            "чужой конфиг хаба этой машине не принадлежит"
        );
    }

    #[test]
    fn web_findings_name_every_manual_step() {
        // DNS и сертификат установщик не заводит: про них он обязан сказать
        // всегда, иначе вход молча не работает.
        let always = web_findings(&web_plan(true, false));
        assert_eq!(always.len(), 1, "лишних находок на здоровой установке нет: {always:?}");
        assert!(always[0].contains("*.web.example.com"), "{always:?}");
        assert!(always[0].contains("/.xr-web/healthz"), "в находке нужна точная проверка");

        // Хаб на другой машине: находка несёт готовый блок с секретом.
        let remote = web_findings(&web_plan(false, false));
        assert!(remote.iter().any(|f| f.contains("shared_secret = \"s3cret\"")), "{remote:?}");

        // Разъехавшиеся секреты: сам установщик их не чинит, но называет оба
        // файла и команду.
        let clash = web_findings(&web_plan(true, true));
        assert!(clash.iter().any(|f| f.contains(WEB_CONF) && f.contains(HUB_CONF)), "{clash:?}");
        assert!(clash.iter().any(|f| f.contains("systemctl restart xr-hub xr-web")), "{clash:?}");
    }

    #[test]
    fn shared_secret_is_reused_from_either_side() {
        // Секрет живёт в двух конфигах, и повторная установка обязана взять
        // уже стоящий, а не выписать третий.
        assert_eq!(
            shared_secret_of("[web]\nshared_secret = \"abc\"\n").as_deref(),
            Some("abc")
        );
        assert_eq!(shared_secret_of("[web]\nshared_secret = \"\"\n"), None);
        assert_eq!(shared_secret_of("[server]\nbind = \"x\"\n"), None);
        assert_eq!(shared_secret_of("это не toml ["), None);
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
    fn rejects_non_routable_ipv6() {
        assert_eq!(pick_public_ip("fe80::1 fd12::1 ::1"), None);
        assert_eq!(
            pick_public_ip("fe80::1 2001:db8::7").as_deref(),
            Some("2001:db8::7")
        );
    }

    #[test]
    fn local_base_follows_bind_port() {
        assert_eq!(local_base_from_bind("127.0.0.1:8080"), "http://127.0.0.1:8080");
        assert_eq!(local_base_from_bind("0.0.0.0:9090"), "http://127.0.0.1:9090");
    }
}
