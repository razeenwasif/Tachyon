import Foundation

/// Execution/attention state from OSC 7501 or zero-hook inference (`src/term/`).
public enum ProgramState: String, Sendable, Codable, CaseIterable {
    case idle
    case working
    case blocked
    case done
    case error

    public var urgencyRank: Int {
        switch self {
        case .blocked: return 0
        case .error: return 1
        case .working: return 2
        case .done: return 3
        case .idle: return 4
        }
    }

    public var label: String {
        switch self {
        case .idle: return "Idle"
        case .working: return "Working"
        case .blocked: return "Blocked"
        case .done: return "Done"
        case .error: return "Error"
        }
    }
}

public struct ProgramStatus: Sendable, Codable, Equatable {
    public var state: ProgramState
    public var app: String?
    public var kind: String?
    public var msg: String?
    public var label: String?

    public init(
        state: ProgramState,
        app: String? = nil,
        kind: String? = nil,
        msg: String? = nil,
        label: String? = nil
    ) {
        self.state = state
        self.app = app
        self.kind = kind
        self.msg = msg
        self.label = label
    }
}

/// Direction of a binary split layout (`src/mux/model.rs`).
public enum SplitDir: String, Sendable, Codable, Equatable {
    case right
    case down
}

/// Binary split tree inside a window.
public indirect enum LayoutNode: Sendable, Equatable, Codable {
    case leaf(String)
    case split(dir: SplitDir, ratio: Double, a: LayoutNode, b: LayoutNode)

    public var leaves: [String] {
        switch self {
        case .leaf(let id):
            return [id]
        case .split(_, _, let a, let b):
            return a.leaves + b.leaves
        }
    }

    private enum CodingKeys: String, CodingKey {
        case type
        case blockId = "block_id"
        case dir
        case ratio
        case a
        case b
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let type = try c.decode(String.self, forKey: .type)
        switch type {
        case "leaf":
            self = .leaf(try c.decode(String.self, forKey: .blockId))
        case "split":
            let dir = try c.decode(SplitDir.self, forKey: .dir)
            let ratio = try c.decode(Double.self, forKey: .ratio)
            let a = try c.decode(LayoutNode.self, forKey: .a)
            let b = try c.decode(LayoutNode.self, forKey: .b)
            self = .split(dir: dir, ratio: ratio, a: a, b: b)
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .type,
                in: c,
                debugDescription: "Unknown layout node type: \(type)"
            )
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .leaf(let id):
            try c.encode("leaf", forKey: .type)
            try c.encode(id, forKey: .blockId)
        case .split(let dir, let ratio, let a, let b):
            try c.encode("split", forKey: .type)
            try c.encode(dir, forKey: .dir)
            try c.encode(ratio, forKey: .ratio)
            try c.encode(a, forKey: .a)
            try c.encode(b, forKey: .b)
        }
    }
}

public struct CommandReceipt: Sendable, Codable, Equatable {
    public var cmdSeq: UInt32?
    public var cmdline: String?
    public var exitCode: Int32?
    public var wallUs: UInt64
    public var cpuUs: UInt64?
    public var pcoreBp: UInt16?
    public var energyMj: UInt64?
    public var peakBytes: UInt64?
    public var finishedAt: UInt64

    enum CodingKeys: String, CodingKey {
        case cmdSeq = "cmd_seq"
        case cmdline
        case exitCode = "exit_code"
        case wallUs = "wall_us"
        case cpuUs = "cpu_us"
        case pcoreBp = "pcore_bp"
        case energyMj = "energy_mj"
        case peakBytes = "peak_bytes"
        case finishedAt = "finished_at"
    }

    public var summary: String {
        var parts = ["wall \(Self.formatMicros(wallUs))"]
        if let cpuUs {
            var s = "cpu \(Self.formatMicros(cpuUs))"
            if wallUs > 0 {
                let pct = (Double(cpuUs) / Double(wallUs)) * 100.0
                if pct >= 5.0 {
                    s += String(format: " (%.0f%%)", pct)
                }
            }
            parts.append(s)
        }
        if let pcoreBp {
            let pct = Double(min(pcoreBp, 10_000)) / 100.0
            parts.append(String(format: "%.0f%% P-core", pct))
        }
        if let energyMj, Double(energyMj) / 1000.0 >= 0.05 {
            parts.append(String(format: "%.1f J", Double(energyMj) / 1000.0))
        }
        if let peakBytes, peakBytes > 0 {
            parts.append("peak \(Self.formatBytes(peakBytes))")
        }
        return parts.joined(separator: " · ")
    }

