import PhotosUI
import SwiftUI
import TachyonCore
import UIKit

/// Touch accessory bar giving one-tap Ctrl, Esc, Tab, arrows, snippets, dictation, and screenshot paste.
struct TouchAccessoryBar: View {
    @Bindable var store: FleetStore
    @Binding var ctrlArmed: Bool
    let onFocusKeyboard: () -> Void
    let onDismissKeyboard: () -> Void

    @State private var showComposeSheet = false
    @State private var showMediaSheet = false

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 6) {
                Button {
                    ctrlArmed.toggle()
                    onFocusKeyboard()
                } label: {
                    Text("Ctrl")
                        .font(.caption.weight(.semibold))
                        .padding(.horizontal, 10)
                        .padding(.vertical, 6)
                        .foregroundStyle(ctrlArmed ? .black : TachyonPalette.textPrimary)
                        .background(ctrlArmed ? TachyonPalette.accent : TachyonPalette.raisedPill, in: RoundedRectangle(cornerRadius: 7))
                }

                keyButton("Esc", .escape)
                keyButton("Tab", .tab)
                keyButton("⇧Tab", .backTab)

                Divider().frame(height: 18).background(TachyonPalette.borderSubtle)

                keyButton("←", .left)
                keyButton("↓", .down)
                keyButton("↑", .up)
                keyButton("→", .right)

                Divider().frame(height: 18).background(TachyonPalette.borderSubtle)

                keyButton("^C", .ctrlC, tint: TachyonPalette.stateError)
                keyButton("^D", .ctrlD)
                keyButton("^Z", .ctrlZ)
                keyButton("^L", .ctrlL)

                Menu {
                    ForEach(store.snippets) { snippet in
                        Button {
                            store.sendSnippetToActive(snippet)
                        } label: {
                            Label(snippet.label, systemImage: "text.insert")
                        }
                    }
                } label: {
                    HStack(spacing: 4) {
                        Image(systemName: "bolt.fill")
                        Text("Snippets")
                    }
                    .font(.caption.weight(.semibold))
                    .padding(.horizontal, 10)
                    .padding(.vertical, 6)
                    .foregroundStyle(TachyonPalette.accent)
                    .background(TachyonPalette.accent.opacity(0.14), in: RoundedRectangle(cornerRadius: 7))
                }

                Button {
                    showComposeSheet = true
                } label: {
                    Image(systemName: "mic.and.signal.meter")
                        .font(.caption.weight(.semibold))
                        .padding(.horizontal, 10)
                        .padding(.vertical, 6)
                        .foregroundStyle(TachyonPalette.textPrimary)
                        .background(TachyonPalette.raisedPill, in: RoundedRectangle(cornerRadius: 7))
                }
                .accessibilityLabel("Compose or Dictate Prompt")

                Button {
                    showMediaSheet = true
                } label: {
                    Image(systemName: "photo.on.rectangle.angled")
                        .font(.caption.weight(.semibold))
                        .padding(.horizontal, 10)
                        .padding(.vertical, 6)
                        .foregroundStyle(TachyonPalette.textPrimary)
                        .background(TachyonPalette.raisedPill, in: RoundedRectangle(cornerRadius: 7))
                }
                .accessibilityLabel("Paste Clipboard or Upload Screenshot")

                Button {
                    onDismissKeyboard()
                } label: {
                    Image(systemName: "keyboard.chevron.compact.down")
                        .font(.caption.weight(.semibold))
                        .padding(.horizontal, 9)
                        .padding(.vertical, 6)
                        .foregroundStyle(TachyonPalette.textSecondary)
                        .background(TachyonPalette.raisedPill, in: RoundedRectangle(cornerRadius: 7))
                }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
        }
        .background(TachyonPalette.surfaceBg)
        .overlay(alignment: .top) {
            Rectangle().fill(TachyonPalette.borderSubtle).frame(height: 1)
        }
        .sheet(isPresented: $showComposeSheet) {
            ComposePromptSheet(store: store)
                .presentationDetents([.medium])
        }
        .sheet(isPresented: $showMediaSheet) {
            MediaPasteSheet(store: store)
                .presentationDetents([.medium])
        }
    }

    private func keyButton(_ label: String, _ key: SpecialKey, tint: Color? = nil) -> some View {
        Button {
            store.sendSpecialKey(key)
        } label: {
            Text(label)
                .font(.caption.monospaced().weight(.semibold))
                .padding(.horizontal, 9)
                .padding(.vertical, 6)
                .foregroundStyle(tint ?? TachyonPalette.textPrimary)
                .background(TachyonPalette.raisedPill, in: RoundedRectangle(cornerRadius: 7))
        }
    }
}

