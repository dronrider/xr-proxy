//! C FFI мост: Network Extension (macOS, iOS) и движок xr-core.
//!
//! Крейт собирается `staticlib` и линкуется в процесс расширения. Граница
//! строковая: JSON на контрол-плейне и сырые байты на пакетном пути. Каждая
//! входная точка объявлена макросом `ffi_entry!` (см. `guard.rs`) и ловит
//! панику ядра до возврата в Swift.
//!
//! Владение строками: функция возвращает `*mut c_char` (UTF-8), владелец
//! Swift, освобождается парной `xr_free_string`. Пакетный путь едет явным
//! буфером без аллокации на вызов.

use std::os::raw::c_char;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

use xr_core::client_config::{self, parse_config};
use xr_core::engine::VpnEngine;
use xr_core::ip_stack::PacketQueue;
use xr_core::session::ProtectSocketFn;

#[macro_use]
mod guard;

/// Движок живёт в статике процесса расширения. Один туннель на процесс: так
/// устроено и `NEPacketTunnelProvider`, переподнимают его остановкой.
static ENGINE: OnceLock<Mutex<Option<EngineHandle>>> = OnceLock::new();

/// Признак «аплинка нет» от ОС. Та же семантика, что в JNI-мосте (XR-296):
/// живёт внутри сессии движка, снимается возвратом сети, сменой сети и
/// остановкой.
static UPLINK_DOWN: AtomicBool = AtomicBool::new(false);

struct EngineHandle {
    engine: VpnEngine,
    queue: PacketQueue,
    #[allow(dead_code)]
    runtime: tokio::runtime::Runtime,
}

fn get_engine() -> &'static Mutex<Option<EngineHandle>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

/// Взять лок, переживший панику под ним. Граница FFI ловит панику, и
/// отравленный лок без этой помощи закрыл бы движок навсегда.
fn lock_surviving_poison<'a, T>(mutex: &'a Mutex<T>, what: &str) -> std::sync::MutexGuard<'a, T> {
    mutex.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("лок {} отравлен паникой, беру через into_inner", what);
        poisoned.into_inner()
    })
}

/// Сокеты расширения по умолчанию идут мимо туннеля: `NEPacketTunnelProvider`
/// не создаёт петлю сам, в отличие от Android VpnService. Протект поэтому
/// тривиальный, андроид-специфика остаётся в своём мосте (LLD-39 п. 3.4).
fn make_protect_fn() -> ProtectSocketFn {
    Arc::new(|_fd: i32| true)
}

