//! Управление NE-туннелем из приложения.

import Foundation
@preconcurrency import NetworkExtension
import XrCore
import Combine
import os.log

/// Управляет профилем туннеля и его состоянием. Провайдер туннеля живёт
/// в отдельном процессе, связь через `NETunnelProviderManager` и App Group.
@MainActor
final class TunnelController: ObservableObject {
    @Published var state: String = "Проверка..."
    @Published var isRunning = false
    @Published var lastError: String?
    @Published var stats: TunnelStats?
    @Published var isConfigured = false

    // Поля первого запуска. Уходят в SetupProfile и App Group.
    @Published var serverAddress = ""
    @Published var serverPort = "443"
    @Published var obfuscationKey = ""

    private let log = Logger(subsystem: "xr.macos.app", category: "controller")
    private var manager: NETunnelProviderManager?

    /// Читает установленный профиль и обновляет состояние экрана.
    func refresh() {
        NETunnelProviderManager.loadAllFromPreferences { [weak self] managers, error in
            DispatchQueue.main.async {
                if let error {
                    self?.lastError = error.localizedDescription
                    self?.state = "Ошибка"
                    return
                }
                self?.manager = managers?.first
                self?.isConfigured = managers?.isEmpty == false
                self?.updateState()
            }
        }
    }

    /// Первый запуск: профиль в App Group, профиль NE с on-demand, старт.
    /// Без saveToPreferences системный диалог добавления VPN не появится.
    func installAndStart() {
        let port = UInt16(serverPort) ?? 443
        let profile = SetupProfile(
            serverAddress: serverAddress.trimmingCharacters(in: .whitespacesAndNewlines),
            serverPort: port,
            obfuscationKey: obfuscationKey.trimmingCharacters(in: .whitespacesAndNewlines)
        )
        guard !profile.serverAddress.isEmpty, !profile.obfuscationKey.isEmpty else {
            lastError = "Заполни адрес сервера и ключ."
            state = "Не настроен"
            return
        }
        guard let json = try? profile.json() else {
            lastError = "Профиль не собрался в JSON."
            state = "Ошибка"
            return
        }
        guard let defaults = UserDefaults(suiteName: appGroupSuiteName) else {
            lastError = "App Group недоступна."
            state = "Ошибка"
            return
        }
        defaults.set(json, forKey: configDefaultsKey)

        let manager = NETunnelProviderManager()
        let proto = NETunnelProviderProtocol()
        proto.providerBundleIdentifier = tunnelProviderBundleId
        proto.serverAddress = profile.serverAddress
        proto.includeAllNetworks = true
        proto.enforceRoutes = true
        manager.protocolConfiguration = proto
        manager.isEnabled = true
        manager.localizedDescription = "XR Proxy"
        manager.isOnDemandEnabled = alwaysOnDemandEnabled
        if alwaysOnDemandEnabled {
            manager.onDemandRules = [NEOnDemandRuleConnect()]
        }

        manager.saveToPreferences { [weak self] error in
            DispatchQueue.main.async {
                if let error {
                    self?.lastError = error.localizedDescription
                    self?.state = "Ошибка"
                    return
                }
                self?.manager = manager
                self?.isConfigured = true
                self?.start()
            }
        }
    }

    func toggle() {
        if isRunning {
            stop()
        } else {
            start()
        }
    }

    private func start() {
        guard let manager else {
            lastError = "Профиль туннеля не установлен. Заполни подключение."
            state = "Не настроен"
            return
        }
        do {
            try manager.connection.startVPNTunnel()
            state = "Подключение..."
            isRunning = true
            schedulePoll()
        } catch {
            lastError = error.localizedDescription
            state = "Ошибка"
        }
    }

    private func stop() {
        manager?.connection.stopVPNTunnel()
        state = "Отключение..."
        isRunning = false
    }

    private func updateState() {
        guard let manager else {
            state = "Не настроен"
            isRunning = false
            return
        }
        switch manager.connection.status {
        case .connected:
            state = "Подключено"
            isRunning = true
        case .connecting:
            state = "Подключение..."
            isRunning = true
        case .disconnecting:
            state = "Отключение..."
            isRunning = false
        case .disconnected:
            state = "Отключено"
            isRunning = false
        case .reasserting:
            state = "Переподключение..."
            isRunning = true
        case .invalid:
            state = "Недоступен"
            isRunning = false
        @unknown default:
            state = "Неизвестно"
        }
    }

    /// Опрос статистики у процесса расширения раз в секунду.
    private func schedulePoll() {
        Task {
            while isRunning {
                pollStats()
                try? await Task.sleep(nanoseconds: 1_000_000_000)
                updateState()
            }
        }
    }

    private func pollStats() {
        guard let session = manager?.connection as? NETunnelProviderSession else { return }
        let message = "stats".data(using: .utf8)
        do {
            try session.sendProviderMessage(message ?? Data()) { [weak self] response in
                DispatchQueue.main.async {
                    guard let response,
                          let object = try? JSONSerialization.jsonObject(with: response) as? [String: Any]
                    else { return }
                    self?.stats = TunnelStats(
                        bytesUp: object["bytes_up"] as? UInt64 ?? 0,
                        bytesDown: object["bytes_down"] as? UInt64 ?? 0,
                        active: object["active"] as? UInt32 ?? 0,
                        quicDropped: object["quic_dropped"] as? UInt64 ?? 0,
                        uptime: object["uptime"] as? UInt64 ?? 0,
                        activeServer: object["active_server"] as? String ?? ""
                    )
                }
            }
        } catch {
            log.debug("статистика недоступна: \(error.localizedDescription, privacy: .public)")
        }
    }
}

struct TunnelStats: Equatable {
    var bytesUp: UInt64
    var bytesDown: UInt64
    var active: UInt32
    var quicDropped: UInt64
    var uptime: UInt64
    var activeServer: String
}
