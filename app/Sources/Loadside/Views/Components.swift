import SwiftUI

// MARK: - Surfaces

extension View {
    /// The quiet rounded panel everything sits on.
    func surface(cornerRadius: CGFloat = 18, padding: CGFloat? = 16) -> some View {
        self
            .padding(padding ?? 0)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(RoundedRectangle(cornerRadius: cornerRadius, style: .continuous).fill(Color.primary.opacity(0.045)))
            .overlay(RoundedRectangle(cornerRadius: cornerRadius, style: .continuous).strokeBorder(Color.primary.opacity(0.07)))
    }

    /// Floating UI (banners): Liquid Glass where available.
    @ViewBuilder func floatingGlass(cornerRadius: CGFloat = 16) -> some View {
        if #available(macOS 26, *) {
            self.glassEffect(.regular, in: RoundedRectangle(cornerRadius: cornerRadius, style: .continuous))
        } else {
            self.background(.regularMaterial, in: RoundedRectangle(cornerRadius: cornerRadius, style: .continuous))
                .shadow(color: .black.opacity(0.15), radius: 12, y: 4)
        }
    }

    /// Prominent action button: glass on macOS 26+, bordered elsewhere.
    @ViewBuilder func primaryButton() -> some View {
        if #available(macOS 26, *) { self.buttonStyle(.glassProminent) } else { self.buttonStyle(.borderedProminent) }
    }

    @ViewBuilder func secondaryButton() -> some View {
        if #available(macOS 26, *) { self.buttonStyle(.glass) } else { self.buttonStyle(.bordered) }
    }
}

/// Kept for the setup guide and settings panes.
struct Card<Content: View>: View {
    @ViewBuilder var content: Content
    var body: some View {
        VStack(alignment: .leading, spacing: 10) { content }
            .surface(cornerRadius: 14, padding: 14)
    }
}

// MARK: - App icon

struct AppIconView: View {
    /// IPA/.app to read the icon from.
    let path: String?
    let name: String
    var size: CGFloat = 56

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: size * 0.225, style: .continuous)
        Group {
            if let path, let img = AppIcons.shared.image(for: path) {
                Image(nsImage: img).resizable().interpolation(.high)
            } else {
                ZStack {
                    LinearGradient(colors: [tint.opacity(0.95), tint.opacity(0.65)], startPoint: .top, endPoint: .bottom)
                    Text(String(name.prefix(1)).uppercased())
                        .font(.system(size: size * 0.45, weight: .semibold, design: .rounded))
                        .foregroundStyle(.white)
                }
            }
        }
        .frame(width: size, height: size)
        .clipShape(shape)
        .overlay(shape.strokeBorder(Color.primary.opacity(0.08), lineWidth: 0.5))
        .shadow(color: .black.opacity(size > 30 ? 0.12 : 0), radius: size / 14, y: size / 28)
    }

    /// A stable colour per app name.
    private var tint: Color {
        let palette: [Color] = [.blue, .indigo, .purple, .pink, .orange, .teal, .green, .cyan]
        let sum = name.unicodeScalars.reduce(0) { $0 &+ Int($1.value) }
        return palette[abs(sum) % palette.count]
    }
}

// MARK: - Expiry

struct Expiry {
    var text: String
    var color: Color
    /// Remaining share of the signing period, 0...1.
    var fraction: Double

    static func of(_ app: AppRecord) -> Expiry {
        of(expiresAt: app.expiresAt, signedAt: app.lastRefreshedAt ?? app.installedAt)
    }

    static func of(expiresAt: Int?, signedAt: Int? = nil) -> Expiry {
        guard let e = expiresAt else { return Expiry(text: "Expiry unknown", color: .secondary, fraction: 0) }
        let now = Int(Date().timeIntervalSince1970)
        let left = e - now
        let total = max(7 * 86400, e - (signedAt ?? (e - 7 * 86400)))
        let fraction = min(1, max(0, Double(left) / Double(total)))
        let text: String
        switch left {
        case ..<1: text = "Expired"
        case ..<3600: text = "Expires in \(max(1, left / 60)) min"
        case ..<86400: text = "\(left / 3600) hour\(left / 3600 == 1 ? "" : "s") left"
        default:
            let d = left / 86400
            text = "\(d) day\(d == 1 ? "" : "s") left"
        }
        let color: Color = left <= 0 ? .red : left < 86400 ? .orange : left < 3 * 86400 ? .yellow : .green
        return Expiry(text: text, color: color, fraction: fraction)
    }
}

