import SwiftUI

enum Prefs {
    static let menuBar = "showMenuBarIcon"
    static var menuBarOn: Bool { UserDefaults.standard.object(forKey: menuBar) as? Bool ?? true }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    // The daemon does the background work. With the menu bar icon off, the UI
    // quits when its window closes so it never sits in memory; with it on,
    // closing the window leaves just the icon (no Dock tile).
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { !Prefs.menuBarOn }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
        NotificationCenter.default.addObserver(forName: NSWindow.willCloseNotification, object: nil, queue: .main) { note in
            guard Prefs.menuBarOn, let closing = note.object as? NSWindow, closing.isMainWindowCandidate else { return }
            DispatchQueue.main.async {
                if !NSApp.windows.contains(where: { $0 !== closing && $0.isMainWindowCandidate && $0.isVisible }) {
                    NSApp.setActivationPolicy(.accessory)
                }
            }
        }
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        NSApp.setActivationPolicy(.regular)
        return true
    }
}

private extension NSWindow {
    /// Real app windows, not the menu bar panel or sheets.
    var isMainWindowCandidate: Bool { identifier?.rawValue.hasPrefix("main") == true }
}

@main
struct LoadsideApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) var delegate
    @State private var store = Store()
    @AppStorage(Prefs.menuBar) private var showMenuBar = true

    var body: some Scene {
        WindowGroup("Loadside", id: "main") {
            ContentView()
                .environment(store)
                .frame(minWidth: 860, minHeight: 560)
                .onAppear { NSApp.setActivationPolicy(.regular) }
        }
        .windowToolbarStyle(.unified)
        .commands {
            CommandGroup(replacing: .newItem) {}
        }

        MenuBarExtra(isInserted: $showMenuBar) {
            MenuBarView().environment(store)
        } label: {
            MenuBarLabel().environment(store)
        }
        .menuBarExtraStyle(.window)
    }
}
