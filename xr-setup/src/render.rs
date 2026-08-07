//! Рендер конфигов и системных файлов чистыми функциями (LLD-13 п. 3.3):
//! вся текстовая часть установки тестируется без реальной машины. Юниты
//! systemd вшиты из deploy/, чтобы установщик и ручной путь не расходились.

/// Юнит xr-server; имя сервиса историческое, xr-proxy-server.
pub const SERVER_UNIT: &str = include_str!("../../deploy/xr-proxy-server.service");
pub const HUB_UNIT: &str = include_str!("../../deploy/xr-hub.service");
/// Юнит браузерного входа (LLD-38 фаза 2, профиль --with-web).
pub const WEB_UNIT: &str = include_str!("../../deploy/xr-web.service");

/// Обвязка роутера, те же файлы, что раскладываются вручную по хэндоффу:
/// procd init (в нём же fd-limit через procd limits), watchdog-cron и
/// nftables-скрипты, на которые init ссылается.
pub const ROUTER_INIT: &str = include_str!("../../deploy/xr-proxy.init");
pub const ROUTER_WATCHDOG: &str = include_str!("../../deploy/xr-watchdog.sh");
pub const KILLSWITCH_SETUP: &str = include_str!("../../scripts/killswitch-setup.sh");
pub const KILLSWITCH_CLEANUP: &str = include_str!("../../scripts/killswitch-cleanup.sh");
pub const UDP_TPROXY_SETUP: &str = include_str!("../../scripts/udp-tproxy-setup.sh");
pub const UDP_TPROXY_CLEANUP: &str = include_str!("../../scripts/udp-tproxy-cleanup.sh");

/// Бэкап состояния хаба (XR-224): тот же скрипт, что кладут руками.
pub const HUB_BACKUP: &str = include_str!("../../deploy/xr-hub-backup.sh");

/// Раз в сутки ночью; cron.d на этих VPS уже носит cert-alert.
pub const HUB_BACKUP_CRON: &str = "\
# xr-hub: ежедневный бэкап состояния на второй VPS (ставит xr-setup, XR-224)
17 4 * * * root /usr/local/bin/xr-hub-backup.sh
";

/// Болванка backup.env: адрес приёмника и ключ заполняет оператор, в
/// репозитории их нет.
pub const HUB_BACKUP_ENV: &str = "\
# Куда xr-hub-backup.sh отправляет архив состояния хаба (XR-224).
# Приёмник это второй VPS, где ключ ниже зажат forced command на
# xr-hub-backup-receive.sh. Раскомментировать и заполнить обе строки,
# иначе бэкап собирается, но никуда не уезжает.
#BACKUP_HOST=
#BACKUP_KEY=/root/.ssh/xr-hub-backup
";

/// BBR с fq и буферы 8М: без них mux упирается в BDP задолго до ширины
/// канала (LLD-09). То же, что закреплено на живом флоте в sysctl.d.
pub const SYSCTL_CONF: &str = "\
# xr-proxy: BBR и TCP-буферы под дальний RTT (ставит xr-setup)
net.core.default_qdisc=fq
net.ipv4.tcp_congestion_control=bbr
net.core.rmem_max=8388608
net.core.wmem_max=8388608
net.ipv4.tcp_rmem=4096 131072 8388608
net.ipv4.tcp_wmem=4096 65536 8388608
";

pub struct ServerTomlParams {
    pub port: u16,
    pub key: String,
    pub salt: u32,
}

pub fn render_server_toml(p: &ServerTomlParams) -> String {
    format!(
        r#"# xr-proxy server config (сгенерирован xr-setup, XR-015)
# Ключ и salt обязаны совпадать с клиентами; они уезжают туда инвайтом.

[server]
listen = "0.0.0.0"
port = {port}

[obfuscation]
key = "{key}"
modifier = "positional_xor_rotate"
salt = 0x{salt:08X}

[limits]
max_connections = 256
connection_timeout_sec = 300

[fallback]
enabled = true

[logging]
level = "info"
"#,
        port = p.port,
        key = p.key,
        salt = p.salt,
    )
}

/// Буферы TCP под дальний RTT, как закреплено на живых роутерах
/// (bbr на OpenWRT-ядре не включаем, его там обычно нет).
pub const ROUTER_SYSCTL_CONF: &str = "\
# xr-proxy: TCP-буферы под mux к дальнему VPS (ставит xr-setup)
net.core.rmem_max=8388608
net.core.wmem_max=8388608
net.ipv4.tcp_rmem=4096 131072 8388608
net.ipv4.tcp_wmem=4096 65536 8388608
";

