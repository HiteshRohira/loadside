import SwiftUI

struct Card<Content: View>: View {
    @ViewBuilder var content: Content
    var body: some View {
        VStack(alignment: .leading, spacing: 10) { content }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(.background.secondary, in: RoundedRectangle(cornerRadius: 12))
    }
}

struct FailureView: View {
    let failure: Failure
    @State private var showDetail = false
    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Label(failure.message, systemImage: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
                .font(.callout.weight(.semibold))
            if let h = failure.hint {
                Text(h).font(.callout).foregroundStyle(.secondary).textSelection(.enabled)
            }
            if let d = failure.detail, !d.isEmpty {
                DisclosureGroup("Technical details", isExpanded: $showDetail) {
                    ScrollView {
                        Text(d).font(.caption.monospaced()).textSelection(.enabled)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .frame(maxHeight: 140)
                    Button("Copy") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(d, forType: .string)
                    }
                    .controlSize(.small)
                }
                .font(.caption)
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.orange.opacity(0.08), in: RoundedRectangle(cornerRadius: 10))
    }
}

struct JobProgressView: View {
    let job: JobInfo
    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(job.title).font(.callout.weight(.medium))
                Spacer()
                Text(job.status == .queued ? "Queued" : "\(Int(job.progress * 100))%")
                    .font(.caption.monospacedDigit()).foregroundStyle(.secondary)
            }
            ProgressView(value: job.status == .queued ? 0 : job.progress)
            Text(job.stage).font(.caption).foregroundStyle(.secondary)
        }
    }
}

struct ExpiryBadge: View {
    let expiresAt: Int?
    var body: some View {
        let now = Int(Date().timeIntervalSince1970)
        let left = (expiresAt ?? now) - now
        let (color, text): (Color, String) = {
            guard let e = expiresAt else { return (.secondary, "Expiry unknown") }
            if left <= 0 { return (.red, "Expired \(Fmt.rel(e))") }
            if left < 24 * 3600 { return (.orange, "Expires \(Fmt.rel(e))") }
            if left < 3 * 24 * 3600 { return (.yellow, "Expires \(Fmt.rel(e))") }
            return (.green, "Expires \(Fmt.rel(e))")
        }()
        Label(text, systemImage: "clock")
            .font(.caption)
            .padding(.horizontal, 8).padding(.vertical, 3)
            .background(color.opacity(0.15), in: Capsule())
            .foregroundStyle(color == .yellow ? Color.orange : color)
    }
}

struct Tag: View {
    let text: String
    var color: Color = .secondary
    var body: some View {
        Text(text).font(.caption2.weight(.medium))
            .padding(.horizontal, 6).padding(.vertical, 2)
            .background(color.opacity(0.15), in: Capsule())
            .foregroundStyle(color)
    }
}


/// Free Apple IDs: what's left this week. New apps (and each of their
/// extensions) use App IDs, 10 per rolling 7 days; each device holds 3
/// sideloaded apps. Refreshing an installed app uses neither.
struct FreeQuotaView: View {
    @Environment(Store.self) private var store
    var compact = false

    var body: some View {
        if store.account?.isFree == true {
            let ids = store.appIds
            let avail = ids?.availableQuantity
            let nextFree = ids?.appIds.compactMap(\.expiresAt).min()
            let slots = store.slotsUsed
            let slotsLeft = slots.map { max(0, Store.freeSlotsPerDevice - $0.used) }
            VStack(alignment: .leading, spacing: compact ? 3 : 6) {
                HStack(spacing: compact ? 10 : 16) {
                    if let avail {
                        Label("\(avail) new app\(avail == 1 ? "" : "s") left this week", systemImage: "number.circle")
                            .foregroundStyle(avail == 0 ? .red : avail < 3 ? .orange : .primary)
                    } else {
                        Label("Checking this week's budget…", systemImage: "number.circle").foregroundStyle(.secondary)
                    }
                    if let slotsLeft, let slots {
                        Label("\(slotsLeft) of \(Store.freeSlotsPerDevice) app slots free\(compact ? "" : " on \(slots.device)")", systemImage: "square.grid.2x2")
                            .foregroundStyle(slotsLeft == 0 ? .orange : .primary)
                    }
                }
                Text(footnote(avail: avail, nextFree: nextFree))
                    .font(.caption2).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .font(compact ? .caption : .callout)
            .task(id: store.account?.email) { await store.ensureQuota() }
        }
    }

    private func footnote(avail: Int?, nextFree: Int?) -> String {
        var parts = ["Refreshes are unlimited; only new apps (and their extensions) use the weekly budget of 10."]
        if avail == 0, let nextFree { parts.append("The next one frees \(Fmt.rel(nextFree)).") }
        return parts.joined(separator: " ")
    }
}
