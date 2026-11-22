import Foundation
import Security

/// Storage abstraction for sensitive host bearer tokens (Keychain on iOS/macOS, in-memory for tests).
public protocol TokenStore: Sendable {
    func saveToken(_ token: String, forKey key: String) throws
    func loadToken(forKey key: String) -> String?
    func deleteToken(forKey key: String)
}

/// Keychain-backed bearer token store (`kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`).
public struct KeychainTokenStore: TokenStore {
    public let service: String

    public init(service: String = "dev.tachyon.ios.host-tokens") {
        self.service = service
    }

    public func saveToken(_ token: String, forKey key: String) throws {
        guard let data = token.data(using: .utf8) else {
            throw MuxError.invalidPayload("token is not UTF-8")
        }
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: key,
        ]
        let attributes: [String: Any] = [
            kSecValueData as String: data,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
        ]
        let status = SecItemUpdate(query as CFDictionary, attributes as CFDictionary)
        if status == errSecItemNotFound {
            var addQuery = query
            addQuery[kSecValueData as String] = data
            addQuery[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
            let addStatus = SecItemAdd(addQuery as CFDictionary, nil)
            guard addStatus == errSecSuccess else {
                throw MuxError.connectionFailed("Keychain save failed (OSStatus \(addStatus))")
            }
        } else if status != errSecSuccess {
            throw MuxError.connectionFailed("Keychain update failed (OSStatus \(status))")
        }
    }

    public func loadToken(forKey key: String) -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: key,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: AnyObject?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        guard status == errSecSuccess, let data = result as? Data else {
            return nil
        }
        return String(data: data, encoding: .utf8)
    }

    public func deleteToken(forKey key: String) {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: key,
        ]
        SecItemDelete(query as CFDictionary)
    }
}

/// Thread-safe in-memory token store used in unit and integration tests.
public final class InMemoryTokenStore: TokenStore, @unchecked Sendable {
    private let lock = NSLock()
    private var storage: [String: String] = [:]

    public init() {}

    public func saveToken(_ token: String, forKey key: String) throws {
        lock.lock()
        defer { lock.unlock() }
        storage[key] = token
    }

    public func loadToken(forKey key: String) -> String? {
        lock.lock()
        defer { lock.unlock() }
        return storage[key]
    }

    public func deleteToken(forKey key: String) {
        lock.lock()
        defer { lock.unlock() }
        storage.removeValue(forKey: key)
    }
}

/// Non-secret user preferences and paired host metadata store backed by `UserDefaults`.
public struct HostProfileStore: Sendable {
    private let suiteName: String?

    private enum Keys {
        static let hosts = "tachyon.ios.hosts.v1"
        static let snippets = "tachyon.ios.snippets.v1"
        static let fontSize = "tachyon.ios.terminalFontSize.v1"
        static let localAlerts = "tachyon.ios.localAlertsEnabled.v1"
    }

    public init(suiteName: String? = nil) {
        self.suiteName = suiteName
    }

    private var defaults: UserDefaults {
        if let suiteName, let ud = UserDefaults(suiteName: suiteName) {
            return ud
        }
        return .standard
    }

    public func loadHosts() -> [HostProfile] {
        guard let data = defaults.data(forKey: Keys.hosts),
              let list = try? JSONDecoder().decode([HostProfile].self, from: data)
        else {
            return []
        }
        return list
    }

    public func saveHosts(_ hosts: [HostProfile]) {
        if let data = try? JSONEncoder().encode(hosts) {
            defaults.set(data, forKey: Keys.hosts)
        }
    }

    public func loadSnippets() -> [QuickSnippet] {
        guard let data = defaults.data(forKey: Keys.snippets),
              let list = try? JSONDecoder().decode([QuickSnippet].self, from: data),
              !list.isEmpty
        else {
            return QuickSnippet.defaults
        }
        return list
    }

    public func saveSnippets(_ snippets: [QuickSnippet]) {
        if let data = try? JSONEncoder().encode(snippets) {
            defaults.set(data, forKey: Keys.snippets)
        }
    }

    public func loadFontSize() -> Double {
        let v = defaults.double(forKey: Keys.fontSize)
        return v > 0 ? min(max(v, 9.0), 22.0) : 12.5
    }

    public func saveFontSize(_ size: Double) {
        defaults.set(min(max(size, 9.0), 22.0), forKey: Keys.fontSize)
    }

    public func loadLocalAlertsEnabled() -> Bool {
        if defaults.object(forKey: Keys.localAlerts) == nil {
            return true
        }
        return defaults.bool(forKey: Keys.localAlerts)
    }

    public func saveLocalAlertsEnabled(_ enabled: Bool) {
        defaults.set(enabled, forKey: Keys.localAlerts)
    }
}
