import Foundation
import Observation

/// Live connection state for a single paired Tachyon host.
public enum HostConnectionStatus: Sendable, Equatable {
    case disconnected
    case connecting
    case connected(HostInfo)
    case error(String)

    public var isConnected: Bool {
        if case .connected = self { return true }
        return false
    }

    public var label: String {
        switch self {
        case .disconnected: return "Offline"
        case .connecting: return "Connecting…"
        case .connected: return "Connected"
        case .error(let msg): return msg
        }
    }
}

/// Observable per-host model owned by `FleetStore`.
@MainActor
@Observable
public final class HostRuntimeState: Identifiable {
    public let id: UUID
    public var profile: HostProfile
    public var status: HostConnectionStatus = .disconnected
    public var rttMs: Double?
    public var snapshot: ServerSnapshot?
    public var hasToken: Bool

    @ObservationIgnored var client: MuxClient?
    @ObservationIgnored var loopTask: Task<Void, Never>?

    public init(profile: HostProfile, hasToken: Bool) {
        self.id = profile.id
        self.profile = profile
        self.hasToken = hasToken
    }
}

/// Active live interactive terminal stream displayed in `TerminalSessionView`.
@MainActor
@Observable
public final class ActiveTerminalAttachment {
    public let hostId: UUID
    public let hostAlias: String
    public let hostOS: String
    public let hostColor: HostColor?
    public let blockId: String
    public let handle: AttachedStreamHandle
    public let buffer: TerminalBuffer
    public var renderSnapshot: TerminalRenderSnapshot
    public var scrollOffset: Int = 0
    public var isAlive: Bool

    @ObservationIgnored var pumpTask: Task<Void, Never>?

    init(
        hostId: UUID,
        hostAlias: String,
        hostOS: String,
        hostColor: HostColor?,
        handle: AttachedStreamHandle
    ) {
        self.hostId = hostId
        self.hostAlias = hostAlias
        self.hostOS = hostOS
        self.hostColor = hostColor
        self.blockId = handle.blockId
        self.handle = handle
        let buf = TerminalBuffer(
            cols: max(20, Int(handle.cols)),
            rows: max(8, Int(handle.rows))
        )
        self.buffer = buf
        self.isAlive = handle.alive
        self.renderSnapshot = buf.renderSnapshot(scrollOffset: 0)
    }

    func refreshRender() {
        renderSnapshot = buffer.renderSnapshot(scrollOffset: scrollOffset)
    }
}

/// Navigation/app coordinate for jumping directly to a pane (e.g. from Agents Inbox or notification tap).
public struct JumpTarget: Sendable, Equatable {
    public let hostId: UUID
    public let blockId: String

    public init(hostId: UUID, blockId: String) {
        self.hostId = hostId
        self.blockId = blockId
    }
}

/// Central multi-host store powering Spaces/Hosts, Live Agents Inbox, Terminal Viewer, and Pairing.
@MainActor
@Observable
public final class FleetStore {
    public var hosts: [HostRuntimeState] = []
    public var activeAttachment: ActiveTerminalAttachment?
    public var snippets: [QuickSnippet]
    public var terminalFontSize: Double
    public var localAlertsEnabled: Bool
    public var selectedTab: Int = 0 // 0 = Agents, 1 = Spaces, 2 = Terminal, 3 = Settings
    public var lastNotice: String?

    @ObservationIgnored private let tokenStore: TokenStore
    @ObservationIgnored private let profileStore: HostProfileStore
    @ObservationIgnored public var onAgentAlert: (@Sendable (HostProfile, AlertEvent) -> Void)?

    public init(
        tokenStore: TokenStore = KeychainTokenStore(),
        profileStore: HostProfileStore = HostProfileStore()
    ) {
        self.tokenStore = tokenStore
        self.profileStore = profileStore
        self.snippets = profileStore.loadSnippets()
        self.terminalFontSize = profileStore.loadFontSize()
        self.localAlertsEnabled = profileStore.loadLocalAlertsEnabled()
        let saved = profileStore.loadHosts()
        self.hosts = saved.map { profile in
            let hasTok = tokenStore.loadToken(forKey: profile.id.uuidString) != nil
            return HostRuntimeState(profile: profile, hasToken: hasTok)
        }
    }

