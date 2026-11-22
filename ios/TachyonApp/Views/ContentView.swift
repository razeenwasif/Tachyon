import SwiftUI
import TachyonCore

struct ContentView: View {
    @Bindable var store: FleetStore
    @State private var showPairingSheet = false

    var body: some View {
        TabView(selection: $store.selectedTab) {
            AgentsInboxView(store: store) {
                showPairingSheet = true
            }
            .tabItem {
                Label("Agents", systemImage: "sparkles")
            }
            .badge(store.blockedCount)
            .tag(0)

            SpacesHostsView(store: store) {
                showPairingSheet = true
            }
            .tabItem {
                Label("Spaces", systemImage: "square.stack.3d.up")
            }
            .tag(1)

            TerminalSessionView(store: store) {
                store.selectedTab = 1
            }
            .tabItem {
                Label("Terminal", systemImage: "terminal")
            }
            .tag(2)

            SettingsView(store: store) {
                showPairingSheet = true
            }
            .tabItem {
                Label("Settings", systemImage: "gearshape")
            }
            .tag(3)
        }
        .tint(TachyonPalette.accent)
        .sheet(isPresented: $showPairingSheet) {
            PairingView(store: store)
        }
        .alert("Pairing Link Error", isPresented: Binding(
            get: { store.lastNotice != nil && (store.lastNotice?.hasPrefix("Pairing failed:") ?? false) },
            set: { if !$0 { store.lastNotice = nil } }
        )) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(store.lastNotice ?? "")
        }
    }
}
