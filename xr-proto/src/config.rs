/// Configuration parsing for client and server.
use crate::obfuscation::ModifierStrategy;
use serde::{Deserialize, Serialize};
use std::net::{SocketAddr, ToSocketAddrs};
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
    /// Название тематической группы («YouTube», «Мессенджеры»), которое хаб
    /// раздаёт вместе с правилом, а клиенты показывают вместо счётчика доменов
    /// (XR-117). Поле опциональное: пресеты, заведённые до него, читаются
    /// по-прежнему, а без значения оно и не сериализуется, поэтому подпись
    /// такого пресета остаётся прежней.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    /// Машинные исключения перехвата (XR-248): готовые условия nftables без
    /// вердикта, например `ip saddr 192.168.1.10 tcp dport != { 80, 443 }`.
    /// Вердикт дописывает потребитель: перехват ставит `return`, kill-switch
    /// ставит `accept`, поэтому одна строка держит обе половины в согласии.
    /// Живут в конфиге машины, а не в init-скрипте: раскладка обвязки конфиг
    /// не переписывает, а init переписывает целиком.
    #[serde(default)]
    pub bypass_rules: Vec<String>,
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
            bypass_rules: vec![],
            mux_pool_size: default_mux_pool_size(),
            block_quic: true,
        }
    }
}

/// Вердикты nftables: своё условие потребитель дописывает сам, поэтому
/// строка с чужим вердиктом внутри уводила бы трафик куда угодно.
pub const NFT_VERDICTS: &[&str] = &[
    "return",
    "accept",
    "drop",
    "reject",
    "redirect",
    "tproxy",
    "dnat",
    "snat",
    "masquerade",
    "jump",
    "goto",
    "queue",
    "log",
];

/// Символы, с которыми машинное условие дальше не идёт: набор правил
/// уезжает в `nft` через `sh -c`, а условие киллсвитча подставляется в
/// команду словами.
pub const BYPASS_RULE_FORBIDDEN_CHARS: &[char] =
    &['\'', '"', ';', '\n', '\\', '#', '$', '`', '&', '|', '<', '>'];

