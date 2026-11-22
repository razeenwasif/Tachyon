import Foundation

/// Special non-printable keys supported by the iOS accessory bar and hardware keyboard bridge.
public enum SpecialKey: String, Sendable, CaseIterable {
    case escape
    case tab
    case backTab
    case enter
    case backspace
    case delete
    case up
    case down
    case left
    case right
    case home
    case end
    case pageUp
    case pageDown
    case ctrlC
    case ctrlD
    case ctrlZ
    case ctrlL

    public var label: String {
        switch self {
        case .escape: return "Esc"
        case .tab: return "Tab"
        case .backTab: return "⇧Tab"
        case .enter: return "↵"
        case .backspace: return "⌫"
        case .delete: return "Del"
        case .up: return "↑"
        case .down: return "↓"
        case .left: return "←"
        case .right: return "→"
        case .home: return "Home"
        case .end: return "End"
        case .pageUp: return "PgUp"
        case .pageDown: return "PgDn"
        case .ctrlC: return "^C"
        case .ctrlD: return "^D"
        case .ctrlZ: return "^Z"
        case .ctrlL: return "^L"
        }
    }
}

/// Reusable command or reply snippet surfaced in the iOS touch accessory bar.
public struct QuickSnippet: Sendable, Codable, Identifiable, Equatable {
    public let id: UUID
    public var label: String
    public var command: String
    public var appendEnter: Bool

    public init(
        id: UUID = UUID(),
        label: String,
        command: String,
        appendEnter: Bool = true
    ) {
        self.id = id
        self.label = label
        self.command = command
        self.appendEnter = appendEnter
    }

    public static let defaults: [QuickSnippet] = [
        QuickSnippet(label: "Approve (y)", command: "y", appendEnter: true),
        QuickSnippet(label: "Yes", command: "yes", appendEnter: true),
        QuickSnippet(label: "Deny (n)", command: "n", appendEnter: true),
        QuickSnippet(label: "Continue", command: "continue", appendEnter: true),
        QuickSnippet(label: "/compact", command: "/compact", appendEnter: true),
        QuickSnippet(label: "git status", command: "git status -sb", appendEnter: true),
        QuickSnippet(label: "git diff", command: "git diff --stat", appendEnter: true),
    ]
}

/// Maps touch/keyboard actions into VT/ANSI byte sequences for remote PTYs.
public enum KeyEncoder {
    public static func encodeSpecialKey(_ key: SpecialKey, applicationCursor: Bool) -> Data {
        switch key {
        case .escape: return Data([0x1B])
        case .tab: return Data([0x09])
        case .backTab: return Data([0x1B, 0x5B, 0x5A]) // ESC [ Z
        case .enter: return Data([0x0D])
        case .backspace: return Data([0x7F])
        case .delete: return Data("\u{1B}[3~".utf8)
        case .up: return Data((applicationCursor ? "\u{1B}OA" : "\u{1B}[A").utf8)
        case .down: return Data((applicationCursor ? "\u{1B}OB" : "\u{1B}[B").utf8)
        case .right: return Data((applicationCursor ? "\u{1B}OC" : "\u{1B}[C").utf8)
        case .left: return Data((applicationCursor ? "\u{1B}OD" : "\u{1B}[D").utf8)
        case .home: return Data((applicationCursor ? "\u{1B}OH" : "\u{1B}[H").utf8)
        case .end: return Data((applicationCursor ? "\u{1B}OF" : "\u{1B}[F").utf8)
        case .pageUp: return Data("\u{1B}[5~".utf8)
        case .pageDown: return Data("\u{1B}[6~".utf8)
        case .ctrlC: return Data([0x03])
        case .ctrlD: return Data([0x04])
        case .ctrlZ: return Data([0x1A])
        case .ctrlL: return Data([0x0C])
        }
    }

    /// Encode text from `UIKeyInput` / software keyboard, honoring sticky `Ctrl`.
    public static func encodeText(_ text: String, ctrlArmed: Bool) -> Data {
        if ctrlArmed, let scalar = text.unicodeScalars.first, text.count == 1 {
            if let ctrlByte = controlByte(for: scalar) {
                return Data([ctrlByte])
            }
        }
        // Normalize soft-keyboard newline to carriage return for interactive PTYs.
        let normalized = text.replacingOccurrences(of: "\n", with: "\r")
        return Data(normalized.utf8)
    }

    /// Encode clipboard or dictation text with optional VT bracketed-paste framing (`?2004h`).
    public static func encodePaste(_ text: String, bracketedPaste: Bool) -> Data {
        let cleaned = text.replacingOccurrences(of: "\r\n", with: "\r")
            .replacingOccurrences(of: "\n", with: "\r")
        if !bracketedPaste {
            return Data(cleaned.utf8)
        }
        var out = Data("\u{1B}[200~".utf8)
        out.append(contentsOf: cleaned.utf8)
        out.append(contentsOf: "\u{1B}[201~".utf8)
        return out
    }

    /// Encode a `QuickSnippet` payload.
    public static func encodeSnippet(_ snippet: QuickSnippet, bracketedPaste: Bool = false) -> Data {
        var data = snippet.command.contains("\n") && bracketedPaste
            ? encodePaste(snippet.command, bracketedPaste: true)
            : Data(snippet.command.replacingOccurrences(of: "\n", with: "\r").utf8)
        if snippet.appendEnter {
            data.append(0x0D)
        }
        return data
    }

    /// Transient command spec that decodes stdin base64 into a temp PNG file on macOS, Linux, or Windows hosts.
    public static func screenshotUploadSpec(
        os: String,
        filename: String
    ) -> (command: [String], remotePath: String) {
        let safeName = filename
            .replacingOccurrences(of: "/", with: "-")
            .replacingOccurrences(of: "\\", with: "-")
            .replacingOccurrences(of: "..", with: "-")
        if os.lowercased() == "windows" {
            let remotePath = "%TEMP%\\\(safeName)"
            let ps = """
            $b = [Console]::In.ReadToEnd().Trim();
            [IO.File]::WriteAllBytes((Join-Path $env:TEMP '\(safeName)'), [Convert]::FromBase64String($b))
            """
            return (["powershell", "-NoProfile", "-Command", ps], remotePath)
        }
        let remotePath = "/tmp/\(safeName)"
        // `base64 -d` works on GNU coreutils (Linux) and modern macOS (`-D` fallback on older macOS).
        let sh = "(base64 -d 2>/dev/null || base64 -D) > \"$1\""
        return (["/bin/sh", "-c", sh, "_", remotePath], remotePath)
    }

    private static func controlByte(for scalar: UnicodeScalar) -> UInt8? {
        let v = scalar.value
        switch v {
        case 64...95: // '@' .. '_' (includes uppercase A..Z)
            return UInt8(v - 64)
        case 97...122: // 'a' .. 'z'
            return UInt8(v - 96)
        case 32: // Space -> NUL
            return 0x00
        case 63: // '?' -> DEL
            return 0x7F
        default:
            return nil
        }
    }
}