/// Один сервер пула роутера; приоритет по порядку в списке (0 = primary).
pub struct RouterServer {
    pub address: String,
    pub port: u16,
}

pub struct RouterTomlParams {
    pub servers: Vec<RouterServer>,
    pub key: String,
    pub salt: u32,
    /// Хаб с пресетом маршрутизации; без него правил нет и всё идёт Direct.
    pub hub: Option<(String, String)>,
    /// Исключения перехвата, снятые со старого конфига машины (XR-248): из
    /// флагов их не восстановить, а перегенерация конфига под `--force`
    /// иначе стирает их вместе с устройствами, которые на них держатся.
    pub bypass_ips: Vec<String>,
    pub bypass_rules: Vec<String>,
}

pub fn render_router_toml(p: &RouterTomlParams) -> String {
    let mut out = String::from(
        "# xr-proxy client config (сгенерирован xr-setup, XR-177)\n\
         # Ключ и salt обязаны совпадать с сервером.\n\n",
    );
    for (priority, s) in p.servers.iter().enumerate() {
        out.push_str(&format!(
            "[[servers]]\naddress = \"{}\"\nport = {}\npriority = {}\n\n",
            s.address, s.port, priority
        ));
    }
    out.push_str(&format!(
        "[obfuscation]\nkey = \"{}\"\nmodifier = \"positional_xor_rotate\"\nsalt = 0x{:08X}\n\n",
        p.key, p.salt
    ));
    // Без локальных правил: их раздаёт пресет хаба, локальный список стал бы
    // вторым источником правды.
    out.push_str("[routing]\ndefault_action = \"direct\"\n\n");
    out.push_str(
        "[client]\nlisten_port = 1080\nauto_redirect = true\non_server_down = \"block\"\nlog_level = \"info\"\n",
    );
    if !p.bypass_ips.is_empty() {
        out.push_str(&format!("bypass_ips = {}\n", toml_string_array(&p.bypass_ips)));
    }
    if !p.bypass_rules.is_empty() {
        out.push_str(&format!(
            "bypass_rules = {}\n",
            toml_string_array(&p.bypass_rules)
        ));
    }
    if let Some((url, preset)) = &p.hub {
        out.push_str(&format!("\n[hub]\nurl = \"{url}\"\npreset = \"{preset}\"\n"));
    }
    out
}

/// Массив строк TOML в одну строку. Кавычки внутри значений не экранируем,
/// а отбрасываем такое значение раньше, в `carry_bypass`.
fn toml_string_array(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|v| format!("\"{v}\"")).collect();
    format!("[{}]", items.join(", "))
}

/// Секция `[control]` после enroll (шов с LLD-17): per-router идентичность
/// для poll-канала. Клиент начнёт читать её с реализацией реестра (XR-025),
/// до тех пор секция безвредна.
pub fn render_control_section(
    hub_url: &str,
    router_id: &str,
    secret: &str,
    command_pubkey: &str,
) -> String {
    format!(
        "\n[control]\nhub_url = \"{hub_url}\"\nrouter_id = \"{router_id}\"\nsecret = \"{secret}\"\ncommand_pubkey = \"{command_pubkey}\"\n"
    )
}

pub struct HubTomlParams {
    /// Хаб слушает localhost: наружу его выводит nginx с TLS, это ручной
    /// follow-up (LLD-13 п. 5.7), как настроен и живой флот.
    pub bind: String,
    pub admin_user: String,
    pub password_hash: String,
    pub signing_key_path: String,
    pub server_addr: String,
    pub server_port: u16,
    pub obfuscation_key: String,
    pub salt: u32,
    pub hub_url: String,
}

pub fn render_hub_toml(p: &HubTomlParams) -> String {
    format!(
        r#"# xr-hub config (сгенерирован xr-setup, XR-015)

[server]
bind = "{bind}"
data_dir = "/var/lib/xr-hub"

[[admin.users]]
username = "{user}"
password_hash = "{hash}"

[signing]
private_key = "{signing}"

[invites]
default_ttl_seconds = 86400
max_ttl_seconds = 604800

[invites.defaults]
server_address = "{addr}"
server_port = {port}
obfuscation_key = "{key}"
modifier = "positional_xor_rotate"
salt = 0x{salt:08X}
hub_url = "{hub_url}"

[[invites.defaults.servers]]
name = "primary"
address = "{addr}"
port = {port}
priority = 0
"#,
        bind = p.bind,
        user = p.admin_user,
        hash = p.password_hash,
        signing = p.signing_key_path,
        addr = p.server_addr,
        port = p.server_port,
        key = p.obfuscation_key,
        salt = p.salt,
        hub_url = p.hub_url,
    )
}

