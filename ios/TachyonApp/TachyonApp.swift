import SwiftUI
import TachyonCore
@preconcurrency import UserNotifications

@main
struct TachyonApp: App {
    @State private var store: FleetStore
    private let notifDelegate = NotificationDelegate()

    init() {
        let s = FleetStore()
        _store = State(initialValue: s)

        let center = UNUserNotificationCenter.current()
        center.delegate = notifDelegate
        notifDelegate.onTapJump = { [weak s] target in
            Task { @MainActor in
                guard let s else { return }
                try? await s.attachToBlock(hostId: target.hostId, blockId: target.blockId)
            }
        }
        s.onAgentAlert = { profile, alert in
            let content = UNMutableNotificationContent()
            content.title = alert.title ?? "Tachyon · \(alert.state) on \(profile.alias)"
            content.body = alert.body ?? alert.message ?? alert.label
            content.sound = .default
            content.userInfo = [
                "host_id": profile.id.uuidString,
                "block_id": alert.blockId,
            ]
            let req = UNNotificationRequest(
                identifier: "tachyon-\(profile.id.uuidString)-\(alert.blockId)-\(alert.timestamp)",
                content: content,
                trigger: nil
            )
            UNUserNotificationCenter.current().add(req)
        }

        configureDarkAppearance()
    }

    var body: some Scene {
        WindowGroup {
            ContentView(store: store)
                .preferredColorScheme(.dark)
                .task {
                    store.connectAll()
                }
                .onOpenURL { url in
                    Task {
                        do {
                            let imported = try await store.importPairingURI(url.absoluteString)
                            store.lastNotice = "Paired `\(imported.alias)`"
                            store.selectedTab = 1
                        } catch {
                            store.lastNotice = "Pairing failed: \(error.localizedDescription)"
                        }
                    }
                }
        }
    }

    private func configureDarkAppearance() {
        let navAppearance = UINavigationBarAppearance()
        navAppearance.configureWithOpaqueBackground()
        navAppearance.backgroundColor = UIColor(red: 0.047, green: 0.051, blue: 0.059, alpha: 1.0)
        navAppearance.titleTextAttributes = [.foregroundColor: UIColor.white]
        navAppearance.largeTitleTextAttributes = [.foregroundColor: UIColor.white]
        UINavigationBar.appearance().standardAppearance = navAppearance
        UINavigationBar.appearance().scrollEdgeAppearance = navAppearance

        let tabAppearance = UITabBarAppearance()
        tabAppearance.configureWithOpaqueBackground()
        tabAppearance.backgroundColor = UIColor(red: 0.067, green: 0.071, blue: 0.078, alpha: 1.0)
        UITabBar.appearance().standardAppearance = tabAppearance
        UITabBar.appearance().scrollEdgeAppearance = tabAppearance
    }
}

private final class NotificationDelegate: NSObject, UNUserNotificationCenterDelegate, @unchecked Sendable {
    var onTapJump: (@Sendable (JumpTarget) -> Void)?

    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        [.banner, .sound, .list]
    }

    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse
    ) async {
        let info = response.notification.request.content.userInfo
        guard let hostStr = info["host_id"] as? String,
              let hostId = UUID(uuidString: hostStr),
              let blockId = info["block_id"] as? String
        else { return }
        onTapJump?(JumpTarget(hostId: hostId, blockId: blockId))
    }
}
