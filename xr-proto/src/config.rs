/// Configuration parsing for client and server.
use serde::{Deserialize, Serialize};
use std::path::Path;

// ── Client config ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ClientConfig {
    /// Legacy одиночный `[server]`. Понимается как пул из одного сервера
    /// с priority=0, чтобы конфиги боевых роутеров работали без правки.
    #[serde(default)]
    pub server: Option<ServerAddress>,
    /// Упорядоченный пул серверов `[[servers]]` (LLD-10). Меньший priority
    /// значит выше в очереди (0 = primary). Взаимоисключим с `[server]` по смыслу:
    /// если задан хотя бы один `[[servers]]`, legacy-секция игнорируется.
    #[serde(default)]
    pub servers: Vec<ServerEntry>,
    pub obfuscation: ObfuscationConfig,
    pub routing: RoutingConfig,
    #[serde(default)]
    pub client: ClientSettings,
    #[serde(default)]
    pub geoip: Option<GeoIpConfig>,
    #[serde(default)]
    pub udp_relay: Option<UdpRelayClientConfig>,
    #[serde(default)]
    pub hub: Option<HubClientConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ServerAddress {
    pub address: String,
    pub port: u16,
}

/// Один сервер пула `[[servers]]`. Общая обфускация берётся из
/// `[obfuscation]`, а `key`/`salt`/`modifier` здесь это опциональный override
/// на случай, когда у резервного VPS другой ключ.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerEntry {
    /// Человекочитаемый лейбл для логов и индикации. Если пустой, берётся адрес.
    #[serde(default)]
    pub name: String,
    pub address: String,
    pub port: u16,
    /// Меньше = выше приоритет; 0 = primary.
    #[serde(default)]
    pub priority: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salt: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modifier: Option<String>,
}

impl ServerEntry {
    /// Лейбл для логов: явное имя либо адрес.
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() { &self.address } else { &self.name }
    }
}

