import SwiftUI
import TachyonCore

/// Obsidian / Herdr dark minimal design tokens matching desktop Tachyon.
enum TachyonPalette {
    static let canvasBg = Color(red: 0.047, green: 0.051, blue: 0.059) // #0C0D0F
    static let cardBg = Color(red: 0.067, green: 0.071, blue: 0.078)   // #111214
    static let surfaceBg = Color(red: 0.090, green: 0.098, blue: 0.114) // #17191D
    static let raisedPill = Color(red: 0.173, green: 0.180, blue: 0.192) // #2C2E31
    static let borderSubtle = Color.white.opacity(0.08)
    static let accent = Color(red: 0.588, green: 0.698, blue: 0.843)   // #96B2D7
    static let textPrimary = Color(red: 0.902, green: 0.929, blue: 0.961)
    static let textSecondary = Color.white.opacity(0.62)
    static let textTertiary = Color.white.opacity(0.42)

    static let stateBlocked = Color(red: 0.961, green: 0.698, blue: 0.176)
    static let stateError = Color(red: 0.949, green: 0.290, blue: 0.290)
    static let stateWorking = Color(red: 0.588, green: 0.698, blue: 0.843)
    static let stateDone = Color(red: 0.220, green: 0.800, blue: 0.463)
    static let stateIdle = Color.white.opacity(0.45)

    static func color(for hostColor: HostColor?) -> Color {
        guard let c = hostColor else { return accent }
        return Color(
            red: Double(c.r) / 255.0,
            green: Double(c.g) / 255.0,
            blue: Double(c.b) / 255.0
        )
    }

    static func color(for state: ProgramState) -> Color {
        switch state {
        case .blocked: return stateBlocked
        case .error: return stateError
        case .working: return stateWorking
        case .done: return stateDone
        case .idle: return stateIdle
        }
    }

    static func osSymbol(for os: String) -> String {
        switch os.lowercased() {
        case "macos", "darwin": return "laptopcomputer"
        case "windows": return "pc"
        case "linux": return "server.rack"
        default: return "network"
        }
    }

    static func stateSymbol(for state: ProgramState) -> String {
        switch state {
        case .blocked: return "exclamationmark.bubble.fill"
        case .error: return "xmark.octagon.fill"
        case .working: return "sparkles"
        case .done: return "checkmark.circle.fill"
        case .idle: return "terminal"
        }
    }
}

struct StatePill: View {
    let state: ProgramState
    var compact: Bool = false

    var body: some View {
        let col = TachyonPalette.color(for: state)
        HStack(spacing: 4) {
            Circle()
                .fill(col)
                .frame(width: 6, height: 6)
            Text(state.label)
                .font(.caption2.weight(.semibold))
        }
        .padding(.horizontal, compact ? 7 : 9)
        .padding(.vertical, compact ? 3 : 4)
        .foregroundStyle(col)
        .background(col.opacity(0.14), in: Capsule())
        .overlay(Capsule().stroke(col.opacity(0.35), lineWidth: 1))
    }
}

struct HostBadge: View {
    let alias: String
    let os: String
    let hostColor: HostColor?

    var body: some View {
        let col = TachyonPalette.color(for: hostColor)
        let danger = hostColor?.danger ?? false
        HStack(spacing: 5) {
            Image(systemName: danger ? "exclamationmark.triangle.fill" : TachyonPalette.osSymbol(for: os))
                .font(.caption2.weight(.semibold))
            Text(alias)
                .font(.caption.weight(.semibold))
                .lineLimit(1)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 3)
        .foregroundStyle(col)
        .background(col.opacity(danger ? 0.18 : 0.12), in: Capsule())
        .overlay(Capsule().stroke(col.opacity(0.38), lineWidth: 1))
    }
}
