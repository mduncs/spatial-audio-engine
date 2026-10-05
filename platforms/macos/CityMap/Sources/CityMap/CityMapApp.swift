import AppKit
import CityMapKit
import SwiftUI

// City Map: live Apple Maps beside the Workbench. Connects to the Workbench's
// loopback link (`FIGHTBOX_MAP_LINK`) and keeps retrying until it is up.
//
// Usage: CityMap [--link PORT]

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationWillFinishLaunching(_ notification: Notification) {
        // A bare SwiftPM executable starts as a background process.
        NSApp.setActivationPolicy(.regular)
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
}

@main
struct CityMapApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var store: MapStore

    init() {
        let arguments = CommandLine.arguments
        let port = arguments.firstIndex(of: "--link").flatMap { index in
            arguments.indices.contains(index + 1) ? UInt16(arguments[index + 1]) : nil
        } ?? 47811
        let link = LinkClient(port: port)
        _store = StateObject(wrappedValue: MapStore(link: link))
        link.start()
    }

    var body: some Scene {
        WindowGroup("City Map") {
            CityMapScreen(store: store) {
                MapHost(store: store)
            }
            .frame(minWidth: 1000, minHeight: 680)
        }
        .defaultSize(width: 1440, height: 900)
    }
}
