//! Конфиг движка персонального клиента: сборка из профиля и разбор обратно.
//!
//! Обе половины живут здесь, а не в обвязке платформы (XR-271). Раньше JSON
//! конфига собирал Kotlin строкой с самодельным экранированием, а разбирал его
//! ad-hoc парсер JNI-крейта: знание о том, какие поля движок ждёт и чем их
//! заполнять по умолчанию, было размазано по двум языкам и одному
//! Android-специфичному крейту. Порту под другую платформу пришлось бы писать
//! ту же сборку третий раз, а расхождение в дефолте (например, в
//! `default_action`) молча меняло бы маршрутизацию.
//!
//! Платформа отдаёт сюда профиль сервера, свои правила и список резолверов
//! системы ([`ClientProfile`]), получает готовый JSON и передаёт его движку;
//! движок читает его же [`parse_config`].

use serde::{Deserialize, Serialize};

use xr_proto::config::RoutingConfig;
use xr_proto::user_rule::UserRule;

use crate::engine::VpnConfig;

/// Действие для трафика, не попавшего ни под одно правило. Одно на конфиг
/// старта и на применение правил к живому туннелю: разъедься эти два места,
/// правка правил меняла бы заодно и маршрут по умолчанию.
pub const DEFAULT_ROUTING_ACTION: &str = "direct";

/// Порт сервера, когда профиль его не назвал.
const DEFAULT_SERVER_PORT: u16 = 8443;

/// Модификатор обфускации по умолчанию (тот же, что у справочных конфигов).
const DEFAULT_MODIFIER: &str = "positional_xor_rotate";

/// Salt по умолчанию.
const DEFAULT_SALT: u64 = 0xDEAD_BEEF;

/// Границы случайного паддинга кадра.
const DEFAULT_PADDING_MIN: u8 = 16;
const DEFAULT_PADDING_MAX: u8 = 128;

/// Как часто движок перечитывает пресет хаба, секунды.
const HUB_REFRESH_INTERVAL_SECS: u64 = 300;

/// Один адрес в пуле профиля (LLD-10): порядок в списке и есть приоритет.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileEndpoint {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub port: u16,
}

/// Профиль активного сервера в том виде, в каком его держит приложение.
/// Всё, чего в нём нет, добирается дефолтами этого модуля.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ClientProfile {
    /// Легаси-адрес профиля: работает, когда пул пуст.
    #[serde(default)]
    pub server_address: String,
    #[serde(default)]
    pub server_port: u16,
    /// Пул адресов, порядок и есть приоритет.
    #[serde(default)]
    pub servers: Vec<ProfileEndpoint>,
    #[serde(default)]
    pub obfuscation_key: String,
    #[serde(default)]
    pub modifier: String,
    #[serde(default)]
    pub salt: Option<u64>,
    #[serde(default)]
    pub hub_url: String,
    #[serde(default)]
    pub hub_preset: String,
    #[serde(default)]
    pub hub_cache_dir: String,
    /// Мои правила поверх пресета (LLD-05), в порядке пользователя.
    #[serde(default)]
    pub user_rules: Vec<UserRule>,
    /// Резолверы системы как их отдала платформа, без чистки.
    #[serde(default)]
    pub dns_resolvers: Vec<String>,
    /// Политика при отказе всего пула: `true` = block (fail-closed), `false` =
    /// direct. Отсутствие поля читается как block: молчание конфига не должно
    /// выпускать проксируемый трафик напрямую.
    #[serde(default)]
    pub fail_closed: Option<bool>,
}

