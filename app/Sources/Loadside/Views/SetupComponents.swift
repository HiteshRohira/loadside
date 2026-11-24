import SwiftUI

struct StatusIcon: View {
    let status: CheckStatus
    var size: CGFloat = 18

    var body: some View {
        Image(systemName: symbol)
            .font(.system(size: size, weight: .semibold))
            .foregroundStyle(color)
            .frame(width: size + 6)
            .accessibilityLabel(Text(label))
    }

    private var symbol: String {
        switch status {
        case .ok: "checkmark.circle.fill"
        case .action: "arrow.right.circle.fill"
        case .warning: "exclamationmark.triangle.fill"
        case .error: "xmark.octagon.fill"
        case .optional: "circle.dashed"
        case .pending: "clock"
        }
    }
    private var color: Color {
        switch status {
        case .ok: .green
        case .action: .accentColor
        case .warning: .orange
        case .error: .red
        case .optional, .pending: .secondary
        }
    }
    private var label: String {
        switch status {
        case .ok: "Done"
        case .action: "Needs action"
        case .warning: "Warning"
        case .error: "Problem"
        case .optional: "Optional"
        case .pending: "Waiting"
        }
    }
}

/// One checklist item: status, explanation and the buttons that fix it.
struct CheckRow: View {
    @Environment(Store.self) private var store
    let check: SetupCheck
    /// Handle actions the row can't do itself (e.g. "login" inside onboarding).
    var onAction: ((SetupAction) -> Bool)? = nil
    var compact = false
    @State private var busy: String?
    @State private var expanded = false

    private var needsAttention: Bool { [.action, .warning, .error].contains(check.status) }

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            StatusIcon(status: check.status).padding(.top, 1)
            VStack(alignment: .leading, spacing: 5) {
                Text(check.title).font(.callout.weight(.semibold))
                Text(check.summary).font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                if let d = check.detail, needsAttention || expanded || (!compact && check.status == .optional) {
                    Text(d).font(.caption).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        .textSelection(.enabled)
                        .padding(.top, 1)
                } else if check.detail != nil {
                    Button(expanded ? "Less" : "More info") { expanded.toggle() }
                        .buttonStyle(.link).font(.caption)
                }
                if !check.actions.isEmpty && check.status != .ok {
                    HStack(spacing: 8) {
                        ForEach(check.actions, id: \.self) { a in actionButton(a) }
                    }
                    .padding(.top, 3)
                }
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, compact ? 4 : 8)
    }

    @ViewBuilder private func actionButton(_ a: SetupAction) -> some View {
        let isBusy = busy == a.label
        let b = Button {
            Task {
                if let onAction, !onAction(a) { return }
                busy = a.label
                await store.perform(a)
                busy = nil
            }
        } label: {
            HStack(spacing: 5) {
                if isBusy { ProgressView().controlSize(.small) }
                Text(a.label)
                if a.id == "open_url" { Image(systemName: "arrow.up.right.square").font(.caption) }
            }
        }
        .disabled(busy != nil)
        .controlSize(.regular)
        if a.primary == true { b.buttonStyle(.borderedProminent) } else { b.buttonStyle(.bordered) }
    }
}

/// Numbered instruction list.
struct Steps: View {
    let items: [String]
    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(Array(items.enumerated()), id: \.offset) { i, t in
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text("\(i + 1)").font(.caption.weight(.bold)).foregroundStyle(.white)
                        .frame(width: 18, height: 18).background(Circle().fill(Color.accentColor))
                    Text(LocalizedStringKey(t)).font(.callout).fixedSize(horizontal: false, vertical: true)
                }
            }
        }
    }
}


/// The one thing people get wrong about Wi-Fi: the cable is a one-time step,
/// and a locked iPhone isn't "disconnected", just asleep.
struct CableOnceNote: View {
    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: "cable.connector").foregroundStyle(.tint).frame(width: 18)
            VStack(alignment: .leading, spacing: 2) {
                Text("Cable once, then Wi-Fi").font(.callout.weight(.semibold))
                Text("After the first connection Loadside remembers your device. A locked iPhone drops off Wi-Fi; unlock it and it reconnects. No need to plug in again.")
                    .font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.accentColor.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
    }
}
