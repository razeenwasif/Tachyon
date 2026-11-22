import SwiftUI
import TachyonCore

/// Multi-host fleet overview showing Spaces, Sessions, Windows, and individual Terminal Panes.
struct SpacesHostsView: View {
    @Bindable var store: FleetStore
    let onOpenPairing: () -> Void

    @State private var newSessionHostId: UUID?
    @State private var newSessionName = ""
    @State private var errorNotice: String?

    var body: some View {
        NavigationStack {
            List {
                if store.hosts.isEmpty {
                    Section {
                        VStack(alignment: .leading, spacing: 8) {
                            Text("No Paired Hosts")
                                .font(.headline)
                                .foregroundStyle(TachyonPalette.textPrimary)
                            Text("Tap + to scan a QR code from `tachyon pair` or enter a Tailscale TCP host manually.")
                                .font(.caption)
                                .foregroundStyle(TachyonPalette.textSecondary)
                            Button("Pair Host Now", action: onOpenPairing)
                                .buttonStyle(.borderedProminent)
                                .tint(TachyonPalette.accent)
                                .foregroundStyle(.black)
                                .padding(.top, 4)
                        }
                        .padding(.vertical, 8)
                    }
                    .listRowBackground(TachyonPalette.cardBg)
                }

                ForEach(store.hosts) { hostState in
                    Section {
                        if let snap = hostState.snapshot {
                            if snap.sessions.isEmpty {
                                HStack {
                                    Text("No open sessions on \(hostState.profile.alias)")
                                        .font(.caption)
                                        .foregroundStyle(TachyonPalette.textTertiary)
                                    Spacer()
                                    Button("New Shell") {
                                        startQuickSession(hostId: hostState.id)
                                    }
                                    .font(.caption.weight(.semibold))
                                    .foregroundStyle(TachyonPalette.accent)
                                }
                                .listRowBackground(TachyonPalette.cardBg)
                            } else {
                                ForEach(snap.sessions) { session in
                                    sessionGroup(hostState: hostState, snapshot: snap, session: session)
                                }
                            }
                        } else {
                            HStack(spacing: 8) {
                                if case .connecting = hostState.status {
                                    ProgressView().controlSize(.small)
                                } else {
                                    Image(systemName: "wifi.slash")
                                        .foregroundStyle(TachyonPalette.textTertiary)
                                }
                                Text(hostState.status.label)
                                    .font(.caption)
                                    .foregroundStyle(TachyonPalette.textSecondary)
                                Spacer()
                                Button("Reconnect") {
                                    store.reconnectHost(id: hostState.id)
                                }
                                .font(.caption.weight(.semibold))
                                .foregroundStyle(TachyonPalette.accent)
                            }
                            .listRowBackground(TachyonPalette.cardBg)
                        }
                    } header: {
                        hostSectionHeader(hostState)
                    }
                }
            }
            .listStyle(.insetGrouped)
            .scrollContentBackground(.hidden)
            .background(TachyonPalette.canvasBg)
            .refreshable {
                store.reconnectAll()
            }
            .navigationTitle("Spaces & Hosts")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button(action: onOpenPairing) {
                        Image(systemName: "plus")
                    }
                    .accessibilityLabel("Pair Host")
                }
            }
            .alert("New Remote Session", isPresented: Binding(
                get: { newSessionHostId != nil },
                set: { if !$0 { newSessionHostId = nil } }
            )) {
                TextField("Session name (optional)", text: $newSessionName)
                Button("Create") {
                    if let hid = newSessionHostId {
                        let name = newSessionName.trimmingCharacters(in: .whitespacesAndNewlines)
                        Task {
                            do {
                                try await store.createSession(hostId: hid, name: name.isEmpty ? nil : name)
                            } catch {
                                errorNotice = error.localizedDescription
                            }
                        }
                    }
                    newSessionHostId = nil
                    newSessionName = ""
                }
                Button("Cancel", role: .cancel) {
                    newSessionHostId = nil
                    newSessionName = ""
                }
            }
            .alert("Action Error", isPresented: Binding(
                get: { errorNotice != nil },
                set: { if !$0 { errorNotice = nil } }
            )) {
                Button("OK", role: .cancel) {}
            } message: {
                Text(errorNotice ?? "")
            }
        }
    }

    private func hostSectionHeader(_ hostState: HostRuntimeState) -> some View {
        let resolvedColor = hostState.profile.resolvedColor
        let dotColor: Color = switch hostState.status {
        case .connected: TachyonPalette.stateDone
        case .connecting: TachyonPalette.stateBlocked
        case .disconnected, .error: TachyonPalette.stateError
        }

        return HStack(spacing: 8) {
            HostBadge(alias: hostState.profile.alias, os: hostState.profile.os, hostColor: resolvedColor)
            Circle().fill(dotColor).frame(width: 7, height: 7)
            if let rtt = hostState.rttMs {
                Text(String(format: "%.1f ms", rtt))
                    .font(.caption2.monospacedDigit())
                    .foregroundStyle(TachyonPalette.textTertiary)
            }
            Spacer()
            Button {
                newSessionHostId = hostState.id
            } label: {
                Image(systemName: "plus.rectangle.on.rectangle")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(TachyonPalette.accent)
            }
            .disabled(!hostState.status.isConnected)
        }
        .textCase(nil)
    }

    @ViewBuilder
    private func sessionGroup(
        hostState: HostRuntimeState,
        snapshot: ServerSnapshot,
        session: SessionInfo
    ) -> some View {
        let blocksById = Dictionary(uniqueKeysWithValues: snapshot.blocks.map { ($0.id, $0) })
        let sessionWindows = snapshot.windows.filter { $0.sessionId == session.id }
        let orderedBlocks = sessionWindows.flatMap { win in
            win.layout.leaves.compactMap { blocksById[$0] }
        }

        ForEach(orderedBlocks) { blk in
            let isAttached = store.activeAttachment?.hostId == hostState.id && store.activeAttachment?.blockId == blk.id
            Button {
                Task {
                    do {
                        try await store.attachToBlock(hostId: hostState.id, blockId: blk.id)
                    } catch {
                        errorNotice = error.localizedDescription
                    }
                }
            } label: {
                HStack(spacing: 10) {
                    VStack(alignment: .leading, spacing: 4) {
                        HStack(spacing: 6) {
                            Text(session.name)
                                .font(.caption.weight(.semibold))
                                .foregroundStyle(TachyonPalette.accent)
                            Text("· \(blk.displayName)")
                                .font(.subheadline.weight(.medium))
                                .foregroundStyle(TachyonPalette.textPrimary)
                                .lineLimit(1)
                            if isAttached {
                                Text("LIVE")
                                    .font(.caption2.weight(.bold))
                                    .padding(.horizontal, 5)
                                    .padding(.vertical, 1)
                                    .foregroundStyle(.black)
                                    .background(TachyonPalette.accent, in: Capsule())
                            }
                        }
                        if !blk.shortCwd.isEmpty {
                            Text(blk.shortCwd)
                                .font(.caption2.monospaced())
                                .foregroundStyle(TachyonPalette.textTertiary)
                                .lineLimit(1)
                        }
                        if let rcpt = blk.lastReceipt {
                            Text(rcpt.summary)
                                .font(.caption2)
                                .foregroundStyle(TachyonPalette.textSecondary)
                                .lineLimit(1)
                        }
                    }
                    Spacer()
                    if let st = blk.status {
                        StatePill(state: st.state, compact: true)
                    } else if !blk.alive {
                        Text("Exited")
                            .font(.caption2)
                            .foregroundStyle(TachyonPalette.textTertiary)
                    }
                    Image(systemName: "chevron.right")
                        .font(.caption2)
                        .foregroundStyle(TachyonPalette.textTertiary)
                }
                .padding(.vertical, 2)
            }
            .listRowBackground(TachyonPalette.cardBg)
            .swipeActions(edge: .trailing) {
                Button(role: .destructive) {
                    Task {
                        try? await store.killBlock(hostId: hostState.id, blockId: blk.id)
                    }
                } label: {
                    Label("Kill", systemImage: "xmark.bin")
                }
            }
        }
    }

    private func startQuickSession(hostId: UUID) {
        Task {
            do {
                try await store.createSession(hostId: hostId)
            } catch {
                errorNotice = error.localizedDescription
            }
        }
    }
}
