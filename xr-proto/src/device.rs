//! Политики устройств и профили выхода (XR-310). Один xr-client выбирает
//! провайдера и способ выхода по адресу LAN-устройства и ресурсу: устройство
//! получает профиль выхода по умолчанию, отдельные домены и подсети могут
//! уходить в другой профиль. Неизвестное устройство живёт по `[routing]` с
//! прежним пулом, поэтому конфиг без новых секций сохраняет поведение.

use crate::config::{
    ClientConfig, DeviceRuleConfig, ExitProfileConfig, ServerEntry, WanChannelConfig,
};
use crate::routing::{Action, CidrRange, DomainSet};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

/// Имя основного маршрута в ссылках `wan`. Канал с таким именем заводить
/// нельзя: он и так есть у каждого конфига.
pub const MAIN_WAN: &str = "main";

/// Развёрнутый профиль выхода: что делать с соединением и через какой канал.
#[derive(Debug)]
pub struct ExitProfile {
    pub name: String,
    pub action: Action,
    /// Интерфейс канала для SO_BINDTODEVICE. `None` означает основной
    /// маршрут ОС.
    pub interface: Option<String>,
    pub use_wan_pool: bool,
    /// Ключ пула серверов профиля, см. [`pool_key`].
    pub pool_key: String,
    /// Ключ резервного пула, если задан `fallback_wan`.
    pub fallback_pool_key: Option<String>,
    pub on_server_down: Action,
}

/// Ключ пула серверов: интерфейс канала плюс подпись подмножества серверов.
/// Профили с равным ключом делят один пул, у пулов на разных каналах
/// раздельные туннели и здоровье.
pub fn pool_key(interface: Option<&str>, servers: &[ServerEntry]) -> String {
    let sig: Vec<String> = servers
        .iter()
        .map(|s| format!("{}:{}", s.address, s.port))
        .collect();
    format!("{}|{}", interface.unwrap_or(MAIN_WAN), sig.join(","))
}

/// Скомпилированное правило ресурса устройства.
struct DeviceRule {
    name: Option<String>,
    exit: Arc<ExitProfile>,
    domains: DomainSet,
    ip_ranges: Vec<CidrRange>,
}

impl DeviceRule {
    fn matches(&self, sni: Option<&str>, dest_ip: IpAddr) -> bool {
        self.domains.matches(sni.unwrap_or(""))
            || self.ip_ranges.iter().any(|r| r.contains(dest_ip))
    }
}

/// Скомпилированная политика устройства.
struct DevicePolicy {
    name: String,
    ips: Vec<CidrRange>,
    default_exit: Arc<ExitProfile>,
    rules: Vec<DeviceRule>,
}

impl DevicePolicy {
    fn matches_ip(&self, src: IpAddr) -> bool {
        self.ips.iter().any(|r| r.contains(src))
    }

    fn has_unmatched_ipv4(&self, earlier: &[DevicePolicy]) -> bool {
        let bounds = |range: &CidrRange| match range {
            CidrRange::V4 { addr, mask } => {
                Some((u64::from(*addr), u64::from(*addr | !*mask)))
            }
            CidrRange::V6 { .. } => None,
        };
        let mut covered: Vec<_> = earlier
            .iter()
            .flat_map(|device| device.ips.iter())
            .filter_map(bounds)
            .collect();
        covered.sort_unstable();
        self.ips.iter().filter_map(bounds).any(|(start, end)| {
            let mut next = start;
            for &(low, high) in &covered {
                if low > next {
                    break;
                }
                next = next.max(high + 1);
                if next > end {
                    return false;
                }
            }
            next <= end
        })
    }
}

/// Решение политики для соединения.
#[derive(Debug)]
pub enum PolicyDecision {
    /// Устройство вне политики, действует прежний путь `[routing]`.
    Global,
    /// Профиль выхода плюс трассировка для журнала: имя устройства и
    /// совпавшего правила, `None` значит маршрут по умолчанию устройства.
    Exit {
        exit: Arc<ExitProfile>,
        device: String,
        rule: Option<String>,
    },
}

/// Скомпилированные политики всех устройств конфига. Строятся один раз на
/// старте из проверенного конфига, дальше работают только на чтение.
#[derive(Default)]
pub struct DevicePolicySet {
    devices: Vec<DevicePolicy>,
}

impl std::fmt::Debug for DevicePolicySet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Отчёт валидации и тесты показывают имена, внутренности политик
        // для этого не нужны.
        let mut list = f.debug_list();
        for device in &self.devices {
            list.entry(&format_args!(
                "{}(default={}, rules={})",
                device.name,
                device.default_exit.name,
                device.rules.iter().filter_map(|r| r.name.as_deref()).count()
            ));
        }
        list.finish()
    }
}

