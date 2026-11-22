import SwiftUI
import TachyonCore
import UserNotifications

/// Host management, terminal typography, quick snippets editor, local notifications & Tailscale guide.
struct SettingsView: View {
    @Bindable var store: FleetStore
    let onOpenPairing: () -> Void

    @State private var newSnippetLabel = ""
    @State private var newSnippetCommand = ""
    @State private var newSnippetEnter = true
    @State private var addSnippetExpanded = false
    @State private var notificationStatus = "Check"

    var body: some View {
        NavigationStack {
            List {
                Section("Paired Hosts") {
                    if store.hosts.isEmpty {
                        Button("Pair First Host via QR / Link…", action: onOpenPairing)
                            .foregroundStyle(TachyonPalette.accent)
                    } else {
                        ForEach(store.hosts) { h in
                            hostRow(h)
                        }
                        .onDelete { idxSet in
                            let targets = idxSet.map { store.hosts[$0].id }
                            for id in targets {
                                Task { await store.removeHost(id: id) }
                            }
                        }
                        Button {
                            onOpenPairing()
                        } label: {
                            Label("Pair Another Host", systemImage: "plus.circle")
                                .foregroundStyle(TachyonPalette.accent)
                        }
                    }
                }
                .listRowBackground(TachyonPalette.cardBg)

                Section("Terminal Display") {
                    VStack(alignment: .leading, spacing: 6) {
                        HStack {
                            Text("Font Size")
                            Spacer()
                            Text(String(format: "%.1f pt", store.terminalFontSize))
                                .font(.caption.monospacedDigit())
                                .foregroundStyle(TachyonPalette.textSecondary)
                        }
                        Slider(
                            value: Binding(
                                get: { store.terminalFontSize },
                                set: { store.setFontSize($0) }
                            ),
                            in: 9.0...20.0,
                            step: 0.5
                        )
                        .tint(TachyonPalette.accent)
                    }
                }
                .listRowBackground(TachyonPalette.cardBg)

                Section("Agent Alerts (`agent.alert`)") {
                    Toggle("Local iOS Alerts while Connected", isOn: Binding(
                        get: { store.localAlertsEnabled },
                        set: { store.setLocalAlertsEnabled($0) }
                    ))
                    .tint(TachyonPalette.accent)

                    HStack {
                        Button("Enable Notification Banners") {
                            requestNotificationPermission()
                        }
                        .foregroundStyle(TachyonPalette.accent)
                        Spacer()
                        Text(notificationStatus)
                            .font(.caption)
                            .foregroundStyle(TachyonPalette.textTertiary)
                    }
                }
                .listRowBackground(TachyonPalette.cardBg)

                Section("Quick Snippets (Touch Bar)") {
                    ForEach(store.snippets) { sn in
                        VStack(alignment: .leading, spacing: 2) {
                            Text(sn.label)
                                .font(.subheadline.weight(.medium))
                                .foregroundStyle(TachyonPalette.textPrimary)
                            Text(sn.command + (sn.appendEnter ? " ↵" : ""))
                                .font(.caption.monospaced())
                                .foregroundStyle(TachyonPalette.textTertiary)
                        }
                    }
                    .onDelete { idxSet in
                        store.deleteSnippets(at: idxSet)
                    }

                    DisclosureGroup("Add Snippet", isExpanded: $addSnippetExpanded) {
                        TextField("Button label (e.g. Run Tests)", text: $newSnippetLabel)
                        TextField("Command (e.g. cargo test)", text: $newSnippetCommand)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                        Toggle("Press Enter after sending", isOn: $newSnippetEnter)
                        Button("Save Snippet") {
                            store.addSnippet(
                                label: newSnippetLabel,
                                command: newSnippetCommand,
                                appendEnter: newSnippetEnter
                            )
                            newSnippetLabel = ""
                            newSnippetCommand = ""
                            addSnippetExpanded = false
                        }
                        .disabled(newSnippetLabel.trimmingCharacters(in: .whitespaces).isEmpty || newSnippetCommand.isEmpty)
                    }

                    Button("Reset Default Snippets") {
                        store.resetSnippetsToDefaults()
                    }
                    .foregroundStyle(TachyonPalette.textSecondary)
                }
                .listRowBackground(TachyonPalette.cardBg)

                Section("Setup Checklist on Desktop / Servers") {
                    VStack(alignment: .leading, spacing: 6) {
                        Text("1. Enable Tailscale TCP daemon on Mac/Linux/Windows:")
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(TachyonPalette.textPrimary)
                        Text("tachyon server install-service --listen tailscale")
                            .font(.caption2.monospaced())
                            .foregroundStyle(TachyonPalette.accent)
                            .textSelection(.enabled)

                        Text("2. Generate scannable QR code on any host:")
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(TachyonPalette.textPrimary)
                            .padding(.top, 4)
                        Text("tachyon pair")
                            .font(.caption2.monospaced())
                            .foregroundStyle(TachyonPalette.accent)
                            .textSelection(.enabled)

                        Text("3. Optional away-from-desk push webhooks when closed:")
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(TachyonPalette.textPrimary)
                            .padding(.top, 4)
                        Text("tachyon server alerts --webhook https://ntfy.sh/your-topic")
                            .font(.caption2.monospaced())
                            .foregroundStyle(TachyonPalette.accent)
                            .textSelection(.enabled)
                    }
                    .padding(.vertical, 4)
                }
                .listRowBackground(TachyonPalette.cardBg)
            }
            .listStyle(.insetGrouped)
            .scrollContentBackground(.hidden)
            .background(TachyonPalette.canvasBg)
            .navigationTitle("Settings")
            .onAppear {
                refreshNotificationStatus()
            }
        }
    }

    private func hostRow(_ h: HostRuntimeState) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                HostBadge(alias: h.profile.alias, os: h.profile.os, hostColor: h.profile.resolvedColor)
                Spacer()
                Text(h.status.label)
                    .font(.caption2)
                    .foregroundStyle(h.status.isConnected ? TachyonPalette.stateDone : TachyonPalette.textSecondary)
            }
            Text(h.profile.tcpEndpoint)
                .font(.caption2.monospaced())
                .foregroundStyle(TachyonPalette.textTertiary)
            Picker("Color", selection: Binding(
                get: { h.profile.color ?? "auto" },
                set: { next in
                    store.updateHost(id: h.id, alias: h.profile.alias, color: next == "auto" ? nil : next)
                }
            )) {
                ForEach(HostColor.presets, id: \.id) { preset in
                    Text(preset.name).tag(preset.id)
                }
            }
            .pickerStyle(.menu)
            .font(.caption)
        }
        .padding(.vertical, 2)
    }

    private func refreshNotificationStatus() {
        UNUserNotificationCenter.current().getNotificationSettings { settings in
            let label = switch settings.authorizationStatus {
            case .authorized, .provisional, .ephemeral: "Allowed"
            case .denied: "Denied in iOS Settings"
            case .notDetermined: "Not requested"
            @unknown default: "Unknown"
            }
            DispatchQueue.main.async {
                notificationStatus = label
            }
        }
    }

    private func requestNotificationPermission() {
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge]) { granted, _ in
            DispatchQueue.main.async {
                notificationStatus = granted ? "Allowed" : "Denied"
            }
        }
    }
}
