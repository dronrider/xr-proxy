//! Юниты чистой логики XrCore: JSON профиля, разбор статистики, состояния.
//!
//! Тесты живут исполнимым файлом, а не XCTest: Command Line Tools без
//! полного Xcode не дают модуль XCTest. Прогон: `swift run xrcore-tests`,
//! код выхода 1 при провале, последняя строка успеха `юниты XrCore: ок`.

import Foundation
import XrCore

struct Check {
    static var failed = 0
    static var passed = 0

    static func ok(_ name: String) {
        passed += 1
        print("ok   \(name)")
    }

    static func fail(_ name: String, _ detail: String) {
        failed += 1
        print("FAIL \(name): \(detail)")
    }

    static func equal<T: Equatable>(_ name: String, _ got: T, _ want: T) {
        if got == want {
            ok(name)
        } else {
            fail(name, "получено \(got), ждали \(want)")
        }
    }

    static func `true`(_ name: String, _ value: Bool) {
        if value {
            ok(name)
        } else {
            fail(name, "ожидали истину")
        }
    }

    static func `false`(_ name: String, _ value: Bool) {
        if value {
            fail(name, "ожидали ложь")
        } else {
            ok(name)
        }
    }
}

func profileJsonKeys(_ json: String) -> [String: Any]? {
    guard let data = json.data(using: .utf8) else { return nil }
    return (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
}

func testProfileJson() {
    let profile = SetupProfile(
        serverAddress: "vps.example",
        serverPort: 443,
        obfuscationKey: "a2V5MTIzNDU2Nzg5MA=="
    )
    guard let json = try? profile.json() else {
        Check.fail("profile.json", "не собрался")
        return
    }
    guard let object = profileJsonKeys(json) else {
        Check.fail("profile.json", "не JSON")
        return
    }
    Check.equal("profile.server_address", object["server_address"] as? String, "vps.example")
    Check.equal("profile.server_port", object["server_port"] as? Int, 443)
    Check.equal("profile.fail_closed", object["fail_closed"] as? Bool, true)
    Check.equal("profile.нет udp_relay_port по умолчанию", object["udp_relay_port"] == nil, true)
}

func testProfileJsonRelayPort() {
    let withPort = SetupProfile(serverAddress: "vps.example", obfuscationKey: "a2V5", udpRelayPort: 12345)
    if let json = try? withPort.json(), let object = profileJsonKeys(json) {
        Check.equal("profile.udp_relay_port", object["udp_relay_port"] as? Int, 12345)
    } else {
        Check.fail("profile.udp_relay_port", "не собрался")
    }

    let zeroPort = SetupProfile(serverAddress: "vps.example", obfuscationKey: "a2V5", udpRelayPort: 0)
    if let json = try? zeroPort.json(), let object = profileJsonKeys(json) {
        Check.equal("profile.нулевой порт отбрасывается", object["udp_relay_port"] == nil, true)
    } else {
        Check.fail("profile.нулевой порт", "не собрался")
    }
}

func testStatsDecode() {
    let json = """
    {"bytes_up":10,"bytes_down":20,"active":3,"total":40,"uptime":50,"dns":60,"syns":70,\
    "smol_recv":80,"smol_send":90,"relay_warn":100,"relay_err":110,"quic_dropped":120,\
    "debug":"d","active_server":"srv","backup_active":true}
    """
    guard let stats = try? JSONDecoder().decode(EngineStats.self, from: Data(json.utf8)) else {
        Check.fail("stats.decode", "не разобрался")
        return
    }
    Check.equal("stats.bytes_up", stats.bytesUp, 10)
    Check.equal("stats.quic_dropped", stats.quicDropped, 120)
    Check.equal("stats.active_server", stats.activeServer, "srv")
    Check.true("stats.backup_active", stats.backupActive)
}

func testStatsDecodeDefaults() {
    let json = """
    {"bytes_up":0,"bytes_down":0,"active":0,"total":0,"uptime":0,"dns":0,"syns":0,\
    "smol_recv":0,"smol_send":0,"relay_warn":0,"relay_err":0,"quic_dropped":0,"debug":""}
    """
    guard let stats = try? JSONDecoder().decode(EngineStats.self, from: Data(json.utf8)) else {
        Check.fail("stats.defaults", "не разобрался")
        return
    }
    Check.equal("stats.debug пустой", stats.debug, "")
    Check.equal("stats.active_server пустой", stats.activeServer, "")
    Check.false("stats.backup_active по умолчанию", stats.backupActive)
}

func testStateMapping() {
    Check.equal("state.Connected", TunnelState(coreValue: "Connected"), .connected)
    Check.equal("state.Connecting", TunnelState(coreValue: "Connecting"), .connecting)
    Check.equal("state.Disconnected", TunnelState(coreValue: "Disconnected"), .disconnected)
    Check.equal("state.Ошибка с префиксом", TunnelState(coreValue: "Error: паника нативного кода"), .error)
    Check.equal("state.Неизвестное значение", TunnelState(coreValue: "что-то новое"), .disconnected)
}

func testIpFamily() {
    // Первый байт 0x45 (IPv4, IHL=5) и 0x60 (IPv6, версия 6).
    Check.equal("ip.v4", ipFamily(of: Data([0x45, 0x00, 0x00, 0x28])), .v4)
    Check.equal("ip.v6", ipFamily(of: Data([0x60, 0x00, 0x00, 0x00])), .v6)
    Check.equal("ip.пустой", ipFamily(of: Data()), .unknown)
    Check.equal("ip.мусор", ipFamily(of: Data([0x00, 0x01])), .unknown)
}

func testIPv4Literal() {
    let octets = [192, 0, 2, 7]
    let literal = octets.map(String.init).joined(separator: ".")
    let padded = ["192", "000", "002", "007"].joined(separator: ".")
    let garbage = literal + ".5"
    Check.equal("addr.литерал", ipv4Literal(literal), literal)
    Check.equal("addr.ведущие нули", ipv4Literal(padded), octets.map(String.init).joined(separator: "."))
    Check.equal("addr.имя", ipv4Literal("vps.example"), nil)
    Check.equal("addr.мусор", ipv4Literal(garbage), nil)
    Check.equal("addr.пустой", ipv4Literal(""), nil)
}

func testAlwaysOnDemand() {
    Check.true("policy.always-on", alwaysOnDemandEnabled)
    Check.equal("policy.bundle", tunnelProviderBundleId, "dev.xr.macos.tunnel")
    Check.equal("policy.app-group", appGroupSuiteName, "group.dev.xr.macos")
}

func testServerAddressFromProfile() {
    let profile = SetupProfile(serverAddress: "vps.example", obfuscationKey: "a2V5")
    let json = (try? profile.json()) ?? ""
    Check.equal("profile.server", serverAddress(fromProfileJSON: json), "vps.example")
    Check.equal("profile.мусор", serverAddress(fromProfileJSON: "не JSON"), "")
    Check.equal("profile.нет ключа", serverAddress(fromProfileJSON: "{}"), "")
}

testProfileJson()
testProfileJsonRelayPort()
testStatsDecode()
testStatsDecodeDefaults()
testStateMapping()
testIpFamily()
testIPv4Literal()
testAlwaysOnDemand()
testServerAddressFromProfile()

if Check.failed > 0 {
    print("юниты XrCore: \(Check.failed) провалов из \(Check.passed + Check.failed)")
    exit(1)
}
print("юниты XrCore: ок (\(Check.passed))")
