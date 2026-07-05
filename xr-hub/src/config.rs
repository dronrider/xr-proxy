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
    /// Directory the APK self-update files (`<version>.apk`, `manifest.json`,
    /// `manifest.sig`) are served from (LLD-12 §3.4). Defaults to
    /// `<data_dir>/releases`.
    #[serde(default)]
    pub releases_dir: Option<String>,
}

impl ServerConfig {
    /// Resolved path to the releases directory (config override or
    /// `<data_dir>/releases`).
    pub fn releases_path(&self) -> std::path::PathBuf {
        match &self.releases_dir {
            Some(dir) => std::path::PathBuf::from(dir),
            None => std::path::Path::new(&self.data_dir).join("releases"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    pub cert: String,
    pub key: String,
}

#[derive(Debug, Deserialize)]
pub struct AdminConfig {
    pub users: Vec<UserConfig>,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct UserConfig {
    pub username: String,
    pub password_hash: String,
}

#[derive(Debug, Deserialize)]
pub struct SigningConfig {
    pub private_key: String,
}

#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct InviteDefaults {
    /// Legacy одиночный сервер. При заданном `servers` игнорируется:
    /// legacy-поля payload'а заполняются primary'м из списка.
    #[serde(default)]
    pub server_address: String,
    #[serde(default = "default_server_port")]
    pub server_port: u16,
    #[serde(default)]
    pub obfuscation_key: String,
    #[serde(default = "default_modifier")]
    pub modifier: String,
    #[serde(default)]
    pub salt: u64,
    #[serde(default)]
    pub hub_url: String,
    /// Пул серверов для failover на клиенте (LLD-10 §2.8): та же схема, что
    /// `[[servers]]` роутера, в TOML это `[[invites.defaults.servers]]`.
    #[serde(default)]
    pub servers: Vec<xr_proto::preset::PayloadServer>,
}

impl InviteDefaults {
    /// Список серверов payload'а, отсортированный по приоритету. Пустой,
    /// если хаб сконфигурирован только legacy-полями; клиент тогда строит
    /// пул из одного `server_address`.
    pub fn sorted_servers(&self) -> Vec<xr_proto::preset::PayloadServer> {
        let mut servers = self.servers.clone();
        servers.sort_by_key(|s| s.priority);
        servers
    }
}

impl Default for InviteDefaults {
    fn default() -> Self {
        Self {
            server_address: String::new(),
            server_port: default_server_port(),
            obfuscation_key: String::new(),
            modifier: default_modifier(),
            salt: 0,
            hub_url: String::new(),
            servers: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct InvitesConfig {
    #[serde(default)]
    pub dev_mode: bool,
    #[serde(default = "default_ttl")]
    pub default_ttl_seconds: u64,
    #[serde(default = "default_max_ttl")]
    pub max_ttl_seconds: u64,
    #[serde(default)]
    pub defaults: InviteDefaults,
}

impl Default for InvitesConfig {
    fn default() -> Self {
        Self {
            dev_mode: false,
            default_ttl_seconds: default_ttl(),
            max_ttl_seconds: default_max_ttl(),
            defaults: InviteDefaults::default(),
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
fn default_server_port() -> u16 {
    8443
}
fn default_modifier() -> String {
    "positional_xor_rotate".into()
}