impl ClientConfig {
    /// Итоговый пул серверов: `[[servers]]`, отсортированный по priority
    /// (при равенстве порядок файла сохраняется), либо legacy `[server]`
    /// как пул из одного элемента. Пустой пул это ошибка конфигурации, как
    /// пустой `source_ips` у UDP relay.
    pub fn server_entries(&self) -> Result<Vec<ServerEntry>, String> {
        if !self.servers.is_empty() {
            let mut entries = self.servers.clone();
            entries.sort_by_key(|e| e.priority);
            Ok(entries)
        } else if let Some(ref s) = self.server {
            Ok(vec![ServerEntry {
                name: String::new(),
                address: s.address.clone(),
                port: s.port,
                priority: 0,
                key: None,
                salt: None,
                modifier: None,
            }])
        } else {
            Err("config: задайте [[servers]] (или legacy [server])".into())
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ObfuscationConfig {
    pub key: String,
    #[serde(default = "default_modifier")]
    pub modifier: String,
    #[serde(default = "default_salt")]
    pub salt: u64,
    #[serde(default = "default_padding_min")]
    pub padding_min: u8,
    #[serde(default = "default_padding_max")]
    pub padding_max: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    #[serde(default = "default_action")]
    pub default_action: String,
    #[serde(default)]
    pub rules: Vec<RoutingRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRule {
    pub action: String,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub ip_ranges: Vec<String>,
    #[serde(default)]
    pub geoip: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ClientSettings {
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
    #[serde(default = "default_true")]
    pub auto_redirect: bool,
    #[serde(default = "default_on_server_down")]
    pub on_server_down: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Source IPs to bypass (not redirect through proxy).
    /// Useful for game consoles, smart TVs, etc.
    #[serde(default)]
    pub bypass_ips: Vec<String>,
    /// Number of parallel mux tunnels to keep open to the server.
    /// 0 falls back to the pool's default (4). Multiple tunnels remove
    /// head-of-line blocking when one TCP enters slow-start or recovery.
    #[serde(default = "default_mux_pool_size")]
    pub mux_pool_size: usize,
    /// Drop QUIC (UDP/443) from LAN so browsers fall back to TCP/443,
    /// which the TPROXY redirect can intercept. Without this, any site
    /// advertising h3 bypasses the proxy entirely over UDP.
    #[serde(default = "default_true")]
    pub block_quic: bool,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            listen_port: default_listen_port(),
            auto_redirect: true,
            on_server_down: default_on_server_down(),
            log_level: default_log_level(),
            bypass_ips: vec![],
            mux_pool_size: default_mux_pool_size(),
            block_quic: true,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct HubClientConfig {
    pub url: String,
    pub trusted_public_key: Option<String>,
    pub preset: String,
    #[serde(default = "default_refresh_interval")]
    pub refresh_interval_secs: u64,
}

#[derive(Debug, Deserialize)]
pub struct GeoIpConfig {
    pub database: String,
}

// ── Server config ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub server: ServerListenConfig,
    pub obfuscation: ObfuscationConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub fallback: FallbackConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub udp_relay: Option<UdpRelayServerConfig>,
}

// ── UDP Relay configs ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct UdpRelayClientConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_udp_listen_port")]
    pub listen_port: u16,
    /// VPS host for UDP relay (defaults to server.address)
    pub vps_host: Option<String>,
    #[serde(default = "default_udp_vps_port")]
    pub vps_port: u16,
    #[serde(default = "default_true")]
    pub use_tproxy: bool,
    /// Source IPs to relay (e.g. Switch IP)
    #[serde(default)]
    pub source_ips: Vec<String>,
    /// Destination ports to exclude from relay
    #[serde(default = "default_exclude_ports")]
    pub exclude_dst_ports: Vec<u16>,
    #[serde(default = "default_flow_timeout")]
    pub flow_timeout_sec: u64,
    #[serde(default = "default_keepalive_interval")]
    pub keepalive_interval_sec: u64,
}

#[derive(Debug, Deserialize)]
pub struct UdpRelayServerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_udp_vps_port")]
    pub listen_port: u16,
    /// Port range for incoming connections from other players
    #[serde(default = "default_incoming_port_min")]
    pub incoming_port_min: u16,
    #[serde(default = "default_incoming_port_max")]
    pub incoming_port_max: u16,
    #[serde(default = "default_flow_timeout")]
    pub flow_timeout_sec: u64,
}

#[derive(Debug, Deserialize)]
pub struct ServerListenConfig {
    #[serde(default = "default_listen_addr")]
    pub listen: String,
    pub port: u16,
}

#[derive(Debug, Deserialize)]
pub struct LimitsConfig {
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    #[serde(default = "default_timeout")]
    pub connection_timeout_sec: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_connections: default_max_connections(),
            connection_timeout_sec: default_timeout(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct FallbackConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub response_file: Option<String>,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            response_file: None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default)]
    pub file: Option<String>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            file: None,
        }
    }
}

// ── Defaults ─────────────────────────────────────────────────────────

fn default_modifier() -> String {
    "positional_xor_rotate".into()
}
fn default_salt() -> u64 {
    0xDEADBEEF
}
fn default_padding_min() -> u8 {
    16
}
fn default_padding_max() -> u8 {
    128
}
fn default_action() -> String {
    "direct".into()
}
fn default_listen_port() -> u16 {
    1080
}
fn default_true() -> bool {
    true
}
fn default_on_server_down() -> String {
    // fail-closed по умолчанию: домены в прокси напрямую либо не работают, либо
    // светят реальный IP (риск блокировки аккаунта), поэтому «либо через прокси,
    // либо никак». Перекрывается явным значением в конфиге.
    "block".into()
}
fn default_log_level() -> String {
    "warn".into()
}
fn default_listen_addr() -> String {
    "0.0.0.0".into()
}
fn default_max_connections() -> u32 {
    256
}
fn default_timeout() -> u64 {
    300
}
fn default_udp_listen_port() -> u16 {
    1081
}
fn default_udp_vps_port() -> u16 {
    9999
}
fn default_exclude_ports() -> Vec<u16> {
    vec![53, 67, 68]
}
fn default_flow_timeout() -> u64 {
    120
}
fn default_keepalive_interval() -> u64 {
    25
}
fn default_incoming_port_min() -> u16 {
    45000
}
fn default_incoming_port_max() -> u16 {
    65535
}
fn default_refresh_interval() -> u64 {
    300
}
fn default_mux_pool_size() -> usize {
    4
}

