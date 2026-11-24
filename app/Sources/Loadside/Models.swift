import Foundation

// MARK: - Generic JSON

enum JSON: Codable, Hashable, Sendable {
    case null
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([JSON])
    case object([String: JSON])

    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let b = try? c.decode(Bool.self) { self = .bool(b) }
        else if let n = try? c.decode(Double.self) { self = .number(n) }
        else if let s = try? c.decode(String.self) { self = .string(s) }
        else if let a = try? c.decode([JSON].self) { self = .array(a) }
        else { self = .object(try c.decode([String: JSON].self)) }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let b): try c.encode(b)
        case .number(let n): try c.encode(n)
        case .string(let s): try c.encode(s)
        case .array(let a): try c.encode(a)
        case .object(let o): try c.encode(o)
        }
    }

    subscript(key: String) -> JSON? {
        if case .object(let o) = self { return o[key] }
        return nil
    }
    var string: String? { if case .string(let s) = self { return s }; return nil }
    var bool: Bool? { if case .bool(let b) = self { return b }; return nil }
    var int: Int? { if case .number(let n) = self { return Int(n) }; return nil }
    var array: [JSON]? { if case .array(let a) = self { return a }; return nil }

    func decode<T: Decodable>(_ type: T.Type) throws -> T {
        let data = try JSONEncoder().encode(self)
        return try Coders.decoder.decode(T.self, from: data)
    }

    static func from<T: Encodable>(_ value: T) throws -> JSON {
        let data = try Coders.encoder.encode(value)
        return try JSONDecoder().decode(JSON.self, from: data)
    }
}

enum Coders {
    static let decoder: JSONDecoder = {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }()
    static let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.keyEncodingStrategy = .convertToSnakeCase
        return e
    }()
}

// MARK: - Daemon types (mirror crates/loadside-core/src/{protocol,state,...}.rs)

struct Failure: Codable, Hashable, Error, LocalizedError, Sendable {
    var code: String
    var message: String
    var hint: String?
    var detail: String?
    var errorDescription: String? { message }

    static func local(_ msg: String, hint: String? = nil) -> Failure {
        Failure(code: "client", message: msg, hint: hint, detail: nil)
    }
}

struct AccountInfo: Codable, Hashable, Sendable {
    var email: String
    var name: String?
    var teamId: String?
    var teamName: String?
    var isFree: Bool?
}

enum ExtensionsMode: String, Codable, CaseIterable, Identifiable, Sendable {
    case registerAll = "register_all"
    case reuseMain = "reuse_main"
    case strip
    var id: String { rawValue }
    var label: String {
        switch self {
        case .registerAll: "Register all"
        case .reuseMain: "Reuse main"
        case .strip: "Strip"
        }
    }
    var explanation: String {
        switch self {
        case .registerAll: "Each extension gets its own App ID. Most compatible, but each one counts toward the free limit of 10 App IDs per week."
        case .reuseMain: "Extensions share the main app's profile. Uses 1 App ID; some extensions (widgets, share sheets) may not work."
        case .strip: "Removes all extensions. Uses 1 App ID and avoids most install errors."
        }
    }
}

struct InstallOptions: Codable, Hashable, Sendable {
    var bundleId: String?
    var displayName: String?
    var removeSupportedDevices: Bool = true
    var stripWatchApp: Bool = true
    var extensions: ExtensionsMode = .registerAll
    var fileSharing: Bool = false
    var increasedMemoryLimit: Bool = false
    var inject: [String] = []
    var autoRefresh: Bool = true
    var allowEncrypted: Bool = false
    var allowSigningKey: Bool = false
}

struct AppRecord: Codable, Hashable, Identifiable, Sendable {
    var id: String
    var name: String
    var version: String?
    var originalBundleId: String
    var bundleId: String
    var sourcePath: String
    var options: InstallOptions
    var deviceUdid: String
    var deviceName: String
    var teamId: String
    var email: String
    var installedAt: Int
    var lastRefreshedAt: Int?
    var expiresAt: Int?
    var lastAttemptAt: Int?
    var lastError: Failure?
    var consecutiveFailures: Int
    var onDevice: Bool?

    var expiresDate: Date? { expiresAt.map { Date(timeIntervalSince1970: TimeInterval($0)) } }
}

struct DeviceInfo: Codable, Hashable, Identifiable, Sendable {
    var udid: String
    var name: String
    var model: String?
    var iosVersion: String?
    var connection: String
    var paired: Bool
    var developerMode: Bool?
    var problem: String?
    var wifiSync: Bool?
    var id: String { udid }