    // MARK: - Computed Cross-Host Views

    public var agentEntries: [AgentEntry] {
        AgentEntry.collect(hosts: hosts.map { ($0.profile, $0.snapshot) })
    }

    public var blockedCount: Int {
        agentEntries.filter { $0.state == .blocked || $0.state == .error }.count
    }

    public var connectedHostCount: Int {
        hosts.filter { $0.status.isConnected }.count
    }

    public func nextBlockedAgent(excludingBlockId: String? = nil) -> AgentEntry? {
        agentEntries.first {
            ($0.state == .blocked || $0.state == .error) && $0.block.id != excludingBlockId
        }
    }

    public func hostState(for hostId: UUID) -> HostRuntimeState? {
        hosts.first { $0.id == hostId }
    }

    public func liveBlock(hostId: UUID, blockId: String) -> BlockInfo? {
        hostState(for: hostId)?.snapshot?.blocks.first { $0.id == blockId }
    }

    // MARK: - Lifecycle & Connections

    public func connectAll() {
        for h in hosts where h.loopTask == nil {
            connectHost(id: h.id)
        }
    }

    public func reconnectAll() {
        for h in hosts {
            reconnectHost(id: h.id)
        }
    }

    public func connectHost(id: UUID) {
        guard let h = hostState(for: id) else { return }
        h.loopTask?.cancel()
        h.loopTask = Task { [weak self, weak h] in
            guard let self, let h else { return }
            await self.runConnectionLoop(for: h)
        }
    }

    public func reconnectHost(id: UUID) {
        guard let h = hostState(for: id) else { return }
        h.loopTask?.cancel()
        h.loopTask = nil
        Task {
            await h.client?.disconnect()
            h.client = nil
            h.status = .disconnected
            self.connectHost(id: id)
        }
    }

    private func runConnectionLoop(for state: HostRuntimeState) async {
        let backoffsNs: [UInt64] = [
            1_000_000_000,
            2_000_000_000,
            5_000_000_000,
            10_000_000_000,
        ]
        var attempt = 0

        while !Task.isCancelled {
            guard hostState(for: state.id) != nil else { return }
            guard let token = tokenStore.loadToken(forKey: state.profile.id.uuidString),
                  !token.isEmpty
            else {
                state.hasToken = false
                state.status = .error("Missing bearer token")
                return
            }
            state.hasToken = true
            state.status = .connecting

            let client = MuxClient()
            state.client = client

            do {
                let info = try await client.connect(
                    host: state.profile.host,
                    port: state.profile.port,
                    token: token,
                    clientName: "tachyon-ios",
                    timeoutSeconds: 6.0
                )
                state.status = .connected(info)
                attempt = 0

                if state.profile.os == "unknown" || state.profile.os != info.os {
                    state.profile.os = info.os
                    state.profile.arch = info.arch
                    persistHosts()
                }

                async let snapTask = client.getState()
                async let pingTask = client.ping(timeoutSeconds: 2.5)
                let snap = try await snapTask
                let rtt = try? await pingTask
                state.snapshot = snap
                state.rttMs = rtt

                let events = try await client.subscribeEvents()
                for await ev in events {
                    if Task.isCancelled { break }
                    await handleEvent(ev, on: state, client: client)
                }
                if !Task.isCancelled {
                    state.status = .error("Connection closed — retrying…")
                }
            } catch {
                if !Task.isCancelled {
                    state.status = .error(error.localizedDescription)
                }
            }

            await client.disconnect()
            if state.client === client {
                state.client = nil
            }
            if activeAttachment?.hostId == state.id {
                activeAttachment?.isAlive = false
            }
            if Task.isCancelled { break }

            let delay = backoffsNs[min(attempt, backoffsNs.count - 1)]
            attempt += 1
            try? await Task.sleep(nanoseconds: delay)
        }
    }

