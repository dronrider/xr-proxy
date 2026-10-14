// swift-tools-version: 5.9
// Чистая логика приложения macOS: сборка профиля, разбор статистики,
// маппинг состояний. FFI-обёртка зовёт C-мост xr-ios-ffi и в юнитах не
// гоняется: без staticlib она не линкуется, а механика владения строками
// проверяется на стороне Rust. Тесты живут исполнимым файлом, а не XCTest:
// Command Line Tools без полного Xcode не дают модуль XCTest.
import PackageDescription

let package = Package(
    name: "XrCore",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "XrCore", targets: ["XrCore"]),
        .executable(name: "xrcore-tests", targets: ["XrCoreTests"])
    ],
    targets: [
        .target(name: "CXrFfi", path: "Sources/CXrFfi", publicHeadersPath: "include"),
        .target(name: "XrCore", path: "Sources/XrCore"),
        .target(
            name: "XrCoreBridge",
            dependencies: ["XrCore", "CXrFfi"],
            path: "Sources/XrCoreBridge"
        ),
        .executableTarget(name: "XrCoreTests", dependencies: ["XrCore"], path: "tests/XrCoreTests")
    ]
)