/// Почему машинное условие перехвата (`client.bypass_rules`, XR-248) нельзя
/// дописывать в правило, либо `None`, если можно.
///
/// Критерий тут один на всех потребителей нарочно. Условие читают двое,
/// перехват в `xr-client` и киллсвитч на shell, и первый дописывает ему
/// `return`, а второй `accept`. Разойдись они в том, что считать негодным,
/// и условие встанет только в одной половине: выпущенное из прокси
/// устройство упрётся в общий drop киллсвитча и останется вообще без
/// выхода. Половина на shell повторяет этот же список, паритет закреплён
/// тестом стенда в `xr-setup`.
pub fn bypass_rule_reject_reason(rule: &str) -> Option<&'static str> {
    let rule = rule.trim();
    if rule.is_empty() {
        return Some("пустая строка");
    }
    if rule.contains(BYPASS_RULE_FORBIDDEN_CHARS) {
        return Some("кавычка, подстановка или разделитель команд");
    }
    if rule
        .split_whitespace()
        .any(|w| NFT_VERDICTS.contains(&w.to_ascii_lowercase().as_str()))
    {
        return Some("вердикт внутри условия");
    }
    None
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
    #[serde(default = "default_udp_max_flows")]
    pub max_flows: usize,
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
    #[serde(default = "default_udp_max_flows")]
    pub max_flows: usize,
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
    /// Общий дедлайн хендшейка сервера в секундах (XR-202): не договоривший
    /// вовремя клиент теряет коннект и permit из `max_connections`, а не
    /// продлевает свой срок каждым пришедшим байтом.
    #[serde(default = "default_timeout")]
    pub connection_timeout_sec: u64,
    /// Кап стримов на весь сервер (XR-199). `max_connections` считает
    /// TCP-коннекты, а в mux-сессии стримов сколько угодно, и каждый стоит fd
    /// апстрима с парой тасок: без этого капа fd на VPS кончаются раньше, чем
    /// сработает лимит коннектов.
    #[serde(default = "default_max_streams")]
    pub max_streams: u32,
    /// Доля одной mux-сессии в общем капе: без неё первый жадный клиент
    /// выбирает `max_streams` целиком и глушит соседние роутеры.
    #[serde(default = "default_max_streams_per_mux")]
    pub max_streams_per_mux: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_connections: default_max_connections(),
            connection_timeout_sec: default_timeout(),
            max_streams: default_max_streams(),
            max_streams_per_mux: default_max_streams_per_mux(),
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
fn default_max_streams() -> u32 {
    4096
}
fn default_max_streams_per_mux() -> u32 {
    512
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
fn default_udp_max_flows() -> usize {
    1024
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

// --- Validation -------------------------------------------------------

/// Сухая проверка конфига клиента (XR-227): всё, что валяет процесс на
/// боевом старте, называется здесь по причине, до листенеров и файрвола.
/// Проверяет то же, что потребует старт: годный для validate конфиг
/// стартует, а конфиг, не прошедший validate, стартом падает.
pub fn validate_client_config(config: &ClientConfig) -> Result<(), String> {
    let entries = config.server_entries()?;
    for entry in &entries {
        let section = format!("servers '{}'", entry.display_name());
        let key = entry.key.as_deref().unwrap_or(&config.obfuscation.key);
        let modifier = entry.modifier.as_deref().unwrap_or(&config.obfuscation.modifier);
        let salt = entry.salt.unwrap_or(config.obfuscation.salt);
        obfuscation_reject_reason(&section, key, modifier, salt)?;
        // Пул подключается по разобранному SocketAddr, доменное имя там не
        // работает, поэтому адрес судится так же строго.
        format!("{}:{}", entry.address, entry.port)
            .parse::<SocketAddr>()
            .map_err(|e| format!("{section}: invalid address '{}': {}", entry.address, e))?;
    }
    if let Some(udp) = config.udp_relay.as_ref().filter(|u| u.enabled) {
        // Relay ходит через primary, его адрес и подставляется без своего
        // vps_host; мусор в адресе на старте роняет только задачу relay,
        // и без проверки это молчание неотличимо от работающего перехвата.
        let host = udp.vps_host.as_deref().unwrap_or(&entries[0].address);
        format!("{}:{}", host, udp.vps_port)
            .parse::<SocketAddr>()
            .map_err(|e| format!("udp_relay: invalid vps address '{host}': {e}"))?;
    }
    Ok(())
}

/// Сухая проверка конфига сервера, см. `validate_client_config`.
pub fn validate_server_config(config: &ServerConfig) -> Result<(), String> {
    obfuscation_reject_reason(
        "obfuscation",
        &config.obfuscation.key,
        &config.obfuscation.modifier,
        config.obfuscation.salt,
    )?;
    let bind = format!("{}:{}", config.server.listen, config.server.port);
    bind.to_socket_addrs()
        .map_err(|e| format!("server: invalid listen address '{bind}': {e}"))?;
    Ok(())
}

/// Почему тройка ключ-модификатор-соль не годится, либо `Ok(())`.
fn obfuscation_reject_reason(
    section: &str,
    key: &str,
    modifier: &str,
    salt: u64,
) -> Result<(), String> {
    decode_key(key).map_err(|e| format!("{section}.key: {e}"))?;
    ModifierStrategy::from_str(modifier).ok_or_else(|| {
        format!(
            "{section}.modifier: unknown modifier strategy '{modifier}' \
             (positional_xor_rotate, rotating_salt, substitution_table)"
        )
    })?;
    // Обфускатор берёт salt как u32, большее значение молча обрезается:
    // клиент и сервер обрежут одинаково и связи это не рвёт, но заданное
    // в конфиге число тогда врёт, и поймать это можно только здесь.
    if salt > u32::MAX as u64 {
        return Err(format!(
            "{section}.salt: {salt} does not fit in u32 and is truncated to {} at runtime",
            salt as u32
        ));
    }
    Ok(())
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

    /// Критерий отбраковки машинных условий один на перехват и на киллсвитч
    /// (XR-248): годное принимают обе половины, негодное отвергают обе.
    /// Разъезд тут стоит устройству выхода целиком, поэтому границы описаны
    /// поимённо, а паритет с половиной на shell закреплён стендом xr-setup.
    #[test]
    fn bypass_rule_criteria_name_every_reason() {
        for good in [
            "ip saddr 192.0.2.10 tcp dport != { 80, 443 }",
            "ip daddr 198.51.100.7 tcp dport 8443",
            "  iifname br-lan ip saddr 192.0.2.11  ",
        ] {
            assert_eq!(bypass_rule_reject_reason(good), None, "годное: {good:?}");
        }

        for (bad, reason) in [
            ("", "пустая строка"),
            ("   ", "пустая строка"),
            ("ip saddr $nas", "кавычка, подстановка или разделитель команд"),
            ("ip saddr `id`", "кавычка, подстановка или разделитель команд"),
            ("ip saddr 192.0.2.10 'x'", "кавычка, подстановка или разделитель команд"),
            ("ip saddr 192.0.2.10 \"x\"", "кавычка, подстановка или разделитель команд"),
            ("ip saddr 192.0.2.10; reboot", "кавычка, подстановка или разделитель команд"),
            ("ip saddr 192.0.2.10 && reboot", "кавычка, подстановка или разделитель команд"),
            ("ip saddr 192.0.2.10 | tee /tmp/x", "кавычка, подстановка или разделитель команд"),
            ("ip saddr 192.0.2.10 > /tmp/x", "кавычка, подстановка или разделитель команд"),
            ("ip saddr 192.0.2.10 # хвост", "кавычка, подстановка или разделитель команд"),
            ("ip saddr 192.0.2.10 accept", "вердикт внутри условия"),
            ("ip saddr 192.0.2.10 ACCEPT", "вердикт внутри условия"),
            ("ip saddr 192.0.2.10 jump chain", "вердикт внутри условия"),
        ] {
            assert_eq!(bypass_rule_reject_reason(bad), Some(reason), "негодное: {bad:?}");
        }
    }

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

    /// XR-117: имя группы читается из TOML, а правило без имени остаётся
    /// валидным. Пресеты и конфиги роутеров, заведённые до поля, лежат на
    /// диске без него, и снятие `#[serde(default)]` сломало бы им разбор.
    #[test]
    fn test_rule_name_is_optional() {
        let toml_str = format!(
            r#"{BASE}
[[routing.rules]]
name = "YouTube"
action = "proxy"
domains = ["youtube.com"]

[[routing.rules]]
action = "proxy"
domains = ["x.com"]
"#
        );
        let cfg: ClientConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(cfg.routing.rules[0].name.as_deref(), Some("YouTube"));
        assert!(cfg.routing.rules[1].name.is_none());
    }

    /// Правило без имени сериализуется ровно так, как до XR-117. От этого
    /// зависит подпись пресета в хабе: она считается по JSON правил, и лишний
    /// `"name":null` сделал бы недействительными все выданные подписи.
    #[test]
    fn test_rule_without_name_serializes_as_before() {
        let rule = RoutingRule {
            name: None,
            action: "proxy".into(),
            domains: vec!["youtube.com".into()],
            ip_ranges: vec![],
            geoip: vec![],
        };
        assert_eq!(
            serde_json::to_string(&rule).unwrap(),
            r#"{"action":"proxy","domains":["youtube.com"],"ip_ranges":[],"geoip":[]}"#
        );

        let named = RoutingRule {
            name: Some("YouTube".into()),
            ..rule
        };
        assert_eq!(
            serde_json::to_string(&named).unwrap(),
            r#"{"name":"YouTube","action":"proxy","domains":["youtube.com"],"ip_ranges":[],"geoip":[]}"#
        );
    }

    /// Эталонный пресет из `configs/routing-russia.toml` разбирается и весь
    /// состоит из именованных групп: по нему заводится боевой пресет хаба, и
    /// правило без имени показалось бы там голым счётчиком доменов.
    #[test]
    fn test_reference_russia_preset_groups_are_named() {
        #[derive(Deserialize)]
        struct Wrapper {
            routing: RoutingConfig,
        }
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../configs/routing-russia.toml");
        let content = std::fs::read_to_string(path).unwrap();
        let cfg: Wrapper = toml::from_str(&content).unwrap();
        let names: Vec<&str> = cfg
            .routing
            .rules
            .iter()
            .map(|r| r.name.as_deref().unwrap_or(""))
            .collect();
        assert!(!names.contains(&""), "группа без имени: {names:?}");
        // Диапазоны Telegram лежат в одной группе с его доменами: часть клиентов
        // ходит мимо DNS, по голым адресам, и разнесённые по разным группам они
        // разъедутся при следующей правке. Группу ищем по домену, а не по имени:
        // имена держатся в соответствии с боевым пресетом и там переименовываются.
        let telegram = cfg
            .routing
            .rules
            .iter()
            .find(|r| r.domains.iter().any(|d| d.ends_with("telegram.org")))
            .expect("в пресете нет группы с доменами Telegram");
        assert!(telegram.name.is_some());
        assert!(telegram.ip_ranges.contains(&"91.108.56.0/22".to_string()));
    }

    /// Пресет, скачанный с хаба и лежащий в кэше клиента без поля `name`,
    /// разбирается по-прежнему (JSON-путь, в отличие от TOML выше).
    #[test]
    fn test_preset_json_without_name_parses() {
        let stored = r#"{
            "default_action": "direct",
            "rules": [
                {"action": "proxy", "domains": ["youtube.com"], "ip_ranges": [], "geoip": []}
            ]
        }"#;
        let cfg: RoutingConfig = serde_json::from_str(stored).unwrap();
        assert!(cfg.rules[0].name.is_none());
        assert_eq!(cfg.rules[0].domains, vec!["youtube.com"]);
    }

    fn client_with(extra: &str) -> ClientConfig {
        let toml_str = format!(
            r#"{BASE}
[[servers]]
name = "primary"
address = "192.0.2.10"
port = 8443
{extra}"#
        );
        toml::from_str(&toml_str).unwrap()
    }

    fn server_with(extra: &str) -> ServerConfig {
        let toml_str = format!(
            r#"[server]
listen = "0.0.0.0"
port = 443

[obfuscation]
key = "dGVzdA=="
{extra}"#
        );
        toml::from_str(&toml_str).unwrap()
    }

    /// Годный конфиг проходит сухую проверку целиком: ложные срабатывания
    /// здесь дороже пропусков, validate обязан соглашаться со стартом.
    #[test]
    fn validate_accepts_good_client_config() {
        let cfg = client_with("");
        validate_client_config(&cfg).unwrap();

        let cfg = client_with("key = \"b3RoZXI=\"\nsalt = 42\nmodifier = \"rotating_salt\"");
        validate_client_config(&cfg).unwrap();

        let cfg: ClientConfig = toml::from_str(&format!(
            r#"{BASE}
[[servers]]
address = "192.0.2.10"
port = 8443

[udp_relay]
enabled = true
vps_host = "198.51.100.7"
vps_port = 9999
"#
        ))
        .unwrap();
        validate_client_config(&cfg).unwrap();
    }

    #[test]
    fn validate_rejects_client_config_without_servers() {
        let cfg: ClientConfig = toml::from_str(BASE).unwrap();
        let err = validate_client_config(&cfg).unwrap_err();
        assert!(err.contains("[[servers]]"), "{err}");
    }

    #[test]
    fn validate_names_reject_reasons() {
        // client_with несёт годный ключ и адрес, портим их точечно.
        let mut cfg = client_with("");
        cfg.obfuscation.key = "".into();
        let err = validate_client_config(&cfg).unwrap_err();
        assert!(err.contains("key must not be empty"), "{err}");

        cfg.obfuscation.key = "не-base64!".into();
        let err = validate_client_config(&cfg).unwrap_err();
        assert!(err.contains("servers 'primary'.key"), "{err}");
        assert!(err.contains("Invalid symbol"), "{err}");

        cfg.obfuscation.key = "dGVzdA==".into();
        cfg.obfuscation.modifier = "xor".into();
        let err = validate_client_config(&cfg).unwrap_err();
        assert!(err.contains("unknown modifier strategy 'xor'"), "{err}");

        cfg.obfuscation.modifier = "rotating_salt".into();
        cfg.obfuscation.salt = u32::MAX as u64 + 1;
        let err = validate_client_config(&cfg).unwrap_err();
        assert!(err.contains("does not fit in u32"), "{err}");

        cfg.obfuscation.salt = 42;
        cfg.servers[0].address = "vps.example.com".into();
        let err = validate_client_config(&cfg).unwrap_err();
        assert!(err.contains("invalid address 'vps.example.com'"), "{err}");
    }

    /// Override ключа в записи пула проверяется наравне с общим: у резерва
    /// бывает свой ключ, и его мусор роняет старт так же.
    #[test]
    fn validate_checks_per_server_override() {
        let mut cfg = client_with("key = \"!!!\"");
        let err = validate_client_config(&cfg).unwrap_err();
        assert!(err.contains("servers 'primary'.key"), "{err}");

        cfg.servers[0].key = Some("dGVzdA==".into());
        cfg.servers[0].modifier = Some("nope".into());
        let err = validate_client_config(&cfg).unwrap_err();
        assert!(err.contains("servers 'primary'.modifier"), "{err}");
    }

    /// Адрес VPS для relay обязателен только у включённого relay: выключенный
    /// мусор в секции старту не мешает и validate зря ругаться не должен.
    #[test]
    fn validate_checks_udp_relay_vps_address_only_when_enabled() {
        let toml_str = format!(
            r#"{BASE}
[[servers]]
address = "192.0.2.10"
port = 8443

[udp_relay]
enabled = false
vps_host = "not an address"
"#
        );
        let cfg: ClientConfig = toml::from_str(&toml_str).unwrap();
        validate_client_config(&cfg).unwrap();

        let toml_str = toml_str.replace("enabled = false", "enabled = true");
        let cfg: ClientConfig = toml::from_str(&toml_str).unwrap();
        let err = validate_client_config(&cfg).unwrap_err();
        assert!(err.contains("invalid vps address 'not an address'"), "{err}");
    }

    #[test]
    fn validate_accepts_good_server_config() {
        validate_server_config(&server_with("")).unwrap();
    }

    #[test]
    fn validate_names_server_reject_reasons() {
        let mut cfg = server_with("");
        cfg.obfuscation.key = "".into();
        let err = validate_server_config(&cfg).unwrap_err();
        assert!(err.contains("obfuscation.key"), "{err}");
        assert!(err.contains("key must not be empty"), "{err}");

        cfg.obfuscation.key = "dGVzdA==".into();
        cfg.obfuscation.salt = 1 << 40;
        let err = validate_server_config(&cfg).unwrap_err();
        assert!(err.contains("obfuscation.salt"), "{err}");

        cfg.obfuscation.salt = 42;
        cfg.server.listen = "not an address".into();
        let err = validate_server_config(&cfg).unwrap_err();
        assert!(err.contains("invalid listen address"), "{err}");
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