private struct ComposePromptSheet: View {
    @Bindable var store: FleetStore
    @Environment(\.dismiss) private var dismiss
    @State private var draft = ""
    @State private var sendEnter = true
    @FocusState private var editorFocused: Bool

    var body: some View {
        NavigationStack {
            VStack(alignment: .leading, spacing: 12) {
                Text("Use the iOS keyboard microphone to dictate multi-line prompts or code review replies, then send cleanly via bracketed paste.")
                    .font(.caption)
                    .foregroundStyle(TachyonPalette.textSecondary)

                TextEditor(text: $draft)
                    .font(.system(size: 14, design: .monospaced))
                    .scrollContentBackground(.hidden)
                    .padding(10)
                    .background(TachyonPalette.cardBg, in: RoundedRectangle(cornerRadius: 10))
                    .overlay(RoundedRectangle(cornerRadius: 10).stroke(TachyonPalette.borderSubtle, lineWidth: 1))
                    .focused($editorFocused)

                Toggle("Press Enter after sending", isOn: $sendEnter)
                    .font(.subheadline)
                    .tint(TachyonPalette.accent)
            }
            .padding()
            .background(TachyonPalette.canvasBg)
            .navigationTitle("Dictate / Compose")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Send") {
                        store.pasteTextToActive(draft)
                        if sendEnter {
                            store.sendSpecialKey(.enter)
                        }
                        dismiss()
                    }
                    .disabled(draft.isEmpty)
                }
            }
            .onAppear { editorFocused = true }
        }
    }
}

private struct MediaPasteSheet: View {
    @Bindable var store: FleetStore
    @Environment(\.dismiss) private var dismiss
    @State private var selectedItem: PhotosPickerItem?
    @State private var uploading = false
    @State private var statusMessage: String?

    var body: some View {
        NavigationStack {
            List {
                Section("Clipboard") {
                    Button {
                        if let text = UIPasteboard.general.string, !text.isEmpty {
                            store.pasteTextToActive(text)
                            dismiss()
                        } else {
                            statusMessage = "Clipboard does not contain text."
                        }
                    } label: {
                        Label("Paste Clipboard Text (Bracketed)", systemImage: "doc.on.clipboard")
                    }

                    if let img = UIPasteboard.general.image, let png = img.pngData() {
                        Button {
                            uploadPNG(png)
                        } label: {
                            Label("Upload Clipboard Image to Host & Insert Path", systemImage: "photo.badge.arrow.down")
                        }
                        .disabled(uploading)
                    }
                }

                Section("Photos Library") {
                    PhotosPicker(selection: $selectedItem, matching: .images) {
                        Label("Choose Screenshot from Photos…", systemImage: "photo.on.rectangle")
                    }
                    .disabled(uploading)
                }

                if uploading {
                    Section {
                        HStack(spacing: 10) {
                            ProgressView()
                            Text("Uploading PNG over Mux v2 to remote host…")
                                .font(.caption)
                                .foregroundStyle(TachyonPalette.textSecondary)
                        }
                    }
                }

                if let statusMessage {
                    Section {
                        Text(statusMessage)
                            .font(.caption)
                            .foregroundStyle(TachyonPalette.accent)
                    }
                }
            }
            .scrollContentBackground(.hidden)
            .background(TachyonPalette.canvasBg)
            .navigationTitle("Paste & Screenshots")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Close") { dismiss() }
                }
            }
            .onChange(of: selectedItem) { _, item in
                guard let item else { return }
                Task {
                    uploading = true
                    defer { uploading = false }
                    if let rawData = try? await item.loadTransferable(type: Data.self),
                       let uiImg = UIImage(data: rawData),
                       let png = uiImg.pngData() {
                        await performUpload(png)
                    } else {
                        statusMessage = "Could not load selected image as PNG."
                    }
                }
            }
        }
    }

    private func uploadPNG(_ png: Data) {
        Task {
            uploading = true
            defer { uploading = false }
            await performUpload(png)
        }
    }

    private func performUpload(_ png: Data) async {
        do {
            let remotePath = try await store.uploadScreenshotToActive(pngData: png)
            statusMessage = "Inserted path: \(remotePath)"
            dismiss()
        } catch {
            statusMessage = "Upload failed: \(error.localizedDescription)"
        }
    }
}
