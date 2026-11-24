import SwiftUI

struct PromptSheet: View {
    let prompt: Prompt
    var body: some View {
        switch prompt.kind {
        case "two_factor": TwoFactorView(prompt: prompt)
        case "max_certs": MaxCertsView(prompt: prompt)
        default: UnknownPromptView(prompt: prompt)
        }
    }
}

struct TwoFactorView: View {
    @Environment(Store.self) private var store
    let prompt: Prompt
    @State private var code = ""
    /// One answer per prompt (pasting "123 456" changes the field twice).
    @State private var answered = false
    @FocusState private var focused: Bool

    private var sms: Bool { prompt.payload["sms"]?.bool ?? false }
    private var unknown: Bool { prompt.payload["unknown"]?.bool ?? false }
    private var lastError: String? { prompt.payload["lastError"]?.string }
    private var numbers: [(id: Int, digits: String)] {
        (prompt.payload["numbers"]?.array ?? []).compactMap { n in
            guard let id = n["id"]?.int else { return nil }
            return (id, n["lastTwoDigits"]?.string ?? "")
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Label("Two-factor authentication", systemImage: "lock.iphone").font(.title3.weight(.semibold))
            Text(unknown ? "Choose how to get a verification code."
                 : sms ? "Enter the code sent to your phone by text message."
                 : "Enter the 6-digit code shown on your iPhone, iPad or Mac.")
                .foregroundStyle(.secondary)
            if let e = lastError { Label(e, systemImage: "exclamationmark.triangle").foregroundStyle(.orange).font(.callout) }
            if !unknown {
                // .roundedBorder keeps the system field height and clips a
                // large font, so draw the box ourselves.
                TextField("", text: $code, prompt: Text("000000").foregroundStyle(.tertiary))
                    .font(.system(size: 28, weight: .medium, design: .monospaced))
                    .tracking(6)
                    .multilineTextAlignment(.center)
                    .textFieldStyle(.plain)
                    .padding(.vertical, 10)
                    .frame(width: 240)
                    .background(RoundedRectangle(cornerRadius: 8).fill(Color(nsColor: .textBackgroundColor)))
                    .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(focused ? Color.accentColor : Color.secondary.opacity(0.35), lineWidth: focused ? 2 : 1))
                    .focused($focused)
                    .frame(maxWidth: .infinity)
                    .onChange(of: code) { _, v in
                        let digits = v.filter(\.isNumber)
                        if digits != v { code = digits }
                        if digits.count == 6 { submit() }
                    }
                    .onSubmit(submit)
            }
            HStack {
                Menu("Didn't get a code?") {
                    Button("Send to my devices again") { send(.string("SendToDevices")) }
                    if sms { Button("Resend text message") { send(.string("ResendCode")) } }
                    ForEach(numbers, id: \.id) { n in
                        Button("Text me at ••\(n.digits)") { send(.object(["SendSms": .number(Double(n.id))])) }
                    }
                }
                .fixedSize()
                Spacer()
                Button("Cancel") { send(.string("Abort")) }
                if !unknown {
                    Button("Verify", action: submit).buttonStyle(.borderedProminent).disabled(code.count < 6 || answered)
                }
            }
        }
        .padding(24)
        .frame(width: 440)
        .onAppear { focused = true }
    }

    private func submit() {
        guard code.count >= 6 else { return }
        send(.object(["SubmitCode": .string(code)]))
    }

    private func send(_ v: JSON) {
        guard !answered else { return }
        answered = true
        Task { await store.respond(prompt, v) }
    }
}

struct MaxCertsView: View {
    @Environment(Store.self) private var store
    let prompt: Prompt
    @State private var selected: Set<String> = []
    @State private var answered = false

    private var certs: [CertSummary] { (try? prompt.payload["certificates"]?.decode([CertSummary].self)) ?? [] }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label("Certificate limit reached", systemImage: "seal").font(.title3.weight(.semibold))
            Text("Your Apple ID already has the maximum number of development certificates. Revoke one so Loadside can create its own. Apps signed with a revoked certificate stop opening until they're re-signed by the tool that installed them.")
                .font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            List(certs, selection: $selected) { c in
                HStack {
                    Text(c.machineName ?? "Unknown machine")
                    if c.ours { Tag(text: "Loadside", color: .blue) }
                    Spacer()
                    if let e = c.expiresAt { Text("expires \(Fmt.rel(e))").font(.caption).foregroundStyle(.secondary) }
                }
                .tag(c.serialNumber)
            }
            .frame(height: 160)
            HStack {
                Spacer()
                Button("Cancel") { answer(.object(["revoke": .array([])])) }
                Button("Revoke Selected") { answer(.object(["revoke": .array(selected.map { .string($0) })])) }
                    .buttonStyle(.borderedProminent).disabled(selected.isEmpty || answered)
            }
        }
        .padding(24)
        .frame(width: 500)
        .onAppear { if let ours = certs.first(where: \.ours) { selected = [ours.serialNumber] } }
    }

    private func answer(_ v: JSON) {
        guard !answered else { return }
        answered = true
        Task { await store.respond(prompt, v) }
    }
}

struct UnknownPromptView: View {
    @Environment(Store.self) private var store
    let prompt: Prompt
    var body: some View {
        VStack(spacing: 12) {
            Text("Loadside needs input (\(prompt.kind)) that this version of the app doesn't understand.")
            Button("Dismiss") { Task { await store.respond(prompt, .null) } }
        }
        .padding(24)
    }
}
