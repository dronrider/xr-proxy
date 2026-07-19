//! JNI bridge: Kotlin ↔ xr-core VPN engine.

use jni::objects::{JClass, JObjectArray, JString, GlobalRef};
use jni::sys::{jboolean, jint, jlong, jmethodID, jstring};
use jni::{JNIEnv, JavaVM};

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use xr_core::engine::{VpnConfig, VpnEngine};
use xr_core::ip_stack::PacketQueue;
use xr_core::onboarding;
use xr_core::session::{ProtectSocketFn, SystemResolverFn};
use xr_core::sync;
use xr_core::update;
use xr_proto::config::RoutingConfig;
use xr_proto::invite_url;
use xr_core::sync::LocalFile;
use xr_proto::share::{RelayGrant, ShareGrant, ShareManifest, ShareManifestEntry, ShareToken};

use base64::Engine as _;
use std::collections::HashSet;

/// Global engine instance.
static ENGINE: OnceLock<Mutex<Option<EngineHandle>>> = OnceLock::new();

/// Единый персистентный журнал приложения (XR-042). Живёт на уровне процесса,
/// а не движка: перезапуск движка (смена сети, пауза) ленту не обнуляет.
/// Инициализируется из Kotlin (`nativeJournalInit`) при старте приложения.
static JOURNAL: OnceLock<xr_core::journal::Journal> = OnceLock::new();

/// Дописать запись в журнал, если он уже инициализирован. До инициализации
/// записи молча теряются (только первые миллисекунды жизни процесса).
fn journal_log(level: &str, source: &str, msg: &str) {
    if let Some(j) = JOURNAL.get() {
        j.append(level, source, msg);
    }
}

/// Global JVM reference.
static JVM: OnceLock<JavaVM> = OnceLock::new();

/// Cached class reference and method ID — resolved on main thread, safe to use from any thread.
static PROTECT_METHOD: OnceLock<ProtectMethodCache> = OnceLock::new();

/// Cached method ID for `NativeBridge.resolveDomain(String): String?`.
/// Same lifetime invariants as PROTECT_METHOD. Resolved lazily on the main
/// thread at engine start.
static RESOLVE_METHOD: OnceLock<ResolveMethodCache> = OnceLock::new();

struct ProtectMethodCache {
    class: GlobalRef,
    method_id: jmethodID,
}

struct ResolveMethodCache {
    class: GlobalRef,
    method_id: jmethodID,
}

// Safety: jmethodID is a raw pointer to a JVM internal structure.
// Once resolved, it is immutable and valid for the lifetime of the JVM.
// GlobalRef is Send+Sync by design.
unsafe impl Send for ProtectMethodCache {}
unsafe impl Sync for ProtectMethodCache {}
unsafe impl Send for ResolveMethodCache {}
unsafe impl Sync for ResolveMethodCache {}

struct EngineHandle {
    engine: VpnEngine,
    queue: PacketQueue,
    #[allow(dead_code)]
    runtime: tokio::runtime::Runtime,
}

fn get_engine() -> &'static Mutex<Option<EngineHandle>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

/// Create a ProtectSocketFn that calls VpnService.protect(fd) via cached JNI references.
fn make_protect_fn() -> ProtectSocketFn {
    Arc::new(|fd: i32| -> bool {
        let jvm = match JVM.get() {
            Some(jvm) => jvm,
            None => return false,
        };
        let cache = match PROTECT_METHOD.get() {
            Some(c) => c,
            None => return false,
        };

        // Attach current thread to JVM.
        let mut env = match jvm.attach_current_thread_as_daemon() {
            Ok(env) => env,
            Err(_) => return false,
        };

        // Call the cached static method directly via raw JNI.
        // This avoids FindClass which doesn't work from native threads.
        unsafe {
            let result = env.call_static_method_unchecked(
                &cache.class,
                jni::objects::JStaticMethodID::from_raw(cache.method_id),
                jni::signature::ReturnType::Primitive(jni::signature::Primitive::Boolean),
                &[jni::sys::jvalue { i: fd }],
            );

            match result {
                Ok(val) => val.z().unwrap_or(false),
                Err(_) => {
                    let _ = env.exception_clear();
                    false
                }
            }
        }
    })
}

/// Build a `SystemResolverFn` that calls Kotlin
/// `NativeBridge.resolveDomain(host)` through the cached JNI method.
///
/// Kotlin returns either an IPv4 literal (e.g. "93.184.216.34") or null
/// when the underlying non-VPN network is unknown / host resolution fails.
/// We only accept IPv4 — direct-mode connect is IPv4-only downstream
/// anyway (see `session.rs` dst.ip() branch).
fn make_system_resolver_fn() -> SystemResolverFn {
    Arc::new(|host: &str| -> Option<Ipv4Addr> {
        let jvm = JVM.get()?;
        let cache = RESOLVE_METHOD.get()?;

        let mut env = jvm.attach_current_thread_as_daemon().ok()?;
        let host_jstr = env.new_string(host).ok()?;

        let result = unsafe {
            env.call_static_method_unchecked(
                &cache.class,
                jni::objects::JStaticMethodID::from_raw(cache.method_id),
                jni::signature::ReturnType::Object,
                &[jni::sys::jvalue {
                    l: host_jstr.as_raw(),
                },],
            )
        };

        let jvalue = match result {
            Ok(v) => v,
            Err(_) => {
                let _ = env.exception_clear();
                return None;
            }
        };

        // Kotlin's nullable String? comes back as a JObject that may be null.
        let obj = jvalue.l().ok()?;
        if obj.is_null() {
            return None;
        }
        let jstr = jni::objects::JString::from(obj);
        let rust_str: String = env.get_string(&jstr).ok()?.into();
        rust_str.parse::<Ipv4Addr>().ok()
    })
}

/// Proper JSON string value extractor. Walks char-by-char from the
/// opening quote and honors `\"`, `\\`, `\n`, `\t`, `\r` escape sequences,
/// so embedded TOML (routing_toml) with quoted strings doesn't truncate
/// at the first `\"`. The original `find('"')` approach cut off
/// `routing_toml` at the very first escaped quote, and `toml::from_str`
/// silently fell back to the default "proxy everything" routing.
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
                Some('t') => result.push('\t'),
                Some('r') => result.push('\r'),
                Some(other) => {
                    // Unknown escape — keep literally, don't break parsing.
                    result.push('\\');
                    result.push(other);
                }
                None => return Err(format!("unterminated escape in {}", key)),
            }
        } else if c == '"' {
            // End of string value (unescaped quote).
            return Ok(result);
        } else {
            result.push(c);
        }
    }
    Err(format!("unterminated {}", key))
}

fn parse_config(json: &str) -> Result<VpnConfig, String> {
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
    let modifier = get_str("modifier").unwrap_or_else(|_| "positional_xor_rotate".into());
    let salt = get_num("salt").unwrap_or(0xDEADBEEF) as u32;
    let padding_min = get_num("padding_min").unwrap_or(16) as u8;
    let padding_max = get_num("padding_max").unwrap_or(128) as u8;
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
        server_address, server_port, servers, obfuscation_key, modifier, salt,
        padding_min, padding_max, routing, geoip_path: None, on_server_down,
        dns_resolvers,
        hub_url, hub_preset, hub_cache_dir, hub_refresh_interval_secs,
        // Wired in at engine start (nativeStart), not from JSON — the
        // resolver is a JNI callback, not a config value.
        system_resolver: None,
        mux_pool_size,
    })
}

