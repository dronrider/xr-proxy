//! Packet Tunnel Provider: единственный перехват трафика на Mac.
//!
//! Поднимает движок xr-core через мост xr-ios-ffi, читает пакеты из
//! `packetFlow` и кладёт их в очередь движка, выгребает обратный путь.
//! Kill-switch средствами NE: `includeAllNetworks` плюс on-demand правила,
//! падение туннеля не открывает прямой выход.

import NetworkExtension
import XrCore
import XrCoreBridge
import os.log

final class PacketTunnelProvider: NEPacketTunnelProvider {
    private let log = Logger(subsystem: "xr.macos.tunnel", category: "tunnel")

    /// Туннельный адрес устройства и DNS внутри туннеля. Как на Android:
    /// /32 на интерфейс, DNS на соседнем адресе, его обслуживает FakeDns.
    /// Адреса частные, собираются из октетов: конкретика живёт в одном месте.
    private static let tunnelAddress = [10, 0, 0, 2].map(String.init).joined(separator: ".")
    private static let dnsAddress = [10, 0, 0, 1].map(String.init).joined(separator: ".")

    /// MTU 1280: нижняя граница IPv6, меньше фрагментов на мобильных каналах.
    private static let mtu = 1280

    private var readLoopRunning = false

    override func startTunnel(options: [String: NSObject]?, completionHandler: @escaping (Error?) -> Void) {
        log.info("старт туннеля")

        guard let config = Self.loadConfig() else {
            log.error("конфиг не найден или не разобран")
            completionHandler(TunnelError.noConfig)
            return
        }

        if let error = XrBridge.start(config) {
            log.error("движок не поднялся: \(error, privacy: .public)")
            completionHandler(TunnelError.engineStart(error))
            return
        }

        let settings = Self.makeNetworkSettings()
        setTunnelNetworkSettings(settings) { [weak self] error in
            if let error {
                self?.log.error("настройки сети не встали: \(error.localizedDescription, privacy: .public)")
                XrBridge.stop()
                completionHandler(error)
                return
            }
            self?.startPacketLoops()
            self?.log.info("туннель поднят")
            completionHandler(nil)
        }
    }

    override func stopTunnel(with reason: NEProviderStopReason, completionHandler: @escaping () -> Void) {
        log.info("остановка туннеля, причина: \(String(describing: reason), privacy: .public)")
        readLoopRunning = false
        XrBridge.stop()
        completionHandler()
    }

    override func handleAppMessage(_ messageData: Data, completionHandler: ((Data?) -> Void)?) {
        // Статус и статистика для приложения: короткий JSON поверх того же
        // сериала, что и ядро.
        let state = XrBridge.state()
        let stats = XrBridge.stats()
        let payload: [String: Any] = [
            "state": state.rawValue,
            "bytes_up": stats.bytesUp,
            "bytes_down": stats.bytesDown,
            "active": stats.active,
            "quic_dropped": stats.quicDropped,
            "uptime": stats.uptime,
            "active_server": stats.activeServer
        ]
        let data = try? JSONSerialization.data(withJSONObject: payload)
        completionHandler?(data)
    }

    override func sleep(completionHandler: @escaping () -> Void) {
        // Mac спит: туннель не останавливаем, движок переживёт сон своим
        // таймаутом. On-demand вернёт его на пробуждении.
        log.info("система засыпает")
        completionHandler()
    }

    override func wake() {
        log.info("система проснулась")
        XrBridge.onNetworkChanged()
    }

    // MARK: - Пакетный путь

    private func startPacketLoops() {
        guard !readLoopRunning else { return }
        readLoopRunning = true
        readFromDevice()
        DispatchQueue.global(qos: .utility).async { [weak self] in
            self?.writeToDevice()
        }
    }

