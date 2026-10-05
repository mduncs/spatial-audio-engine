import SwiftUI

@main
@MainActor
struct FightboxApp: App {
    @Environment(\.scenePhase) private var scenePhase
    @StateObject private var host = FightboxHostModel()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(host)
                .onChange(of: scenePhase) { phase in
                    switch phase {
                    case .background:
                        host.stop()
                    case .active, .inactive:
                        break
                    @unknown default:
                        break
                    }
                }
        }
    }
}

