import SwiftUI
import TachyonCore

/// Live interactive Mux v2 terminal viewer + Touch Accessory Bar + split/agent navigation.
struct TerminalSessionView: View {
    @Bindable var store: FleetStore
    let onBrowseSpaces: () -> Void

    @State private var ctrlArmed = false
    @State private var focusTrigger = 0
    @State private var resignTrigger = 0
    @State private var captureSheetText: String?
    @State private var actionError: String?

    var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                if let att = store.activeAttachment {
                    headerStrip(attachment: att)

                    ZStack(alignment: .bottomTrailing) {
                        TerminalInputBridge(
                            focusTrigger: focusTrigger,
                            resignTrigger: resignTrigger,
                            onText: { text in
                                let armed = ctrlArmed
                                if armed { ctrlArmed = false }
                                store.sendActiveInput(KeyEncoder.encodeText(text, ctrlArmed: armed))
                            },
                            onSpecialKey: { key in
                                store.sendSpecialKey(key)
                            }
                        )
                        .frame(width: 1, height: 1)
                        .opacity(0.01)

                        TerminalCanvasView(
                            snapshot: att.renderSnapshot,
                            fontSize: store.terminalFontSize,
                            hostAccent: TachyonPalette.color(for: att.hostColor),
                            onResize: { cols, rows, cellW, cellH in
                                store.resizeActive(cols: cols, rows: rows, cellW: cellW, cellH: cellH)
                            },
                            onScroll: { delta in
                                store.scrollActive(deltaLines: delta)
                            },
                            onFontScale: { nextSize in
                                store.setFontSize(nextSize)
                            },
                            onTapFocus: {
                                focusTrigger &+= 1
                            }
                        )

                        if att.scrollOffset > 0 {
                            Button {
                                store.scrollToBottom()
                            } label: {
                                Label("Jump to Live", systemImage: "arrow.down.to.line")
                                    .font(.caption.weight(.semibold))
                                    .padding(.horizontal, 12)
                                    .padding(.vertical, 7)
                                    .foregroundStyle(.black)
                                    .background(TachyonPalette.accent, in: Capsule())
                                    .shadow(radius: 4)
                            }
                            .padding(12)
                        }
                    }

                    TouchAccessoryBar(
                        store: store,
                        ctrlArmed: $ctrlArmed,
                        onFocusKeyboard: { focusTrigger &+= 1 },
                        onDismissKeyboard: { resignTrigger &+= 1 }
                    )
                } else {
                    emptyTerminalView
                }
            }
            .background(TachyonPalette.cardBg)
            .navigationBarHidden(true)
            .sheet(item: Binding(
                get: { captureSheetText.map { CapturedTextWrapper(text: $0) } },
                set: { if $0 == nil { captureSheetText = nil } }
            )) { item in
                NavigationStack {
                    ScrollView {
                        Text(item.text)
                            .font(.system(size: 12, design: .monospaced))
                            .foregroundStyle(TachyonPalette.textPrimary)
                            .textSelection(.enabled)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding()
                    }
                    .background(TachyonPalette.canvasBg)
                    .navigationTitle("Captured Scrollback")
                    .navigationBarTitleDisplayMode(.inline)
                    .toolbar {
                        ToolbarItem(placement: .confirmationAction) {
                            Button("Done") { captureSheetText = nil }
                        }
                    }
                }
            }
            .alert("Terminal Action Failed", isPresented: Binding(
                get: { actionError != nil },
                set: { if !$0 { actionError = nil } }
            )) {
                Button("OK", role: .cancel) {}
            } message: {
                Text(actionError ?? "")
            }
        }
    }

    private func headerStrip(attachment att: ActiveTerminalAttachment) -> some View {
        let hostAccent = TachyonPalette.color(for: att.hostColor)
        let liveBlock = store.liveBlock(hostId: att.hostId, blockId: att.blockId)
        let title = !att.renderSnapshot.title.isEmpty
            ? att.renderSnapshot.title
            : (liveBlock?.displayName ?? "Terminal")
        let effectiveStatus = att.renderSnapshot.agentStatus ?? liveBlock?.status
        let nextBlocked = store.nextBlockedAgent(excludingBlockId: att.blockId)

        return VStack(spacing: 0) {
            Rectangle().fill(hostAccent.opacity(0.85)).frame(height: 2)
            HStack(spacing: 8) {
                HostBadge(alias: att.hostAlias, os: att.hostOS, hostColor: att.hostColor)

                Text(title)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(TachyonPalette.textPrimary)
                    .lineLimit(1)

                if let st = effectiveStatus {
                    StatePill(state: st.state, compact: true)
                }

                Spacer()

                if let nextBlocked {
                    Button {
                        Task {
                            try? await store.attachToBlock(hostId: nextBlocked.hostId, blockId: nextBlocked.block.id)
                        }
                    } label: {
                        HStack(spacing: 4) {
                            Image(systemName: "exclamationmark.triangle.fill")
                            Text("Blocked")
                        }
                        .font(.caption2.weight(.bold))
                        .padding(.horizontal, 8)
                        .padding(.vertical, 4)
                        .foregroundStyle(.black)
                        .background(TachyonPalette.stateBlocked, in: Capsule())
                    }
                }

                Menu {
                    Button {
                        focusTrigger &+= 1
                    } label: {
                        Label("Show Keyboard", systemImage: "keyboard")
                    }
                    Button {
                        captureSheetText = att.buffer.plainText(lastLines: 400)
                    } label: {
                        Label("Copy / Select Output", systemImage: "doc.on.doc")
                    }
                    Divider()
                    Button {
                        Task {
                            do {
                                try await store.splitActivePane(dir: .right)
                            } catch {
                                actionError = error.localizedDescription
                            }
                        }
                    } label: {
                        Label("Split Right", systemImage: "rectangle.split.2x1")
                    }
                    Button {
                        Task {
                            do {
                                try await store.splitActivePane(dir: .down)
                            } catch {
                                actionError = error.localizedDescription
                            }
                        }
                    } label: {
                        Label("Split Down", systemImage: "rectangle.split.1x2")
                    }
                    Divider()
                    Button {
                        Task { await store.detachActive() }
                    } label: {
                        Label("Detach Stream", systemImage: "eject")
                    }
                    Button(role: .destructive) {
                        Task {
                            try? await store.killBlock(hostId: att.hostId, blockId: att.blockId)
                        }
                    } label: {
                        Label("Kill Pane", systemImage: "xmark.bin")
                    }
                } label: {
                    Image(systemName: "ellipsis.circle")
                        .font(.body)
                        .foregroundStyle(TachyonPalette.accent)
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            .background(TachyonPalette.surfaceBg)
            Rectangle().fill(TachyonPalette.borderSubtle).frame(height: 1)
        }
    }

    private var emptyTerminalView: some View {
        VStack(spacing: 14) {
            Spacer()
            Image(systemName: "terminal.fill")
                .font(.system(size: 42))
                .foregroundStyle(TachyonPalette.textTertiary)
            Text("No Active Pane Attached")
                .font(.title3.weight(.semibold))
                .foregroundStyle(TachyonPalette.textPrimary)
            Text("Pick a running agent from Live Agents or select any session in Spaces to stream live Mux v2 output.")
                .font(.subheadline)
                .foregroundStyle(TachyonPalette.textSecondary)
                .multilineTextAlignment(.center)
                .padding(.horizontal, 32)

            if let urgent = store.nextBlockedAgent() {
                Button {
                    Task {
                        try? await store.attachToBlock(hostId: urgent.hostId, blockId: urgent.block.id)
                    }
                } label: {
                    Label("Jump to Blocked: \(urgent.appName) (\(urgent.hostAlias))", systemImage: "bolt.fill")
                        .font(.subheadline.weight(.semibold))
                        .padding(.horizontal, 16)
                        .padding(.vertical, 10)
                        .foregroundStyle(.black)
                        .background(TachyonPalette.stateBlocked, in: Capsule())
                }
            } else {
                Button("Browse Spaces", action: onBrowseSpaces)
                    .buttonStyle(.borderedProminent)
                    .tint(TachyonPalette.accent)
                    .foregroundStyle(.black)
            }
            Spacer()
        }
        .frame(maxWidth: .infinity)
        .background(TachyonPalette.canvasBg)
    }
}

private struct CapturedTextWrapper: Identifiable {
    let id = UUID()
    let text: String
}