    public static func formatMicros(_ us: UInt64) -> String {
        let secs = Double(us) / 1_000_000.0
        if secs < 0.001 {
            return "\(us) µs"
        } else if secs < 1.0 {
            return "\(us / 1000) ms"
        } else if secs < 60.0 {
            return String(format: "%.2f s", secs)
        } else {
            let total = Int(secs)
            let h = total / 3600
            let m = (total % 3600) / 60
            let s = total % 60
            return h > 0 ? String(format: "%dh %02dm %02ds", h, m, s) : String(format: "%dm %02ds", m, s)
        }
    }

    public static func formatBytes(_ bytes: UInt64) -> String {
        let b = Double(bytes)
        let kib = 1024.0
        let mib = kib * 1024.0
        let gib = mib * 1024.0
        if b >= gib {
            return String(format: "%.2f GiB", b / gib)
        } else if b >= mib {
            return String(format: "%.1f MiB", b / mib)
        } else if b >= kib {
            return String(format: "%.0f KiB", b / kib)
        } else {
            return "\(bytes) B"
        }
    }
}

public struct HostInfo: Sendable, Codable, Equatable {
    public var hostname: String
    public var os: String
    public var arch: String
    public var version: String
    public var protocolVersion: UInt32
    public var pid: UInt32
    public var build: String?
    public var capabilities: [String]
    public var authEnabled: Bool?
    public var listenAddr: String?

    enum CodingKeys: String, CodingKey {
        case hostname, os, arch, version
        case protocolVersion = "protocol"
        case pid, build, capabilities
        case authEnabled = "auth_enabled"
        case listenAddr = "listen_addr"
    }

    public init(
        hostname: String = "",
        os: String = "unknown",
        arch: String = "unknown",
        version: String = "",
        protocolVersion: UInt32 = muxProtocolVersion,
        pid: UInt32 = 0,
        build: String? = nil,
        capabilities: [String] = [],
        authEnabled: Bool? = nil,
        listenAddr: String? = nil
    ) {
        self.hostname = hostname
        self.os = os
        self.arch = arch
        self.version = version
        self.protocolVersion = protocolVersion
        self.pid = pid
        self.build = build
        self.capabilities = capabilities
        self.authEnabled = authEnabled
        self.listenAddr = listenAddr
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        hostname = try c.decode(String.self, forKey: .hostname)
        os = try c.decode(String.self, forKey: .os)
        arch = try c.decode(String.self, forKey: .arch)
        version = try c.decode(String.self, forKey: .version)
        protocolVersion = try c.decode(UInt32.self, forKey: .protocolVersion)
        pid = try c.decodeIfPresent(UInt32.self, forKey: .pid) ?? 0
        build = try c.decodeIfPresent(String.self, forKey: .build)
        capabilities = try c.decodeIfPresent([String].self, forKey: .capabilities) ?? []
        authEnabled = try c.decodeIfPresent(Bool.self, forKey: .authEnabled)
        listenAddr = try c.decodeIfPresent(String.self, forKey: .listenAddr)
    }
}

public struct BlockInfo: Sendable, Codable, Equatable, Identifiable {
    public var id: String
    public var sessionId: String
    public var windowId: String
    public var label: String?
    public var title: String?
    public var cwd: String?
    public var command: String?
    public var commandRunning: Bool
    public var pid: UInt32
    public var cols: UInt16
    public var rows: UInt16
    public var alive: Bool
    public var exitCode: Int32?
    public var awake: Bool
    public var attachedClients: Int
    public var createdAt: UInt64
    public var status: ProgramStatus?
    public var statusInferred: Bool
    public var lastReceipt: CommandReceipt?

