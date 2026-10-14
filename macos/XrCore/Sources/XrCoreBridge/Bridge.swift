//! FFI-обёртка поверх C-моста xr-ios-ffi. Владение строками замыкается
//! здесь: Swift получает `UnsafeMutablePointer<CChar>`, сразу превращает в
//! `String` и освобождает через `xr_free_string`. Сигнатуры сверяет
//! компилятор: C-таргет `CXrFfi` несёт заголовок моста.

import Foundation
import XrCore
#if canImport(CXrFfi)
import CXrFfi
#endif

/// Обёртка над C-мостом. Каждый метод держит пару «получил строку ->
/// освободил», чтобы утечка аллокатора Rust не осталась за горизонтом.
public enum XrBridge {
    private static func take(_ ptr: UnsafeMutablePointer<CChar>?) -> String? {
        guard let ptr else { return nil }
        let s = String(cString: ptr)
        xr_free_string(ptr)
        return s
    }

    /// Собрать JSON конфига из профиля. `nil` на ошибке, причина в JSON.
    public static func buildConfig(_ profile: SetupProfile) -> String? {
        guard let setup = try? profile.json() else { return nil }
        return setup.withCString { take(xr_build_config($0)) }
    }

    /// Собрать JSON конфига из готового профиля-строки (формат
    /// `SetupProfile.json()`). `nil` на ошибке.
    public static func buildConfig(json setup: String) -> String? {
        setup.withCString { take(xr_build_config($0)) }
    }

    /// Проверить конфиг. `"ok"` или JSON `{"error":...}`.
    public static func validateConfig(_ config: String) -> String {
        config.withCString { take(xr_validate_config($0)) } ?? #""{"error":"нет ответа"}"#
    }

    /// Запустить движок. `nil` = успех, иначе текст ошибки.
    public static func start(_ config: String) -> String? {
        config.withCString { take(xr_start($0)) }
    }

    public static func stop() {
        xr_stop()
    }

    public static func state() -> TunnelState {
        let raw = take(xr_get_state()) ?? "Disconnected"
        return TunnelState(coreValue: raw)
    }

    public static func stats() -> EngineStats {
        guard let raw = take(xr_get_stats()),
              let data = raw.data(using: .utf8),
              let parsed = try? JSONDecoder().decode(EngineStats.self, from: data)
        else {
            return EngineStats()
        }
        return parsed
    }

    public static func pushPacket(_ data: Data) {
        data.withUnsafeBytes { raw in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            xr_push_packet(base, raw.count)
        }
    }

    /// Пакет из туннеля. `nil`, если пакета нет.
    public static func popPacket(capacity: Int = 2048) -> Data? {
        var buffer = [UInt8](repeating: 0, count: capacity)
        let n = buffer.withUnsafeMutableBufferPointer { ptr in
            xr_pop_packet(ptr.baseAddress, capacity)
        }
        guard n > 0 else { return nil }
        return Data(buffer.prefix(n))
    }

    public static func onNetworkChanged() {
        xr_on_network_changed()
    }

    public static func onUplinkLost() {
        xr_on_uplink_lost()
    }

    public static func onUplinkRestored() {
        xr_on_uplink_restored()
    }
}
