import SwiftUI
import TachyonCore

/// Live Agents Mission Control inbox ranked by urgency (`blocked` -> `error` -> `working` -> `done`).
struct AgentsInboxView: View {
    @Bindable var store: FleetStore
    let onOpenPairing: () -> Void

    enum FilterKind: String, CaseIterable, Identifiable {
        case all = "All"
        case blocked = "Blocked"
        case working = "Working"
        case done = "Done"
        var id: String { rawValue }
    }

    @State private var filter: FilterKind = .all
    @State private var searchText = ""
    @State private var feedbackMessage: String?

    private var filteredEntries: [AgentEntry] {
        let base = store.agentEntries.filter { entry in
            switch filter {
            case .all: return true
            case .blocked: return entry.state == .blocked || entry.state == .error
            case .working: return entry.state == .working
            case .done: return entry.state == .done || entry.state == .idle
            }
        }
        let q = searchText.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        guard !q.isEmpty else { return base }
        return base.filter { entry in
            entry.hostAlias.lowercased().contains(q)
                || entry.appName.lowercased().contains(q)
                || entry.sessionName.lowercased().contains(q)
                || entry.block.displayName.lowercased().contains(q)
                || (entry.block.status?.msg?.lowercased().contains(q) ?? false)
                || (entry.block.cwd?.lowercased().contains(q) ?? false)
        }
    }

