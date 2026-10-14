//! Интеграционные проверки контракта моста: копии заголовка и символы FFI.

use std::path::Path;

fn crate_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn bridge_header_copies_are_identical() {
    let crate_header = std::fs::read_to_string(crate_root().join("include/xr_ios_ffi.h")).unwrap();
    let swift_header = std::fs::read_to_string(
        crate_root().join("../macos/XrCore/Sources/CXrFfi/include/xr_ios_ffi.h"),
    )
    .unwrap();
    assert_eq!(
        crate_header, swift_header,
        "копии xr_ios_ffi.h разошлись, синхронизируй include/ и CXrFfi/include"
    );
}

#[test]
fn bridge_header_declares_entry_points() {
    let header = std::fs::read_to_string(crate_root().join("include/xr_ios_ffi.h")).unwrap();
    for name in [
        "xr_build_config",
        "xr_validate_config",
        "xr_start",
        "xr_stop",
        "xr_get_state",
        "xr_get_stats",
        "xr_push_packet",
        "xr_pop_packet",
        "xr_on_network_changed",
        "xr_on_uplink_lost",
        "xr_on_uplink_restored",
        "xr_free_string",
    ] {
        assert!(
            header.contains(name),
            "в заголовке нет точки входа {name}"
        );
    }
}
