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