/// Собрать JSON конфига движка из профиля. `Err` только когда сервера в
/// профиле нет вовсе: пустой адрес дал бы конфиг, на котором движок всё равно
/// не поднимется, а сказать об этом надо до старта туннеля.
pub fn build_config_json(profile: &ClientProfile) -> Result<String, String> {
    let endpoints = effective_endpoints(profile);
    let primary = endpoints.first().ok_or_else(|| "no_server".to_string())?;

    let servers: Vec<serde_json::Value> = endpoints
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name,
                "address": e.address,
                "port": e.port,
            })
        })
        .collect();

    let user_rules: Vec<serde_json::Value> = profile
        .user_rules
        .iter()
        .map(|r| serde_json::json!({ "action": r.action, "pattern": r.pattern }))
        .collect();

    let modifier = non_blank(&profile.modifier).unwrap_or(DEFAULT_MODIFIER);
    let salt = profile.salt.filter(|v| *v != 0).unwrap_or(DEFAULT_SALT);
    let on_server_down = if profile.fail_closed.unwrap_or(true) {
        "block"
    } else {
        "direct"
    };

    let mut config = serde_json::json!({
        "server_address": primary.address,
        "server_port": primary.port,
        "servers": servers,
        "obfuscation_key": profile.obfuscation_key,
        "modifier": modifier,
        "salt": salt,
        "padding_min": DEFAULT_PADDING_MIN,
        "padding_max": DEFAULT_PADDING_MAX,
        "default_action": DEFAULT_ROUTING_ACTION,
        "user_rules": user_rules,
        "on_server_down": on_server_down,
        "dns_resolvers": usable_resolvers(&profile.dns_resolvers),
    });

    // Пресет хаба доклеивается только целиком: без имени пресета качать нечего,
    // без адреса хаба неоткуда.
    if let (Some(hub_url), Some(preset)) = (
        non_blank(&profile.hub_url),
        non_blank(&profile.hub_preset),
    ) {
        let obj = config.as_object_mut().expect("config is an object");
        obj.insert("hub_url".into(), hub_url.into());
        obj.insert("hub_preset".into(), preset.into());
        obj.insert("hub_cache_dir".into(), profile.hub_cache_dir.clone().into());
        obj.insert(
            "hub_refresh_interval_secs".into(),
            HUB_REFRESH_INTERVAL_SECS.into(),
        );
    }

    Ok(config.to_string())
}

/// Разобрать профиль из JSON платформы.
pub fn parse_client_profile(json: &str) -> Result<ClientProfile, String> {
    serde_json::from_str::<ClientProfile>(json).map_err(|e| format!("profile parse: {e}"))
}

/// Пул профиля: сначала список `servers`, иначе одиночный легаси-адрес.
/// Записи без адреса выбрасываются, нулевой порт читается как дефолтный.
fn effective_endpoints(profile: &ClientProfile) -> Vec<ProfileEndpoint> {
    let pool: Vec<ProfileEndpoint> = profile
        .servers
        .iter()
        .filter_map(|e| {
            let address = e.address.trim();
            if address.is_empty() {
                return None;
            }
            Some(ProfileEndpoint {
                name: e.name.trim().to_string(),
                address: address.to_string(),
                port: if e.port == 0 { DEFAULT_SERVER_PORT } else { e.port },
            })
        })
        .collect();
    if !pool.is_empty() {
        return pool;
    }
    let legacy = profile.server_address.trim();
    if legacy.is_empty() {
        return Vec::new();
    }
    vec![ProfileEndpoint {
        name: String::new(),
        address: legacy.to_string(),
        port: if profile.server_port == 0 {
            DEFAULT_SERVER_PORT
        } else {
            profile.server_port
        },
    }]
}

/// Резолверы, которые движку есть смысл спрашивать. Адрес самого туннеля
/// (`10.0.0.0/24`, там сидит fake-DNS клиента), loopback и link-local
/// выбрасываются: спрашивать через них значит спрашивать самого себя или
/// никого. Порядок системы сохраняется, повторы уходят.
fn usable_resolvers(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in raw {
        let ip = item.trim();
        if ip.is_empty()
            || ip.starts_with("10.0.0.")
            || ip == "127.0.0.1"
            || ip.starts_with("169.254.")
        {
            continue;
        }
        if !out.iter().any(|seen| seen == ip) {
            out.push(ip.to_string());
        }
    }
    out
}

fn non_blank(s: &str) -> Option<&str> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

// -- Разбор конфига движком ------------------------------------------