    private func handleEvent(_ ev: MuxEvent, on state: HostRuntimeState, client: MuxClient) async {
        switch ev {
        case .topologyChanged:
            if let fresh = try? await client.getState() {
                state.snapshot = fresh
            }
        case .blockStatus(let updatedBlock, _), .blockUpdated(let updatedBlock):
            if var snap = state.snapshot {
                if let idx = snap.blocks.firstIndex(where: { $0.id == updatedBlock.id }) {
                    snap.blocks[idx] = updatedBlock
                } else {
                    snap.blocks.append(updatedBlock)
                }
                state.snapshot = snap
            }
        case .blockReceipt(let bid, let rcpt):
            if var snap = state.snapshot,
               let idx = snap.blocks.firstIndex(where: { $0.id == bid }) {
                snap.blocks[idx].lastReceipt = rcpt
                state.snapshot = snap
            }
        case .blockExited(let bid, let code):
            if var snap = state.snapshot,
               let idx = snap.blocks.firstIndex(where: { $0.id == bid }) {
                snap.blocks[idx].alive = false
                snap.blocks[idx].exitCode = code
                state.snapshot = snap
            }
            if activeAttachment?.hostId == state.id && activeAttachment?.blockId == bid {
                activeAttachment?.isAlive = false
            }
        case .agentAlert(let alert):
            if localAlertsEnabled {
                onAgentAlert?(state.profile, alert)
            }
        case .other:
            break
        }
    }

    // MARK: - Pairing & Host Management

    @discardableResult
    public func importPairingURI(_ uri: String) async throws -> HostProfile {
        let payload = try PairingPayload.parse(uri)
        return try addOrUpdateHost(
            alias: payload.alias,
            host: payload.host,
            port: payload.port,
            token: payload.token,
            os: payload.os,
            arch: payload.arch,
            ssh: payload.ssh,
            color: payload.color
        )
    }

    @discardableResult
    public func addOrUpdateHost(
        alias: String,
        host: String,
        port: UInt16 = defaultMuxPort,
        token: String,
        os: String = "unknown",
        arch: String = "unknown",
        ssh: String? = nil,
        color: String? = nil
    ) throws -> HostProfile {
        let cleanAlias = alias.trimmingCharacters(in: .whitespacesAndNewlines)
        let cleanHost = host.trimmingCharacters(in: .whitespacesAndNewlines)
        let cleanToken = token.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !cleanHost.isEmpty else {
            throw MuxError.invalidPairingURI("Host address cannot be empty")
        }
        guard !cleanToken.isEmpty else {
            throw MuxError.invalidPairingURI("Bearer token cannot be empty")
        }
        let effectiveAlias = cleanAlias.isEmpty ? cleanHost : cleanAlias

        if let existing = hosts.first(where: {
            $0.profile.alias.caseInsensitiveCompare(effectiveAlias) == .orderedSame
                || ($0.profile.host == cleanHost && $0.profile.port == port)
        }) {
            existing.profile.alias = effectiveAlias
            existing.profile.host = cleanHost
            existing.profile.port = port
            if os != "unknown" { existing.profile.os = os }
            if arch != "unknown" { existing.profile.arch = arch }
            if let ssh { existing.profile.ssh = ssh }
            if let color { existing.profile.color = color }
            try tokenStore.saveToken(cleanToken, forKey: existing.profile.id.uuidString)
            existing.hasToken = true
            persistHosts()
            reconnectHost(id: existing.id)
            return existing.profile
        }

        let profile = HostProfile(
            alias: effectiveAlias,
            host: cleanHost,
            port: port,
            os: os,
            arch: arch,
            ssh: ssh,
            color: color
        )
        try tokenStore.saveToken(cleanToken, forKey: profile.id.uuidString)
        let state = HostRuntimeState(profile: profile, hasToken: true)
        hosts.append(state)
        persistHosts()
        connectHost(id: profile.id)
        return profile
    }

    public func updateHost(id: UUID, alias: String, color: String?) {
        guard let h = hostState(for: id) else { return }
        let trimmed = alias.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmed.isEmpty { h.profile.alias = trimmed }
        h.profile.color = color
        persistHosts()
    }

