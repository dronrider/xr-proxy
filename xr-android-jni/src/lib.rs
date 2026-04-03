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

fn parse_config(json: &str) -> Result<VpnConfig, String> {
    let get_str = |key: &str| -> Result<String, String> {
        let pattern = format!("\"{}\"", key);
        let pos = json.find(&pattern).ok_or(format!("missing {}", key))?;
        let after = &json[pos + pattern.len()..];
        let start = after.find('"').ok_or(format!("bad value for {}", key))? + 1;
        let rest = &after[start..];
        let end = rest.find('"').ok_or(format!("unterminated {}", key))?;
        Ok(rest[..end].replace("\\n", "\n").replace("\\\"", "\""))
    };

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

    Ok(VpnConfig {
        server_address, server_port, obfuscation_key, modifier, salt,
        padding_min, padding_max, routing, geoip_path: None, on_server_down,
    })
}

fn default_routing() -> RoutingConfig {
    RoutingConfig { default_action: "proxy".into(), rules: vec![] }
}

// ── JNI exports ─────────────────────────────────────────────────────

#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    _tun_fd: jint,
    config_json: JString,
) -> jint {
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

    let config_str: String = match env.get_string(&config_json) {
        Ok(s) => s.into(),
        Err(_) => return -1,
    };

    let config = match parse_config(&config_str) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Config parse error: {}", e);
            return -2;
        }
    };

    let mut engine = VpnEngine::new(config);
    let queue = PacketQueue::new();
    let protect = make_protect_fn();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("Failed to create runtime: {}", e);
            return -3;
        }
    };

    let _guard = runtime.enter();
    match engine.start(queue.clone(), protect) {
        Ok(()) => {
            let mut lock = get_engine().lock().unwrap();
            *lock = Some(EngineHandle { engine, queue, runtime });
            tracing::info!("VPN engine started");
            0
        }
        Err(e) => {
            tracing::error!("Engine start failed: {}", e);
            -4
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
                "{{\"bytes_up\":{},\"bytes_down\":{},\"active\":{},\"total\":{},\"uptime\":{},\"dns\":{},\"syns\":{},\"smol_recv\":{},\"smol_send\":{},\"relay_err\":{},\"debug\":\"{}\",\"errors\":[{}]}}",
                s.bytes_up, s.bytes_down, s.active_connections, s.total_connections, s.uptime_seconds,
                s.dns_queries, s.tcp_syns, s.smol_recv, s.smol_send, s.relay_errors, debug_escaped,
                errors_json.join(",")
            )
        }
        None => "{\"bytes_up\":0,\"bytes_down\":0,\"active\":0,\"total\":0,\"uptime\":0,\"dns\":0,\"syns\":0,\"smol_recv\":0,\"smol_send\":0,\"relay_err\":0,\"debug\":\"\"}".into(),
    };
    env.new_string(&json).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
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
