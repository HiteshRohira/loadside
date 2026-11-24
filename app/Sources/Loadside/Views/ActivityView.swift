import SwiftUI

/// The service log, in its own window (Activity popover → Logs).
struct LogsView: View {
    @Environment(Store.self) private var store
    @State private var showDebug = false
    @State private var filter = ""

    var body: some View {
        let lines = store.logs.filter {
            (showDebug || ($0.level != "debug" && $0.level != "trace"))
                && (filter.isEmpty || $0.message.localizedCaseInsensitiveContains(filter))
        }
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 2) {
                    ForEach(Array(lines.enumerated()), id: \.offset) { i, l in
                        HStack(alignment: .firstTextBaseline, spacing: 10) {
                            Text(Date(timeIntervalSince1970: TimeInterval(l.ts)).formatted(date: .omitted, time: .standard))
                                .foregroundStyle(.tertiary)
                            Text(l.level.uppercased()).frame(width: 44, alignment: .leading)
                                .foregroundStyle(l.level == "error" ? .red : l.level == "warn" ? .orange : .secondary)
                            Text(l.message).textSelection(.enabled)
                        }
                        .font(.caption.monospaced())
                        .id(i)
                    }
                }
                .padding(14)
            }
            .onChange(of: lines.count) { _, n in proxy.scrollTo(n - 1, anchor: .bottom) }
            .onAppear { proxy.scrollTo(lines.count - 1, anchor: .bottom) }
        }
        .searchable(text: $filter, placement: .toolbar, prompt: "Filter")
        .toolbar {
            ToolbarItemGroup {
                Toggle("Debug", isOn: $showDebug).toggleStyle(.button)
                Button("Open Log File") {
                    let url = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Logs/Loadside/loadsided.log")
                    NSWorkspace.shared.activateFileViewerSelecting([url])
                }
            }
        }
        .frame(minWidth: 560, minHeight: 320)
    }
}