    var isUSB: Bool { connection == "usb" }
    var iosMajor: Int? { iosVersion?.split(separator: ".").first.flatMap { Int($0) } }
    var connectionLabel: String { connection == "wifi" ? "Wi-Fi" : "USB" }
    var connectionIcon: String { connection == "wifi" ? "wifi" : "cable.connector" }
}

enum JobStatus: String, Codable, Sendable { case queued, running, succeeded, failed, cancelled }
enum JobKind: String, Codable, Sendable { case install, refresh, export, login }

struct JobInfo: Codable, Hashable, Identifiable, Sendable {
    var id: String
    var kind: JobKind
    var title: String
    var appId: String?
    var deviceUdid: String?
    var status: JobStatus
    var stage: String
    var progress: Double
    var background: Bool
    var createdAt: Int
    var finishedAt: Int?
    var error: Failure?
    var outputPath: String?
    var warnings: [String]
    var isActive: Bool { status == .queued || status == .running }
}

struct Prompt: Codable, Hashable, Identifiable, Sendable {
    var id: String
    var kind: String
    var payload: JSON
    var createdAt: Int
}

struct LogLine: Codable, Hashable, Sendable {
    var ts: Int
    var level: String
    var target: String
    var message: String
}

struct DaemonConfig: Codable, Hashable, Sendable {
    var anisetteUrl: String
    var autoRefresh: Bool
    var refreshThresholdHours: Int
    var checkIntervalMinutes: Int
    var machineName: String
    var notifications: Bool
    var autoRevokeOwnCerts: Bool
    var keepLibraryCopy: Bool
    var onboardingDone: Bool?
    var modernClientIdentity: Bool?
}

struct Snapshot: Codable, Sendable {
    var version: String
    var pid: Int
    var startedAt: Int
    var account: AccountInfo?
    var loggedIn: Bool
    var devices: [DeviceInfo]
    var apps: [AppRecord]
    var jobs: [JobInfo]
    var prompts: [Prompt]
    var config: DaemonConfig
    var nextCheckAt: Int?
}

struct SubBundle: Codable, Hashable, Sendable {
    var name: String
    var bundleId: String
    var kind: String
}

struct IpaInfo: Codable, Hashable, Sendable {
    var name: String
    var bundleId: String
    var version: String?
    var build: String?
    var minimumOs: String?
    var executable: String
    var subBundles: [SubBundle]
    var restrictsDevices: Bool
    var encrypted: Bool?
    var sizeBytes: Int
    var extensionCount: Int { subBundles.filter { $0.kind == "extension" }.count }
    var hasWatchApp: Bool { subBundles.contains { $0.kind == "watch" } }
}

struct InspectResult: Codable, Sendable {
    var info: IpaInfo
    var appIdsNeeded: Int
}

struct AppIdSummary: Codable, Hashable, Identifiable, Sendable {
    var appIdId: String
    var identifier: String
    var name: String
    var expiresAt: Int?
    var id: String { appIdId }
}

struct AppIdList: Codable, Sendable {
    var appIds: [AppIdSummary]
    var maxQuantity: Int?
    var availableQuantity: Int?
}

struct CertSummary: Codable, Hashable, Identifiable, Sendable {
    var serialNumber: String
    var name: String?
    var machineName: String?
    var expiresAt: Int?
    var ours: Bool
    var id: String { serialNumber }
}

struct TeamSummary: Codable, Hashable, Identifiable, Sendable {
    var teamId: String
    var name: String?
    var kind: String?
    var isFree: Bool
    var id: String { teamId }
}

struct TeamsResult: Codable, Sendable {
    var selected: String
    var teams: [TeamSummary]
}

struct InstallParams: Encodable {
    var path: String
    var deviceUdid: String?
    var options: InstallOptions
    var exportPath: String?
    var install: Bool
}

struct JobRef: Decodable { var jobId: String }


// MARK: - Setup / onboarding

enum CheckStatus: String, Codable, Sendable { case ok, action, warning, error, optional, pending }

struct SetupAction: Codable, Hashable, Sendable {
    var id: String
    var label: String
    var url: String?
    var deviceUdid: String?
    var primary: Bool?
}

struct SetupCheck: Codable, Hashable, Identifiable, Sendable {
    var id: String
    var group: String
    var title: String
    var status: CheckStatus
    var summary: String
    var detail: String?
    var actions: [SetupAction]

    var isBlocking: Bool { group == "essentials" && (status == .action || status == .error) }
}

struct SetupReport: Codable, Sendable {
    var checks: [SetupCheck]
    var ready: Bool
    var onboardingDone: Bool
    var generatedAt: Int

    func check(_ id: String) -> SetupCheck? { checks.first { $0.id == id } }
    var blockingCount: Int { checks.filter(\.isBlocking).count }
}
