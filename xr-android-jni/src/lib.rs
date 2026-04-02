//! JNI bridge: Kotlin ↔ xr-core VPN engine.
//!
//! Exposes native methods called from XrVpnService.kt:
//! - nativeStart(tunFd, configJson) → starts the VPN engine
//! - nativeStop() → stops the engine
//! - nativeGetStats() → returns JSON stats
//! - nativeGetState() → returns state string

use jni::objects::{JClass, JString};
use jni::sys::{jint, jlong, jstring};
use jni::JNIEnv;

use std::sync::{Arc, Mutex, OnceLock};

use xr_core::engine::{VpnConfig, VpnEngine};
use xr_core::ip_stack::PacketQueue;
use xr_proto::config::RoutingConfig;

/// Global engine instance.
static ENGINE: OnceLock<Mutex<Option<EngineHandle>>> = OnceLock::new();

struct EngineHandle {
    engine: VpnEngine,
    queue: PacketQueue,
    runtime: tokio::runtime::Runtime,
}

fn get_engine() -> &'static Mutex<Option<EngineHandle>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

/// Parse a JSON config string into VpnConfig.
fn parse_config(json: &str) -> Result<VpnConfig, String> {
    // Simple JSON parsing without serde_json dependency.
    // Expected format:
    // {
    //   "server_address": "1.2.3.4",
    //   "server_port": 8443,
    //   "obfuscation_key": "base64...",
    //   "modifier": "positional_xor_rotate",
    //   "salt": 3735928559,
    //   "padding_min": 16,
    //   "padding_max": 128,
    //   "routing_toml": "default_action = \"proxy\"\n...",
    //   "on_server_down": "direct"
    // }
    //
    // For MVP, we'll parse key fields manually.
    // TODO: add serde_json for proper parsing.

    let get_str = |key: &str| -> Result<String, String> {
        let pattern = format!("\"{}\"", key);
        let pos = json.find(&pattern).ok_or(format!("missing {}", key))?;
        let after = &json[pos + pattern.len()..];
        // Skip : and whitespace, find opening quote.
        let start = after
            .find('"')
            .ok_or(format!("bad value for {}", key))?
            + 1;
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

    // Parse routing from embedded TOML string or use default.
    let routing = if let Ok(toml_str) = get_str("routing_toml") {
        toml::from_str::<RoutingConfig>(&toml_str)
            .unwrap_or_else(|_| default_routing())
    } else {
        default_routing()
    };

    Ok(VpnConfig {
        server_address,
        server_port,
        obfuscation_key,
        modifier,
        salt,
        padding_min,
        padding_max,
        routing,
        geoip_path: None,
        on_server_down,
    })
}

fn default_routing() -> RoutingConfig {
    RoutingConfig {
        default_action: "proxy".into(),
        rules: vec![],
    }
}

// ── JNI exports ─────────────────────────────────────────────────────

#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeStart(
    mut env: JNIEnv,
    _class: JClass,
    tun_fd: jint,
    config_json: JString,
) -> jint {
    // Initialize logging (once).
    let _ = tracing_subscriber::fmt()
        .with_env_filter("xr_core=info,xr_proto=info")
        .with_target(false)
        .try_init();

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

    // Build tokio runtime for async operations.
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

    // Start the engine.
    let result = runtime.block_on(async {
        engine.start(queue.clone())
    });

    match result {
        Ok(()) => {
            let mut lock = get_engine().lock().unwrap();
            *lock = Some(EngineHandle { engine, queue, runtime });
            tracing::info!("VPN engine started (tun_fd={})", tun_fd);
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
    _env: JNIEnv,
    _class: JClass,
) {
    let mut lock = get_engine().lock().unwrap();
    if let Some(mut handle) = lock.take() {
        handle.engine.stop();
        tracing::info!("VPN engine stopped");
    }
}

#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeGetState(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let lock = get_engine().lock().unwrap();
    let state_str = if let Some(ref handle) = *lock {
        handle.engine.state().get().to_string()
    } else {
        "Disconnected".to_string()
    };

    env.new_string(&state_str)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativeGetStats(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    let lock = get_engine().lock().unwrap();
    let stats_json = if let Some(ref handle) = *lock {
        let s = handle.engine.stats().snapshot();
        format!(
            "{{\"bytes_up\":{},\"bytes_down\":{},\"active\":{},\"total\":{},\"uptime\":{}}}",
            s.bytes_up, s.bytes_down, s.active_connections, s.total_connections, s.uptime_seconds
        )
    } else {
        "{\"bytes_up\":0,\"bytes_down\":0,\"active\":0,\"total\":0,\"uptime\":0}".to_string()
    };

    env.new_string(&stats_json)
        .map(|s| s.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// Push a raw IP packet from TUN into the engine.
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativePushPacket(
    env: JNIEnv,
    _class: JClass,
    packet: jni::objects::JByteArray,
) {
    let lock = get_engine().lock().unwrap();
    if let Some(ref handle) = *lock {
        if let Ok(bytes) = env.convert_byte_array(&packet) {
            handle.queue.push_inbound(bytes);
        }
    }
}

/// Pop outbound packets from the engine to write to TUN.
/// Returns a byte array of the next packet, or null (represented as empty array).
#[no_mangle]
pub extern "system" fn Java_com_xrproxy_app_jni_NativeBridge_nativePopPacket(
    env: JNIEnv,
    _class: JClass,
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