pub struct WebTomlParams {
    pub bind: String,
    /// Домен публикаций: в коде его нет, он приходит флагом установки.
    pub domain: String,
    pub hub_url: String,
    pub shared_secret: String,
}

pub fn render_web_toml(p: &WebTomlParams) -> String {
    format!(
        r#"# xr-web config (сгенерирован xr-setup, LLD-38)
# Наружу сервис выводит тот же фронт, что и хаб, разводя их по Host:
# *.{domain} на этот порт. DNS-запись и wildcard-сертификат заводятся один раз
# (docs/HUB-DEPLOY.md, раздел 3).

[web]
bind = "{bind}"
domain = "{domain}"
hub_url = "{hub_url}"
shared_secret = "{secret}"
session_ttl_secs = 604800
log_level = "info"
"#,
        bind = p.bind,
        domain = p.domain,
        hub_url = p.hub_url,
        secret = p.shared_secret,
    )
}

/// Блок `[web]` для конфига хаба: тот же секрет и тот же домен, что у фронта.
/// Дописывается к существующему конфигу, потому что перезаписать его нельзя
/// (хеш пароля админки из него не восстановить).
pub fn render_hub_web_block(domain: &str, secret: &str) -> String {
    format!(
        "\n# Браузерный вход (LLD-38, добавил xr-setup --with-web).\n\
         [web]\n\
         domain = \"{domain}\"\n\
         shared_secret = \"{secret}\"\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_toml_is_valid_and_carries_domain_and_secret() {
        let text = render_web_toml(&WebTomlParams {
            bind: "127.0.0.1:8090".into(),
            domain: "web.example.com".into(),
            hub_url: "http://127.0.0.1:8080".into(),
            shared_secret: "s3cret".into(),
        });
        let v: toml::Value = text.parse().expect("рендер обязан быть валидным TOML");
        assert_eq!(v["web"]["domain"].as_str(), Some("web.example.com"));
        assert_eq!(v["web"]["shared_secret"].as_str(), Some("s3cret"));
        assert_eq!(v["web"]["hub_url"].as_str(), Some("http://127.0.0.1:8080"));
        assert_eq!(v["web"]["bind"].as_str(), Some("127.0.0.1:8090"));
    }

    #[test]
    fn hub_web_block_appends_to_a_live_hub_config() {
        // Конфиг хаба не перерисовывается, блок дописывается в конец: разбор
        // склейки обязан дать оба раздела.
        let existing = "[server]\nbind = \"127.0.0.1:8080\"\n";
        let joined = format!("{existing}{}", render_hub_web_block("web.example.com", "s3cret"));
        let v: toml::Value = joined.parse().expect("склейка обязана остаться валидным TOML");
        assert_eq!(v["server"]["bind"].as_str(), Some("127.0.0.1:8080"));
        assert_eq!(v["web"]["domain"].as_str(), Some("web.example.com"));
        assert_eq!(v["web"]["shared_secret"].as_str(), Some("s3cret"));
    }

    #[test]
    fn server_toml_is_valid_and_carries_params() {
        let text = render_server_toml(&ServerTomlParams {
            port: 8443,
            key: "QUJD".into(),
            salt: 0xDEADBEEF,
        });
        let v: toml::Value = text.parse().expect("рендер обязан быть валидным TOML");
        assert_eq!(v["server"]["port"].as_integer(), Some(8443));
        assert_eq!(v["obfuscation"]["key"].as_str(), Some("QUJD"));
        assert_eq!(v["obfuscation"]["salt"].as_integer(), Some(0xDEADBEEFu32 as i64));
        assert_eq!(
            v["obfuscation"]["modifier"].as_str(),
            Some("positional_xor_rotate")
        );
        assert_eq!(v["fallback"]["enabled"].as_bool(), Some(true));
    }

    #[test]
    fn hub_toml_is_valid_and_carries_params() {
        let text = render_hub_toml(&HubTomlParams {
            bind: "127.0.0.1:8080".into(),
            admin_user: "admin".into(),
            password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aGFzaA".into(),
            signing_key_path: "/var/lib/xr-hub/signing.key".into(),
            server_addr: "203.0.113.1".into(),
            server_port: 8443,
            obfuscation_key: "QUJD".into(),
            salt: 0x0BADCAFE,
            hub_url: "https://hub.example.com".into(),
        });
        let v: toml::Value = text.parse().expect("рендер обязан быть валидным TOML");
        assert_eq!(v["server"]["bind"].as_str(), Some("127.0.0.1:8080"));
        let users = v["admin"]["users"].as_array().unwrap();
        assert_eq!(users[0]["username"].as_str(), Some("admin"));
        assert!(users[0]["password_hash"].as_str().unwrap().starts_with("$argon2id$"));
        let defaults = &v["invites"]["defaults"];
        assert_eq!(defaults["server_address"].as_str(), Some("203.0.113.1"));
        assert_eq!(defaults["hub_url"].as_str(), Some("https://hub.example.com"));
        let servers = defaults["servers"].as_array().unwrap();
        assert_eq!(servers[0]["address"].as_str(), Some("203.0.113.1"));
        assert_eq!(servers[0]["priority"].as_integer(), Some(0));
    }

    #[test]
    fn units_point_at_expected_paths() {
        assert!(SERVER_UNIT.contains("/usr/local/bin/xr-server -c /etc/xr-proxy/server.toml"));
        assert!(HUB_UNIT.contains("/usr/local/bin/xr-hub --config /etc/xr-hub/config.toml"));
    }

    #[test]
    fn sysctl_pins_bbr_and_buffers() {
        assert!(SYSCTL_CONF.contains("net.ipv4.tcp_congestion_control=bbr"));
        assert!(SYSCTL_CONF.contains("net.core.default_qdisc=fq"));
        assert!(SYSCTL_CONF.contains("net.ipv4.tcp_rmem=4096 131072 8388608"));
    }
}

