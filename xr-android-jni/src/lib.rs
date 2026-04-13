//! JNI bridge: Kotlin ↔ xr-core VPN engine.

use jni::objects::{JClass, JString, GlobalRef};
use jni::sys::{jint, jmethodID, jstring};
use jni::{JNIEnv, JavaVM};

use std::sync::{Arc, Mutex, OnceLock};

use xr_core::engine::{VpnConfig, VpnEngine};
use xr_core::ip_stack::PacketQueue;
use xr_core::session::ProtectSocketFn;
use xr_proto::config::RoutingConfig;

/// Global engine instance.
static ENGINE: OnceLock<Mutex<Option<EngineHandle>>> = OnceLock::new();

/// Global JVM reference.
static JVM: OnceLock<JavaVM> = OnceLock::new();

/// Cached class reference and method ID — resolved on main thread, safe to use from any thread.
static PROTECT_METHOD: OnceLock<ProtectMethodCache> = OnceLock::new();

struct ProtectMethodCache {
    class: GlobalRef,
    method_id: jmethodID,
}

// Safety: jmethodID is a raw pointer to a JVM internal structure.
// Once resolved, it is immutable and valid for the lifetime of the JVM.
// GlobalRef is Send+Sync by design.
unsafe impl Send for ProtectMethodCache {}
unsafe impl Sync for ProtectMethodCache {}

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
    let on_server_down = get_str("on_server_down").unwrap_or_else(|_| "direct".into());

    let routing = if let Ok(toml_str) = get_str("routing_toml") {
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

    Ok(VpnConfig {
        server_address, server_port, obfuscation_key, modifier, salt,
        padding_min, padding_max, routing, geoip_path: None, on_server_down,
        hub_url, hub_preset, hub_cache_dir,
    })
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

    // Cache class + method ID on the MAIN thread where ClassLoader works.
    // NativeBridge is a Kotlin `object`, so `class` parameter may be the instance,
    // not the JClass. Use FindClass explicitly to get the correct JClass.
    if PROTECT_METHOD.get().is_none() {
        match env.find_class("com/xrproxy/app/jni/NativeBridge") {
            Ok(found_class) => {
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
            Err(e) => {
                tracing::error!("Failed to find NativeBridge class: {:?}", e);
            }
        }
    }

    // Helper: return an error string to Kotlin.
    let err = |env: &mut JNIEnv, msg: &str| -> jstring {
        tracing::error!("{}", msg);
        env.new_string(msg).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
    };

    let config_str: String = match env.get_string(&config_json) {
        Ok(s) => s.into(),
        Err(_) => return err(&mut env, "Failed to read config string"),
    };

    let config = match parse_config(&config_str) {
        Ok(c) => c,
        Err(e) => return err(&mut env, &format!("Ошибка конфигурации: {}", e)),
    };

    let mut engine = VpnEngine::new(config);
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
            let errors = h.engine.stats().recent_errors();
            let errors_json: Vec<String> = errors.iter()
                .map(|e| format!("\"{}\"", e.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', " ")))
                .collect();
            format!(
                "{{\"bytes_up\":{},\"bytes_down\":{},\"active\":{},\"total\":{},\"uptime\":{},\"dns\":{},\"syns\":{},\"smol_recv\":{},\"smol_send\":{},\"relay_warn\":{},\"relay_err\":{},\"debug\":\"{}\",\"errors\":[{}]}}",
                s.bytes_up, s.bytes_down, s.active_connections, s.total_connections, s.uptime_seconds,
                s.dns_queries, s.tcp_syns, s.smol_recv, s.smol_send, s.relay_warns, s.relay_errors, debug_escaped,
                errors_json.join(",")
            )
        }
        None => "{\"bytes_up\":0,\"bytes_down\":0,\"active\":0,\"total\":0,\"uptime\":0,\"dns\":0,\"syns\":0,\"smol_recv\":0,\"smol_send\":0,\"relay_warn\":0,\"relay_err\":0,\"debug\":\"\"}".into(),
    };
    env.new_string(&json).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// Get full error log as newline-separated string.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeGetErrorLog(
    env: JNIEnv, _class: JClass,
) -> jstring {
    let lock = get_engine().lock().unwrap();
    let log = match *lock {
        Some(ref h) => h.engine.stats().recent_errors().join("\n"),
        None => String::new(),
    };
    env.new_string(&log).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// Clear error log.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeClearErrorLog(
    _env: JNIEnv, _class: JClass,
) {
    let lock = get_engine().lock().unwrap();
    if let Some(ref h) = *lock {
        h.engine.stats().clear_errors();
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
}
