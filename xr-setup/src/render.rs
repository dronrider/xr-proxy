//! Рендер конфигов и системных файлов чистыми функциями (LLD-13 п. 3.3):
//! вся текстовая часть установки тестируется без реальной машины. Юниты
//! systemd вшиты из deploy/, чтобы установщик и ручной путь не расходились.

/// Юнит xr-server; имя сервиса историческое, xr-proxy-server.
pub const SERVER_UNIT: &str = include_str!("../../deploy/xr-proxy-server.service");
pub const HUB_UNIT: &str = include_str!("../../deploy/xr-hub.service");

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

#[cfg(test)]
mod tests {
    use super::*;

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