/// Extract the `servers` array (LLD-10): `[{"name":"aeza","address":"1.2.3.4",
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
                port: item.get("port").and_then(|v| v.as_u64()).unwrap_or(8443) as u16,
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
/// рядом (по умолчанию "direct"). `None`, когда ключа нет вовсе (легаси-конфиг
/// со старым приложением). Битые записи выбрасывает `to_routing_config`
/// с WARN, старт туннеля они не валят.
fn parse_user_rules(json: &str) -> Option<RoutingConfig> {
    let value = serde_json::from_str::<serde_json::Value>(json).ok()?;
    let arr = value.get("user_rules")?.as_array()?;
    let rules: Vec<xr_proto::user_rule::UserRule> = arr
        .iter()
        .filter_map(|item| serde_json::from_value(item.clone()).ok())
        .collect();
    let default_action = value
        .get("default_action")
        .and_then(|v| v.as_str())
        .unwrap_or("direct");
    Some(xr_proto::user_rule::to_routing_config(&rules, default_action))
}

/// Extract the `dns_resolvers` JSON array of strings, e.g.
/// `"dns_resolvers": ["1.1.1.1", "8.8.8.8:53"]`.
/// Tolerant of missing key (returns empty vec) and minor whitespace.
fn parse_dns_resolvers(json: &str) -> Vec<String> {
    let key = "\"dns_resolvers\"";
    let pos = match json.find(key) { Some(p) => p, None => return vec![] };
    let after = &json[pos + key.len()..];
    let bracket = match after.find('[') { Some(p) => p, None => return vec![] };
    let close = match after[bracket..].find(']') { Some(p) => p, None => return vec![] };
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
    RoutingConfig { default_action: "proxy".into(), rules: vec![] }
}

/// Translate engine InvalidInput errors into user-friendly messages.
fn humanize_config_error(e: &std::io::Error) -> String {
    let raw = e.to_string();
    if raw.contains("invalid symbol") || raw.contains("Invalid padding") ||
       raw.contains("Invalid byte") {
        "Неверный ключ шифрования — проверьте, что вставлена только base64-строка без лишних символов".to_string()
    } else if raw.contains("key must not be empty") {
        "Ключ шифрования не указан".to_string()
    } else if raw.contains("unknown modifier") {
        "Неизвестный модификатор обфускации".to_string()
    } else if raw.contains("invalid socket address") || raw.contains("invalid IP address") {
        "Неверный адрес сервера — проверьте IP и порт".to_string()
    } else {
        format!("Ошибка настроек: {}", raw)
    }
}

// ── JNI exports ─────────────────────────────────────────────────────

