//! Профиль подключения и его JSON: чистая логика без FFI.

import Foundation

/// Профиль, как его задаёт пользователь. Уезжает в `xr_build_config`.
public struct SetupProfile: Codable, Equatable, Sendable {
    public var serverAddress: String
    public var serverPort: UInt16
    public var obfuscationKey: String
    public var modifier: String
    public var failClosed: Bool
    public var udpRelayPort: UInt16?

    public init(
        serverAddress: String,
        serverPort: UInt16 = 443,
        obfuscationKey: String,
        modifier: String = "positional_xor_rotate",
        failClosed: Bool = true,
        udpRelayPort: UInt16? = nil
    ) {
        self.serverAddress = serverAddress
        self.serverPort = serverPort
        self.obfuscationKey = obfuscationKey
        self.modifier = modifier
        self.failClosed = failClosed
        self.udpRelayPort = udpRelayPort
    }

    /// JSON в том виде, в каком его ждёт `xr_build_config`.
    public func json() throws -> String {
        var object: [String: Any] = [
            "server_address": serverAddress,
            "server_port": serverPort,
            "obfuscation_key": obfuscationKey,
            "modifier": modifier,
            "fail_closed": failClosed
        ]
        if let port = udpRelayPort, port != 0 {
            object["udp_relay_port"] = port
        }
        let data = try JSONSerialization.data(withJSONObject: object, options: [.sortedKeys])
        guard let text = String(data: data, encoding: .utf8) else {
            throw SetupError.notUTF8
        }
        return text
    }
}

public enum SetupError: Error, Equatable, Sendable {
    case notUTF8
}

/// Статистика движка, как её отдаёт `xr_get_stats`.
public struct EngineStats: Codable, Equatable, Sendable {
    public var bytesUp: UInt64
    public var bytesDown: UInt64
    public var active: UInt32
    public var total: UInt64
    public var uptime: UInt64
    public var dns: UInt64
    public var syns: UInt64
    public var smolRecv: UInt64
    public var smolSend: UInt64
    public var relayWarn: UInt64
    public var relayErr: UInt64
    public var quicDropped: UInt64
    public var debug: String
    public var activeServer: String
    public var backupActive: Bool

    enum CodingKeys: String, CodingKey {
        case bytesUp = "bytes_up"
        case bytesDown = "bytes_down"
        case active
        case total
        case uptime
        case dns
        case syns
        case smolRecv = "smol_recv"
        case smolSend = "smol_send"
        case relayWarn = "relay_warn"
        case relayErr = "relay_err"
        case quicDropped = "quic_dropped"
        case debug
        case activeServer = "active_server"
        case backupActive = "backup_active"
    }

    public init(
        bytesUp: UInt64 = 0, bytesDown: UInt64 = 0, active: UInt32 = 0, total: UInt64 = 0,
        uptime: UInt64 = 0, dns: UInt64 = 0, syns: UInt64 = 0, smolRecv: UInt64 = 0,
        smolSend: UInt64 = 0, relayWarn: UInt64 = 0, relayErr: UInt64 = 0, quicDropped: UInt64 = 0,
        debug: String = "", activeServer: String = "", backupActive: Bool = false
    ) {
        self.bytesUp = bytesUp
        self.bytesDown = bytesDown
        self.active = active
        self.total = total
        self.uptime = uptime
        self.dns = dns
        self.syns = syns
        self.smolRecv = smolRecv
        self.smolSend = smolSend
        self.relayWarn = relayWarn
        self.relayErr = relayErr
        self.quicDropped = quicDropped
        self.debug = debug
        self.activeServer = activeServer
        self.backupActive = backupActive
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        bytesUp = try container.decodeIfPresent(UInt64.self, forKey: .bytesUp) ?? 0
        bytesDown = try container.decodeIfPresent(UInt64.self, forKey: .bytesDown) ?? 0
        active = try container.decodeIfPresent(UInt32.self, forKey: .active) ?? 0
        total = try container.decodeIfPresent(UInt64.self, forKey: .total) ?? 0
        uptime = try container.decodeIfPresent(UInt64.self, forKey: .uptime) ?? 0
        dns = try container.decodeIfPresent(UInt64.self, forKey: .dns) ?? 0
        syns = try container.decodeIfPresent(UInt64.self, forKey: .syns) ?? 0
        smolRecv = try container.decodeIfPresent(UInt64.self, forKey: .smolRecv) ?? 0
        smolSend = try container.decodeIfPresent(UInt64.self, forKey: .smolSend) ?? 0
        relayWarn = try container.decodeIfPresent(UInt64.self, forKey: .relayWarn) ?? 0
        relayErr = try container.decodeIfPresent(UInt64.self, forKey: .relayErr) ?? 0
        quicDropped = try container.decodeIfPresent(UInt64.self, forKey: .quicDropped) ?? 0
        debug = try container.decodeIfPresent(String.self, forKey: .debug) ?? ""
        activeServer = try container.decodeIfPresent(String.self, forKey: .activeServer) ?? ""
        backupActive = try container.decodeIfPresent(Bool.self, forKey: .backupActive) ?? false
    }
}

/// Состояние туннеля. Строки совпадают с `VpnState` ядра.
public enum TunnelState: String, Equatable, Sendable {
    case disconnected = "Disconnected"
    case connecting = "Connecting"
    case connected = "Connected"
    case reconnecting = "Reconnecting"
    case stopping = "Stopping"
    case error = "Error"

    public init(coreValue: String) {
        if let known = TunnelState(rawValue: coreValue) {
            self = known
        } else if coreValue.hasPrefix("Error") {
            self = .error
        } else {
            self = .disconnected
        }
    }
}

/// Семейство IP-пакета: первые четыре бита версии в первом байте.
public enum IPFamily: Equatable, Sendable {
    case v4
    case v6
    case unknown
}

public func ipFamily(of packet: Data) -> IPFamily {
    guard let version = packet.first else { return .unknown }
    switch version >> 4 {
    case 4: return .v4
    case 6: return .v6
    default: return .unknown
    }
}

// MARK: - Раскладка NE

/// App Group, через которую приложение кладёт конфиг расширению.
public let appGroupSuiteName = "group.dev.xr.macos"

/// Ключ конфига в App Group.
public let configDefaultsKey = "config_json"

/// Bundle identifier расширения Packet Tunnel.
public let tunnelProviderBundleId = "dev.xr.macos.tunnel"

/// Always-on и fail-closed: туннель возвращается сам после перезагрузки
/// и обрыва сети. Разрыв с Android (доверенные сети) осознанный, XR-300.
public let alwaysOnDemandEnabled = true

/// Адрес сервера как IPv4-литерал. `nil` для имени хоста и мусора:
/// исключение из перехвата строится только по адресу, имя резолвится
/// отдельно до установки сетевых настроек.
public func ipv4Literal(_ raw: String) -> String? {
    let parts = raw.split(separator: ".", omittingEmptySubsequences: false)
    guard parts.count == 4 else { return nil }
    var octets: [String] = []
    for part in parts {
        guard !part.isEmpty, part.allSatisfy(\.isNumber), let value = UInt8(part) else {
            return nil
        }
        octets.append(String(value))
    }
    return octets.joined(separator: ".")
}

/// Адрес сервера из JSON профиля (`SetupProfile.json()`). Пустая строка,
/// если ключа нет или JSON не разобрался.
public func serverAddress(fromProfileJSON json: String) -> String {
    guard let data = json.data(using: .utf8),
          let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
          let address = object["server_address"] as? String
    else { return "" }
    return address
}