/// Достать строковое значение ключа из JSON вручную, посимвольно разбирая
/// эскейпы, чтобы вложенный TOML (`routing_toml`) с кавычками внутри не
/// обрывался на первой же `\"`. Первая версия искала закрывающую кавычку
/// простым `find('"')` и резала правила маршрутизации, а `toml::from_str`
/// молча падал на дефолт «проксировать всё».
fn json_get_str(json: &str, key: &str) -> Result<String, String> {
    let pattern = format!("\"{}\"", key);
    let pos = json.find(&pattern).ok_or(format!("missing {}", key))?;
    let after = &json[pos + pattern.len()..];
    let start = after.find('"').ok_or(format!("bad value for {}", key))? + 1;
    let rest = &after[start..];

    let mut result = String::new();
    let mut iter = rest.chars();
    while let Some(c) = iter.next() {
        if c == '\\' {
            match iter.next() {
                Some('n') => result.push('\n'),
                Some('"') => result.push('"'),
                Some('\\') => result.push('\\'),
                // org.json на Android всегда пишет `/` как `\/`, и после
                // пересериализации конфига сервисом этот эскейп несёт каждый
                // URL и почти каждый base64-ключ (XR-118).
                Some('/') => result.push('/'),
                Some('t') => result.push('\t'),
                Some('r') => result.push('\r'),
                Some(other) => {
                    // Неизвестный эскейп оставляем как есть, разбор не роняем.
                    result.push('\\');
                    result.push(other);
                }
                None => return Err(format!("unterminated escape in {}", key)),
            }
        } else if c == '"' {
            // Конец строкового значения (неэкранированная кавычка).
            return Ok(result);
        } else {
            result.push(c);
        }
    }
    Err(format!("unterminated {}", key))
}

/// Конфиг движка из JSON, собранного [`build_config_json`] (или сохранённого
/// прежней версией приложения).
pub fn parse_config(json: &str) -> Result<VpnConfig, String> {
    let get_str = |key: &str| json_get_str(json, key);

    let get_num = |key: &str| -> Result<u64, String> {
        let pattern = format!("\"{}\"", key);
        let pos = json.find(&pattern).ok_or(format!("missing {}", key))?;
        let after = &json[pos + pattern.len()..];
        let colon = after.find(':').ok_or(format!("bad {}", key))? + 1;
        let rest = after[colon..].trim_start();
        let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        num_str.parse().map_err(|_| format!("bad number for {}", key))
    };

    let server_address = get_str("server_address")?;
    let server_port = get_num("server_port")? as u16;
    let obfuscation_key = get_str("obfuscation_key")?;
    let modifier = get_str("modifier").unwrap_or_else(|_| DEFAULT_MODIFIER.into());
    let salt = get_num("salt").unwrap_or(DEFAULT_SALT) as u32;
    let padding_min = get_num("padding_min").unwrap_or(DEFAULT_PADDING_MIN as u64) as u8;
    let padding_max = get_num("padding_max").unwrap_or(DEFAULT_PADDING_MAX as u64) as u8;
    let on_server_down = get_str("on_server_down").unwrap_or_else(|_| "block".into());

    // Пользовательские правила (LLD-05): массив `user_rules` главнее
    // `routing_toml`. Легаси-ветка с TOML остаётся для старых конфигов.
    let routing = if let Some(cfg) = parse_user_rules(json) {
        cfg
    } else if let Ok(toml_str) = get_str("routing_toml") {
        toml::from_str::<RoutingConfig>(&toml_str).unwrap_or_else(|e| {
            tracing::warn!("Failed to parse routing TOML: {}", e);
            default_routing()
        })
    } else {
        default_routing()
    };

    let hub_url = get_str("hub_url").ok();
    let hub_preset = get_str("hub_preset").ok();
    let hub_cache_dir = get_str("hub_cache_dir").ok();
    let hub_refresh_interval_secs = get_num("hub_refresh_interval_secs").ok();
    let mux_pool_size = get_num("mux_pool_size").map(|v| v as usize).unwrap_or(0);

    let dns_resolvers = parse_dns_resolvers(json);
    let servers = parse_servers(json);

    Ok(VpnConfig {
        server_address,
        server_port,
        servers,
        obfuscation_key,
        modifier,
        salt,
        padding_min,
        padding_max,
        routing,
        geoip_path: None,
        on_server_down,
        dns_resolvers,
        hub_url,
        hub_preset,
        hub_cache_dir,
        hub_refresh_interval_secs,
        // Ставится при старте движка, не из JSON: резолвер это колбэк
        // платформы, а не значение конфига.
        system_resolver: None,
        mux_pool_size,
    })
}

/// Массив `servers` (LLD-10): `[{"name":"aeza","address":"1.2.3.4",
/// "port":8443}, ...]`, порядок в массиве и есть приоритет. Разбирается
/// полноценным serde_json (в отличие от остального ad-hoc парсера): это
/// вложенные объекты, ручной парсер их не потянет. Отсутствие ключа или
/// битый JSON дают пустой список, движок тогда работает по legacy-полю.
fn parse_servers(json: &str) -> Vec<xr_proto::config::ServerEntry> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return vec![];
    };
    let Some(arr) = value.get("servers").and_then(|v| v.as_array()) else {
        return vec![];
    };
    arr.iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            let address = item.get("address")?.as_str()?.trim().to_string();
            if address.is_empty() {
                return None;
            }
            Some(xr_proto::config::ServerEntry {
                name: item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                address,
                port: item
                    .get("port")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(DEFAULT_SERVER_PORT as u64) as u16,
                priority: idx as u32,
                key: None,
                salt: None,
                modifier: None,
            })
        })
        .collect()
}