    enum CodingKeys: String, CodingKey {
        case id
        case sessionId = "session_id"
        case windowId = "window_id"
        case label, title, cwd, command
        case commandRunning = "command_running"
        case pid, cols, rows, alive
        case exitCode = "exit_code"
        case awake
        case attachedClients = "attached_clients"
        case createdAt = "created_at"
        case status
        case statusInferred = "status_inferred"
        case lastReceipt = "last_receipt"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        sessionId = try c.decode(String.self, forKey: .sessionId)
        windowId = try c.decode(String.self, forKey: .windowId)
        label = try c.decodeIfPresent(String.self, forKey: .label)
        title = try c.decodeIfPresent(String.self, forKey: .title)
        cwd = try c.decodeIfPresent(String.self, forKey: .cwd)
        command = try c.decodeIfPresent(String.self, forKey: .command)
        commandRunning = try c.decodeIfPresent(Bool.self, forKey: .commandRunning) ?? false
        pid = try c.decodeIfPresent(UInt32.self, forKey: .pid) ?? 0
        cols = try c.decodeIfPresent(UInt16.self, forKey: .cols) ?? 80
        rows = try c.decodeIfPresent(UInt16.self, forKey: .rows) ?? 24
        alive = try c.decodeIfPresent(Bool.self, forKey: .alive) ?? true
        exitCode = try c.decodeIfPresent(Int32.self, forKey: .exitCode)
        awake = try c.decodeIfPresent(Bool.self, forKey: .awake) ?? false
        attachedClients = try c.decodeIfPresent(Int.self, forKey: .attachedClients) ?? 0
        createdAt = try c.decodeIfPresent(UInt64.self, forKey: .createdAt) ?? 0
        status = try c.decodeIfPresent(ProgramStatus.self, forKey: .status)
        statusInferred = try c.decodeIfPresent(Bool.self, forKey: .statusInferred) ?? false
        lastReceipt = try c.decodeIfPresent(CommandReceipt.self, forKey: .lastReceipt)
    }

    public var displayName: String {
        if let label, !label.isEmpty { return label }
        if let title, !title.isEmpty { return title }
        if let app = status?.app, !app.isEmpty { return app.capitalized }
        if let command, !command.isEmpty { return command }
        if let cwd, !cwd.isEmpty { return Self.shortPath(cwd) }
        return "shell"
    }

    public var shortCwd: String {
        guard let cwd, !cwd.isEmpty else { return "" }
        return Self.shortPath(cwd)
    }

    public static func shortPath(_ path: String) -> String {
        let components = path.split(separator: "/", omittingEmptySubsequences: true)
        guard components.count > 2 else { return path }
        return "…/" + components.suffix(2).joined(separator: "/")
    }
}

public struct WindowInfo: Sendable, Codable, Equatable, Identifiable {
    public var id: String
    public var sessionId: String
    public var name: String?
    public var layout: LayoutNode
    public var focusedBlock: String
    public var zoomed: Bool
    public var createdAt: UInt64

    enum CodingKeys: String, CodingKey {
        case id
        case sessionId = "session_id"
        case name, layout
        case focusedBlock = "focused_block"
        case zoomed
        case createdAt = "created_at"
    }
}

public struct SessionInfo: Sendable, Codable, Equatable, Identifiable {
    public var id: String
    public var name: String
    public var windows: [String]
    public var activeWindow: String?
    public var createdAt: UInt64

    enum CodingKeys: String, CodingKey {
        case id, name, windows
        case activeWindow = "active_window"
        case createdAt = "created_at"
    }
}

public struct ClientInfo: Sendable, Codable, Equatable, Identifiable {
    public var id: String
    public var kind: String
    public var name: String
}

public struct ServerSnapshot: Sendable, Codable, Equatable {
    public var host: HostInfo
    public var sessions: [SessionInfo]
    public var windows: [WindowInfo]
    public var blocks: [BlockInfo]
    public var clients: [ClientInfo]

    public init(
        host: HostInfo,
        sessions: [SessionInfo] = [],
        windows: [WindowInfo] = [],
        blocks: [BlockInfo] = [],
        clients: [ClientInfo] = []
    ) {
        self.host = host
        self.sessions = sessions
        self.windows = windows
        self.blocks = blocks
        self.clients = clients
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        host = try c.decode(HostInfo.self, forKey: .host)
        sessions = try c.decode([SessionInfo].self, forKey: .sessions)
        windows = try c.decode([WindowInfo].self, forKey: .windows)
        blocks = try c.decode([BlockInfo].self, forKey: .blocks)
        clients = try c.decodeIfPresent([ClientInfo].self, forKey: .clients) ?? []
    }
}

/// Host accent/danger colour resolution matching `src/mux/model.rs`.
public struct HostColor: Sendable, Equatable {
    public let r: UInt8
    public let g: UInt8
    public let b: UInt8
    public let danger: Bool

    public init(r: UInt8, g: UInt8, b: UInt8, danger: Bool = false) {
        self.r = r
        self.g = g
        self.b = b
        self.danger = danger
    }

