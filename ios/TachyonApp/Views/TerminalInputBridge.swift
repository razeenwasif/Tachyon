import SwiftUI
import TachyonCore
import UIKit

/// Invisible UIKit `UIKeyInput` bridge powering soft keyboard input, iOS Dictation, and hardware keyboards.
struct TerminalInputBridge: UIViewRepresentable {
    let focusTrigger: Int
    let resignTrigger: Int
    let onText: (String) -> Void
    let onSpecialKey: (SpecialKey) -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator()
    }

    func makeUIView(context: Context) -> TerminalKeyInputView {
        let v = TerminalKeyInputView()
        v.onText = onText
        v.onSpecialKey = onSpecialKey
        return v
    }

    func updateUIView(_ uiView: TerminalKeyInputView, context: Context) {
        uiView.onText = onText
        uiView.onSpecialKey = onSpecialKey
        if context.coordinator.lastFocusTrigger != focusTrigger {
            context.coordinator.lastFocusTrigger = focusTrigger
            DispatchQueue.main.async {
                uiView.becomeFirstResponder()
            }
        }
        if context.coordinator.lastResignTrigger != resignTrigger {
            context.coordinator.lastResignTrigger = resignTrigger
            DispatchQueue.main.async {
                uiView.resignFirstResponder()
            }
        }
    }

    final class Coordinator {
        var lastFocusTrigger: Int = 0
        var lastResignTrigger: Int = 0
    }
}

final class TerminalKeyInputView: UIView, UIKeyInput {
    var onText: ((String) -> Void)?
    var onSpecialKey: ((SpecialKey) -> Void)?

    override var canBecomeFirstResponder: Bool { true }
    var hasText: Bool { true }

    var autocorrectionType: UITextAutocorrectionType = .no
    var autocapitalizationType: UITextAutocapitalizationType = .none
    var spellCheckingType: UITextSpellCheckingType = .no
    var smartQuotesType: UITextSmartQuotesType = .no
    var smartDashesType: UITextSmartDashesType = .no
    var smartInsertDeleteType: UITextSmartInsertDeleteType = .no
    var keyboardAppearance: UIKeyboardAppearance = .dark
    var keyboardType: UIKeyboardType = .asciiCapable
    var returnKeyType: UIReturnKeyType = .default

    func insertText(_ text: String) {
        if text == "\n" {
            onSpecialKey?(.enter)
        } else {
            onText?(text)
        }
    }

    func deleteBackward() {
        onSpecialKey?(.backspace)
    }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        var handled = false
        for press in presses {
            guard let key = press.key else { continue }
            switch key.keyCode {
            case .keyboardEscape:
                onSpecialKey?(.escape)
                handled = true
            case .keyboardTab:
                onSpecialKey?(key.modifierFlags.contains(.shift) ? .backTab : .tab)
                handled = true
            case .keyboardUpArrow:
                onSpecialKey?(.up)
                handled = true
            case .keyboardDownArrow:
                onSpecialKey?(.down)
                handled = true
            case .keyboardLeftArrow:
                onSpecialKey?(.left)
                handled = true
            case .keyboardRightArrow:
                onSpecialKey?(.right)
                handled = true
            case .keyboardHome:
                onSpecialKey?(.home)
                handled = true
            case .keyboardEnd:
                onSpecialKey?(.end)
                handled = true
            case .keyboardPageUp:
                onSpecialKey?(.pageUp)
                handled = true
            case .keyboardPageDown:
                onSpecialKey?(.pageDown)
                handled = true
            case .keyboardDeleteForward:
                onSpecialKey?(.delete)
                handled = true
            default:
                if key.modifierFlags.contains(.control),
                   let scalar = key.charactersIgnoringModifiers.unicodeScalars.first {
                    let v = scalar.value
                    if (97...122).contains(v) {
                        onText?(String(UnicodeScalar(UInt8(v - 96))))
                        handled = true
                    } else if (64...95).contains(v) {
                        onText?(String(UnicodeScalar(UInt8(v - 64))))
                        handled = true
                    }
                }
            }
        }
        if !handled {
            super.pressesBegan(presses, with: event)
        }
    }
}
