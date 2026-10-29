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

        guard let profileJSON = Self.loadProfile() else {
            log.error("профиль не найден")
            completionHandler(TunnelError.noConfig)
            return
        }
        guard let config = XrBridge.buildConfig(json: profileJSON) else {
            log.error("конфиг из профиля не собрался")
            completionHandler(TunnelError.noConfig)
            return
        }

        if let error = XrBridge.start(config) {
            log.error("движок не поднялся: \(error, privacy: .public)")
            completionHandler(TunnelError.engineStart(error))
            return
        }

        let serverAddress = XrCore.serverAddress(fromProfileJSON: profileJSON)
        let settings = Self.makeNetworkSettings(serverAddress: serverAddress)
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
        // таймаутом. Вернёт его on-demand (`isOnDemandEnabled`), настроенный
        // приложением при установке профиля.
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
    /// не открывает прямой выход. Адрес сервера исключается из перехвата:
    /// без исключения includeAllNetworks заворачивает и соединение к серверу,
    /// и переподключение остаётся без пути. Имя хоста резолвится здесь же,
    /// до установки настроек. Соседние VPN не трогаются: свойства
    /// `excludeLocalNetworks` и маршруты по умолчанию оставляем системе.
    static func makeNetworkSettings(serverAddress: String) -> NEPacketTunnelNetworkSettings {
        let remote = serverAddress.isEmpty
            ? [127, 0, 0, 1].map(String.init).joined(separator: ".")
            : serverAddress
        let settings = NEPacketTunnelNetworkSettings(tunnelRemoteAddress: remote)

        let ipv4 = NEIPv4Settings(addresses: [tunnelAddress], subnetMasks: ["255.255.255.255"])
        // Дефолтный маршрут весь, выбор приложений не входит (XR-016).
        ipv4.includedRoutes = [NEIPv4Route.default()]
        ipv4.excludedRoutes = Self.excludedServerRoutes(serverAddress: serverAddress)
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
        return settings
    }

    /// Маршруты, которые не уходят в туннель: адрес сервера как /32.
    /// Имя хоста резолвится системным резолвером до подъёма интерфейса.
    static func excludedServerRoutes(serverAddress: String) -> [NEIPv4Route] {
        var addresses: [String] = []
        if let literal = ipv4Literal(serverAddress) {
            addresses.append(literal)
        } else if !serverAddress.isEmpty {
            addresses.append(contentsOf: resolveIPv4(host: serverAddress))
        }
        return addresses.map {
            NEIPv4Route(destinationAddress: $0, subnetMask: "255.255.255.255")
        }
    }

    private static func resolveIPv4(host: String) -> [String] {
        let name = host as CFString
        let lookup = CFHostCreateWithName(nil, name).takeRetainedValue()
        guard CFHostStartInfoResolution(lookup, .addresses, nil)
        else { return [] }
        guard let data = CFHostGetAddressing(lookup, nil)?.takeUnretainedValue() as? [Data] else {
            return []
        }
        var result: [String] = []
        for raw in data {
            let sa = raw.withUnsafeBytes { $0.baseAddress?.assumingMemoryBound(to: sockaddr.self) }
            guard let sa, sa.pointee.sa_family == UInt8(AF_INET) else { continue }
            var hostname = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            if getnameinfo(sa, socklen_t(raw.count), &hostname, socklen_t(hostname.count), nil, 0, NI_NUMERICHOST) == 0 {
                result.append(String(cString: hostname))
            }
        }
        return result
    }

    // MARK: - Профиль

    /// Профиль достаётся из App Group, куда его кладёт приложение. Без
    /// профиля туннель не стартует: лучше честный отказ, чем туннель
    /// в никуда. Ключ совпадает с `configDefaultsKey`.
    private static func loadProfile() -> String? {
        guard let defaults = UserDefaults(suiteName: appGroupSuiteName) else {
            return nil
        }
        return defaults.string(forKey: configDefaultsKey)
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
