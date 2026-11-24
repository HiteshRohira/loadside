import SwiftUI

struct ActivityView: View {
    @Environment(Store.self) private var store
    @State private var tab = 0
    @State private var showDebug = false

    var body: some View {
        VStack(spacing: 0) {
            Picker("", selection: $tab) {
                Text("Jobs").tag(0)
                Text("Logs").tag(1)
            }
            .pickerStyle(.segmented).labelsHidden().frame(width: 200).padding(10)
            Divider()
            if tab == 0 { jobs } else { logs }
        }
        .navigationTitle("Activity")
        .toolbar {
            if tab == 0 {
                Button("Clear Finished") { Task { await store.clearJobs() } }
            } else {
                Toggle("Debug", isOn: $showDebug).toggleStyle(.checkbox)
                Button("Open Log File") {
                    let url = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Logs/Loadside/loadsided.log")
                    NSWorkspace.shared.open(url)
                }
            }
        }
    }

    private var jobs: some View {
        List {
            if store.jobs.isEmpty { Text("Nothing yet.").foregroundStyle(.secondary) }
            ForEach(store.jobs.reversed()) { j in
                VStack(alignment: .leading, spacing: 6) {
                    HStack {
                        statusIcon(j)
                        Text(j.title).font(.callout.weight(.medium))
                        if j.background { Tag(text: "automatic") }
                        Spacer()
                        Text(Fmt.rel(j.finishedAt ?? j.createdAt)).font(.caption).foregroundStyle(.secondary)
                        if j.status == .queued {
                            Button("Cancel") { Task { await store.cancel(j) } }.controlSize(.small)
                        }
                    }
                    if j.isActive { JobProgressView(job: j) }
                    if let e = j.error { FailureView(failure: e) }
                    ForEach(j.warnings, id: \.self) { Label($0, systemImage: "exclamationmark.circle").font(.caption).foregroundStyle(.orange) }
                }
                .padding(.vertical, 4)
            }
        }
        .listStyle(.inset)
    }

    @ViewBuilder private func statusIcon(_ j: JobInfo) -> some View {
        switch j.status {
        case .queued: Image(systemName: "clock").foregroundStyle(.secondary)
        case .running: ProgressView().controlSize(.mini)
        case .succeeded: Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
        case .failed: Image(systemName: "xmark.octagon.fill").foregroundStyle(.red)
        case .cancelled: Image(systemName: "minus.circle").foregroundStyle(.secondary)
        }
    }

    private var logs: some View {
        let lines = store.logs.filter { showDebug || $0.level != "debug" && $0.level != "trace" }
        return ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 1) {
                    ForEach(Array(lines.enumerated()), id: \.offset) { i, l in
                        HStack(alignment: .firstTextBaseline, spacing: 8) {
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
                .padding(10)
            }
            .onChange(of: lines.count) { _, n in proxy.scrollTo(n - 1, anchor: .bottom) }
            .onAppear { proxy.scrollTo(lines.count - 1, anchor: .bottom) }
        }
    }
}
