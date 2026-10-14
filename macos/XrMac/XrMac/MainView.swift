//! Минимальный экран: состояние туннеля и кнопка вкл/выкл.

import SwiftUI

struct MainView: View {
    @ObservedObject var controller: TunnelController

    var body: some View {
        VStack(spacing: 16) {
            Text("XR Proxy")
                .font(.title2)

            Text(controller.state)
                .font(.headline)
                .foregroundStyle(controller.isRunning ? .green : .secondary)

            if let error = controller.lastError {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .multilineTextAlignment(.center)
                    .padding(.horizontal)
            }

            Button(action: controller.toggle) {
                Text(controller.isRunning ? "Отключить" : "Подключить")
                    .frame(maxWidth: .infinity)
            }
            .controlSize(.large)
            .padding(.horizontal)

            if let stats = controller.stats {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Активных соединений: \(stats.active)")
                    Text("QUIC срезано: \(stats.quicDropped)")
                    if !stats.activeServer.isEmpty {
                        Text("Сервер: \(stats.activeServer)")
                    }
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            }

            Spacer()
        }
        .padding(.top, 24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}