/// Start the VPN engine. Returns null on success, or an error message string on failure.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    _tun_fd: jint,
    config_json: JString,
) -> jstring {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("xr_core=debug,xr_proto=info")
        .with_target(false)
        .try_init();

    // Cache JVM reference (once).
    if JVM.get().is_none() {
        if let Ok(jvm) = env.get_java_vm() {
            let _ = JVM.set(jvm);
        }
    }

    // Cache class + method IDs on the MAIN thread where ClassLoader works.
    // NativeBridge is a Kotlin `object`, so `class` parameter may be the instance,
    // not the JClass. Use FindClass explicitly to get the correct JClass.
    if PROTECT_METHOD.get().is_none() || RESOLVE_METHOD.get().is_none() {
        match env.find_class("com/xrproxy/app/jni/NativeBridge") {
            Ok(found_class) => {
                if PROTECT_METHOD.get().is_none() {
                    match env.get_static_method_id(&found_class, "protectSocket", "(I)Z") {
                        Ok(mid) => {
                            if let Ok(global_class) = env.new_global_ref(&found_class) {
                                let _ = PROTECT_METHOD.set(ProtectMethodCache {
                                    class: global_class,
                                    method_id: mid.into_raw(),
                                });
                                tracing::info!("Cached protectSocket method ID");
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to get protectSocket method: {:?}", e);
                        }
                    }
                }
                if RESOLVE_METHOD.get().is_none() {
                    // Return type is nullable String, so signature is `(String)Ljava/lang/String;`.
                    match env.get_static_method_id(
                        &found_class,
                        "resolveDomain",
                        "(Ljava/lang/String;)Ljava/lang/String;",
                    ) {
                        Ok(mid) => {
                            if let Ok(global_class) = env.new_global_ref(&found_class) {
                                let _ = RESOLVE_METHOD.set(ResolveMethodCache {
                                    class: global_class,
                                    method_id: mid.into_raw(),
                                });
                                tracing::info!("Cached resolveDomain method ID");
                            }
                        }
                        Err(e) => {
                            // Not fatal — direct mode will fall back to UDP:53
                            // probes. Older builds without the Kotlin bridge
                            // method keep working.
                            tracing::warn!("resolveDomain not found, UDP-only DNS: {:?}", e);
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to find NativeBridge class: {:?}", e);
            }
        }
    }

    // Helper: return an error string to Kotlin (журналируем здесь же, чтобы
    // отказ старта был виден на вкладке Log, а не только в снекбаре).
    let err = |env: &mut JNIEnv, msg: &str| -> jstring {
        tracing::error!("{}", msg);
        journal_log("ERROR", "vpn", msg);
        env.new_string(msg).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
    };

    let config_str: String = match env.get_string(&config_json) {
        Ok(s) => s.into(),
        Err(_) => return err(&mut env, "Failed to read config string"),
    };

    let mut config = match parse_config(&config_str) {
        Ok(c) => c,
        Err(e) => return err(&mut env, &format!("Ошибка конфигурации: {}", e)),
    };
    // Only wire the resolver if Kotlin exposes the bridge method; otherwise
    // leave it None so direct mode goes through the UDP fallback as before.
    if RESOLVE_METHOD.get().is_some() {
        config.system_resolver = Some(make_system_resolver_fn());
    }

    let mut engine = VpnEngine::new(config);
    // Лента движка уходит в общий персистентный журнал (XR-042): перезапуск
    // движка при смене сети больше не обнуляет лог на вкладке Log.
    if let Some(j) = JOURNAL.get() {
        engine.stats().set_journal(j.clone());
    }
    let queue = PacketQueue::new();
    let protect = make_protect_fn();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return err(&mut env, &format!("Tokio runtime error: {}", e)),
    };

    let _guard = runtime.enter();
    match engine.start(queue.clone(), protect) {
        Ok(()) => {
            let mut lock = get_engine().lock().unwrap();
            *lock = Some(EngineHandle { engine, queue, runtime });
            tracing::info!("VPN engine started");
            std::ptr::null_mut() // null = success
        }
        Err(e) => {
            let user_msg = match e.kind() {
                std::io::ErrorKind::InvalidInput => humanize_config_error(&e),
                std::io::ErrorKind::AlreadyExists => "Туннель уже запущен".to_string(),
                _ => format!("Ошибка запуска: {}", e),
            };
            err(&mut env, &user_msg)
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeStop(
    _env: JNIEnv, _class: JClass,
) {
    let mut lock = get_engine().lock().unwrap();
    if let Some(mut handle) = lock.take() {
        handle.engine.stop();
        tracing::info!("VPN engine stopped");
    }
}

/// Notify the engine that the underlying network switched (LTE↔Wi-Fi).
/// Recycles the mux pool and drops live sessions so the tunnel re-binds onto
/// the new uplink without a manual off/on. No-op if the engine isn't running.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeOnNetworkChanged(
    _env: JNIEnv, _class: JClass,
) {
    let lock = get_engine().lock().unwrap();
    if let Some(ref handle) = *lock {
        // Enter the engine's runtime: on_network_changed spawns a recycle task.
        let _guard = handle.runtime.enter();
        handle.engine.on_network_changed();
    }
}

/// Read a Java `String[]` into a `Vec<String>`, skipping null/unreadable
/// elements. Used by the trusted-network SSID bridge below.
fn read_jstring_array(env: &mut JNIEnv, arr: &JObjectArray) -> Vec<String> {
    let len = env.get_array_length(arr).unwrap_or(0);
    let mut out = Vec::with_capacity(len.max(0) as usize);
    for i in 0..len {
        if let Ok(obj) = env.get_object_array_element(arr, i) {
            if obj.is_null() {
                continue;
            }
            let js = JString::from(obj);
            if let Ok(s) = env.get_string(&js) {
                out.push(s.into());
            };
        }
    }
    out
}

/// True if the raw current SSID (as returned by `WifiInfo.getSSID()`) matches
/// any entry in the trusted list. Pure string logic — works whether or not the
/// engine is running (the auto-pause watcher calls it while paused). See
/// `xr_core::trusted`. Errors / unreadable args degrade to `false` (no match
/// → never auto-pause), matching the "fail soft" contract of the feature.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeSsidMatches(
    mut env: JNIEnv,
    _class: JClass,
    current: JString,
    trusted: JObjectArray,
) -> jboolean {
    let current_str: String = match env.get_string(&current) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let trusted_vec = read_jstring_array(&mut env, &trusted);
    if xr_core::trusted::ssid_matches(&current_str, &trusted_vec) {
        1
    } else {
        0
    }
}

/// Normalize a raw `WifiInfo.getSSID()` value for display (strip the quote
/// wrapping Android adds). Returns the clean SSID, or null for an
/// unavailable/hidden network. See `xr_core::trusted::normalize_ssid`.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeNormalizeSsid(
    mut env: JNIEnv,
    _class: JClass,
    raw: JString,
) -> jstring {
    let raw_str: String = match env.get_string(&raw) {
        Ok(s) => s.into(),
        Err(_) => return std::ptr::null_mut(),
    };
    match xr_core::trusted::normalize_ssid(&raw_str) {
        Some(clean) => env
            .new_string(&clean)
            .map(|s| s.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeGetState(
    env: JNIEnv, _class: JClass,
) -> jstring {
    let lock = get_engine().lock().unwrap();
    let state_str = match *lock {
        Some(ref h) => h.engine.state().get().to_string(),
        None => "Disconnected".to_string(),
    };
    env.new_string(&state_str).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeGetStats(
    env: JNIEnv, _class: JClass,
) -> jstring {
    let lock = get_engine().lock().unwrap();
    let json = match *lock {
        Some(ref h) => {
            let s = h.engine.stats().snapshot();
            let debug_escaped = s.debug_msg.replace('\\', "\\\\").replace('"', "\\\"");
            // Активный сервер пула (LLD-10): имя + признак резерва для
            // статусной строки «через X (резерв)» на главном экране.
            // Лента событий сюда больше не входит: UI читает её напрямую
            // из журнала (`nativeJournalTail`), движок и не должен быть
            // запущен, чтобы лог был виден (XR-042).
            let (srv_name, srv_backup) = h.engine.active_server_info()
                .unwrap_or_default();
            let srv_escaped = srv_name.replace('\\', "\\\\").replace('"', "\\\"");
            format!(
                "{{\"bytes_up\":{},\"bytes_down\":{},\"active\":{},\"total\":{},\"uptime\":{},\"dns\":{},\"syns\":{},\"smol_recv\":{},\"smol_send\":{},\"relay_warn\":{},\"relay_err\":{},\"debug\":\"{}\",\"active_server\":\"{}\",\"backup_active\":{}}}",
                s.bytes_up, s.bytes_down, s.active_connections, s.total_connections, s.uptime_seconds,
                s.dns_queries, s.tcp_syns, s.smol_recv, s.smol_send, s.relay_warns, s.relay_errors, debug_escaped,
                srv_escaped, srv_backup,
            )
        }
        None => "{\"bytes_up\":0,\"bytes_down\":0,\"active\":0,\"total\":0,\"uptime\":0,\"dns\":0,\"syns\":0,\"smol_recv\":0,\"smol_send\":0,\"relay_warn\":0,\"relay_err\":0,\"debug\":\"\"}".into(),
    };
    env.new_string(&json).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

// ── Журнал приложения (XR-042) ──────────────────────────────────────

/// Инициализировать персистентный журнал в `dir` (повторный вызов только
/// обновляет параметры ротации: настройки поменяли на лету). Вызывается из
/// `Application.onCreate`, до любых других обращений к журналу.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeJournalInit(
    mut env: JNIEnv,
    _class: JClass,
    dir: JString,
    max_file_bytes: jlong,
    max_files: jint,
) {
    let dir: String = match env.get_string(&dir) {
        Ok(s) => s.into(),
        Err(_) => return,
    };
    let bytes = max_file_bytes.max(0) as u64;
    let files = max_files.max(1) as u32;
    match JOURNAL.get() {
        Some(j) => j.set_rotation(bytes, files),
        None => {
            let _ = JOURNAL.set(xr_core::journal::Journal::open(PathBuf::from(dir), bytes, files));
        }
    }
}

/// Запись из Kotlin-слоя (пробы, смены сети/режима, жизненный цикл сервиса).
/// `level` из {"INFO","WARN","ERROR"}, `source` это короткий тег источника.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeJournalLog(
    mut env: JNIEnv,
    _class: JClass,
    level: JString,
    source: JString,
    message: JString,
) {
    let level: String = match env.get_string(&level) {
        Ok(s) => s.into(),
        Err(_) => return,
    };
    let source: String = match env.get_string(&source) {
        Ok(s) => s.into(),
        Err(_) => return,
    };
    let message: String = match env.get_string(&message) {
        Ok(s) => s.into(),
        Err(_) => return,
    };
    journal_log(&level, &source, &message);
}

/// Хвост журнала (последние строки, от старых к новым) одной строкой с `\n`.
/// Работает независимо от того, запущен ли движок.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeJournalTail(
    env: JNIEnv, _class: JClass,
) -> jstring {
    let text = match JOURNAL.get() {
        Some(j) => j.tail().join("\n"),
        None => String::new(),
    };
    env.new_string(&text).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// Полное содержимое журнала с диска (экспорт/шаринг), от старых записей к новым.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeJournalDump(
    env: JNIEnv, _class: JClass,
) -> jstring {
    let text = match JOURNAL.get() {
        Some(j) => j.dump(),
        None => String::new(),
    };
    env.new_string(&text).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// Очистить журнал (кнопка Clear на вкладке Log). Заодно сбрасывает
/// кумулятивные счётчики WARN/ERROR движка, если тот запущен, чтобы бадж
/// и заголовок вкладки начали счёт заново.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeJournalClear(
    _env: JNIEnv, _class: JClass,
) {
    let lock = get_engine().lock().unwrap();
    if let Some(ref h) = *lock {
        // clear_errors чистит подключённый журнал (тот же буфер) и счётчики.
        h.engine.stats().clear_errors();
    } else if let Some(j) = JOURNAL.get() {
        j.clear();
    }
}

#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativePushPacket(
    env: JNIEnv, _class: JClass, packet: jni::objects::JByteArray,
) {
    let lock = get_engine().lock().unwrap();
    if let Some(ref handle) = *lock {
        if let Ok(bytes) = env.convert_byte_array(&packet) {
            handle.queue.push_inbound(bytes);
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativePopPacket(
    env: JNIEnv, _class: JClass,
) -> jni::sys::jbyteArray {
    let lock = get_engine().lock().unwrap();
    if let Some(ref handle) = *lock {
        if let Some(packet) = handle.queue.pop_outbound() {
            if let Ok(arr) = env.byte_array_from_slice(&packet) {
                return arr.into_raw();
            }
        }
    }
    std::ptr::null_mut()
}

// ── Onboarding bridge ───────────────────────────────────────────────

fn jstring_into_raw(env: &mut JNIEnv, s: String) -> jstring {
    env.new_string(&s).map(|js| js.into_raw()).unwrap_or(std::ptr::null_mut())
}

fn read_jstring(env: &mut JNIEnv, js: &JString) -> Result<String, String> {
    env.get_string(js)
        .map(|s| s.into())
        .map_err(|e| format!("jstring: {e}"))
}

fn json_error(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

fn with_onboarding_runtime<F, R>(f: F) -> Result<R, String>
where
    F: std::future::Future<Output = R>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    Ok(rt.block_on(f))
}

/// Parse a raw invite URL (scanned / pasted / deep-linked). Returns JSON:
/// success → `{"kind":"https|custom","hub_url":..,"token":..}`,
/// failure → `{"error":".."}`.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeParseInviteLink(
    mut env: JNIEnv,
    _class: JClass,
    raw: JString,
) -> jstring {
    let raw_str = match read_jstring(&mut env, &raw) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };

    let json = match invite_url::parse_invite_link(&raw_str) {
        Ok(link) => serde_json::to_string(&link)
            .unwrap_or_else(|e| json_error(&format!("serialize: {e}"))),
        Err(e) => json_error(&e.to_string()),
    };
    jstring_into_raw(&mut env, json)
}

/// Fetch invite metadata (no consume). Returns InviteInfo JSON on
/// success (contains `status`: "active" | "consumed" | "expired"), or
/// `{"error":".."}` on failure (network, 404, parse).
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeFetchInviteInfo(
    mut env: JNIEnv,
    _class: JClass,
    hub_url: JString,
    token: JString,
    timeout_ms: jlong,
) -> jstring {
    let hub_url = match read_jstring(&mut env, &hub_url) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let token = match read_jstring(&mut env, &token) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let timeout = Duration::from_millis(timeout_ms.max(0) as u64);

    let result = with_onboarding_runtime(onboarding::fetch_invite_info(&hub_url, &token, timeout));
    let json = match result {
        Ok(Ok(info)) => serde_json::to_string(&info)
            .unwrap_or_else(|e| json_error(&format!("serialize: {e}"))),
        Ok(Err(e)) => json_error(&e),
        Err(e) => json_error(&e),
    };
    jstring_into_raw(&mut env, json)
}

/// Claim the invite + TOFU public key + pre-warm preset cache.
/// Always returns a structured JSON:
/// `{"payload":..?,"public_key":..?,"preset_cached":bool,"errors":[..]}`.
/// `payload` null means the whole apply failed (check `errors`).
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeApplyInvite(
    mut env: JNIEnv,
    _class: JClass,
    hub_url: JString,
    token: JString,
    preset: JString,
    cache_dir: JString,
    timeout_ms: jlong,
) -> jstring {
    let hub_url = match read_jstring(&mut env, &hub_url) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let token = match read_jstring(&mut env, &token) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let preset = match read_jstring(&mut env, &preset) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let cache_dir = match read_jstring(&mut env, &cache_dir) {
        Ok(s) => PathBuf::from(s),
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let timeout = Duration::from_millis(timeout_ms.max(0) as u64);

    let result = with_onboarding_runtime(onboarding::apply_invite(
        &hub_url, &token, &preset, &cache_dir, timeout,
    ));

    let json = match result {
        Ok(apply) => {
            let payload_value = apply
                .payload
                .as_ref()
                .and_then(|p| serde_json::to_value(p).ok())
                .unwrap_or(serde_json::Value::Null);
            serde_json::json!({
                "payload": payload_value,
                "public_key": apply.public_key,
                "preset_cached": apply.preset_cached,
                "errors": apply.errors,
            })
            .to_string()
        }
        Err(e) => serde_json::json!({
            "payload": serde_json::Value::Null,
            "public_key": serde_json::Value::Null,
            "preset_cached": false,
            "errors": [e],
        })
        .to_string(),
    };
    jstring_into_raw(&mut env, json)
}

// ── Редактор правил (LLD-05, XR-047) ────────────────────────────────

/// Классифицировать паттерн пользовательского правила. Валидация одна на
/// Rust и Kotlin, поэтому UI зовёт её сюда, а не дублирует regex'ами.
/// Возвращает JSON: `{"kind":"domain|wildcard|cidr4|cidr6","normalized":".."}`
/// либо `{"kind":"invalid","error":"текст для пользователя"}`.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeClassifyPattern(
    mut env: JNIEnv,
    _class: JClass,
    raw: JString,
) -> jstring {
    let raw_str = match read_jstring(&mut env, &raw) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let normalized = xr_proto::user_rule::normalize_pattern(&raw_str);
    let json = match xr_proto::user_rule::classify_pattern(&normalized) {
        Ok(kind) => serde_json::json!({
            "kind": kind.as_str(),
            "normalized": normalized,
        })
        .to_string(),
        Err(e) => serde_json::json!({
            "kind": "invalid",
            "error": e.to_string(),
        })
        .to_string(),
    };
    jstring_into_raw(&mut env, json)
}

/// Форсированный fetch пресета с хаба («Обновить сейчас» на карточке
/// пресета). Обновляет тот же дисковый кэш, из которого движок собирает
/// merged-роутер при старте и фоновом рефреше. Возвращает JSON:
/// `{"updated":bool,"version":N}` либо `{"error":".."}` (сеть, not_found).
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeRefreshPreset(
    mut env: JNIEnv,
    _class: JClass,
    hub_url: JString,
    preset: JString,
    cache_dir: JString,
    timeout_ms: jlong,
) -> jstring {
    let hub_url = match read_jstring(&mut env, &hub_url) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let preset = match read_jstring(&mut env, &preset) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let cache_dir = match read_jstring(&mut env, &cache_dir) {
        Ok(s) => PathBuf::from(s),
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let timeout = Duration::from_millis(timeout_ms.max(0) as u64);

    let result = with_onboarding_runtime(async {
        let mut cache = xr_core::presets::PresetCache::new(&cache_dir, &hub_url, &preset);
        cache.load_from_disk();
        cache.refresh(timeout).await
    });
    let json = match result {
        Ok(Ok(outcome)) => {
            let (updated, version) = match outcome {
                xr_core::presets::RefreshOutcome::Updated(v) => (true, v),
                xr_core::presets::RefreshOutcome::UpToDate(v) => (false, v),
            };
            serde_json::json!({ "updated": updated, "version": version }).to_string()
        }
        Ok(Err(e)) | Err(e) => {
            journal_log("WARN", "rules", &format!("обновление пресета {}: {}", preset, e));
            json_error(&e)
        }
    };
    jstring_into_raw(&mut env, json)
}

// ── APK self-update bridge (LLD-12) ─────────────────────────────────

/// Check the hub for a newer signed release. The manifest signature is
/// verified with the **pinned** release public key (passed in from
/// `BuildConfig.RELEASE_PUBLIC_KEY` — never fetched from the network) inside
/// Rust before anything is reported. Returns JSON:
///   newer available → `{"available":true,"manifest":{...}}`
///   up-to-date/older → `{"available":false}`
///   any failure      → `{"available":false,"error":".."}`
///   (network, bad signature, wrong key, unparseable manifest)
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeCheckUpdate(
    mut env: JNIEnv,
    _class: JClass,
    hub_url: JString,
    current_code: jlong,
    pinned_key: JString,
    timeout_ms: jlong,
) -> jstring {
    let unavailable = |env: &mut JNIEnv, error: Option<String>| -> jstring {
        let json = match error {
            Some(e) => serde_json::json!({ "available": false, "error": e }),
            None => serde_json::json!({ "available": false }),
        };
        jstring_into_raw(env, json.to_string())
    };

    let hub_url = match read_jstring(&mut env, &hub_url) {
        Ok(s) => s,
        Err(e) => return unavailable(&mut env, Some(e)),
    };
    let pinned_key = match read_jstring(&mut env, &pinned_key) {
        Ok(s) => s,
        Err(e) => return unavailable(&mut env, Some(e)),
    };
    if hub_url.trim().is_empty() {
        return unavailable(&mut env, Some("no_hub".into()));
    }
    if pinned_key.trim().is_empty() {
        // No release key compiled into this build — feature disabled.
        return unavailable(&mut env, Some("no_release_key".into()));
    }
    let current = current_code.max(0) as u64;
    let timeout = Duration::from_millis(timeout_ms.max(0) as u64);

    let fetched = with_onboarding_runtime(update::fetch_manifest(&hub_url, timeout));
    let signed = match fetched {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return unavailable(&mut env, Some(e)),
        Err(e) => return unavailable(&mut env, Some(e)),
    };

    let manifest = match update::verify_manifest(&signed, &pinned_key) {
        Ok(m) => m,
        // A failed signature/SHA is a security event — log WARN (§2.2).
        Err(e) => {
            tracing::warn!("update manifest rejected: {e}");
            return unavailable(&mut env, Some(format!("verify: {e}")));
        }
    };

    if !update::manifest_offers_update(&manifest, current) {
        return unavailable(&mut env, None);
    }

    let manifest_value = serde_json::to_value(&manifest).unwrap_or(serde_json::Value::Null);
    let json = serde_json::json!({
        "available": true,
        "manifest": manifest_value,
    });
    jstring_into_raw(&mut env, json.to_string())
}

/// Verify a downloaded APK's SHA-256 against `sha256_hex` (from the already
/// signature-verified manifest). Returns `false` on any I/O error or mismatch
/// — a truncated download is rejected and the file must be deleted.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeVerifyApk(
    mut env: JNIEnv,
    _class: JClass,
    path: JString,
    sha256_hex: JString,
) -> jboolean {
    let path = match read_jstring(&mut env, &path) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let sha = match read_jstring(&mut env, &sha256_hex) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    if update::verify_apk_sha256(std::path::Path::new(&path), &sha) {
        1
    } else {
        0
    }
}

// ── File-sharing bridge (LLD-19) ────────────────────────────────────
//
// The Kotlin "Files" screen drives the one-way mirror engine in xr-core: list
// shares from the hub, browse a share's manifest, download selected files, or
// run a full mirror sync. All file/diff logic lives in Rust (`xr_core::sync`);
// Kotlin only supplies storage paths and a schedule. The token (a ShareToken
// JSON the owner handed out) is verified by the agent offline — never here.
//
// NOTE: token blobs are never logged. TLS pinning of a self-signed agent
// identity is an Android-side follow-up; today the engine works over HTTP and
// CA-valid HTTPS.

/// Parse a ShareToken JSON string into the typed token.
fn parse_token(json: &str) -> Result<ShareToken, String> {
    serde_json::from_str::<ShareToken>(json).map_err(|e| format!("bad token json: {e}"))
}

/// Parse the optional relay leg (a [`RelayGrant`] JSON, or empty for
/// direct-only, LLD-23 §2.4). A malformed leg is treated as absent: a share must
/// still work on its direct address if the relay descriptor is somehow broken.
fn parse_relay(json: &str) -> Option<RelayGrant> {
    let s = json.trim();
    if s.is_empty() {
        return None;
    }
    serde_json::from_str(s).ok()
}

/// Parse the sync selection into the three cases the mirror engine tells apart
/// (XR-135). The consumer sends a JSON array of chosen manifest paths / folder
/// prefixes:
///   - missing, empty, or the literal `null` -> `None`: no selection at all,
///     which for the engine means "the whole share" (`plan_sync`);
///   - `"[]"` -> `Some(empty)`: the selection is present but empty. Nothing is
///     desired, so the plan is pure deletes, a delete-only pass that wipes the
///     local copy. Folding this into `None` was the XR-135 bug: unticking the
///     last file left the download undeletable through sync;
///   - a non-empty array -> `Some(set)`: mirror exactly that subset.
/// A malformed array is an error, not a silent whole-share or delete-all: both
/// defaults would surprise, and delete-all is destructive.
fn parse_selection(json: &str) -> Result<Option<HashSet<String>>, String> {
    let s = json.trim();
    if s.is_empty() || s == "null" {
        return Ok(None);
    }
    serde_json::from_str::<Vec<String>>(s)
        .map(|v| Some(v.into_iter().collect()))
        .map_err(|e| format!("bad selection json: {e}"))
}

/// GET the hub's public share index. Returns `{"shares":[{name,addr,port,
/// agent_pubkey,share_id}...]}` or `{"error":".."}`.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeListShares(
    mut env: JNIEnv,
    _class: JClass,
    hub_url: JString,
    timeout_ms: jlong,
) -> jstring {
    let hub_url = match read_jstring(&mut env, &hub_url) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let timeout = Duration::from_millis(timeout_ms.max(0) as u64);

    let json = match with_onboarding_runtime(sync::list_shares(&hub_url, timeout)) {
        Ok(Ok(shares)) => serde_json::json!({ "shares": shares }).to_string(),
        Ok(Err(e)) | Err(e) => json_error(&e),
    };
    jstring_into_raw(&mut env, json)
}

/// GET the shares attached to an invite (the access anchor, §9.5). Returns
/// `{"shares":[{share_id,name,addr,port,agent_pubkey,token,exp}...]}` where
/// `token` is the **decoded ShareToken JSON** (ready for `nativeFetchManifest` /
/// `nativeDownloadFile`, which select the share by the token via the agent's
/// legacy routes). `{"error":".."}` on failure (a 410 means invite expired).
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeInviteShares(
    mut env: JNIEnv,
    _class: JClass,
    hub_url: JString,
    invite_token: JString,
    timeout_ms: jlong,
) -> jstring {
    let hub_url = match read_jstring(&mut env, &hub_url) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let invite_token = match read_jstring(&mut env, &invite_token) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let timeout = Duration::from_millis(timeout_ms.max(0) as u64);

    let json = match with_onboarding_runtime(sync::list_invite_shares(&hub_url, &invite_token, timeout)) {
        Ok(Ok(grants)) => {
            let shares: Vec<_> = grants
                .into_iter()
                .map(|g| {
                    // The grant token is a base64url(ShareToken JSON) blob; decode
                    // it back to JSON so the existing manifest/download path (which
                    // expects a ShareToken JSON) consumes it unchanged.
                    let token_json = base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(g.token.trim())
                        .ok()
                        .and_then(|b| String::from_utf8(b).ok())
                        .unwrap_or_default();
                    let mut o = serde_json::json!({
                        "share_id": g.share_id,
                        "name": g.name,
                        "addr": g.addr,
                        "port": g.port,
                        "agent_pubkey": g.agent_pubkey,
                        "token": token_json,
                        "exp": g.exp,
                    });
                    // The relay leg (LLD-23 §2.4), passed through verbatim so the
                    // consumer stores it and falls back to the relay when the
                    // direct address is unreachable. Absent for a direct share.
                    if let Some(relay) = &g.relay {
                        if let Ok(v) = serde_json::to_value(relay) {
                            o["relay"] = v;
                        }
                    }
                    o
                })
                .collect();
            serde_json::json!({ "shares": shares }).to_string()
        }
        Ok(Err(e)) | Err(e) => json_error(&e),
    };
    jstring_into_raw(&mut env, json)
}

/// Fetch a share's manifest from the agent (presenting the token). Returns the
/// ShareManifest JSON (`{"entries":[{path,size,mtime,sha256}...]}`) or
/// `{"error":".."}`. Used to populate the file picker for one-time download.
/// `agent_pubkey` is the base64 identity key from the grant; the agent's
/// manifest signature is verified against it, fail-closed (XR-046). Empty
/// string skips the pinning (no key to check against).
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeFetchManifest(
    mut env: JNIEnv,
    _class: JClass,
    agent_url: JString,
    token_json: JString,
    agent_pubkey: JString,
    relay_json: JString,
    timeout_ms: jlong,
) -> jstring {
    let agent_url = match read_jstring(&mut env, &agent_url) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let token = match read_jstring(&mut env, &token_json).and_then(|s| parse_token(&s)) {
        Ok(t) => t,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let agent_pubkey = match read_jstring(&mut env, &agent_pubkey) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let relay = read_jstring(&mut env, &relay_json).ok().and_then(|s| parse_relay(&s));
    let timeout = Duration::from_millis(timeout_ms.max(0) as u64);

    let json = match with_onboarding_runtime(sync::fetch_manifest_relay(&agent_url, &token, &agent_pubkey, relay.as_ref(), timeout)) {
        Ok(Ok(manifest)) => serde_json::to_string(&manifest)
            .unwrap_or_else(|e| json_error(&format!("serialize: {e}"))),
        Ok(Err(e)) | Err(e) => {
            // WARN, не ERROR: агент за выключенным ноутом это штатная
            // ситуация, UI уходит в офлайн-фолбэк (XR-059). Но след в
            // журнале нужен, иначе «шара не открывается» не диагностируется.
            journal_log("WARN", "files", &format!("манифест шары {}: {}", token.share_id, e));
            json_error(&e)
        }
    };
    jstring_into_raw(&mut env, json)
}

/// Download a single manifest entry to `dest_dir` (one-time download). The
/// entry is a ShareManifestEntry JSON; the file is SHA-256-verified before being
/// published. Returns `{"ok":true}` or `{"error":".."}`.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeDownloadFile(
    mut env: JNIEnv,
    _class: JClass,
    agent_url: JString,
    token_json: JString,
    entry_json: JString,
    dest_dir: JString,
    agent_pubkey: JString,
    relay_json: JString,
    timeout_ms: jlong,
) -> jstring {
    let agent_url = match read_jstring(&mut env, &agent_url) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let token = match read_jstring(&mut env, &token_json).and_then(|s| parse_token(&s)) {
        Ok(t) => t,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let agent_pubkey = match read_jstring(&mut env, &agent_pubkey) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let relay = read_jstring(&mut env, &relay_json).ok().and_then(|s| parse_relay(&s));
    let entry = match read_jstring(&mut env, &entry_json) {
        Ok(s) => match serde_json::from_str::<ShareManifestEntry>(&s) {
            Ok(e) => e,
            Err(e) => return jstring_into_raw(&mut env, json_error(&format!("bad entry json: {e}"))),
        },
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let dest = match read_jstring(&mut env, &dest_dir) {
        Ok(s) => PathBuf::from(s),
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let timeout = Duration::from_millis(timeout_ms.max(0) as u64);

    // Drive the shared progress/cancel controller so the UI can show a bar and
    // stop a large single-file download too. The guard locks out a concurrent
    // transfer (e.g. the background mirror worker) instead of sharing counters
    // and the same `.part`; "busy" tells the UI to ask the user to retry.
    let json = match sync::TransferGuard::acquire(&token.share_id, 1, entry.size) {
        None => json_error("busy"),
        Some(_guard) => {
            sync::transfer_file(&entry.path, 0);
            let result = with_onboarding_runtime(sync::download_entry_relay(
                &agent_url, &token, &entry, &dest, &agent_pubkey, relay.as_ref(), timeout,
            ));
            match result {
                Ok(Ok(())) => {
                    journal_log("INFO", "files", &format!(
                        "скачан {} ({} байт), шара {}", entry.path, entry.size, token.share_id,
                    ));
                    serde_json::json!({ "ok": true }).to_string()
                }
                Ok(Err(e)) | Err(e) => {
                    journal_log("ERROR", "files", &format!(
                        "скачивание {}: {}, шара {}", entry.path, e, token.share_id,
                    ));
                    json_error(&e)
                }
            }
        }
    };
    jstring_into_raw(&mut env, json)
}

/// Pure diff for SAF storage: the consumer enumerates its local copy from the
/// SAF tree (in Kotlin) and passes it as `local_json` (a `[{path,sha256}...]`
/// array) along with the agent's `manifest_json`. Returns the plan JSON
/// (`{"fetch":[...],"delete":[...]}`). No network, no filesystem — Kotlin then
/// downloads fetches (via `nativeDownloadFile` to a temp file) and applies the
/// deletes against the SAF tree.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativePlanSync(
    mut env: JNIEnv,
    _class: JClass,
    manifest_json: JString,
    local_json: JString,
    selection_json: JString,
) -> jstring {
    let manifest = match read_jstring(&mut env, &manifest_json) {
        Ok(s) => match serde_json::from_str::<ShareManifest>(&s) {
            Ok(m) => m,
            Err(e) => return jstring_into_raw(&mut env, json_error(&format!("bad manifest json: {e}"))),
        },
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let local = match read_jstring(&mut env, &local_json) {
        Ok(s) => match serde_json::from_str::<Vec<LocalFile>>(&s) {
            Ok(v) => v,
            Err(e) => return jstring_into_raw(&mut env, json_error(&format!("bad local json: {e}"))),
        },
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    // Selection: a JSON array of chosen manifest paths. Empty / "[]" / unreadable
    // means "the whole share" (mirror everything), matching plan_sync.
    let selection: Option<HashSet<String>> = match read_jstring(&mut env, &selection_json) {
        Ok(s) if !s.trim().is_empty() && s.trim() != "[]" => {
            serde_json::from_str::<Vec<String>>(&s).ok().map(|v| v.into_iter().collect())
        }
        _ => None,
    };

    let plan = sync::plan_with_selection(&manifest, &local, selection.as_ref());
    let json =
        serde_json::to_string(&plan).unwrap_or_else(|e| json_error(&format!("serialize: {e}")));
    jstring_into_raw(&mut env, json)
}

/// Drop `target` (a file or folder path) from a selection, splitting a covering
/// folder prefix into its sibling branches (XR-044, `sync::expand_deselect`).
/// `selection_json` and `manifest_json` are JSON string arrays (selection
/// entries and manifest paths); returns the reworked selection as a JSON array.
/// Pure logic, no I/O; lives in Rust so the split and the mirror planner agree
/// on what a selection entry covers.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeExpandDeselect(
    mut env: JNIEnv,
    _class: JClass,
    selection_json: JString,
    manifest_json: JString,
    target: JString,
) -> jstring {
    let selection: HashSet<String> = match read_jstring(&mut env, &selection_json) {
        Ok(s) => serde_json::from_str::<Vec<String>>(&s).unwrap_or_default().into_iter().collect(),
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let manifest: Vec<String> = match read_jstring(&mut env, &manifest_json) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let target = match read_jstring(&mut env, &target) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let mut out: Vec<String> = sync::expand_deselect(&selection, &manifest, &target).into_iter().collect();
    out.sort_unstable();
    let json = serde_json::to_string(&out).unwrap_or_else(|e| json_error(&format!("serialize: {e}")));
    jstring_into_raw(&mut env, json)
}

/// Mirror a share into `dest_dir` (background sync). With `dry_run` true, returns
/// only the plan (`{"plan":{"fetch":[...],"delete":[...]}}`) so the UI can warn
/// about deletions; with `dry_run` false it applies and also returns the report
/// (`{"plan":..,"report":{"fetched":[...],"deleted":[...],"failed":[...]}}`).
/// `agent_pubkey` pins the agent identity for the manifest fetch (XR-046), as
/// in `nativeFetchManifest`. `index_path` names the persistent hash-index file
/// (XR-098) so a warm rescan skips re-hashing; empty = scan without an index.
/// `selection_json` picks the subset to mirror: absent / empty / `null` = the
/// whole share, `"[]"` = an empty selection (delete-only), else the chosen
/// paths (XR-135, see `parse_selection`).
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeSyncShare(
    mut env: JNIEnv,
    _class: JClass,
    agent_url: JString,
    token_json: JString,
    agent_pubkey: JString,
    dest_dir: JString,
    index_path: JString,
    selection_json: JString,
    relay_json: JString,
    dry_run: jboolean,
    timeout_ms: jlong,
) -> jstring {
    let agent_url = match read_jstring(&mut env, &agent_url) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let token = match read_jstring(&mut env, &token_json).and_then(|s| parse_token(&s)) {
        Ok(t) => t,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let agent_pubkey = match read_jstring(&mut env, &agent_pubkey) {
        Ok(s) => s,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let relay = read_jstring(&mut env, &relay_json).ok().and_then(|s| parse_relay(&s));
    let dest = match read_jstring(&mut env, &dest_dir) {
        Ok(s) => PathBuf::from(s),
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let index_path: Option<PathBuf> = match read_jstring(&mut env, &index_path) {
        Ok(s) if !s.trim().is_empty() => Some(PathBuf::from(s)),
        _ => None,
    };
    // Selection tells "the whole share" (None) apart from an empty-but-present
    // selection ("[]" -> delete-only), see parse_selection (XR-135).
    let selection: Option<HashSet<String>> =
        match read_jstring(&mut env, &selection_json).and_then(|s| parse_selection(&s)) {
            Ok(sel) => sel,
            Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
        };
    let timeout = Duration::from_millis(timeout_ms.max(0) as u64);

    let dry = dry_run != 0;
    let json = match with_onboarding_runtime(sync::sync_share_selected_relay(
        &agent_url,
        &token,
        &agent_pubkey,
        &dest,
        selection.as_ref(),
        index_path.as_deref(),
        dry,
        relay.as_ref(),
        timeout,
    )) {
        Ok(Ok(result)) => {
            // dry_run это предпросмотр плана, событием не считается. "busy"
            // тоже не журналируем: ожидаемая контенция с другим переносом,
            // фоновый синк просто повторит на следующем цикле.
            if let (false, Some(report)) = (dry, result.report.as_ref()) {
                if report.fetched.is_empty() && report.deleted.is_empty() && report.failed.is_empty() {
                    // Пустой план (всё уже на месте): не шумим в журнал,
                    // фоновый синк ходит по расписанию.
                } else {
                    journal_log("INFO", "files", &format!(
                        "синк шары {}: получено {}, удалено {}",
                        token.share_id, report.fetched.len(), report.deleted.len(),
                    ));
                }
                if let Some((path, reason)) = report.failed.first() {
                    journal_log("ERROR", "files", &format!(
                        "синк шары {}: ошибок {}, первая: {}: {}",
                        token.share_id, report.failed.len(), path, reason,
                    ));
                }
            }
            serde_json::to_string(&result)
                .unwrap_or_else(|e| json_error(&format!("serialize: {e}")))
        }
        Ok(Err(e)) | Err(e) => {
            if !dry && e != "busy" {
                journal_log("ERROR", "files", &format!("синк шары {}: {}", token.share_id, e));
            }
            json_error(&e)
        }
    };
    jstring_into_raw(&mut env, json)
}

/// Rebuild the [`ShareGrant`] the import calls take from the pieces Kotlin
/// stores per share (LLD-29): the decoded token JSON goes back to its blob
/// form, the share id comes from the token itself.
fn grant_from_parts(
    addr: String,
    port: u16,
    token: ShareToken,
    agent_pubkey: String,
    relay: Option<RelayGrant>,
) -> ShareGrant {
    let blob = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&token).expect("serialize token"));
    ShareGrant {
        share_id: token.share_id.clone(),
        name: String::new(),
        addr,
        port,
        agent_pubkey,
        token: blob,
        exp: token.exp,
        relay,
    }
}

/// Start a URL-import job on a share (LLD-29): the agent downloads the page's
/// content with its plugin into `dest` (share-relative dir, "" = root).
/// `height` is the wanted frame height, `<= 0` means "the owner's cap".
/// Returns `{"job_id":".."}` or `{"error":".."}`; the scope check runs before
/// any network, so a read-only grant fails fast with a human message.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeImportUrl(
    mut env: JNIEnv,
    _class: JClass,
    addr: JString,
    port: jint,
    token_json: JString,
    agent_pubkey: JString,
    relay_json: JString,
    url: JString,
    dest: JString,
    height: jint,
    timeout_ms: jlong,
) -> jstring {
    let parts = (|| -> Result<(String, ShareToken, String, Option<RelayGrant>, String, String), String> {
        let addr = read_jstring(&mut env, &addr)?;
        let token = read_jstring(&mut env, &token_json).and_then(|s| parse_token(&s))?;
        let pubkey = read_jstring(&mut env, &agent_pubkey)?;
        let relay = read_jstring(&mut env, &relay_json).ok().and_then(|s| parse_relay(&s));
        let url = read_jstring(&mut env, &url)?;
        let dest = read_jstring(&mut env, &dest)?;
        Ok((addr, token, pubkey, relay, url, dest))
    })();
    let (addr, token, pubkey, relay, url, dest) = match parts {
        Ok(p) => p,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let share_id = token.share_id.clone();
    let grant = grant_from_parts(addr, port.max(0) as u16, token, pubkey, relay);
    let wanted = (height > 0).then_some(height as u32);
    let timeout = Duration::from_millis(timeout_ms.max(0) as u64);

    let json = match with_onboarding_runtime(sync::import_url(&grant, &url, &dest, wanted, timeout)) {
        Ok(Ok(job_id)) => {
            journal_log("INFO", "files", &format!("импорт запущен, шара {share_id}, джоба {job_id}"));
            serde_json::json!({ "job_id": job_id }).to_string()
        }
        Ok(Err(e)) | Err(e) => {
            journal_log("WARN", "files", &format!("импорт не запустился, шара {share_id}: {e}"));
            json_error(&e)
        }
    };
    jstring_into_raw(&mut env, json)
}

/// Poll an import job: returns the agent's status JSON
/// (`{"state":"..","progress":..,"files":[..],"error":".."}`) or
/// `{"error":".."}`. A lost job (agent restart) comes back as the named
/// `job_lost: ...` error, worded in Rust.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeImportStatus(
    mut env: JNIEnv,
    _class: JClass,
    addr: JString,
    port: jint,
    token_json: JString,
    agent_pubkey: JString,
    relay_json: JString,
    job_id: JString,
    timeout_ms: jlong,
) -> jstring {
    let parts = (|| -> Result<(String, ShareToken, String, Option<RelayGrant>, String), String> {
        let addr = read_jstring(&mut env, &addr)?;
        let token = read_jstring(&mut env, &token_json).and_then(|s| parse_token(&s))?;
        let pubkey = read_jstring(&mut env, &agent_pubkey)?;
        let relay = read_jstring(&mut env, &relay_json).ok().and_then(|s| parse_relay(&s));
        let job_id = read_jstring(&mut env, &job_id)?;
        Ok((addr, token, pubkey, relay, job_id))
    })();
    let (addr, token, pubkey, relay, job_id) = match parts {
        Ok(p) => p,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let grant = grant_from_parts(addr, port.max(0) as u16, token, pubkey, relay);
    let timeout = Duration::from_millis(timeout_ms.max(0) as u64);

    let json = match with_onboarding_runtime(sync::import_status(&grant, &job_id, timeout)) {
        Ok(Ok(status)) => serde_json::to_string(&status)
            .unwrap_or_else(|e| json_error(&format!("serialize: {e}"))),
        Ok(Err(e)) | Err(e) => json_error(&e),
    };
    jstring_into_raw(&mut env, json)
}

/// Cancel an import job (the agent kills the plugin and forgets the job).
/// Returns `{"ok":true}` or `{"error":".."}`.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeImportCancel(
    mut env: JNIEnv,
    _class: JClass,
    addr: JString,
    port: jint,
    token_json: JString,
    agent_pubkey: JString,
    relay_json: JString,
    job_id: JString,
    timeout_ms: jlong,
) -> jstring {
    let parts = (|| -> Result<(String, ShareToken, String, Option<RelayGrant>, String), String> {
        let addr = read_jstring(&mut env, &addr)?;
        let token = read_jstring(&mut env, &token_json).and_then(|s| parse_token(&s))?;
        let pubkey = read_jstring(&mut env, &agent_pubkey)?;
        let relay = read_jstring(&mut env, &relay_json).ok().and_then(|s| parse_relay(&s));
        let job_id = read_jstring(&mut env, &job_id)?;
        Ok((addr, token, pubkey, relay, job_id))
    })();
    let (addr, token, pubkey, relay, job_id) = match parts {
        Ok(p) => p,
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let share_id = token.share_id.clone();
    let grant = grant_from_parts(addr, port.max(0) as u16, token, pubkey, relay);
    let timeout = Duration::from_millis(timeout_ms.max(0) as u64);

    let json = match with_onboarding_runtime(sync::import_cancel(&grant, &job_id, timeout)) {
        Ok(Ok(())) => {
            journal_log("INFO", "files", &format!("импорт отменён, шара {share_id}, джоба {job_id}"));
            serde_json::json!({ "ok": true }).to_string()
        }
        Ok(Err(e)) | Err(e) => json_error(&e),
    };
    jstring_into_raw(&mut env, json)
}

/// Move a share's downloaded files from `src_dir` to `dst_dir` after a storage-
/// directory change (XR-043), without re-downloading. Holds the single-transfer
/// lock so it can't race the mirror engine (`"busy"` if one is running) and feeds
/// the same progress controller the UI polls. Synchronous (a same-volume move is
/// renames). Returns the report `{"moved":N,"bytes":N,"conflicts":[..],
/// "failed":[[path,reason]..],"cancelled":bool}` or `{"error":".."}`.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeMigrateShareDir(
    mut env: JNIEnv,
    _class: JClass,
    src_dir: JString,
    dst_dir: JString,
) -> jstring {
    let src = match read_jstring(&mut env, &src_dir) {
        Ok(s) => PathBuf::from(s),
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let dst = match read_jstring(&mut env, &dst_dir) {
        Ok(s) => PathBuf::from(s),
        Err(e) => return jstring_into_raw(&mut env, json_error(&e)),
    };
    let (files, bytes) = sync::dir_totals(&src);
    let json = match sync::TransferGuard::acquire("", files, bytes) {
        None => json_error("busy"),
        Some(_guard) => match sync::migrate_dir(&src, &dst) {
            Ok(report) => {
                journal_log("INFO", "files", &format!(
                    "перенос хранилища шары: файлов {}, байт {}{}",
                    report.moved, report.bytes,
                    if report.cancelled { ", отменён пользователем" } else { "" },
                ));
                if let Some((path, reason)) = report.failed.first() {
                    journal_log("ERROR", "files", &format!(
                        "перенос хранилища: ошибок {}, первая: {}: {}",
                        report.failed.len(), path, reason,
                    ));
                }
                serde_json::to_string(&report)
                    .unwrap_or_else(|e| json_error(&format!("serialize: {e}")))
            }
            Err(e) => {
                journal_log("ERROR", "files", &format!("перенос хранилища шары: {}", e));
                json_error(&e)
            }
        },
    };
    jstring_into_raw(&mut env, json)
}

/// Poll the running transfer's progress as JSON (`{active,cancelled,file,
/// files_done,files_total,bytes_done,bytes_total}`); `active:false` when idle.
/// The UI polls this for a progress bar and computes speed from the byte delta.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeTransferProgress(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    let json = serde_json::to_string(&sync::transfer_snapshot())
        .unwrap_or_else(|e| json_error(&format!("serialize: {e}")));
    jstring_into_raw(&mut env, json)
}

/// Request cancellation of the running sync/download. It aborts at the next
/// chunk and discards any half-written file.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeCancelTransfer(
    _env: JNIEnv,
    _class: JClass,
) {
    sync::transfer_cancel();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_str_simple_value() {
        let json = r#"{"foo":"bar","baz":"qux"}"#;
        assert_eq!(json_get_str(json, "foo").unwrap(), "bar");
        assert_eq!(json_get_str(json, "baz").unwrap(), "qux");
    }

    #[test]
    fn get_str_escaped_quote_inside_value() {
        // Regression: the previous naive parser truncated here at the
        // first `\"`, returning only `he said `. Now the escape is honored.
        let json = r#"{"msg":"he said \"hi\" and left","other":"x"}"#;
        assert_eq!(json_get_str(json, "msg").unwrap(), r#"he said "hi" and left"#);
        assert_eq!(json_get_str(json, "other").unwrap(), "x");
    }

    #[test]
    fn get_str_routing_toml_with_quoted_rules() {
        // The actual case that broke user routing — routing_toml contains
        // many escaped quotes. Without the fix, this returned only
        // `default_action = \`, `toml::from_str` failed, and the engine
        // fell back to proxy-everything default.
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
        assert_eq!(result, routing_toml, "routing_toml must round-trip through escape/unescape");

        // Sanity: other fields still parse after the multi-line value.
        assert_eq!(json_get_str(&json, "server_address").unwrap(), "1.2.3.4");
        assert_eq!(json_get_str(&json, "on_server_down").unwrap(), "direct");
    }

    #[test]
    fn get_str_newline_escape() {
        let json = r#"{"multi":"line1\nline2\nline3"}"#;
        assert_eq!(json_get_str(json, "multi").unwrap(), "line1\nline2\nline3");
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
    fn selection_absent_is_whole_share() {
        // No selection at all mirrors everything (plan_sync). The three ways the
        // Kotlin side can spell "no selection": empty string, whitespace, null.
        assert_eq!(parse_selection("").unwrap(), None);
        assert_eq!(parse_selection("  ").unwrap(), None);
        assert_eq!(parse_selection("null").unwrap(), None);
    }

    #[test]
    fn selection_empty_array_is_delete_only() {
        // XR-135 regression: "[]" is a present-but-empty selection, not the whole
        // share. The old plumbing mapped it to None (whole share) and locked
        // deletion of the last unticked file; it must now be Some(empty), which
        // the engine turns into a pure delete plan.
        assert_eq!(parse_selection("[]").unwrap(), Some(HashSet::new()));
        assert_eq!(parse_selection(" [] ").unwrap(), Some(HashSet::new()));
    }

    #[test]
    fn selection_subset_round_trips() {
        let sel = parse_selection(r#"["a.txt","docs"]"#).unwrap().unwrap();
        assert_eq!(sel, HashSet::from(["a.txt".to_string(), "docs".to_string()]));
    }

    #[test]
    fn selection_malformed_is_error() {
        // A broken array is neither silently whole-share nor silently delete-all.
        assert!(parse_selection("{").is_err());
        assert!(parse_selection(r#"{"x":1}"#).is_err());
        assert!(parse_selection("[1,2]").is_err());
    }

    #[test]
    fn parse_dns_resolvers_basic() {
        let json = r#"{"dns_resolvers":["1.1.1.1","8.8.8.8:53","77.88.8.8"]}"#;
        assert_eq!(parse_dns_resolvers(json), vec!["1.1.1.1", "8.8.8.8:53", "77.88.8.8"]);
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

    /// Пул серверов в JNI-конфиге (LLD-10): массив объектов, порядок в массиве
    /// и есть приоритет.
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