    var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                if store.hosts.isEmpty {
                    emptyNoHostsView
                } else {
                    filterHeader
                    if filteredEntries.isEmpty {
                        emptyAgentsView
                    } else {
                        ScrollView {
                            LazyVStack(spacing: 10) {
                                ForEach(filteredEntries) { entry in
                                    agentCard(entry)
                                }
                            }
                            .padding(.horizontal, 14)
                            .padding(.vertical, 12)
                        }
                        .refreshable {
                            store.reconnectAll()
                        }
                    }
                }
            }
            .background(TachyonPalette.canvasBg)
            .navigationTitle("Live Agents")
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    HStack(spacing: 6) {
                        Circle()
                            .fill(store.connectedHostCount > 0 ? TachyonPalette.stateDone : TachyonPalette.stateIdle)
                            .frame(width: 7, height: 7)
                        Text("\(store.connectedHostCount)/\(store.hosts.count) hosts")
                            .font(.caption.weight(.medium))
                            .foregroundStyle(TachyonPalette.textSecondary)
                    }
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Button(action: onOpenPairing) {
                        Image(systemName: "qrcode.viewfinder")
                    }
                    .accessibilityLabel("Pair Host")
                }
            }
            .searchable(text: $searchText, prompt: "Filter agents, hosts, or repos")
            .overlay(alignment: .bottom) {
                if let msg = feedbackMessage {
                    Text(msg)
                        .font(.caption.weight(.medium))
                        .padding(.horizontal, 12)
                        .padding(.vertical, 8)
                        .background(TachyonPalette.surfaceBg, in: Capsule())
                        .overlay(Capsule().stroke(TachyonPalette.borderSubtle, lineWidth: 1))
                        .padding(.bottom, 12)
                        .transition(.move(edge: .bottom).combined(with: .opacity))
                }
            }
        }
    }

    private var filterHeader: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(FilterKind.allCases) { f in
                    let active = filter == f
                    let count = countFor(f)
                    Button {
                        filter = f
                    } label: {
                        HStack(spacing: 5) {
                            Text(f.rawValue)
                            if count > 0 {
                                Text("\(count)")
                                    .font(.caption2.monospacedDigit().weight(.bold))
                                    .padding(.horizontal, 5)
                                    .padding(.vertical, 1)
                                    .background(active ? Color.black.opacity(0.2) : TachyonPalette.raisedPill, in: Capsule())
                            }
                        }
                        .font(.caption.weight(.semibold))
                        .padding(.horizontal, 11)
                        .padding(.vertical, 6)
                        .foregroundStyle(active ? Color.black : TachyonPalette.textPrimary)
                        .background(active ? TachyonPalette.accent : TachyonPalette.surfaceBg, in: Capsule())
                        .overlay(Capsule().stroke(active ? Color.clear : TachyonPalette.borderSubtle, lineWidth: 1))
                    }
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
        }
        .background(TachyonPalette.canvasBg)
    }

    private func countFor(_ kind: FilterKind) -> Int {
        switch kind {
        case .all: return store.agentEntries.count
        case .blocked: return store.blockedCount
        case .working: return store.agentEntries.filter { $0.state == .working }.count
        case .done: return store.agentEntries.filter { $0.state == .done || $0.state == .idle }.count
        }
    }

    private func agentCard(_ entry: AgentEntry) -> some View {
        let stateColor = TachyonPalette.color(for: entry.state)
        let isUrgent = entry.state == .blocked || entry.state == .error
        let canSend = entry.block.alive

        return VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                HostBadge(alias: entry.hostAlias, os: entry.hostOS, hostColor: entry.hostColor)

                Text(entry.appName)
                    .font(.subheadline.weight(.semibold))
                    .foregroundStyle(TachyonPalette.textPrimary)
                    .lineLimit(1)

                if entry.block.statusInferred {
                    Text("auto")
                        .font(.caption2)
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .foregroundStyle(TachyonPalette.textTertiary)
                        .background(TachyonPalette.raisedPill, in: Capsule())
                }

                Spacer()
                StatePill(state: entry.state, compact: true)
            }

            if let msg = entry.block.status?.msg, !msg.isEmpty {
                Text(msg)
                    .font(.callout)
                    .foregroundStyle(TachyonPalette.textPrimary)
                    .lineLimit(3)
            }

            HStack(spacing: 8) {
                Text(entry.sessionName)
                    .font(.caption.weight(.medium))
                    .foregroundStyle(TachyonPalette.textSecondary)
                if !entry.block.shortCwd.isEmpty {
                    Text("· \(entry.block.shortCwd)")
                        .font(.caption)
                        .foregroundStyle(TachyonPalette.textTertiary)
                        .lineLimit(1)
                }
                Spacer()
                Image(systemName: "chevron.right")
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(TachyonPalette.textTertiary)
            }

            if canSend && (entry.state == .blocked || entry.state == .error || entry.state == .working) {
                HStack(spacing: 8) {
                    quickActionButton("Approve (y)", tint: TachyonPalette.stateDone) {
                        sendQuick(entry, payload: "y\n", label: "Sent `y` to \(entry.hostAlias)")
                    }
                    quickActionButton("Deny (n)", tint: TachyonPalette.stateBlocked) {
                        sendQuick(entry, payload: "n\n", label: "Sent `n` to \(entry.hostAlias)")
                    }
                    quickActionButton("^C", tint: TachyonPalette.stateError) {
                        sendQuick(entry, payload: "\u{03}", label: "Sent Ctrl+C to \(entry.hostAlias)")
                    }
                    Spacer()
                }
            }
        }
        .padding(12)
        .background(TachyonPalette.cardBg, in: RoundedRectangle(cornerRadius: 12))
        .overlay(
            RoundedRectangle(cornerRadius: 12)
                .stroke(isUrgent ? stateColor.opacity(0.65) : TachyonPalette.borderSubtle, lineWidth: isUrgent ? 1.2 : 1)
        )
        .contentShape(Rectangle())
        .onTapGesture {
            Task {
                do {
                    try await store.attachToBlock(hostId: entry.hostId, blockId: entry.block.id)
                } catch {
                    showToast("Attach failed: \(error.localizedDescription)")
                }
            }
        }
    }

    private func quickActionButton(_ title: String, tint: Color, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(title)
                .font(.caption.weight(.semibold))
                .padding(.horizontal, 10)
                .padding(.vertical, 5)
                .foregroundStyle(tint)
                .background(tint.opacity(0.14), in: RoundedRectangle(cornerRadius: 7))
        }
        .buttonStyle(.plain)
    }

    private func sendQuick(_ entry: AgentEntry, payload: String, label: String) {
        Task {
            do {
                try await store.quickSend(hostId: entry.hostId, blockId: entry.block.id, text: payload)
                showToast(label)
            } catch {
                showToast("Send failed: \(error.localizedDescription)")
            }
        }
    }

    private func showToast(_ text: String) {
        withAnimation { feedbackMessage = text }
        Task {
            try? await Task.sleep(nanoseconds: 2_200_000_000)
            withAnimation {
                if feedbackMessage == text { feedbackMessage = nil }
            }
        }
    }

    private var emptyNoHostsView: some View {
        VStack(spacing: 14) {
            Spacer()
            Image(systemName: "qrcode.viewfinder")
                .font(.system(size: 46))
                .foregroundStyle(TachyonPalette.accent)
            Text("Pair Your Tachyon Fleet")
                .font(.title3.weight(.semibold))
                .foregroundStyle(TachyonPalette.textPrimary)
            Text("Run `tachyon pair` on your Mac, Mac mini, Linux, or Windows host over Tailscale and scan the QR code or paste the pairing link.")
                .font(.subheadline)
                .foregroundStyle(TachyonPalette.textSecondary)
                .multilineTextAlignment(.center)
                .padding(.horizontal, 28)
            Button(action: onOpenPairing) {
                Label("Pair First Host", systemImage: "plus.circle.fill")
                    .font(.subheadline.weight(.semibold))
                    .padding(.horizontal, 18)
                    .padding(.vertical, 10)
                    .foregroundStyle(.black)
                    .background(TachyonPalette.accent, in: Capsule())
            }
            Spacer()
        }
    }

    private var emptyAgentsView: some View {
        VStack(spacing: 10) {
            Spacer()
            Image(systemName: "sparkles")
                .font(.system(size: 36))
                .foregroundStyle(TachyonPalette.textTertiary)
            Text("No matching live agents")
                .font(.headline)
                .foregroundStyle(TachyonPalette.textPrimary)
            Text("Open a pane from Spaces or launch Claude Code / Codex / Antigravity in any session.")
                .font(.caption)
                .foregroundStyle(TachyonPalette.textSecondary)
                .multilineTextAlignment(.center)
                .padding(.horizontal, 32)
            Spacer()
        }
    }
}
