//! Точка входа приложения macOS: запуск и остановка туннеля.

import Cocoa
import SwiftUI
import NetworkExtension

@main
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private let controller = TunnelController()
    private var window: NSWindow!

    func applicationDidFinishLaunching(_ notification: Notification) {
        setupWindow()
        controller.refresh()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    private func setupWindow() {
        let view = MainView(controller: controller)
        let hosting = NSHostingView(rootView: view)
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 420, height: 320),
            styleMask: [.titled, .closable, .miniaturizable],
            backing: .buffered,
            defer: false
        )
        window.title = "XR Proxy"
        window.contentView = hosting
        window.center()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }
}