    public func removeHost(id: UUID) async {
        guard let idx = hosts.firstIndex(where: { $0.id == id }) else { return }
        let state = hosts.remove(at: idx)
        state.loopTask?.cancel()
        if activeAttachment?.hostId == id {
            await detachActive()
        }
        await state.client?.disconnect()
        tokenStore.deleteToken(forKey: state.profile.id.uuidString)
        persistHosts()
    }

    public func testConnection(host: String, port: UInt16, token: String) async throws -> (HostInfo, Double) {
        let client = MuxClient()
        let info = try await client.connect(
            host: host.trimmingCharacters(in: .whitespacesAndNewlines),
            port: port,
            token: token.trimmingCharacters(in: .whitespacesAndNewlines),
            clientName: "tachyon-ios-test",
            timeoutSeconds: 5.0
        )
        let rtt = try await client.ping(timeoutSeconds: 3.0)
        await client.disconnect()
        return (info, rtt)
    }

    // MARK: - Sessions, Splits, Quick Replies & Terminal Attachment

    public func quickSend(hostId: UUID, blockId: String, text: String) async throws {
        guard let client = hostState(for: hostId)?.client else {
            throw MuxError.disconnected("Host is not connected")
        }
        try await client.sendInput(blockId: blockId, data: Data(text.utf8))
    }

    public func createSession(hostId: UUID, name: String? = nil) async throws {
        guard let state = hostState(for: hostId), let client = state.client else {
            throw MuxError.disconnected("Host is not connected")
        }
        let (_, _, blk) = try await client.createSession(name: name, cols: 80, rows: 24)
        if let snap = try? await client.getState() {
            state.snapshot = snap
        }
        try await attachToBlock(hostId: hostId, blockId: blk.id)
    }

    public func splitActivePane(dir: SplitDir) async throws {
        guard let att = activeAttachment,
              let state = hostState(for: att.hostId),
              let client = state.client
        else {
            throw MuxError.disconnected("No active terminal pane")
        }
        let (_, newBlk) = try await client.splitBlock(
            blockId: att.blockId,
            dir: dir,
            cols: UInt16(att.buffer.cols),
            rows: UInt16(att.buffer.rows)
        )
        if let snap = try? await client.getState() {
            state.snapshot = snap
        }
        try await attachToBlock(hostId: att.hostId, blockId: newBlk.id)
    }

    public func killBlock(hostId: UUID, blockId: String) async throws {
        guard let state = hostState(for: hostId), let client = state.client else {
            throw MuxError.disconnected("Host is not connected")
        }
        if activeAttachment?.hostId == hostId && activeAttachment?.blockId == blockId {
            await detachActive()
        }
        try await client.killBlock(blockId: blockId)
        if let snap = try? await client.getState() {
            state.snapshot = snap
        }
    }

    public func attachToBlock(hostId: UUID, blockId: String) async throws {
        guard let state = hostState(for: hostId), let client = state.client else {
            throw MuxError.disconnected("Host is not connected")
        }
        await detachActive()

        let handle = try await client.attach(blockId: blockId, interactive: true, restore: true)
        let attachment = ActiveTerminalAttachment(
            hostId: hostId,
            hostAlias: state.profile.alias,
            hostOS: state.profile.os,
            hostColor: state.profile.resolvedColor,
            handle: handle
        )
        self.activeAttachment = attachment
        self.selectedTab = 2

        attachment.pumpTask = Task { [weak self, weak attachment] in
            guard let self, let attachment else { return }
            for await chunk in handle.chunks {
                if Task.isCancelled { break }
                switch chunk {
                case .snapshot(let vtBytes):
                    attachment.buffer.reset()
                    attachment.buffer.feed(vtBytes)
                    attachment.scrollOffset = 0
                case .output(let outBytes):
                    attachment.buffer.feed(outBytes)
                }
                attachment.refreshRender()
            }
            if !Task.isCancelled && self.activeAttachment === attachment {
                attachment.isAlive = false
            }
        }
    }

    public func detachActive() async {
        guard let att = activeAttachment else { return }
        activeAttachment = nil
        att.pumpTask?.cancel()
        if let client = hostState(for: att.hostId)?.client {
            await client.detach(streamId: att.handle.streamId)
        }
    }