/// Thin countdown bar.
struct ExpiryBar: View {
    let expiry: Expiry
    var body: some View {
        GeometryReader { g in
            ZStack(alignment: .leading) {
                Capsule().fill(Color.primary.opacity(0.08))
                Capsule().fill(expiry.color.gradient).frame(width: max(4, g.size.width * expiry.fraction))
            }
        }
        .frame(height: 5)
    }
}

/// Compact expiry label (menu bar).
struct ExpiryBadge: View {
    let expiresAt: Int?
    var body: some View {
        let e = Expiry.of(expiresAt: expiresAt)
        Text(e.text)
            .font(.caption.weight(.medium))
            .padding(.horizontal, 7).padding(.vertical, 2)
            .background(e.color.opacity(0.15), in: Capsule())
            .foregroundStyle(e.color == .yellow ? Color.orange : e.color)
    }
}

struct StatusDot: View {
    let color: Color
    var body: some View {
        Circle().fill(color).frame(width: 7, height: 7)
            .shadow(color: color.opacity(0.6), radius: 3)
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

// MARK: - Errors and progress

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
                    .fixedSize(horizontal: false, vertical: true)
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
        .background(Color.orange.opacity(0.09), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }
}

struct JobProgressView: View {
    let job: JobInfo
    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            ProgressView(value: job.status == .queued ? 0 : job.progress)
                .progressViewStyle(.linear)
            HStack {
                Text(job.status == .queued ? "Waiting…" : job.stage).lineLimit(1)
                Spacer()
                if job.status != .queued { Text("\(Int(job.progress * 100))%").monospacedDigit() }
            }
            .font(.caption).foregroundStyle(.secondary)
        }
    }
}

// MARK: - Free account budget

/// Free Apple IDs: what's left this week. New apps (and each of their
/// extensions) use App IDs, 10 per rolling 7 days; each device holds 3
/// sideloaded apps. Refreshing an installed app uses neither.
struct FreeQuotaView: View {
    @Environment(Store.self) private var store
    var compact = false

    var body: some View {
        if store.account?.isFree == true {
            let avail = store.appIds?.availableQuantity
            let max = store.appIds?.maxQuantity ?? 10
            let slotsLeft = store.freeSlotsLeft
            HStack(spacing: 14) {
                if let avail {
                    Label("\(avail) of \(max) App IDs left this week", systemImage: "number.circle")
                        .foregroundStyle(avail == 0 ? .red : avail < 3 ? .orange : .secondary)
                } else {
                    Label("Checking this week's App IDs…", systemImage: "number.circle").foregroundStyle(.secondary)
                }
                if let slotsLeft {
                    Label("\(slotsLeft) of \(Store.freeSlotsPerDevice) app slots free", systemImage: "square.grid.2x2")
                        .foregroundStyle(slotsLeft == 0 ? .orange : .secondary)
                }
            }
            .font(compact ? .caption : .callout)
            .labelStyle(.titleAndIcon)
            .help(footnote(avail: avail))
            .task(id: store.account?.email) { await store.ensureQuota() }
        }
    }

    private func footnote(avail: Int?) -> String {
        var parts = ["Free Apple IDs get 10 App IDs per 7 days. Every new app uses one, and so does each of its widgets and other extensions. Refreshing an app you already have uses none.",
                     "Each device holds 3 sideloaded apps at a time, including ones installed with other tools."]
        if avail == 0, let next = store.appIds?.appIds.compactMap(\.expiresAt).min() { parts.append("The next App ID frees up \(Fmt.rel(next)).") }
        return parts.joined(separator: " ")
    }
}

/// Sizes to its content, and scrolls once that's taller than `maxHeight`
/// (a plain ScrollView in a sheet has no height of its own).
struct FittingScrollView<Content: View>: View {
    var maxHeight: CGFloat
    @ViewBuilder var content: Content
    @State private var height: CGFloat = 0

    var body: some View {
        ScrollView {
            content.background(GeometryReader { g in
                Color.clear.preference(key: HeightKey.self, value: g.size.height)
            })
        }
        .onPreferenceChange(HeightKey.self) { height = $0 }
        .frame(height: max(1, min(height, maxHeight)))
        .scrollBounceBehavior(.basedOnSize)
    }
}

private struct HeightKey: PreferenceKey {
    static let defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = max(value, nextValue()) }
}

/// Form section footer: left-aligned like body text.
struct Footnote: View {
    let text: String
    init(_ text: String) { self.text = text }
    var body: some View {
        Text(text)
            .multilineTextAlignment(.leading)
            .frame(maxWidth: .infinity, alignment: .leading)
    }
}