/// Прогон роутерных скриптов на стенде из заглушек (XR-247). Скрипты уезжают
/// на роутер этими же строками, поэтому гоняем вшитый текст, а не копию.
#[cfg(all(test, unix))]
mod router_script_tests {
    use super::{KILLSWITCH_SETUP, ROUTER_INIT, UDP_TPROXY_SETUP};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Настоящие nft и ip звать нельзя, они правят firewall машины, где идут
    /// тесты. Заглушки пишут вызовы в лог и держат состояние файлами-метками,
    /// поэтому `nft list table` и `ip rule show` отвечают по тому, что до них
    /// действительно доехало.
    const STUB_NFT: &str = r#"#!/bin/sh
echo "nft $*" >> "$STAND/calls.log"
case "$1" in
    -f)
        cat >> "$STAND/ruleset.txt"
        [ -f "$STAND/swallow-table" ] || : > "$STAND/table"
        ;;
    list)   [ -f "$STAND/table" ] || exit 1 ;;
    delete) rm -f "$STAND/table" ;;
esac
exit 0
"#;

    const STUB_IP: &str = r#"#!/bin/sh
echo "ip $*" >> "$STAND/calls.log"
case "$1 $2" in
    "rule show")     [ -f "$STAND/rule" ] && echo "32765: from all fwmark 0x200 lookup 201" ;;
    "route show")    [ -f "$STAND/route" ] && echo "local default dev lo scope host" ;;
    "rule add")      [ -f "$STAND/swallow-rule" ] || : > "$STAND/rule" ;;
    "rule del")      rm -f "$STAND/rule" ;;
    "route replace") [ -f "$STAND/swallow-route" ] || : > "$STAND/route" ;;
esac
exit 0
"#;

    const STUB_LOGGER: &str = r#"#!/bin/sh