/// Серверы профиля по именам из `[[servers]]`. Пустой список имён означает
/// пул целиком. Имя ищется по лейблу записи (явное имя либо адрес), чтобы
/// безымянные записи были адресуемы.
fn resolve_servers<'a>(
    exit_name: &str,
    names: &[String],
    entries: &'a [ServerEntry],
) -> Result<Vec<&'a ServerEntry>, String> {
    if names.is_empty() {
        return Ok(entries.iter().collect());
    }
    let mut resolved = Vec::with_capacity(names.len());
    for name in names {
        let found = entries
            .iter()
            .find(|e| e.display_name() == name)
            .ok_or_else(|| {
                format!("exit '{exit_name}': сервер '{name}' не найден в [[servers]]")
            })?;
        let duplicate = resolved
            .iter()
            .any(|e: &&ServerEntry| e.address == found.address && e.port == found.port);
        if !duplicate {
            resolved.push(found);
        }
    }
    Ok(resolved)
}

/// Почему домен правила устройства не годится. Проверка та же, что у
/// пользовательских правил, но итог fail-closed: правила устройств пишутся
/// руками в конфиге конкретной машины, и опечатка не должна молча выпадать
/// из политики конфиденциальности.
fn domain_reject_reason(domain: &str) -> Option<String> {
    use crate::user_rule::{classify_pattern, normalize_pattern, RuleKind};
    let d = normalize_pattern(domain);
    match classify_pattern(&d) {
        Ok(RuleKind::Domain) | Ok(RuleKind::Wildcard) => None,
        Ok(RuleKind::CidrV4) | Ok(RuleKind::CidrV6) => {
            Some(format!("'{domain}' это CIDR, ему место в ip_ranges"))
        }
        Err(_) => Some(format!("'{domain}' не разбирается как домен")),
    }
}

/// Разобрать список адресов и подсетей. Одиночный адрес без префикса
/// читается как хостовая маска.
fn compile_ips(section: &str, values: &[String]) -> Result<Vec<CidrRange>, String> {
    values
        .iter()
        .map(|v| {
            let range = CidrRange::parse_ip_or_cidr(v)
                .ok_or_else(|| format!("{section}: '{v}' не адрес и не CIDR"))?;
            if matches!(range, CidrRange::V6 { .. }) {
                return Err(format!("{section}: IPv6 '{v}' не поддерживается политиками устройств; перехват работает только с IPv4 TCP"));
            }
            Ok(range)
        })
        .collect()
}

impl DevicePolicySet {
    /// Собрать политики из конфига. Тот же путь проходит `validate`, поэтому
    /// отказ здесь значит отказ `xr-client validate`, а не только старта
    /// (паттерн XR-227).
    pub fn compile(config: &ClientConfig) -> Result<Self, String> {
        // Структурные проверки живут здесь, а не рядом с validate: сборка и
        // есть единственный разбор, `pool_specs` полагается на неповторяющиеся
        // имена каналов.
        for wan in &config.wan {
            if wan.name.trim().is_empty() {
                return Err("wan: пустое имя канала".into());
            }
            if wan.name == MAIN_WAN {
                return Err(format!(
                    "wan '{}': имя 'main' зарезервировано за основным маршрутом",
                    wan.name
                ));
            }
            if wan.interface.trim().is_empty() {
                return Err(format!("wan '{}': пустой interface", wan.name));
            }
            if config.wan.iter().filter(|w| w.name == wan.name).count() > 1 {
                return Err(format!("wan '{}': имя канала повторяется", wan.name));
            }
        }
        for exit in &config.exit {
            if exit.name.trim().is_empty() {
                return Err("exit: пустое имя профиля".into());
            }
            if config.exit.iter().filter(|e| e.name == exit.name).count() > 1 {
                return Err(format!("exit '{}': имя профиля повторяется", exit.name));
            }
        }
        for device in &config.device {
            if device.name.trim().is_empty() {
                return Err("device: пустое имя устройства".into());
            }
            if config.device.iter().filter(|d| d.name == device.name).count() > 1 {
                return Err(format!("device '{}': имя устройства повторяется", device.name));
            }
        }

        let entries = config.server_entries()?;
        let channels: HashMap<&str, &WanChannelConfig> = config
            .wan
            .iter()
            .map(|w| (w.name.as_str(), w))
            .collect();

        let mut exits: HashMap<String, Arc<ExitProfile>> = HashMap::new();
        for exit in &config.exit {
            let profile = compile_exit(exit, &channels, &entries)?;
            exits.insert(exit.name.clone(), Arc::new(profile));
        }

        let find_exit = |name: &str| {
            exits
                .get(name)
                .cloned()
                .ok_or_else(|| format!("ссылка на неизвестный профиль выхода '{name}'"))
        };

        let mut devices = Vec::with_capacity(config.device.len());
        for device in &config.device {
            let section = format!("device '{}'", device.name);
            if device.ips.is_empty() {
                return Err(format!("{section}: пустой ips, устройство не совпадёт ни с кем"));
            }
            let ips = compile_ips(&section, &device.ips)?;
            let default_exit = find_exit(&device.default_exit)
                .map_err(|e| format!("{section}: default_exit {e}"))?;
            let mut rules = Vec::with_capacity(device.rules.len());
            for (i, rule) in device.rules.iter().enumerate() {
                rules.push(compile_device_rule(&section, i, rule, &find_exit)?);
            }
            devices.push(DevicePolicy {
                name: device.name.clone(),
                ips,
                default_exit,
                rules,
            });
        }
        let set = Self { devices };
        set.validate_udp_relay(config)?;
        Ok(set)
    }