/// Собирает `RoutingConfig` из массива `user_rules` (LLD-05): `[{"action":
/// "proxy","pattern":"*.github.com"}, ...]` плюс строка `default_action`
/// рядом. `None`, когда ключа нет вовсе (легаси-конфиг со старым приложением).
/// Битые записи выбрасывает `to_routing_config` с WARN, старт туннеля они не
/// валят.
fn parse_user_rules(json: &str) -> Option<RoutingConfig> {
    let value = serde_json::from_str::<serde_json::Value>(json).ok()?;
    let arr = value.get("user_rules")?.as_array()?;
    let rules: Vec<UserRule> = arr
        .iter()
        .filter_map(|item| serde_json::from_value(item.clone()).ok())
        .collect();
    let default_action = value
        .get("default_action")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_ROUTING_ACTION);
    Some(xr_proto::user_rule::to_routing_config(&rules, default_action))
}

/// Массив строк `dns_resolvers`, например `["1.1.1.1", "8.8.8.8:53"]`.
/// Отсутствие ключа даёт пустой список, лишние пробелы не мешают.
fn parse_dns_resolvers(json: &str) -> Vec<String> {
    let key = "\"dns_resolvers\"";
    let pos = match json.find(key) {
        Some(p) => p,
        None => return vec![],
    };
    let after = &json[pos + key.len()..];
    let bracket = match after.find('[') {
        Some(p) => p,
        None => return vec![],
    };
    let close = match after[bracket..].find(']') {
        Some(p) => p,
        None => return vec![],
    };
    let inner = &after[bracket + 1..bracket + close];
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    for c in inner.chars() {
        match c {
            '"' if !in_string => in_string = true,
            '"' if in_string => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                in_string = false;
            }
            _ if in_string => current.push(c),
            _ => {}
        }
    }
    out
}

