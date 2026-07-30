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
                .onAppear {
                    host.start()
                }
                .onChange(of: scenePhase) { phase in
                    switch phase {
                    case .active:
                        host.start()
                    case .background:
                        host.stop()
                    case .inactive:
                        break
                    @unknown default:
                        break
                    }
                }
        }
    }
}