    /// Политики пусты, весь трафик идёт прежним путём `[routing]`.
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// Решение для соединения с LAN-адресом `src`. Устройство ищется первым
    /// совпавшим, внутри него первым совпавшим правилом ресурса, без
    /// совпадения действует маршрут по умолчанию устройства.
    pub fn decide(&self, src: IpAddr, sni: Option<&str>, dest_ip: IpAddr) -> PolicyDecision {
        for device in &self.devices {
            if !device.matches_ip(src) {
                continue;
            }
            for rule in &device.rules {
                if rule.matches(sni, dest_ip) {
                    return PolicyDecision::Exit {
                        exit: rule.exit.clone(),
                        device: device.name.clone(),
                        rule: rule.name.clone(),
                    };
                }
            }
            return PolicyDecision::Exit {
                exit: device.default_exit.clone(),
                device: device.name.clone(),
                rule: None,
            };
        }
        PolicyDecision::Global
    }

    /// Спецификации пулов, на которые ссылаются профили: основной и
    /// резервный ключ каждого. Вызывающий строит по ним пулы и заряжает
    /// каждому свой health_loop. Спецификация с ключом пула по умолчанию
    /// относится к уже существующему пулу.
    pub fn pool_specs(&self, config: &ClientConfig) -> Result<Vec<PoolSpec>, String> {
        let entries = config.server_entries()?;
        let by_name: HashMap<&str, &WanChannelConfig> = config
            .wan
            .iter()
            .map(|w| (w.name.as_str(), w))
            .collect();
        let mut specs: Vec<PoolSpec> = Vec::new();
        for exit in &config.exit {
            let servers = resolve_servers(&exit.name, &exit.servers, &entries)?;
            // Профиль без `wan` живёт на основном маршруте, но со своим
            // подмножеством серверов: такой пул тоже строится отдельно и
            // совпадает с общим только при полном списке.
            for name in [exit.wan.as_deref(), exit.fallback_wan.as_deref()] {
                let interface = match name {
                    None | Some("") | Some(MAIN_WAN) => None,
                    Some(n) => Some(
                        by_name
                            .get(n)
                            .unwrap_or_else(|| panic!("канал '{n}' прошёл compile, но не найден"))
                            .interface
                            .clone(),
                    ),
                };
                let owned: Vec<ServerEntry> = servers.iter().map(|s| (*s).clone()).collect();
                let key = pool_key(interface.as_deref(), &owned);
                if !specs.iter().any(|s| s.key == key) {
                    specs.push(PoolSpec {
                        key,
                        interface,
                        servers: owned,
                    });
                }
            }
        }
        Ok(specs)
    }