    public static let presets: [(id: String, name: String)] = [
        ("auto", "Auto (by OS)"),
        ("blue", "Blue"),
        ("purple", "Purple"),
        ("teal", "Teal"),
        ("green", "Green"),
        ("amber", "Amber (Warning)"),
        ("orange", "Orange (Caution)"),
        ("red", "Red (Danger)"),
        ("pink", "Pink"),
        ("none", "None"),
    ]

    public static func resolve(spec: String?, os: String) -> HostColor? {
        if let spec = spec?.trimmingCharacters(in: .whitespacesAndNewlines),
           !spec.isEmpty,
           spec.lowercased() != "auto" {
            if let parsed = parse(spec) {
                return parsed // Note: .some(nil) means explicit opt-out
            }
        }
        return defaultOSColor(os: os)
    }

    public static func defaultOSColor(os: String) -> HostColor? {
        switch os.lowercased() {
        case "macos": return HostColor(r: 66, g: 153, b: 255)
        case "windows": return HostColor(r: 168, g: 118, b: 255)
        case "linux": return HostColor(r: 45, g: 200, b: 195)
        default: return nil
        }
    }

    /// Double optional: `nil` = invalid spec, `.some(nil)` = explicit `"none"`, `.some(color)` = resolved color.
    public static func parse(_ raw: String) -> HostColor?? {
        let lower = raw.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        guard !lower.isEmpty else { return nil }
        switch lower {
        case "none", "off", "clear": return .some(nil)
        case "blue", "sapphire": return HostColor(r: 66, g: 153, b: 255)
        case "purple", "violet": return HostColor(r: 168, g: 118, b: 255)
        case "teal", "cyan": return HostColor(r: 45, g: 200, b: 195)
        case "green", "emerald": return HostColor(r: 56, g: 204, b: 118)
        case "pink", "rose": return HostColor(r: 245, g: 110, b: 170)
        case "amber", "yellow", "warn", "warning": return HostColor(r: 245, g: 178, b: 45, danger: true)
        case "orange", "caution": return HostColor(r: 250, g: 132, b: 48, danger: true)
        case "red", "danger", "crimson", "prod": return HostColor(r: 242, g: 74, b: 74, danger: true)
        default:
            if lower.hasPrefix("danger:") {
                let hex = String(lower.dropFirst("danger:".count)).trimmingCharacters(in: .whitespaces)
                guard let rgb = parseHex(hex) else { return nil }
                return HostColor(r: rgb.0, g: rgb.1, b: rgb.2, danger: true)
            }
            guard let rgb = parseHex(lower) else { return nil }
            return HostColor(r: rgb.0, g: rgb.1, b: rgb.2, danger: false)
        }
    }

    private static func parseHex(_ hex: String) -> (UInt8, UInt8, UInt8)? {
        guard hex.hasPrefix("#"), hex.count == 7 else { return nil }
        let body = String(hex.dropFirst())
        guard let v = UInt32(body, radix: 16) else { return nil }
        return (UInt8((v >> 16) & 0xFF), UInt8((v >> 8) & 0xFF), UInt8(v & 0xFF))
    }
}

/// Saved paired remote server profile (bearer token kept separately in Keychain `TokenStore`).
public struct HostProfile: Sendable, Codable, Equatable, Identifiable {
    public let id: UUID
    public var alias: String
    public var host: String
    public var port: UInt16
    public var os: String
    public var arch: String
    public var ssh: String?
    public var color: String?
    public var addedAt: Date

    public init(
        id: UUID = UUID(),
        alias: String,
        host: String,
        port: UInt16 = defaultMuxPort,
        os: String = "unknown",
        arch: String = "unknown",
        ssh: String? = nil,
        color: String? = nil,
        addedAt: Date = Date()
    ) {
        self.id = id
        self.alias = alias
        self.host = host
        self.port = port
        self.os = os
        self.arch = arch
        self.ssh = ssh
        self.color = color
        self.addedAt = addedAt
    }

    public var tcpEndpoint: String {
        if host.contains(":") && !host.hasPrefix("[") {
            return "[\(host)]:\(port)"
        }
        return "\(host):\(port)"
    }

    public var resolvedColor: HostColor? {
        HostColor.resolve(spec: color, os: os)
    }
}

/// Cross-host live agent row sorted by urgency (Blocked -> Error -> Working -> Done).
public struct AgentEntry: Sendable, Equatable, Identifiable {
    public var id: String { "\(hostId.uuidString):\(block.id)" }
    public let hostId: UUID
    public let hostAlias: String
    public let hostOS: String
    public let hostColor: HostColor?
    public let sessionId: String
    public let sessionName: String
    public let windowId: String
    public let block: BlockInfo