echo "$*" >> "$STAND/syslog.log"
"#;

    struct Stand {
        dir: tempfile::TempDir,
    }

    impl Stand {
        fn new() -> Self {
            let stand = Stand {
                dir: tempfile::tempdir().expect("временный каталог"),
            };
            fs::create_dir(stand.at("bin")).expect("каталог заглушек");
            for (name, body) in [("nft", STUB_NFT), ("ip", STUB_IP), ("logger", STUB_LOGGER)] {
                write_exec(&stand.at("bin").join(name), body);
            }
            stand
        }

        fn at(&self, rel: &str) -> PathBuf {
            self.dir.path().join(rel)
        }

        fn read(&self, rel: &str) -> String {
            fs::read_to_string(self.at(rel)).unwrap_or_default()
        }

        /// Заглушки идут первыми и без системных sbin: настоящие nft и ip не
        /// должны находиться даже по случайности.
        fn path_env(&self) -> String {
            format!("{}:/usr/bin:/bin", self.at("bin").display())
        }

        /// Вшитый скрипт настройки, у которого подменены только два пути:
        /// откуда брать nft (искать его в /usr/sbin на машине с тестами
        /// нельзя) и где лежит конфиг. Остальной текст тот же, что уезжает
        /// на роутер.
        fn setup_script(&self, config: &Path) -> PathBuf {
            let body = replace_once(
                UDP_TPROXY_SETUP,
                "for p in /usr/sbin/nft /sbin/nft /usr/bin/nft; do",
                &format!("for p in {}; do", self.at("bin").join("nft").display()),
            );
            let body = replace_once(
                &body,
                "CONFIG=\"/etc/xr-proxy/config.toml\"",
                &format!("CONFIG=\"{}\"", config.display()),
            );
            let path = self.at("udp-tproxy-setup.sh");
            write_exec(&path, &body);
            path
        }

        fn write_config(&self, source_ips: &str) -> PathBuf {
            self.write_config_with_bypass(source_ips, &[])
        }

        /// Тот же конфиг машины плюс машинные исключения перехвата (XR-248):
        /// они живут в конфиге, а не в init-скрипте, и раскладка их не видит.
        fn write_config_with_bypass(&self, source_ips: &str, bypass_rules: &[&str]) -> PathBuf {
            let path = self.at("config.toml");
            let rules: Vec<String> = bypass_rules.iter().map(|r| format!("\"{r}\"")).collect();
            fs::write(
                &path,
                format!(
                    "[client]\nlisten_port = 1080\nbypass_ips = []\nbypass_rules = [\n{}\n]\n\n\
                     [udp_relay]\nenabled = true\nlisten_port = 1081\nsource_ips = [{source_ips}]\nflow_timeout_sec = 120\n",
                    rules.join(",\n")
                ),
            )
            .expect("конфиг стенда");
            path
        }

        /// Вшитый kill-switch, у которого подменён только поиск nft.
        fn killswitch_script(&self) -> PathBuf {
            let body = replace_once(
                KILLSWITCH_SETUP,
                "for p in /usr/sbin/nft /sbin/nft /usr/bin/nft; do",
                &format!("for p in {}; do", self.at("bin").join("nft").display()),
            );
            let path = self.at("killswitch-setup.sh");
            write_exec(&path, &body);
            path
        }

        /// `start_service` из вшитого init, без procd и с путями стенда.
        /// PATH переставляем после подключения файла: сам init ставит впереди
        /// системные sbin, а нам нужны заглушки.
        fn init_driver(&self, setup_script: &Path, config: &Path) -> PathBuf {
            let missing = self.at("no-such-file");
            self.init_driver_with_killswitch(setup_script, config, &missing)
        }

        fn init_driver_with_killswitch(
            &self,
            setup_script: &Path,
            config: &Path,
            killswitch: &Path,
        ) -> PathBuf {
            let path = self.at("driver.sh");
            let missing = self.at("no-such-file");
            write_exec(
                &path,
                &format!(
                    "set -e\n\
                     . '{init}'\n\
                     PATH='{path_env}'\n\
                     CONFIG='{config}'\n\
                     UDP_TPROXY='{setup}'\n\
                     WATCHDOG='{missing}'\n\
                     KILLSWITCH_SETUP='{killswitch}'\n\
                     KILLSWITCH_CLEANUP='{missing}'\n\
                     procd_open_instance() {{ :; }}\n\
                     procd_set_param() {{ :; }}\n\
                     procd_close_instance() {{ :; }}\n\
                     start_service\n",
                    init = self.write_init().display(),
                    path_env = self.path_env(),
                    config = config.display(),
                    setup = setup_script.display(),
                    killswitch = killswitch.display(),
                    missing = missing.display(),
                ),
            );
            path
        }

        fn write_init(&self) -> PathBuf {
            let path = self.at("xr-proxy.init");
            fs::write(&path, ROUTER_INIT).expect("init стенда");
            path
        }

        /// Запуск с закрытым stdout: ровно так скрипт видит вызвавшего,
        /// который свой конец вывода уже закрыл.
        fn run_without_stdout(&self, script: &Path, args: &str) -> std::process::Output {
            self.run(&format!("exec '{}' {args} >&-", script.display()))
        }

        fn run(&self, command: &str) -> std::process::Output {
            Command::new("/bin/sh")
                .arg("-c")
                .arg(command)
                .env("STAND", self.dir.path())
                .env("PATH", self.path_env())
                .output()
                .expect("запуск скрипта")
        }
    }

    /// Подмена с проверкой: строка, которой в скрипте нет, значит стенд
    /// разъехался со скриптом и тест проверяет не то, что думает.
    fn replace_once(body: &str, needle: &str, replacement: &str) -> String {
        assert!(
            body.contains(needle),
            "в скрипте нет строки {needle:?}, стенд надо чинить"
        );
        body.replace(needle, replacement)
    }

    fn write_exec(path: &Path, body: &str) {
        fs::write(path, body).expect("запись скрипта стенда");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("права на скрипт");
    }

    /// Регрессия XR-247: `/etc/init.d/xr-proxy start` из-под вызвавшего, который
    /// закрыл вывод, обязан поднять таблицу перехвата. До правки скрипт умирал
    /// на первой печати, правила не вставали, а в logread шло голое
    /// предупреждение.
    #[test]
    fn init_installs_tproxy_table_with_closed_stdout() {
        let stand = Stand::new();
        let config = stand.write_config("\"192.0.2.10\"");
        let setup = stand.setup_script(&config);
        let driver = stand.init_driver(&setup, &config);

        let out = stand.run_without_stdout(&driver, "");
        assert!(
            out.status.success(),
            "init отвалился: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let ruleset = stand.read("ruleset.txt");
        assert!(
            ruleset.contains("tproxy to :1081") && ruleset.contains("192.0.2.10"),
            "правила перехвата не встали, nft получил: {ruleset:?}"
        );
        let syslog = stand.read("syslog.log");
        assert!(
            syslog.contains("UDP TPROXY rules installed"),
            "в лог не попала установка правил: {syslog:?}"
        );
        assert!(
            !syslog.contains("WARNING"),
            "успешная установка отмечена предупреждением: {syslog:?}"
        );
    }

    /// Тот же закрытый вывод, но скрипт зовут напрямую: он и сам обязан
    /// доводить установку до конца, а не полагаться на то, как его позвали.
    #[test]
    fn setup_script_survives_closed_stdout() {
        let stand = Stand::new();
        let setup = stand.setup_script(&stand.write_config("\"192.0.2.10\""));

        let out = stand.run_without_stdout(&setup, "192.0.2.10");
        assert!(
            out.status.success(),
            "скрипт отвалился с кодом {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stand.read("ruleset.txt").contains("tproxy to :1081"),
            "правила перехвата не встали"
        );
        assert!(
            stand.read("calls.log").contains("ip rule add fwmark 0x200"),
            "policy route не поднят"
        );
    }

    /// Тот же скрипт, но вывод уходит в трубу, чей читатель уже закрылся: так
    /// его видит `ssh роутер '/etc/init.d/xr-proxy restart' | grep -q ...`.
    /// Без игнора PIPE его тут убивает сигналом на первой же печати.
    #[test]
    fn setup_script_survives_reader_that_closed_the_pipe() {
        let stand = Stand::new();
        let setup = stand.setup_script(&stand.write_config("\"192.0.2.10\""));

        stand.run(&format!(
            "{{ '{}' 192.0.2.10; echo \"code=$?\" > \"$STAND/code\"; }} | true",
            setup.display()
        ));

        assert_eq!(
            stand.read("code").trim(),
            "code=0",
            "скрипт не пережил закрытую трубу"
        );
        assert!(
            stand.read("ruleset.txt").contains("tproxy to :1081"),
            "правила перехвата не встали"
        );
    }

    /// XR-244: код возврата обязан считаться по тому, что реально встало.
    /// Здесь nft молча проглатывает набор правил, и скрипт обязан это заметить.
    #[test]
    fn setup_script_fails_when_table_did_not_appear() {
        let stand = Stand::new();
        fs::write(stand.at("swallow-table"), "").expect("метка стенда");
        let setup = stand.setup_script(&stand.write_config("\"192.0.2.10\""));

        let out = stand.run(&format!("exec '{}' 192.0.2.10", setup.display()));
        assert!(!out.status.success(), "проглоченный набор правил сошёл за успех");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("table ip xr_udp_relay is not installed"),
            "причина отказа не названа: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    /// Та же проверка состояния для policy route: без него правила nft лежат
    /// мёртвым грузом, и молчаливого успеха тут быть не должно.
    #[test]
    fn setup_script_fails_when_policy_route_did_not_appear() {
        for (swallowed, reason) in [
            ("swallow-rule", "policy rule for fwmark 0x200 is missing"),
            ("swallow-route", "local default route in table 201 is missing"),
        ] {
            let stand = Stand::new();
            fs::write(stand.at(swallowed), "").expect("метка стенда");
            let setup = stand.setup_script(&stand.write_config("\"192.0.2.10\""));

            let out = stand.run(&format!("exec '{}' 192.0.2.10", setup.display()));
            assert!(
                !out.status.success(),
                "{swallowed}: потерянный policy route сошёл за успех"
            );
            assert!(
                String::from_utf8_lossy(&out.stdout).contains(reason),
                "{swallowed}: причина отказа не названа: {}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
    }

    /// Регрессия XR-248: машинные исключения перехвата жили правкой
    /// init-скрипта на живой машине, и раскладка обвязки стирала их молча.
    /// Теперь они приходят из конфига, поэтому переживают раскладку: init
    /// берётся репозиторный, а исключение всё равно доезжает до kill-switch.
    /// Без этого устройство остаётся и без прокси, и без прямого выхода.
    #[test]
    fn machine_bypass_survives_init_redeploy() {
        let stand = Stand::new();
        let config = stand.write_config_with_bypass(
            "\"192.0.2.10\"",
            &["ip saddr 192.168.1.164 tcp dport != { 80, 443 }"],
        );
        let setup = stand.setup_script(&config);
        let killswitch = stand.killswitch_script();
        let driver = stand.init_driver_with_killswitch(&setup, &config, &killswitch);

        let out = stand.run_without_stdout(&driver, "");
        assert!(
            out.status.success(),
            "init отвалился: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let calls = stand.read("calls.log");
        let accept = "nft add rule ip xr_killswitch forward \
                      ip saddr 192.168.1.164 tcp dport != { 80, 443 } accept";
        assert!(
            calls.contains(accept),
            "исключение не доехало до kill-switch: {calls}"
        );
        // Порядок решает: после общего drop правило уже никого не выпустит.
        let at_accept = calls.find("192.168.1.164").expect("исключение в вызовах nft");
        let at_drop = calls
            .find("tcp dport != { 22, 25")
            .expect("общий drop в вызовах nft");
        assert!(at_accept < at_drop, "исключение встало после общего drop");
    }

    /// Набор условий, на котором сверяются обе половины отбраковки. Кавычки
    /// сюда не берём: значение с кавычкой не пережило бы даже запись в TOML,
    /// и сверялась бы разметка стенда, а не фильтры. Кавычки закрыты юнитом
    /// `xr_proto::config::bypass_rule_reject_reason`.
    const FILTER_CORPUS: &[&str] = &[
        "ip saddr 192.0.2.10 tcp dport != { 80, 443 }",
        "ip daddr 198.51.100.7 tcp dport 8443",
        "iifname br-lan ip saddr 192.0.2.11",
        // Ревью XR-248: подстановка переменной проходила перехват и
        // отбраковывалась киллсвитчем, то есть давала return без accept.
        "ip saddr $nas tcp dport 80",
        "ip saddr 192.0.2.10 accept",
        "ip saddr 192.0.2.10 ACCEPT",
        "ip saddr 192.0.2.10 return",
        "ip saddr 192.0.2.10 tcp dport 80; reboot",
        "ip saddr 192.0.2.10 && reboot",
        "ip saddr 192.0.2.10 | tee /tmp/x",
        "ip saddr 192.0.2.10 > /tmp/x",
        "ip saddr 192.0.2.10 # хвост",
        "ip saddr 192.0.2.10 \\",
        "   ",
    ];

    /// Прогнать условие через отбраковку киллсвитча: встало ли оно в цепочку.
    fn killswitch_accepts(rule: &str) -> bool {
        let stand = Stand::new();
        let config = stand.write_config_with_bypass("\"192.0.2.10\"", &[rule]);
        let script = stand.killswitch_script();
        let out = stand.run(&format!("exec '{}' '{}'", script.display(), config.display()));
        assert!(
            out.status.success(),
            "kill-switch отвалился на {rule:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        // Заглушка пишет вызов уже после разбиения на слова, поэтому и
        // ожидание собираем по словам.
        let words: Vec<&str> = rule.split_whitespace().collect();
        let expected = format!(
            "nft add rule ip xr_killswitch forward {} accept",
            words.join(" ")
        );
        !words.is_empty() && stand.read("calls.log").contains(&expected)
    }

    /// Проверка XR-248 на проде: в живом конфиге массив bypass_rules записан
    /// одной строкой, а следом идут ключи с кавычёнными значениями. Диапазон
    /// sed закрывался только со следующей строки, поэтому в кандидаты уезжали
    /// on_server_down и log_level, и киллсвитч пробовал ставить 'block' с
    /// 'debug' как условия. Судим по вызовам nft: чужим значениям в цепочке
    /// не место, а само условие доезжает.
    #[test]
    fn single_line_bypass_rules_take_no_neighbours() {
        let stand = Stand::new();
        let config = stand.at("config.toml");
        fs::write(
            &config,
            "[client]\n\
             bypass_rules = [\"ip daddr 198.51.100.7 tcp dport 8443\"]\n\
             listen_port = 1080\n\
             on_server_down = \"block\"   # \"direct\" = bypass, \"block\" = block\n\
             log_level = \"debug\"\n\
             bypass_ips = []\n\
             # [geoip]\n\
             [udp_relay]\nenabled = true\nlisten_port = 1081\nsource_ips = [\"192.0.2.10\"]\n",
        )
        .expect("конфиг стенда");

        let script = stand.killswitch_script();
        let out = stand.run(&format!("exec '{}' '{}'", script.display(), config.display()));
        assert!(
            out.status.success(),
            "kill-switch отвалился: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let calls = stand.read("calls.log");
        assert!(
            calls.contains(
                "nft add rule ip xr_killswitch forward ip daddr 198.51.100.7 tcp dport 8443 accept"
            ),
            "условие не доехало до kill-switch: {calls}"
        );
        for stray in ["forward block accept", "forward debug accept", "forward direct accept"] {
            assert!(
                !calls.contains(stray),
                "чужое значение конфига уехало в киллсвитч ({stray}): {calls}"
            );
        }
    }

    /// Ревью XR-248: критерий отбраковки у перехвата и у киллсвитча обязан
    /// быть один. Перехват режет чужие вердикты, киллсвитч резал подстановки,
    /// и условие с `$` вставало в цепочку перехвата с `return`, но не
    /// доезжало до киллсвитча с `accept`. Устройство при этом уходит из-под
    /// редиректа и падает в общий drop, то есть получает ровно тот блэкхол,
    /// который задача и чинит. Гоняем один набор через обе половины.
    #[test]
    fn bypass_rule_filters_agree_on_both_sides() {
        for rule in FILTER_CORPUS {
            let by_client = xr_proto::config::bypass_rule_reject_reason(rule).is_none();
            let by_killswitch = killswitch_accepts(rule);
            assert_eq!(
                by_client, by_killswitch,
                "фильтры разошлись на {rule:?}: перехват принимает {by_client}, \
                 kill-switch принимает {by_killswitch}"
            );
        }
    }

    /// Тот же разрыв, названный по имени: подстановка либо не проходит
    /// никуда, либо проходит везде, но `return` без `accept` невозможен.
    #[test]
    fn rule_with_shell_substitution_never_gets_return_without_accept() {
        let rule = "ip saddr $nas tcp dport 80";
        assert!(
            xr_proto::config::bypass_rule_reject_reason(rule).is_some(),
            "подстановка обязана отбраковываться перехватом"
        );
        assert!(
            !killswitch_accepts(rule),
            "подстановка обязана отбраковываться киллсвитчем"
        );
    }

    /// Отказ настройки обязан доезжать до logread с причиной: без неё
    /// предупреждение в логе не отличить от любого другого отказа.
    #[test]
    fn init_logs_why_tproxy_setup_failed() {
        let stand = Stand::new();
        let config = stand.write_config("\"192.0.2.10\"");
        let failing = stand.at("failing-setup.sh");
        write_exec(
            &failing,
            "#!/bin/sh\necho \"ERROR: No source IPs specified.\"\nexit 1\n",
        );
        let driver = stand.init_driver(&failing, &config);

        stand.run(&format!("exec '{}'", driver.display()));

        let syslog = stand.read("syslog.log");
        assert!(
            syslog.contains("No source IPs specified"),
            "причина отказа не доехала до лога: {syslog:?}"
        );
    }
}