fn default_routing() -> RoutingConfig {
    RoutingConfig {
        default_action: "proxy".into(),
        rules: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_json(extra: &str) -> String {
        format!(
            r#"{{"server_address":"1.2.3.4","server_port":8443,
                 "obfuscation_key":"a2V5","modifier":"positional_xor_rotate",
                 "salt":3735928559{}}}"#,
            extra
        )
    }

    /// Собранный конфиг читается тем же разбором, каким его читает движок:
    /// пул, ключ, паддинг и политика отказа доезжают без потерь.
    #[test]
    fn build_round_trips_through_parse() {
        let profile = parse_client_profile(&profile_json(
            r#","servers":[{"name":"aeza","address":"1.2.3.4","port":8443},
                           {"name":"timeweb","address":"5.6.7.8","port":9000}],
               "user_rules":[{"action":"proxy","pattern":"youtube.com"}],
               "dns_resolvers":["77.88.8.8"],"fail_closed":false"#,
        ))
        .unwrap();

        let json = build_config_json(&profile).unwrap();
        let cfg = parse_config(&json).unwrap();

        assert_eq!(cfg.server_address, "1.2.3.4");
        assert_eq!(cfg.server_port, 8443);
        assert_eq!(cfg.servers.len(), 2);
        assert_eq!(cfg.servers[1].address, "5.6.7.8");
        assert_eq!(cfg.servers[1].priority, 1);
        assert_eq!(cfg.obfuscation_key, "a2V5");
        assert_eq!(cfg.salt, 0xDEAD_BEEF);
        assert_eq!(cfg.padding_min, 16);
        assert_eq!(cfg.padding_max, 128);
        assert_eq!(cfg.on_server_down, "direct");
        assert_eq!(cfg.routing.default_action, DEFAULT_ROUTING_ACTION);
        assert_eq!(cfg.routing.rules.len(), 1);
        assert_eq!(cfg.dns_resolvers, vec!["77.88.8.8"]);
        assert!(cfg.hub_url.is_none());
    }

    /// Primary берётся из головы пула, а не из легаси-полей: они могли
    /// отстать от списка.
    #[test]
    fn build_takes_primary_from_pool() {
        let profile = parse_client_profile(&profile_json(
            r#","servers":[{"address":"9.9.9.9","port":443},{"address":"1.2.3.4"}]"#,
        ))
        .unwrap();
        let cfg = parse_config(&build_config_json(&profile).unwrap()).unwrap();
        assert_eq!(cfg.server_address, "9.9.9.9");
        assert_eq!(cfg.server_port, 443);
        // Запись без порта получает дефолтный.
        assert_eq!(cfg.servers[1].port, 8443);
    }

    /// Легаси-профиль без пула поднимается по одиночному адресу, как раньше.
    #[test]
    fn build_falls_back_to_legacy_address() {
        let profile = parse_client_profile(&profile_json("")).unwrap();
        let cfg = parse_config(&build_config_json(&profile).unwrap()).unwrap();
        assert_eq!(cfg.server_address, "1.2.3.4");
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.servers[0].address, "1.2.3.4");
    }

    /// Профиля без единого адреса конфигом не прикидываемся.
    #[test]
    fn build_without_server_is_an_error() {
        let profile = parse_client_profile(r#"{"obfuscation_key":"a2V5"}"#).unwrap();
        assert_eq!(build_config_json(&profile).unwrap_err(), "no_server");

        // Пул из одних пустых адресов это тот же случай.
        let empty_pool =
            parse_client_profile(r#"{"servers":[{"address":"  "}],"obfuscation_key":"a2V5"}"#)
                .unwrap();
        assert_eq!(build_config_json(&empty_pool).unwrap_err(), "no_server");
    }

    /// Кавычка и обратный слэш в имени сервера переживают сборку: ровно на
    /// этом ломалась ручная склейка строки в приложении.
    #[test]
    fn build_escapes_quotes_in_names() {
        let profile = parse_client_profile(
            r#"{"servers":[{"name":"дом \"дача\" \\ офис","address":"1.2.3.4","port":8443}],
                "obfuscation_key":"a2V5"}"#,
        )
        .unwrap();
        let json = build_config_json(&profile).unwrap();
        let cfg = parse_config(&json).unwrap();
        assert_eq!(cfg.servers[0].name, "дом \"дача\" \\ офис");
        assert_eq!(cfg.servers[0].address, "1.2.3.4");
    }

    /// Отсутствие политики отказа читается как fail-closed: молчание не должно
    /// выпускать проксируемый трафик мимо туннеля.
    #[test]
    fn build_defaults_to_fail_closed() {
        let profile = parse_client_profile(&profile_json("")).unwrap();
        let cfg = parse_config(&build_config_json(&profile).unwrap()).unwrap();
        assert_eq!(cfg.on_server_down, "block");
    }

    /// Поля хаба едут только парой: одного имени пресета без адреса хаба (и
    /// наоборот) движку мало.
    #[test]
    fn build_hub_fields_go_together() {
        let full = parse_client_profile(&profile_json(
            r#","hub_url":"https://hub.example","hub_preset":"russia","hub_cache_dir":"/data/presets""#,
        ))
        .unwrap();
        let cfg = parse_config(&build_config_json(&full).unwrap()).unwrap();
        assert_eq!(cfg.hub_url.as_deref(), Some("https://hub.example"));
        assert_eq!(cfg.hub_preset.as_deref(), Some("russia"));
        assert_eq!(cfg.hub_cache_dir.as_deref(), Some("/data/presets"));
        assert_eq!(cfg.hub_refresh_interval_secs, Some(300));

        let half = parse_client_profile(&profile_json(r#","hub_url":"https://hub.example""#)).unwrap();
        let cfg = parse_config(&build_config_json(&half).unwrap()).unwrap();
        assert!(cfg.hub_url.is_none());
        assert!(cfg.hub_preset.is_none());
    }

    /// Резолверы системы чистятся до сборки: свой же fake-DNS туннеля,
    /// loopback и link-local спрашивать бессмысленно, повторы не нужны.
    #[test]
    fn build_drops_unusable_resolvers() {
        let profile = parse_client_profile(&profile_json(
            r#","dns_resolvers":["10.0.0.1","127.0.0.1","169.254.1.1","8.8.8.8"," 8.8.8.8 ","1.1.1.1"]"#,
        ))
        .unwrap();
        let cfg = parse_config(&build_config_json(&profile).unwrap()).unwrap();
        assert_eq!(cfg.dns_resolvers, vec!["8.8.8.8", "1.1.1.1"]);
    }

    /// Пустой профиль без модификатора и salt получает те же дефолты, что
    /// раньше подставляло приложение.
    #[test]
    fn build_fills_modifier_and_salt_defaults() {
        let profile =
            parse_client_profile(r#"{"server_address":"1.2.3.4","obfuscation_key":"a2V5"}"#)
                .unwrap();
        let cfg = parse_config(&build_config_json(&profile).unwrap()).unwrap();
        assert_eq!(cfg.modifier, DEFAULT_MODIFIER);
        assert_eq!(cfg.salt, 0xDEAD_BEEF);
        assert_eq!(cfg.server_port, 8443);
    }

    #[test]
    fn get_str_simple_value() {
        let json = r#"{"foo":"bar","baz":"qux"}"#;
        assert_eq!(json_get_str(json, "foo").unwrap(), "bar");
        assert_eq!(json_get_str(json, "baz").unwrap(), "qux");
    }

    #[test]
    fn get_str_escaped_quote_inside_value() {
        // Регрессия: прежний наивный разбор обрывался на первой `\"` и отдавал
        // только `he said `. Теперь эскейп читается.
        let json = r#"{"msg":"he said \"hi\" and left","other":"x"}"#;
        assert_eq!(json_get_str(json, "msg").unwrap(), r#"he said "hi" and left"#);
        assert_eq!(json_get_str(json, "other").unwrap(), "x");
    }

    #[test]
    fn get_str_routing_toml_with_quoted_rules() {
        // Тот самый случай, на котором ломалась маршрутизация: в routing_toml
        // много экранированных кавычек. Без починки возвращалось
        // `default_action = \`, `toml::from_str` падал, и движок уходил на
        // дефолт «проксировать всё».
        let routing_toml = r#"default_action = "direct"
[[rules]]
action = "proxy"
domains = ["youtube.com", "*.youtube.com"]"#;
        let escaped = routing_toml.replace('"', "\\\"").replace('\n', "\\n");
        let json = format!(
            r#"{{"server_address":"1.2.3.4","routing_toml":"{}","on_server_down":"direct"}}"#,
            escaped
        );

        let result = json_get_str(&json, "routing_toml").unwrap();
        assert_eq!(
            result, routing_toml,
            "routing_toml must round-trip through escape/unescape"
        );

        // Соседние поля после многострочного значения читаются по-прежнему.
        assert_eq!(json_get_str(&json, "server_address").unwrap(), "1.2.3.4");
        assert_eq!(json_get_str(&json, "on_server_down").unwrap(), "direct");
    }

    #[test]
    fn get_str_newline_escape() {
        let json = r#"{"multi":"line1\nline2\nline3"}"#;
        assert_eq!(json_get_str(json, "multi").unwrap(), "line1\nline2\nline3");
    }

    /// Android'овский org.json при пересериализации конфига всегда экранирует
    /// `/` как `\/` (легальный JSON-эскейп), а парсер держал обратный слэш в
    /// значении: base64-ключ с `/` не декодировался, hub_url ломался, и
    /// туннель в APK 0.80.0 не стартовал ни одним путём (XR-118).
    #[test]
    fn get_str_forward_slash_escape() {
        let json = r#"{"obfuscation_key":"a2V5\/c2\/x=","hub_url":"https:\/\/hub.example"}"#;
        assert_eq!(json_get_str(json, "obfuscation_key").unwrap(), "a2V5/c2/x=");
        assert_eq!(json_get_str(json, "hub_url").unwrap(), "https://hub.example");
    }

    #[test]
    fn get_str_missing_key() {
        let json = r#"{"foo":"bar"}"#;
        assert!(json_get_str(json, "nope").is_err());
    }

    #[test]
    fn get_str_unterminated() {
        let json = r#"{"foo":"bar"#;
        assert!(json_get_str(json, "foo").is_err());
    }

    #[test]
    fn parse_dns_resolvers_basic() {
        let json = r#"{"dns_resolvers":["1.1.1.1","8.8.8.8:53","77.88.8.8"]}"#;
        assert_eq!(
            parse_dns_resolvers(json),
            vec!["1.1.1.1", "8.8.8.8:53", "77.88.8.8"]
        );
    }

    #[test]
    fn parse_dns_resolvers_missing() {
        let json = r#"{"server_address":"1.2.3.4"}"#;
        assert!(parse_dns_resolvers(json).is_empty());
    }

    #[test]
    fn parse_dns_resolvers_empty_array() {
        let json = r#"{"dns_resolvers":[]}"#;
        assert!(parse_dns_resolvers(json).is_empty());
    }

    #[test]
    fn parse_dns_resolvers_with_whitespace() {
        let json = r#"{"dns_resolvers": [ "1.1.1.1" , "8.8.4.4" ]}"#;
        assert_eq!(parse_dns_resolvers(json), vec!["1.1.1.1", "8.8.4.4"]);
    }

    /// Пользовательские правила (LLD-05): массив `user_rules` главнее
    /// `routing_toml`, порядок сохраняется, домены и CIDR расходятся по полям.
    #[test]
    fn parse_config_user_rules_take_precedence_over_toml() {
        let json = r#"{
            "server_address": "1.2.3.4",
            "server_port": 8443,
            "obfuscation_key": "a2V5",
            "routing_toml": "default_action = \"proxy\"",
            "default_action": "direct",
            "user_rules": [
                {"action": "direct", "pattern": "youtube.com"},
                {"action": "proxy", "pattern": "*.github.corp"},
                {"action": "proxy", "pattern": "10.0.0.0/8"}
            ]
        }"#;
        let cfg = parse_config(json).unwrap();
        assert_eq!(cfg.routing.default_action, "direct");
        assert_eq!(cfg.routing.rules.len(), 3);
        assert_eq!(cfg.routing.rules[0].domains, vec!["youtube.com"]);
        assert_eq!(cfg.routing.rules[0].action, "direct");
        assert_eq!(cfg.routing.rules[2].ip_ranges, vec!["10.0.0.0/8"]);
    }

    /// Пустой список правил это валидный конфиг: только default_action,
    /// пресет хаба доклеится в движке.
    #[test]
    fn parse_config_empty_user_rules() {
        let json = r#"{
            "server_address": "1.2.3.4",
            "server_port": 8443,
            "obfuscation_key": "a2V5",
            "default_action": "direct",
            "user_rules": []
        }"#;
        let cfg = parse_config(json).unwrap();
        assert_eq!(cfg.routing.default_action, "direct");
        assert!(cfg.routing.rules.is_empty());
    }

    /// Легаси-конфиг без `user_rules` идёт по старой ветке routing_toml.
    #[test]
    fn parse_config_without_user_rules_falls_back_to_toml() {
        let json = r#"{
            "server_address": "1.2.3.4",
            "server_port": 8443,
            "obfuscation_key": "a2V5",
            "routing_toml": "default_action = \"proxy\""
        }"#;
        let cfg = parse_config(json).unwrap();
        assert_eq!(cfg.routing.default_action, "proxy");
    }

    /// Пул серверов в конфиге (LLD-10): массив объектов, порядок в массиве и
    /// есть приоритет.
    #[test]
    fn parse_servers_basic() {
        let json = r#"{
            "server_address": "1.2.3.4",
            "servers": [
                {"name": "aeza", "address": "1.2.3.4", "port": 8443},
                {"name": "timeweb", "address": "5.6.7.8", "port": 9000}
            ]
        }"#;
        let servers = parse_servers(json);
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "aeza");
        assert_eq!(servers[0].priority, 0);
        assert_eq!(servers[1].address, "5.6.7.8");
        assert_eq!(servers[1].port, 9000);
        assert_eq!(servers[1].priority, 1);
    }

    /// Легаси-конфиг без `servers` даёт пустой список: движок строит пул из
    /// одиночного `server_address`, поведение прежнее.
    #[test]
    fn parse_servers_missing_falls_back_to_legacy() {
        let json = r#"{"server_address":"1.2.3.4","server_port":8443,"obfuscation_key":"a2V5"}"#;
        assert!(parse_servers(json).is_empty());

        let cfg = parse_config(json).unwrap();
        assert!(cfg.servers.is_empty());
        assert_eq!(cfg.server_address, "1.2.3.4");
    }

    /// Записи без адреса выбрасываются, порт по умолчанию 8443.
    #[test]
    fn parse_servers_skips_bad_entries() {
        let json = r#"{"servers": [
            {"name": "no-addr"},
            {"address": "  "},
            {"address": "9.9.9.9"}
        ]}"#;
        let servers = parse_servers(json);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].address, "9.9.9.9");
        assert_eq!(servers[0].port, 8443);
    }
}