    public var state: ProgramState {
        block.status?.state ?? (block.alive ? .idle : .done)
    }

    public var appName: String {
        if let app = block.status?.app, !app.isEmpty {
            return app
        }
        return block.displayName
    }

    public var urgencyRank: Int {
        state.urgencyRank
    }

    public static func collect(
        hosts: [(profile: HostProfile, snapshot: ServerSnapshot?)]
    ) -> [AgentEntry] {
        var entries: [AgentEntry] = []
        for (profile, snapOpt) in hosts {
            guard let snap = snapOpt else { continue }
            let sessionMap = Dictionary(uniqueKeysWithValues: snap.sessions.map { ($0.id, $0) })
            let os = snap.host.os.isEmpty ? profile.os : snap.host.os
            let color = HostColor.resolve(spec: profile.color, os: os)
            for block in snap.blocks {
                guard block.status != nil else { continue }
                let sName = sessionMap[block.sessionId]?.name ?? "session"
                entries.append(
                    AgentEntry(
                        hostId: profile.id,
                        hostAlias: profile.alias,
                        hostOS: os,
                        hostColor: color,
                        sessionId: block.sessionId,
                        sessionName: sName,
                        windowId: block.windowId,
                        block: block
                    )
                )
            }
        }
        return entries.sorted { a, b in
            if a.urgencyRank != b.urgencyRank {
                return a.urgencyRank < b.urgencyRank
            }
            if a.block.createdAt != b.block.createdAt {
                return a.block.createdAt > b.block.createdAt
            }
            return a.block.id < b.block.id
        }
    }
}

/// Payload carried by `agent.alert` events (`src/server/alerts.rs`).
public struct AlertEvent: Sendable, Codable, Equatable {
    public let host: String
    public let blockId: String
    public let sessionId: String
    public let windowId: String
    public let label: String
    public let state: String
    public let app: String?
    public let message: String?
    public let title: String?
    public let body: String?
    public let timestamp: UInt64

    enum CodingKeys: String, CodingKey {
        case host
        case blockId = "block_id"
        case sessionId = "session_id"
        case windowId = "window_id"
        case label, state, app, message, title, body, timestamp
    }
}

/// Typed representation of incoming `FrameKind.event` frames.
public enum MuxEvent: Sendable, Equatable {
    case topologyChanged(String)
    case blockStatus(block: BlockInfo, status: ProgramStatus?)
    case blockUpdated(block: BlockInfo)
    case blockExited(blockId: String, exitCode: Int32)
    case blockReceipt(blockId: String, receipt: CommandReceipt)
    case agentAlert(AlertEvent)
    case other(String)

    public static func decode(from data: Data) -> MuxEvent {
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let eventName = obj["event"] as? String
        else {
            return .other("unknown")
        }
        let decoder = JSONDecoder()
        switch eventName {
        case "block.status":
            if let blkObj = obj["block"],
               let blkData = try? JSONSerialization.data(withJSONObject: blkObj),
               let block = try? decoder.decode(BlockInfo.self, from: blkData) {
                return .blockStatus(block: block, status: block.status)
            }
            return .topologyChanged(eventName)
        case "block.updated":
            if let blkObj = obj["block"],
               let blkData = try? JSONSerialization.data(withJSONObject: blkObj),
               let block = try? decoder.decode(BlockInfo.self, from: blkData) {
                return .blockUpdated(block: block)
            }
            return .topologyChanged(eventName)
        case "block.exited":
            let bid = obj["block_id"] as? String ?? ""
            let code = (obj["exit_code"] as? NSNumber)?.int32Value ?? 0
            return .blockExited(blockId: bid, exitCode: code)
        case "block.receipt":
            if let bid = obj["block_id"] as? String,
               let rcptObj = obj["receipt"],
               let rcptData = try? JSONSerialization.data(withJSONObject: rcptObj),
               let rcpt = try? decoder.decode(CommandReceipt.self, from: rcptData) {
                return .blockReceipt(blockId: bid, receipt: rcpt)
            }
            return .topologyChanged(eventName)
        case "agent.alert":
            if let alert = try? decoder.decode(AlertEvent.self, from: data) {
                return .agentAlert(alert)
            }
            return .other(eventName)
        case "session.created", "session.renamed", "session.killed",
             "window.created", "window.renamed", "window.killed",
             "window.focused", "window.updated",
             "block.created", "block.killed":
            return .topologyChanged(eventName)
        default:
            return .other(eventName)
        }
    }
}
