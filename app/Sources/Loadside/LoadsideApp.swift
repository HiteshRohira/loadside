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

    /// Started by the login item: menu bar icon only, no window or Dock tile.
    /// Cleared once the first window has been dismissed.
    @MainActor static var startHidden = CommandLine.arguments.contains(LoginItem.launchArgument) && Prefs.menuBarOn

    func applicationWillFinishLaunching(_ notification: Notification) {
        if Self.startHidden { NSApp.setActivationPolicy(.accessory) }
    }

    func applicationShouldOpenUntitledFile(_ sender: NSApplication) -> Bool { !Self.startHidden }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSWindow.allowsAutomaticWindowTabbing = false
        LoginItem.sync(enabled: Prefs.menuBarOn)
        var lastMenuBar = Prefs.menuBarOn
        NotificationCenter.default.addObserver(forName: UserDefaults.didChangeNotification, object: nil, queue: .main) { _ in
            let on = Prefs.menuBarOn
            guard on != lastMenuBar else { return }
            lastMenuBar = on
            LoginItem.sync(enabled: on)
        }
        if Self.startHidden {
            // Only the launch itself is hidden: opening Loadside later (Dock,
            // Finder, the menu bar's "Show all") must show the window.
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) { Self.startHidden = false }
        } else {
            NSApp.setActivationPolicy(.regular)
            NSApp.activate(ignoringOtherApps: true)
        }
        NotificationCenter.default.addObserver(forName: NSWindow.willCloseNotification, object: nil, queue: .main) { note in
            guard Prefs.menuBarOn, let closing = note.object as? NSWindow, closing.isMainWindowCandidate else { return }
            DispatchQueue.main.async {
                if !NSApp.windows.contains(where: { $0 !== closing && $0.isMainWindowCandidate && $0.isVisible }) {
                    NSApp.setActivationPolicy(.accessory)
                }
            }
        }
    }

    /// SwiftUI's openWindow, registered by the menu bar label (the one view
    /// that exists while no window is open).
    @MainActor static var openMainWindow: (() -> Void)?

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        Self.startHidden = false
        NSApp.setActivationPolicy(.regular)
        // AppKit's default reopen doesn't create a SwiftUI window when the app
        // was started without one (login item), so open it explicitly.
        if !NSApp.windows.contains(where: { $0.isMainWindowCandidate && $0.isVisible }), let open = Self.openMainWindow {
            open()
            NSApp.activate(ignoringOtherApps: true)
            return false
        }
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
                .frame(minWidth: 560, minHeight: 460)
                .onAppear {
                    if AppDelegate.startHidden {
                        // SwiftUI opens the first window regardless; close it unseen.
                        AppDelegate.startHidden = false
                        DispatchQueue.main.async {
                            for w in NSApp.windows where w.identifier?.rawValue.hasPrefix("main") == true { w.close() }
                        }
                    } else {
                        NSApp.setActivationPolicy(.regular)
                    }
                }
        }
        // Opened files go to the existing window instead of a new one.
        .handlesExternalEvents(matching: ["*"])
        .defaultSize(width: 820, height: 600)
        .windowToolbarStyle(.unified)
        .commands {
            CommandGroup(replacing: .newItem) {
                Button("Install App…") { store.navigate = "install" }
                    .keyboardShortcut("o")
                    .disabled(store.connection != .connected)
            }
            CommandGroup(after: .newItem) {
                Button("Refresh All Apps") { Task { await store.refreshAll() } }
                    .keyboardShortcut("r")
                    .disabled(store.apps.isEmpty)
            }
            CommandGroup(replacing: .help) {
                Button("Setup Guide") { store.reopenOnboarding() }
                Button("Setup Checklist") { store.navigate = "setup" }
            }
        }

        Window("Logs", id: "logs") {
            LogsView().environment(store)
        }
        .defaultSize(width: 760, height: 480)

        Settings {
            SettingsView().environment(store)
        }

        MenuBarExtra(isInserted: $showMenuBar) {
            MenuBarView().environment(store)
        } label: {
            MenuBarLabel().environment(store)
        }
        .menuBarExtraStyle(.window)
    }
}