fn cstring_into_raw(s: String) -> *mut c_char {
    std::ffi::CString::new(s)
        .map(|c| c.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

fn cstr_to_string(ptr: *const c_char) -> Result<String, String> {
    if ptr.is_null() {
        return Err("нулевой указатель".into());
    }
    let bytes = unsafe { std::ffi::CStr::from_ptr(ptr) }.to_bytes();
    std::str::from_utf8(bytes)
        .map(|s| s.to_string())
        .map_err(|e| format!("не UTF-8: {e}"))
}

/// Конфиг NE всегда уходит в ядро с профилем тюнинга LLD-39 и срезом QUIC:
/// это требования mac-пути, и забыть их на хосте не должно выходить боком.
/// Ключи уже стоящие в JSON не перетираются: хост мог уточнить отдельное
/// значение поверх профиля.
fn ensure_ne_flags(config_json: &str) -> Result<String, String> {
    let mut value: serde_json::Value =
        serde_json::from_str(config_json).map_err(|e| format!("конфиг не JSON: {e}"))?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| "конфиг не объект".to_string())?;
    obj.entry("ne_profile".to_string())
        .or_insert(serde_json::Value::Bool(true));
    obj.entry("block_quic".to_string())
        .or_insert(serde_json::Value::Bool(true));
    Ok(value.to_string())
}

fn json_error(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

// Тексты запасных ответов на панику в границе FFI. Хост получает их вместо
// значения, которое не успело родиться.
const FFI_PANIC_TEXT: &str = "внутренняя ошибка (паника нативного кода)";
const FFI_PANIC_START: &str = "Внутренняя ошибка при запуске движка, попробуйте ещё раз";
const FFI_PANIC_STATE: &str = "Error: паника нативного кода";

ffi_entry!(cstring_into_raw(json_error(FFI_PANIC_TEXT)); fn xr_build_config(
    setup_json: *const c_char,
) -> *mut c_char {
    let setup = match cstr_to_string(setup_json) {
        Ok(s) => s,
        Err(e) => return cstring_into_raw(json_error(&e)),
    };
    let profile: client_config::ClientProfile = match serde_json::from_str(&setup) {
        Ok(p) => p,
        Err(e) => return cstring_into_raw(json_error(&format!("профиль не разобран: {e}"))),
    };
    match client_config::build_config_json(&profile) {
        Ok(config) => match ensure_ne_flags(&config) {
            Ok(with_flags) => cstring_into_raw(with_flags),
            Err(e) => cstring_into_raw(json_error(&e)),
        },
        Err(e) => cstring_into_raw(json_error(&format!("конфиг не собран: {e}"))),
    }
});

ffi_entry!(cstring_into_raw(json_error(FFI_PANIC_TEXT)); fn xr_validate_config(
    config_json: *const c_char,
) -> *mut c_char {
    let raw = match cstr_to_string(config_json) {
        Ok(s) => s,
        Err(e) => return cstring_into_raw(json_error(&e)),
    };
    let config = match ensure_ne_flags(&raw) {
        Ok(c) => c,
        Err(e) => return cstring_into_raw(json_error(&e)),
    };
    match parse_config(&config) {
        Ok(_) => cstring_into_raw("ok".into()),
        Err(e) => cstring_into_raw(json_error(&e)),
    }
});

ffi_entry!(cstring_into_raw(FFI_PANIC_START.to_string()); fn xr_start(
    config_json: *const c_char,
) -> *mut c_char {
    let raw = match cstr_to_string(config_json) {
        Ok(s) => s,
        Err(e) => return cstring_into_raw(format!("Ошибка конфигурации: {e}")),
    };
    let config_str = match ensure_ne_flags(&raw) {
        Ok(c) => c,
        Err(e) => return cstring_into_raw(format!("Ошибка конфигурации: {e}")),
    };
    let config = match parse_config(&config_str) {
        Ok(c) => c,
        Err(e) => return cstring_into_raw(format!("Ошибка конфигурации: {e}")),
    };

    let queue_capacity = config.tuning.queue_capacity;
    let mut engine = VpnEngine::new(config);
    let queue = PacketQueue::with_capacity(queue_capacity);
    let protect = make_protect_fn();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return cstring_into_raw(format!("Не удалось запустить runtime: {e}")),
    };

    let _guard = runtime.enter();
    match engine.start(queue.clone(), protect) {
        Ok(()) => {
            let mut lock = lock_surviving_poison(get_engine(), "engine");
            *lock = Some(EngineHandle { engine, queue, runtime });
            if UPLINK_DOWN.load(Ordering::Relaxed) {
                if let Some(ref handle) = *lock {
                    handle.engine.on_uplink_lost();
                }
            }
            tracing::info!("VPN engine started (NE profile)");
            std::ptr::null_mut()
        }
        Err(e) => {
            let user_msg = match e.kind() {
                std::io::ErrorKind::AlreadyExists => "Туннель уже запущен".to_string(),
                _ => format!("Ошибка запуска: {e}"),
            };
            cstring_into_raw(user_msg)
        }
    }
});

ffi_entry!(fn xr_stop() {
    let mut lock = lock_surviving_poison(get_engine(), "engine");
    if let Some(ref mut handle) = *lock {
        handle.engine.stop();
    }
    *lock = None;
    UPLINK_DOWN.store(false, Ordering::Relaxed);
    tracing::info!("VPN engine stopped");
});

ffi_entry!(cstring_into_raw(FFI_PANIC_STATE.to_string()); fn xr_get_state() -> *mut c_char {
    let lock = lock_surviving_poison(get_engine(), "engine");
    match *lock {
        Some(ref handle) => cstring_into_raw(handle.engine.state().get().to_string()),
        None => cstring_into_raw("Disconnected".into()),
    }
});

ffi_entry!(cstring_into_raw(json_error(FFI_PANIC_TEXT)); fn xr_get_stats() -> *mut c_char {
    let lock = lock_surviving_poison(get_engine(), "engine");
    let json = match *lock {
        Some(ref handle) => {
            let s = handle.engine.stats().snapshot();
            let debug_escaped = s.debug_msg.replace('\\', "\\\\").replace('"', "\\\"");
            let (srv_name, srv_backup) = handle.engine.active_server_info().unwrap_or_default();
            let srv_escaped = srv_name.replace('\\', "\\\\").replace('"', "\\\"");
            format!(
                "{{\"bytes_up\":{},\"bytes_down\":{},\"active\":{},\"total\":{},\"uptime\":{},\"dns\":{},\"syns\":{},\"smol_recv\":{},\"smol_send\":{},\"relay_warn\":{},\"relay_err\":{},\"quic_dropped\":{},\"debug\":\"{}\",\"active_server\":\"{}\",\"backup_active\":{}}}",
                s.bytes_up, s.bytes_down, s.active_connections, s.total_connections,
                s.uptime_seconds, s.dns_queries, s.tcp_syns, s.smol_recv, s.smol_send,
                s.relay_warns, s.relay_errors, s.quic_dropped, debug_escaped, srv_escaped, srv_backup,
            )
        }
        None => "{\"bytes_up\":0,\"bytes_down\":0,\"active\":0,\"total\":0,\"uptime\":0,\"dns\":0,\"syns\":0,\"smol_recv\":0,\"smol_send\":0,\"relay_warn\":0,\"relay_err\":0,\"quic_dropped\":0,\"debug\":\"\",\"active_server\":\"\",\"backup_active\":false}".into(),
    };
    cstring_into_raw(json)
});

ffi_entry!(fn xr_push_packet(data: *const u8, len: usize) {
    if data.is_null() || len == 0 {
        return;
    }
    let bytes = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    let lock = lock_surviving_poison(get_engine(), "engine");
    if let Some(ref handle) = *lock {
        handle.queue.push_inbound(bytes);
    }
});

ffi_entry!(-1isize; fn xr_pop_packet(buf: *mut u8, cap: usize) -> isize {
    if buf.is_null() || cap == 0 {
        return -1;
    }
    let lock = lock_surviving_poison(get_engine(), "engine");
    let Some(ref handle) = *lock else {
        return -1;
    };
    let Some(packet) = handle.queue.pop_outbound() else {
        return -1;
    };
    if packet.len() > cap {
        // Буфер хоста мельче пакета: ронять пакет молча нельзя, но и писать
        // за границу тем более. Возврат -1 с пустым чтением, хост дорастит
        // буфер и попробует снова; пакет при этом теряется, что честнее
        // переполнения. NE читает пачками в буфер 2048 и выше, на практике
        // не встречается.
        tracing::warn!(
            "пакет {} байт не влез в буфер хоста {} байт, роняю",
            packet.len(),
            cap
        );
        return -1;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(packet.as_ptr(), buf, packet.len());
    }
    packet.len() as isize
});

ffi_entry!(fn xr_on_network_changed() {
    let lock = lock_surviving_poison(get_engine(), "engine");
    if let Some(ref handle) = *lock {
        handle.engine.on_network_changed();
    }
});

ffi_entry!(fn xr_on_uplink_lost() {
    UPLINK_DOWN.store(true, Ordering::Relaxed);
    let lock = lock_surviving_poison(get_engine(), "engine");
    if let Some(ref handle) = *lock {
        handle.engine.on_uplink_lost();
    }
});

ffi_entry!(fn xr_on_uplink_restored() {
    UPLINK_DOWN.store(false, Ordering::Relaxed);
    let lock = lock_surviving_poison(get_engine(), "engine");
    if let Some(ref handle) = *lock {
        handle.engine.on_uplink_restored();
    }
});

// Освободить строку, возвращённую любой функцией моста. Пара обязательна:
// строка родилась в аллокаторе Rust и обязана там же умереть.
ffi_entry!(fn xr_free_string(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    drop(unsafe { std::ffi::CString::from_raw(s) });
});

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> std::ffi::CString {
        std::ffi::CString::new(s).unwrap()
    }

    unsafe fn take(ptr: *mut c_char) -> String {
        assert!(!ptr.is_null());
        let s = cstr_to_string(ptr).unwrap();
        xr_free_string(ptr);
        s
    }

    #[test]
    fn build_config_roundtrip() {
        let setup = r#"{
            "server_address": "vps.example",
            "server_port": 443,
            "obfuscation_key": "a2V5MTIzNDU2Nzg5MA==",
            "modifier": "positional_xor_rotate"
        }"#;
        let ptr = xr_build_config(c(setup).as_ptr());
        let json = unsafe { take(ptr) };
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["server_address"], "vps.example");
        assert_eq!(value["ne_profile"], true);
        assert_eq!(value["block_quic"], true);
    }

    #[test]
    fn validate_rejects_garbage() {
        let ptr = xr_validate_config(c("не JSON").as_ptr());
        let json = unsafe { take(ptr) };
        assert!(json.contains("error"), "{json}");
    }

    #[test]
    fn validate_accepts_built_config() {
        let setup = r#"{
            "server_address": "vps.example",
            "server_port": 443,
            "obfuscation_key": "a2V5MTIzNDU2Nzg5MA=="
        }"#;
        let built = unsafe { take(xr_build_config(c(setup).as_ptr())) };
        let ptr = xr_validate_config(c(&built).as_ptr());
        let reply = unsafe { take(ptr) };
        assert_eq!(reply, "ok");
    }

    #[test]
    fn state_without_engine_is_disconnected() {
        // Двигатель не поднят: функция обязана ответить, а не паниковать.
        let ptr = xr_get_state();
        let state = unsafe { take(ptr) };
        assert_eq!(state, "Disconnected");
    }

    #[test]
    fn stats_without_engine_carry_server_keys() {
        // Запасной JSON обязан нести те же ключи, что и ответ при живом
        // движке, иначе разбор статистики у хоста двоится.
        let ptr = xr_get_stats();
        let json = unsafe { take(ptr) };
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["active_server"], "");
        assert_eq!(value["backup_active"], false);
    }

    #[test]
    fn headers_of_bridge_are_identical() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let crate_header = std::fs::read_to_string(root.join("include/xr_ios_ffi.h")).unwrap();
        let swift_header = std::fs::read_to_string(
            root.join("../macos/XrCore/Sources/CXrFfi/include/xr_ios_ffi.h"),
        )
        .unwrap();
        assert_eq!(
            crate_header, swift_header,
            "копии xr_ios_ffi.h разошлись, синхронизируй include/ и CXrFfi/include"
        );
    }

    #[test]
    fn free_string_accepts_null() {
        xr_free_string(std::ptr::null_mut());
    }

    #[test]
    fn push_and_pop_without_engine_are_safe() {
        let data = [1u8, 2, 3, 4];
        xr_push_packet(data.as_ptr(), data.len());
        let mut buf = [0u8; 16];
        assert_eq!(xr_pop_packet(buf.as_mut_ptr(), buf.len()), -1);
    }

    #[test]
    fn ensure_ne_flags_keeps_explicit_values() {
        let raw = r#"{"server_address":"vps.example","ne_profile":false,"block_quic":false}"#;
        let out = ensure_ne_flags(raw).unwrap();
        assert!(out.contains("\"ne_profile\":false"), "{out}");
        assert!(out.contains("\"block_quic\":false"), "{out}");
    }

    #[test]
    fn ensure_ne_flags_injects_missing() {
        let out = ensure_ne_flags(r#"{"server_address":"vps.example"}"#).unwrap();
        assert!(out.contains("\"ne_profile\":true"), "{out}");
        assert!(out.contains("\"block_quic\":true"), "{out}");
    }
}