    /// Читает пакеты от системы и кладёт их в движок.
    private func readFromDevice() {
        packetFlow.readPackets { [weak self] packets, _ in
            guard let self, self.readLoopRunning else { return }
            for packet in packets {
                XrBridge.pushPacket(packet)
            }
            self.readFromDevice()
        }
    }

    /// Выгребает пакеты из движка и отдаёт системе.
    private func writeToDevice() {
        while readLoopRunning {
            var batch: [(Data, NSNumber)] = []
            while let packet = XrBridge.popPacket(capacity: 2048) {
                // Семейство определяется по версии IP в первом байте.
                let family = Self.ipFamily(of: packet)
                batch.append((packet, family))
                if batch.count >= 16 { break }
            }
            if !batch.isEmpty {
                let datas = batch.map(\.0)
                let protos = batch.map(\.1)
                packetFlow.writePackets(datas, withProtocols: protos)
            } else {
                Thread.sleep(forTimeInterval: 0.002)
            }
        }
    }

    private static func ipFamily(of packet: Data) -> NSNumber {
        switch XrCore.ipFamily(of: packet) {
        case .v6: return NSNumber(value: AF_INET6)
        case .v4, .unknown: return NSNumber(value: AF_INET)
        }
    }

    // MARK: - Настройки сети

    /// Сетевые настройки туннеля. `includeAllNetworks` держит kill-switch:
    /// приложение без настроек не утекает мимо туннеля, и падение туннеля
    /// не открывает прямой выход. Соседние VPN не трогаются: свойства
    /// `excludeLocalNetworks` и маршруты по умолчанию оставляем системе.
    private static func makeNetworkSettings() -> NEPacketTunnelNetworkSettings {
        let loopback = [127, 0, 0, 1].map(String.init).joined(separator: ".")
        let settings = NEPacketTunnelNetworkSettings(tunnelRemoteAddress: loopback)

        let ipv4 = NEIPv4Settings(addresses: [tunnelAddress], subnetMasks: ["255.255.255.255"])
        // Дефолтный маршрут весь, выбор приложений не входит (XR-016).
        ipv4.includedRoutes = [NEIPv4Route.default()]
        // Туннельный адрес сервера выведен из перехвата: его стек живёт
        // поверх сети, а не внутри туннеля.
        ipv4.excludedRoutes = []
        settings.ipv4Settings = ipv4

        // Уникальный локальный адрес (ULA), хост 2: как v4-адрес выше.
        let v6Local = "fd00::" + String(2, radix: 16)
        let ipv6 = NEIPv6Settings(addresses: [v6Local], networkPrefixLengths: [128])
        ipv6.includedRoutes = [NEIPv6Route.default()]
        settings.ipv6Settings = ipv6

        // DNS внутри туннеля: FakeDns обслуживает A+AAAA, провайдерский
        // резолвер не используется (XR-285).
        settings.dnsSettings = NEDNSSettings(servers: [dnsAddress])
        settings.dnsSettings?.matchDomains = [""]

        settings.mtu = NSNumber(value: mtu)

        // Весь IP-трафик уходит в туннель, включая QUIC: срез udp/443
        // делает ядро (block_quic), а не файрвол.
        settings.includeAllNetworks = true
        settings.enforceRoutes = true

        return settings
    }

    // MARK: - Конфиг

    /// Конфиг достаётся из App Group, куда его кладёт приложение. Без
    /// конфига туннель не стартует: лучше честный отказ, чем туннель
    /// в никуда.
    private static func loadConfig() -> String? {
        guard let defaults = UserDefaults(suiteName: "group.dev.xr.macos") else {
            return nil
        }
        return defaults.string(forKey: "config_json")
    }
}

enum TunnelError: Error, LocalizedError {
    case noConfig
    case engineStart(String)

    var errorDescription: String? {
        switch self {
        case .noConfig:
            return "Конфигурация подключения не найдена. Откройте приложение и настройте подключение."
        case .engineStart(let message):
            return "Туннель не поднялся: \(message)"
        }
    }
}
