/// Configuration parsing for client and server.
use serde::Deserialize;
use std::path::Path;

// ── Client config ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ClientConfig {
    pub server: ServerAddress,
    pub obfuscation: ObfuscationConfig,
    pub routing: RoutingConfig,
    #[serde(default)]
    pub client: ClientSettings,
    #[serde(default)]
    pub geoip: Option<GeoIpConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ServerAddress {
    pub address: String,
    pub port: u16,
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

#[derive(Debug, Deserialize)]
pub struct RoutingConfig {
    #[serde(default = "default_action")]
    pub default_action: String,
    #[serde(default)]
    pub rules: Vec<RoutingRule>,
}

#[derive(Debug, Deserialize)]
pub struct RoutingRule {
    pub action: String,
    #[serde(default)]
    pub domains: Vec<String>,
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
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            listen_port: default_listen_port(),
            auto_redirect: true,
            on_server_down: default_on_server_down(),
            log_level: default_log_level(),
        }
    }
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
    "direct".into()
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

/// Decode base64 key from config string into raw bytes.
pub fn decode_key(key_str: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(key_str.trim())?;
    if bytes.is_empty() {
        return Err("key must not be empty".into());
    }
    Ok(bytes)
}
