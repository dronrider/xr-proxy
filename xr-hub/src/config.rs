use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct HubConfig {
    pub server: ServerConfig,
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    pub admin: AdminConfig,
    #[serde(default)]
    pub signing: Option<SigningConfig>,
    #[serde(default)]
    pub invites: InvitesConfig,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    pub cert: String,
    pub key: String,
}

#[derive(Debug, Deserialize)]
pub struct AdminConfig {
    pub token: String,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct SigningConfig {
    pub private_key: String,
}

#[derive(Debug, Deserialize)]
pub struct InvitesConfig {
    #[serde(default)]
    pub dev_mode: bool,
    #[serde(default = "default_ttl")]
    pub default_ttl_seconds: u64,
    #[serde(default = "default_max_ttl")]
    pub max_ttl_seconds: u64,
}

impl Default for InvitesConfig {
    fn default() -> Self {
        Self {
            dev_mode: false,
            default_ttl_seconds: default_ttl(),
            max_ttl_seconds: default_max_ttl(),
        }
    }
}

fn default_bind() -> String {
    "0.0.0.0:8080".into()
}
fn default_data_dir() -> String {
    "/var/lib/xr-hub".into()
}
fn default_ttl() -> u64 {
    86400
}
fn default_max_ttl() -> u64 {
    604800
}