    public func sendActiveInput(_ data: Data) {
        guard let att = activeAttachment,
              let client = hostState(for: att.hostId)?.client,
              !data.isEmpty
        else { return }
        if att.scrollOffset > 0 {
            att.scrollOffset = 0
            att.refreshRender()
        }
        let sid = att.handle.streamId
        Task { try? await client.writePty(streamId: sid, data: data) }
    }

    public func sendSpecialKey(_ key: SpecialKey) {
        guard let att = activeAttachment else { return }
        let appCursor = att.buffer.applicationCursorKeys
        sendActiveInput(KeyEncoder.encodeSpecialKey(key, applicationCursor: appCursor))
    }

    public func pasteTextToActive(_ text: String) {
        guard let att = activeAttachment else { return }
        let bytes = KeyEncoder.encodePaste(text, bracketedPaste: att.buffer.bracketedPasteMode)
        sendActiveInput(bytes)
    }

    public func sendSnippetToActive(_ snippet: QuickSnippet) {
        guard let att = activeAttachment else { return }
        let bytes = KeyEncoder.encodeSnippet(snippet, bracketedPaste: att.buffer.bracketedPasteMode)
        sendActiveInput(bytes)
    }

    public func resizeActive(cols: Int, rows: Int, cellW: Int, cellH: Int) {
        guard let att = activeAttachment,
              let client = hostState(for: att.hostId)?.client
        else { return }
        let clampedCols = max(20, min(cols, 400))
        let clampedRows = max(6, min(rows, 200))
        guard clampedCols != att.buffer.cols || clampedRows != att.buffer.rows else { return }
        att.buffer.resize(cols: clampedCols, rows: clampedRows)
        att.refreshRender()
        let sid = att.handle.streamId
        Task {
            try? await client.resizePty(
                streamId: sid,
                cols: UInt16(clampedCols),
                rows: UInt16(clampedRows),
                cellW: UInt16(max(0, cellW)),
                cellH: UInt16(max(0, cellH))
            )
        }
    }

    public func scrollActive(deltaLines: Int) {
        guard let att = activeAttachment else { return }
        let maxOff = att.buffer.scrollbackCount
        let next = min(max(0, att.scrollOffset + deltaLines), maxOff)
        guard next != att.scrollOffset else { return }
        att.scrollOffset = next
        att.refreshRender()
    }

    public func scrollToBottom() {
        guard let att = activeAttachment, att.scrollOffset != 0 else { return }
        att.scrollOffset = 0
        att.refreshRender()
    }

    public func uploadScreenshotToActive(pngData: Data) async throws -> String {
        guard let att = activeAttachment,
              let state = hostState(for: att.hostId),
              let client = state.client
        else {
            throw MuxError.disconnected("No active host connection")
        }
        let remotePath = try await client.uploadScreenshotPNG(os: state.profile.os, pngData: pngData)
        pasteTextToActive(remotePath)
        lastNotice = "Uploaded screenshot -> \(remotePath)"
        return remotePath
    }

    // MARK: - Preferences

    public func setFontSize(_ size: Double) {
        let clamped = min(max(size, 9.0), 22.0)
        terminalFontSize = clamped
        profileStore.saveFontSize(clamped)
    }

    public func setLocalAlertsEnabled(_ enabled: Bool) {
        localAlertsEnabled = enabled
        profileStore.saveLocalAlertsEnabled(enabled)
    }

    public func addSnippet(label: String, command: String, appendEnter: Bool = true) {
        let l = label.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !l.isEmpty, !command.isEmpty else { return }
        snippets.append(QuickSnippet(label: l, command: command, appendEnter: appendEnter))
        profileStore.saveSnippets(snippets)
    }

    public func deleteSnippets(at offsets: IndexSet) {
        for idx in offsets.sorted(by: >) where idx < snippets.count {
            snippets.remove(at: idx)
        }
        profileStore.saveSnippets(snippets)
    }

    public func resetSnippetsToDefaults() {
        snippets = QuickSnippet.defaults
        profileStore.saveSnippets(snippets)
    }

    private func persistHosts() {
        profileStore.saveHosts(hosts.map(\.profile))
    }
}