// ── Loaders ──────────────────────────────────────────────────────────

pub fn load_client_config(path: &Path) -> Result<ClientConfig, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let config: ClientConfig = toml::from_str(&content)?;
    Ok(config)
}

pub fn load_server_config(path: &Path) -> Result<ServerConfig, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let config: ServerConfig = toml::from_str(&content)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
[obfuscation]
key = "dGVzdA=="

[routing]
default_action = "direct"
"#;

    /// Конфиги боевых роутеров со старым `[server]` должны работать без
    /// правки: одиночная секция читается как пул из одного primary.
    #[test]
    fn test_legacy_single_server_parses() {
        let toml_str = format!(
            r#"{BASE}
[server]
address = "1.2.3.4"
port = 8443
"#
        );
        let cfg: ClientConfig = toml::from_str(&toml_str).unwrap();
        let entries = cfg.server_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].address, "1.2.3.4");
        assert_eq!(entries[0].port, 8443);
        assert_eq!(entries[0].priority, 0);
        assert!(entries[0].key.is_none());
        assert_eq!(entries[0].display_name(), "1.2.3.4");
    }

    #[test]
    fn test_servers_sorted_by_priority() {
        let toml_str = format!(
            r#"{BASE}
[[servers]]
name = "timeweb"
address = "5.6.7.8"
port = 8443
priority = 1

[[servers]]
name = "aeza"
address = "1.2.3.4"
port = 8443
priority = 0
"#
        );
        let cfg: ClientConfig = toml::from_str(&toml_str).unwrap();
        let entries = cfg.server_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "aeza", "primary must sort first");
        assert_eq!(entries[1].name, "timeweb");
    }

    /// При равных приоритетах порядок файла сохраняется (stable sort),
    /// иначе выбор primary был бы недетерминированным.
    #[test]
    fn test_equal_priority_keeps_file_order() {
        let toml_str = format!(
            r#"{BASE}
[[servers]]
name = "first"
address = "1.1.1.1"
port = 8443

[[servers]]
name = "second"
address = "2.2.2.2"
port = 8443
"#
        );
        let cfg: ClientConfig = toml::from_str(&toml_str).unwrap();
        let entries = cfg.server_entries().unwrap();
        assert_eq!(entries[0].name, "first");
        assert_eq!(entries[1].name, "second");
    }

    /// `[[servers]]` при наличии выигрывает у legacy `[server]`.
    #[test]
    fn test_servers_take_precedence_over_legacy() {
        let toml_str = format!(
            r#"{BASE}
[server]
address = "9.9.9.9"
port = 1111

[[servers]]
name = "pool"
address = "1.2.3.4"
port = 8443
"#
        );
        let cfg: ClientConfig = toml::from_str(&toml_str).unwrap();
        let entries = cfg.server_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].address, "1.2.3.4");
    }

    /// Ни `[server]`, ни `[[servers]]` даёт явную ошибку старта, а не панику
    /// где-то дальше по стеку.
    #[test]
    fn test_no_servers_is_config_error() {
        let cfg: ClientConfig = toml::from_str(BASE).unwrap();
        assert!(cfg.server_entries().is_err());
    }

    /// Per-server override ключа обфускации парсится (кейс «у резерва другой
    /// провайдер и другой ключ», §2.1).
    #[test]
    fn test_per_server_key_override_parses() {
        let toml_str = format!(
            r#"{BASE}
[[servers]]
name = "other"
address = "5.6.7.8"
port = 8443
key = "b3RoZXI="
salt = 42
"#
        );
        let cfg: ClientConfig = toml::from_str(&toml_str).unwrap();
        let entries = cfg.server_entries().unwrap();
        assert_eq!(entries[0].key.as_deref(), Some("b3RoZXI="));
        assert_eq!(entries[0].salt, Some(42));
        assert!(entries[0].modifier.is_none());
    }
}

/// Decode base64 key from config string into raw bytes.
pub fn decode_key(key_str: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(key_str.trim())?;
    if bytes.is_empty() {
        return Err("key must not be empty".into());
    }
    Ok(bytes)
}