    fn validate_udp_relay(&self, config: &ClientConfig) -> Result<(), String> {
        let Some(udp) = config.udp_relay.as_ref().filter(|udp| udp.enabled) else {
            return Ok(());
        };
        let sources: Vec<IpAddr> = udp
            .source_ips
            .iter()
            .filter_map(|ip| ip.parse::<std::net::Ipv4Addr>().ok().map(IpAddr::V4))
            .collect();
        for (index, device) in self.devices.iter().enumerate() {
            // Как и relay, пустой список разобранных адресов считаем всей LAN.
            let reachable = if sources.is_empty() {
                device.has_unmatched_ipv4(&self.devices[..index])
            } else {
                sources.iter().any(|src| {
                    self.devices.iter().position(|device| device.matches_ip(*src)) == Some(index)
                })
            };
            if !reachable {
                continue;
            }
            for exit in std::iter::once(&device.default_exit).chain(device.rules.iter().map(|r| &r.exit)) {
                let spec = config
                    .exit
                    .iter()
                    .find(|spec| spec.name == exit.name)
                    .expect("профиль уже проверен при сборке устройства");
                for (field, wan) in [("wan", &spec.wan), ("fallback_wan", &spec.fallback_wan)] {
                    if let Some(wan) = wan.as_deref().filter(|wan| !wan.is_empty() && *wan != MAIN_WAN) {
                        return Err(format!(
                            "device '{}': exit '{}' задаёт {field} '{wan}', но udp_relay этого устройства использует основной маршрут; уберите устройство из relay или привязку WAN из его TCP-политики",
                            device.name, exit.name
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

fn compile_exit(
    exit: &ExitProfileConfig,
    channels: &HashMap<&str, &WanChannelConfig>,
    entries: &[ServerEntry],
) -> Result<ExitProfile, String> {
    let section = format!("exit '{}'", exit.name);
    let action = match exit.action.as_str() {
        "direct" => Action::Direct,
        "proxy" => Action::Proxy,
        "block" => Action::Block,
        other => return Err(format!("{section}: неизвестное action '{other}'")),
    };
    let on_server_down = match exit.on_server_down.as_str() {
        "" | "block" => Action::Block,
        "direct" => Action::Direct,
        other => {
            return Err(format!(
                "{section}: on_server_down '{other}' не годится, допустимы direct и block"
            ))
        }
    };
    let interface_of = |wan: &Option<String>, field: &str| -> Result<Option<String>, String> {
        match wan.as_deref() {
            None | Some("") => Ok(None),
            Some(MAIN_WAN) => Ok(None),
            Some(name) => Ok(Some(
                channels
                    .get(name)
                    .ok_or_else(|| format!("{section}: {field} ссылается на неизвестный канал '{name}'"))?
                    .interface
                    .clone(),
            )),
        }
    };
    let interface = interface_of(&exit.wan, "wan")?;
    let fallback_interface = interface_of(&exit.fallback_wan, "fallback_wan")?;

    let servers = resolve_servers(&exit.name, &exit.servers, entries)?;
    if action == Action::Proxy && servers.is_empty() {
        return Err(format!("{section}: proxy без серверов пула"));
    }
    let owned: Vec<ServerEntry> = servers.iter().map(|s| (*s).clone()).collect();
    let key = pool_key(interface.as_deref(), &owned);
    let fallback_pool_key = exit.fallback_wan.as_ref()
        .filter(|name| !name.is_empty())
        .map(|_| pool_key(fallback_interface.as_deref(), &owned));
    Ok(ExitProfile {
        name: exit.name.clone(),
        action,
        interface,
        use_wan_pool: exit.wan.as_deref().is_none_or(str::is_empty),
        pool_key: key,
        fallback_pool_key,
        on_server_down,
    })
}

fn compile_device_rule<F>(
    section: &str,
    index: usize,
    rule: &DeviceRuleConfig,
    find_exit: F,
) -> Result<DeviceRule, String>
where
    F: Fn(&str) -> Result<Arc<ExitProfile>, String>,
{
    let label = rule
        .name
        .as_deref()
        .map(|n| format!("'{n}'"))
        .unwrap_or_else(|| format!("#{index}"));
    let here = format!("{section} rule {label}");
    if rule.domains.is_empty() && rule.ip_ranges.is_empty() {
        return Err(format!("{here}: нет ни domains, ни ip_ranges, правилу нечего матчить"));
    }
    for domain in &rule.domains {
        if let Some(reason) = domain_reject_reason(domain) {
            return Err(format!("{here}: {reason}"));
        }
    }
    let ranges = compile_ips(&here, &rule.ip_ranges)?;
    let exit = find_exit(&rule.exit).map_err(|e| format!("{here}: exit {e}"))?;
    Ok(DeviceRule {
        name: rule.name.clone(),
        exit,
        domains: DomainSet::compile(&rule.domains),
        ip_ranges: ranges,
    })
}

/// Спецификация пула для построения на старте клиента.
pub struct PoolSpec {
    /// Ключ из [`pool_key`].
    pub key: String,
    /// Интерфейс канала, `None` для основного маршрута.
    pub interface: Option<String>,
    /// Подмножество серверов пула в порядке приоритета конфига.
    pub servers: Vec<ServerEntry>,
}

/// Проверка секций XR-310 для `validate_client_config`: имена уникальны,
/// ссылки существуют, значения допустимы. Сборка политик в
/// [`DevicePolicySet::compile`] проходит те же пути, поэтому validate
/// видит ровно то, что потребует старт.
pub fn policy_reject_reason(config: &ClientConfig) -> Result<(), String> {
    DevicePolicySet::compile(config).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> ClientConfig {
        toml::from_str(text).expect("конфиг обязан разбираться")
    }

    const BASE: &str = r#"
        [obfuscation]
        key = "QUJD"
        [routing]
        default_action = "direct"
        [[servers]]
        name = "vps1"
        address = "198.51.100.1"
        port = 8443
        [[servers]]
        name = "vps2"
        address = "198.51.100.2"
        port = 8443
        priority = 1
    "#;

    #[test]
    fn explicit_main_exit_stays_pinned_when_wan_pool_is_enabled() {
        let cfg = config(&format!(r#"{BASE}
[client]
wan_pool = [{{ interface="wan", weight=1 }}, {{ interface="lan1", weight=3 }}]
[[exit]]
name = "fixed"
wan = "main"
action = "proxy"
[[device]]
name = "admin"
ips = ["192.168.1.20"]
default_exit = "fixed"
"#));
        let set = DevicePolicySet::compile(&cfg).unwrap();
        let PolicyDecision::Exit { exit, .. } = set.decide(
            "192.168.1.20".parse().unwrap(), None, "1.1.1.1".parse().unwrap()
        ) else { panic!("explicit exit must match") };
        assert!(!exit.use_wan_pool);
        assert!(exit.interface.is_none());
        assert!(set.pool_specs(&cfg).unwrap().iter().all(|spec| spec.interface.is_none()));
    }

    #[test]
    fn empty_policy_decides_global() {
        let set = DevicePolicySet::compile(&config(BASE)).expect("пустые политики собираются");
        assert!(set.is_empty());
        assert!(matches!(
            set.decide(
                "192.0.2.10".parse().unwrap(),
                None,
                "203.0.113.1".parse().unwrap()
            ),
            PolicyDecision::Global
        ));
    }

    #[test]
    fn device_default_exit_by_ip_and_subnet() {
        let set = DevicePolicySet::compile(&config(&format!(
            r#"{BASE}
            [[wan]]
            name = "two"
            interface = "lan4"
            [[exit]]
            name = "fast-direct"
            wan = "two"
            action = "direct"
            [[device]]
            name = "pc"
            ips = ["192.0.2.10", "198.51.100.128/25"]
            default_exit = "fast-direct"
            "#
        )))
        .expect("политики собираются");
        for src in ["192.0.2.10", "198.51.100.200"] {
            match set.decide(src.parse().unwrap(), None, "203.0.113.1".parse().unwrap()) {
                PolicyDecision::Exit { exit, device, rule } => {
                    assert_eq!(device, "pc");
                    assert_eq!(rule, None);
                    assert_eq!(exit.name, "fast-direct");
                    assert_eq!(exit.interface.as_deref(), Some("lan4"));
                }
                other => panic!("ожидался выход устройства, получено {other:?} для {src}"),
            }
        }
        // Чужой адрес живёт по общим правилам.
        assert!(matches!(
            set.decide(
                "203.0.113.99".parse().unwrap(),
                None,
                "203.0.113.1".parse().unwrap()
            ),
            PolicyDecision::Global
        ));
    }

    #[test]
    fn resource_rule_beats_device_default() {
        let set = DevicePolicySet::compile(&config(&format!(
            r#"{BASE}
            [[wan]]
            name = "fast"
            interface = "eth1"
            [[wan]]
            name = "two"
            interface = "lan4"
            [[exit]]
            name = "fast-direct"
            wan = "fast"
            action = "direct"
            [[exit]]
            name = "fast-proxy"
            wan = "fast"
            action = "proxy"
            servers = ["vps1"]
            [[exit]]
            name = "tv-proxy"
            wan = "two"
            action = "proxy"
            [[device]]
            name = "pc"
            ips = ["192.0.2.10"]
            default_exit = "fast-direct"
            [[device.rule]]
            name = "bank"
            domains = ["bank.example.com", "*.corp.example"]
            exit = "fast-proxy"
            "#
        )))
        .expect("политики собираются");
        let src: IpAddr = "192.0.2.10".parse().unwrap();
        let dst: IpAddr = "203.0.113.5".parse().unwrap();
        // Домен из правила отправляет соединение в прокси-профиль.
        match set.decide(src, Some("bank.example.com"), dst) {
            PolicyDecision::Exit { exit, rule, .. } => {
                assert_eq!(exit.name, "fast-proxy");
                assert_eq!(rule.as_deref(), Some("bank"));
            }
            other => panic!("правило ресурса обязано победить, получено {other:?}"),
        }
        // Wildcard-суффикс тоже матчится.
        match set.decide(src, Some("git.corp.example"), dst) {
            PolicyDecision::Exit { exit, .. } => assert_eq!(exit.name, "fast-proxy"),
            other => panic!("wildcard обязан матчиться, получено {other:?}"),
        }
        // Прочий домен живёт маршрутом по умолчанию устройства.
        match set.decide(src, Some("example.org"), dst) {
            PolicyDecision::Exit { exit, rule, .. } => {
                assert_eq!(exit.name, "fast-direct");
                assert_eq!(rule, None);
            }
            other => panic!("без совпадения действует default_exit, получено {other:?}"),
        }
    }

    #[test]
    fn rule_matches_by_ip_range_and_first_wins() {
        let set = DevicePolicySet::compile(&config(&format!(
            r#"{BASE}
            [[exit]]
            name = "direct-all"
            action = "direct"
            [[exit]]
            name = "proxy-all"
            action = "proxy"
            [[device]]
            name = "tv"
            ips = ["192.0.2.20"]
            default_exit = "proxy-all"
            [[device.rule]]
            name = "video"
            ip_ranges = ["198.51.100.0/25"]
            exit = "direct-all"
            [[device.rule]]
            name = "wider"
            ip_ranges = ["198.51.100.0/24"]
            exit = "proxy-all"
            "#
        )))
        .expect("политики собираются");
        let src: IpAddr = "192.0.2.20".parse().unwrap();
        let in_both: IpAddr = "198.51.100.5".parse().unwrap();
        match set.decide(src, None, in_both) {
            PolicyDecision::Exit { rule, exit, .. } => {
                assert_eq!(rule.as_deref(), Some("video"), "первое совпавшее правило побеждает");
                assert_eq!(exit.name, "direct-all");
            }
            other => panic!("ожидалось правило по подсети, получено {other:?}"),
        }
        // Адрес вне первого правила попадает во второе.
        match set.decide(src, None, "198.51.100.200".parse().unwrap()) {
            PolicyDecision::Exit { rule, .. } => assert_eq!(rule.as_deref(), Some("wider")),
            other => panic!("ожидалось второе правило, получено {other:?}"),
        }
    }

    #[test]
    fn unknown_wan_or_exit_reference_is_rejected() {
        let err = DevicePolicySet::compile(&config(&format!(
            r#"{BASE}
            [[exit]]
            name = "lost"
            wan = "nosuch"
            action = "direct"
            "#
        )))
        .expect_err("ссылка на неизвестный канал обязана падать");
        assert!(err.contains("nosuch"), "{err}");

        let err = DevicePolicySet::compile(&config(&format!(
            r#"{BASE}
            [[exit]]
            name = "ok"
            action = "direct"
            [[device]]
            name = "pc"
            ips = ["192.0.2.10"]
            default_exit = "ghost"
            "#
        )))
        .expect_err("ссылка на неизвестный профиль обязана падать");
        assert!(err.contains("ghost"), "{err}");
    }

    #[test]
    fn proxy_exit_with_unknown_server_rejected() {
        let err = DevicePolicySet::compile(&config(&format!(
            r#"{BASE}
            [[exit]]
            name = "p"
            action = "proxy"
            servers = ["ghost-vps"]
            "#
        )))
        .expect_err("неизвестный сервер в подмножестве обязан падать");
        assert!(err.contains("ghost-vps"), "{err}");
    }

    #[test]
    fn unknown_action_and_bad_on_server_down_rejected() {
        let err = DevicePolicySet::compile(&config(&format!(
            r#"{BASE}
            [[exit]]
            name = "weird"
            action = "сокс5"
            "#
        )))
        .expect_err("неизвестное action обязано падать, а не молча давать direct");
        assert!(err.contains("action"), "{err}");

        let err = DevicePolicySet::compile(&config(&format!(
            r#"{BASE}
            [[exit]]
            name = "weird"
            action = "proxy"
            on_server_down = "прямо"
            "#
        )))
        .expect_err("кривое on_server_down обязано падать");
        assert!(err.contains("on_server_down"), "{err}");
    }

    #[test]
    fn bad_domain_ip_and_matcherless_rule_rejected() {
        let base = format!(
            r#"{BASE}
            [[exit]]
            name = "d"
            action = "direct"
            [[device]]
            name = "pc"
            ips = ["192.0.2.10"]
            default_exit = "d"
            "#
        );
        let err = DevicePolicySet::compile(&config(&format!(
            "{base}\n[[device.rule]]\ndomains = [\"not a domain\"]\nexit = \"d\"\n"
        )))
        .expect_err("битый домен в правиле устройства обязан падать");
        assert!(err.contains("не разбирается"), "{err}");

        let err = DevicePolicySet::compile(&config(&format!(
            "{base}\n[[device.rule]]\nip_ranges = [\"300.0.0.0/8\"]\nexit = \"d\"\n"
        )))
        .expect_err("битый CIDR в правиле устройства обязан падать");
        assert!(err.contains("не адрес и не CIDR"), "{err}");

        let err = DevicePolicySet::compile(&config(&format!(
            "{base}\n[[device.rule]]\nexit = \"d\"\n"
        )))
        .expect_err("правило без матчеров обязано падать");
        assert!(err.contains("нечего матчить"), "{err}");

        let err = DevicePolicySet::compile(&config(&format!(
            r#"{BASE}
            [[exit]]
            name = "d"
            action = "direct"
            [[device]]
            name = "pc"
            ips = ["192.0.two.10"]
            default_exit = "d"
            "#
        )))
        .expect_err("битый адрес устройства обязан падать");
        assert!(err.contains("192.0.two.10"), "{err}");
    }

    #[test]
    fn main_wan_name_is_reserved_and_duplicates_rejected() {
        let err = DevicePolicySet::compile(&config(&format!(
            r#"{BASE}
            [[wan]]
            name = "main"
            interface = "eth1"
            "#
        )))
        .expect_err("имя main зарезервировано");
        assert!(err.contains("main"), "{err}");

        let err = DevicePolicySet::compile(&config(&format!(
            r#"{BASE}
            [[wan]]
            name = "two"
            interface = "lan4"
            [[wan]]
            name = "two"
            interface = "lan5"
            "#
        )))
        .expect_err("повтор имени канала обязан падать");
        assert!(err.contains("повторяется"), "{err}");
    }

    #[test]
    fn pool_keys_share_and_split_by_interface() {
        let cfg = config(&format!(
            r#"{BASE}
            [[wan]]
            name = "two"
            interface = "lan4"
            [[exit]]
            name = "main-proxy"
            action = "proxy"
            servers = ["vps1"]
            [[exit]]
            name = "main-proxy-copy"
            action = "proxy"
            servers = ["vps1"]
            [[exit]]
            name = "two-proxy"
            wan = "two"
            action = "proxy"
            servers = ["vps1"]
            "#
        ));
        let set = DevicePolicySet::compile(&cfg).expect("политики собираются");
        let keys: Vec<String> = set
            .pool_specs(&cfg)
            .expect("спецификации собираются")
            .into_iter()
            .map(|s| s.key)
            .collect();
        assert_eq!(keys.len(), 2, "три профиля дают два ключа: {keys:?}");
        let two = keys
            .iter()
            .find(|k| k.starts_with("lan4|"))
            .expect("профиль на втором канале даёт свой ключ");
        let main_keys: Vec<&String> = keys.iter().filter(|k| k.starts_with("main|")).collect();
        assert_eq!(
            main_keys.len(),
            1,
            "одинаковый канал и серверы делят один пул: {keys:?}"
        );
        assert!(!main_keys[0].starts_with("lan4"));
        assert_ne!(main_keys[0], two);
    }

    #[test]
    fn pool_specs_cover_fallback_channel() {
        let cfg = config(&format!(
            r#"{BASE}
            [[wan]]
            name = "fast"
            interface = "eth1"
            [[wan]]
            name = "two"
            interface = "lan4"
            [[exit]]
            name = "tv"
            wan = "two"
            action = "proxy"
            servers = ["vps1"]
            fallback_wan = "fast"
            "#
        ));
        let set = DevicePolicySet::compile(&cfg).expect("политики собираются");
        let specs = set
            .pool_specs(&cfg)
            .expect("спецификации собираются");
        assert_eq!(
            specs.len(),
            2,
            "основной и резервный пул: {:?}",
            specs.iter().map(|s| &s.key).collect::<Vec<_>>()
        );
        assert!(
            specs.iter().any(|s| s.interface.as_deref() == Some("eth1")),
            "резервный канал тоже получает пул"
        );
        assert!(
            specs.iter().any(|s| s.interface.as_deref() == Some("lan4")),
            "основной канал профиля в списке"
        );
    }

    #[test]
    fn policy_reject_reason_covers_compile() {
        assert!(policy_reject_reason(&config(BASE)).is_ok());
        assert!(policy_reject_reason(&config(&format!(
            r#"{BASE}
            [[exit]]
            name = "d"
            action = "direct"
            [[exit]]
            name = "d"
            action = "proxy"
            "#
        )))
        .is_err());
    }

    #[test]
    fn unsupported_policy_ipv6_sources_and_destinations_are_rejected() {
        for address in ["2001:db8::1", "2001:db8::/64", "::/0", "::ffff:192.0.2.30"] {
            let text = format!(r#"{BASE}
[[exit]]
name = "plain"
action = "direct"
[[device]]
name = "console"
ips = ["192.0.2.30", "{address}"]
default_exit = "plain"
"#);
            let err = policy_reject_reason(&config(&text)).unwrap_err();
            assert!(err.contains("device 'console'") && err.contains("IPv6"), "{err}");

            let text = text.replace(&format!(", \"{address}\""), "") + &format!(r#"
[[device.rule]]
name = "resource"
ip_ranges = ["192.0.2.0/24", "{address}"]
exit = "plain"
"#);
            let err = policy_reject_reason(&config(&text)).unwrap_err();
            assert!(err.contains("rule 'resource'") && err.contains("IPv6"), "{err}");
        }
    }

    const UDP_POLICY: &str = r#"
[[wan]]
name = "two"
interface = "lan4"
[[exit]]
name = "plain"
wan = "main"
action = "proxy"
[[exit]]
name = "off-channel"
wan = "two"
action = "proxy"
[[exit]]
name = "reserve"
fallback_wan = "two"
action = "proxy"
"#;

    fn udp_policy(ips: &str, exit: &str, enabled: bool, source_ips: &str) -> ClientConfig {
        config(&format!(r#"{BASE}{UDP_POLICY}
[udp_relay]
enabled = {enabled}
source_ips = [{source_ips}]
[[device]]
name = "console"
ips = ["{ips}"]
default_exit = "{exit}"
"#))
    }

    #[test]
    fn unsupported_policy_udp_relay_rejects_host_and_cidr_with_primary_or_fallback_wan() {
        for source in ["192.0.2.30", "192.0.2.30/32", "192.0.2.0/24", "0.0.0.0/0"] {
            for exit in ["off-channel", "reserve"] {
                let cfg = udp_policy(source, exit, true, "\"192.0.2.30\"");
                let err = policy_reject_reason(&cfg).unwrap_err();
                assert!(err.contains("console") && err.contains("udp_relay") && err.contains(exit), "{err}");
            }
        }
        let cfg = udp_policy("192.0.2.30", "off-channel", true, "");
        assert!(policy_reject_reason(&cfg).is_err(), "relay без source_ips принимает все устройства");
    }

    #[test]
    fn unsupported_policy_udp_relay_checks_resource_exits_too() {
        let mut cfg = udp_policy("192.0.2.0/24", "plain", true, "\"192.0.2.30\"");
        cfg.device[0].rules.push(toml::from_str(r#"
name = "resource"
domains = ["example.com"]
exit = "reserve"
"#).unwrap());
        let err = policy_reject_reason(&cfg).unwrap_err();
        assert!(err.contains("console") && err.contains("reserve") && err.contains("udp_relay"), "{err}");
    }

    #[test]
    fn unsupported_policy_udp_relay_preserves_disabled_unrelated_and_main_exits() {
        for cfg in [
            udp_policy("192.0.2.0/24", "off-channel", false, "\"192.0.2.30\""),
            udp_policy("198.51.100.0/24", "off-channel", true, "\"192.0.2.30\""),
            udp_policy("192.0.2.0/24", "plain", true, "\"192.0.2.30\""),
        ] {
            policy_reject_reason(&cfg).unwrap();
        }
        let mut cfg = udp_policy("192.0.2.0/24", "off-channel", true, "\"192.0.2.30\"");
        cfg.udp_relay = None;
        policy_reject_reason(&cfg).unwrap();
        let mut legacy = config(BASE);
        legacy.udp_relay = udp_policy("192.0.2.30", "plain", true, "").udp_relay;
        legacy.routing.rules.push(toml::from_str(r#"
action = "proxy"
ip_ranges = ["2001:db8::/32"]
"#).unwrap());
        policy_reject_reason(&legacy).unwrap();
        let mut cfg = udp_policy("192.0.2.0/24", "plain", true, "\"192.0.2.30\"");
        cfg.exit[0].fallback_wan = Some("main".into());
        policy_reject_reason(&cfg).unwrap();
        let cfg = udp_policy("192.0.2.30/32", "off-channel", true, "\"192.0.2.31\"");
        policy_reject_reason(&cfg).unwrap();
        let cfg = udp_policy("192.0.2.30/32", "off-channel", true, "\"192.0.2.31\", \"192.0.2.30\"");
        assert!(policy_reject_reason(&cfg).is_err());
    }

    #[test]
    fn unsupported_policy_udp_relay_uses_first_matching_device() {
        let mut cfg = udp_policy("192.0.2.0/24", "plain", true, "\"192.0.2.30\"");
        cfg.device.push(toml::from_str(r#"
name = "shadowed"
ips = ["192.0.2.30"]
default_exit = "off-channel"
"#).unwrap());
        policy_reject_reason(&cfg).unwrap();
        cfg.device.swap(0, 1);
        let err = policy_reject_reason(&cfg).unwrap_err();
        assert!(err.contains("shadowed"), "{err}");
    }

    #[test]
    fn unsupported_policy_udp_relay_empty_sources_preserves_shadowed_devices() {
        for (earlier, later) in [
            (vec!["0.0.0.0/0"], "192.0.2.30"),
            (vec!["192.0.2.0/25", "192.0.2.128/25"], "192.0.2.0/24"),
            (vec!["192.0.2.128/25", "192.0.2.0/25"], "192.0.2.0/24"),
            (vec!["0.0.0.0/1", "128.0.0.0/1"], "0.0.0.0/0"),
            (vec!["255.255.255.255"], "255.255.255.255/32"),
            (vec!["0.0.0.0"], "0.0.0.0/32"),
        ] {
            let mut cfg = udp_policy(later, "off-channel", true, "");
            for (index, range) in earlier.iter().enumerate() {
                let device = toml::from_str(&format!(r#"
name = "earlier-{index}"
ips = ["{range}"]
default_exit = "plain"
"#)).unwrap();
                cfg.device.insert(index, device);
            }
            policy_reject_reason(&cfg)
                .unwrap_or_else(|err| panic!("{earlier:?} полностью закрывает {later}: {err}"));
        }
    }

    #[test]
    fn unsupported_policy_udp_relay_empty_sources_rejects_partial_shadow() {
        for (earlier, later) in [
            (vec!["192.0.2.0/25"], "192.0.2.0/24"),
            (vec!["192.0.2.0/26", "192.0.2.128/25"], "192.0.2.0/24"),
            (vec!["192.0.2.0/25", "192.0.2.0/26"], "192.0.2.0/24"),
            (vec!["0.0.0.0/1", "128.0.0.0/2"], "0.0.0.0/0"),
            (vec!["255.255.255.254"], "255.255.255.254/31"),
        ] {
            let mut cfg = udp_policy(later, "reserve", true, "");
            for (index, range) in earlier.iter().enumerate() {
                let device = toml::from_str(&format!(r#"
name = "earlier-{index}"
ips = ["{range}"]
default_exit = "plain"
"#)).unwrap();
                cfg.device.insert(index, device);
            }
            let err = policy_reject_reason(&cfg).unwrap_err();
            assert!(err.contains("console") && err.contains("reserve"), "{earlier:?} не закрывает {later}: {err}");
        }
    }
}
