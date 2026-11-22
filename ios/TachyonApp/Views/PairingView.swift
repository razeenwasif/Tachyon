@preconcurrency import AVFoundation
import SwiftUI
import TachyonCore
import UIKit

/// Tailscale/TCP host pairing sheet supporting Camera QR scanning, `tachyon://pair?...` paste, and manual entry.
struct PairingView: View {
    @Bindable var store: FleetStore
    @Environment(\.dismiss) private var dismiss

    @State private var alias = ""
    @State private var host = ""
    @State private var port = "7501"
    @State private var token = ""
    @State private var colorSpec = "auto"

    @State private var statusText: String?
    @State private var isError = false
    @State private var testing = false
    @State private var showScanner = true

    var body: some View {
        NavigationStack {
            Form {
                if showScanner {
                    Section("Scan QR from `tachyon pair` or Desktop Settings") {
                        QRScannerRepresentable { scanned in
                            handleScannedCode(scanned)
                        }
                        .frame(height: 210)
                        .listRowInsets(EdgeInsets(top: 6, leading: 6, bottom: 6, trailing: 6))

                        Button {
                            pasteFromClipboard()
                        } label: {
                            Label("Paste `tachyon://pair?...` from Clipboard", systemImage: "doc.on.clipboard")
                        }
                    }
                    .listRowBackground(TachyonPalette.cardBg)
                }

                Section("Manual Tailscale / TCP Host") {
                    TextField("Alias (e.g. tsukuyomi, kagutsuchi)", text: $alias)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    TextField("Host / Tailscale IP (e.g. 100.x.y.z)", text: $host)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .keyboardType(.URL)
                    TextField("Port", text: $port)
                        .keyboardType(.numberPad)
                    SecureField("256-bit Bearer Token (`tachyon server token`)", text: $token)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    Picker("Host Accent", selection: $colorSpec) {
                        ForEach(HostColor.presets, id: \.id) { preset in
                            Text(preset.name).tag(preset.id)
                        }
                    }
                }
                .listRowBackground(TachyonPalette.cardBg)

                if let statusText {
                    Section {
                        Text(statusText)
                            .font(.caption)
                            .foregroundStyle(isError ? TachyonPalette.stateError : TachyonPalette.stateDone)
                    }
                    .listRowBackground(TachyonPalette.cardBg)
                }

                Section {
                    Button {
                        runConnectionTest()
                    } label: {
                        HStack {
                            Text("Test Direct TCP Connection")
                            Spacer()
                            if testing { ProgressView().controlSize(.small) }
                        }
                    }
                    .disabled(testing || host.trimmingCharacters(in: .whitespaces).isEmpty || token.isEmpty)

                    Button {
                        saveHost()
                    } label: {
                        Text("Save & Connect")
                            .fontWeight(.semibold)
                            .frame(maxWidth: .infinity, alignment: .center)
                    }
                    .foregroundStyle(TachyonPalette.accent)
                    .disabled(host.trimmingCharacters(in: .whitespaces).isEmpty || token.isEmpty)
                }
                .listRowBackground(TachyonPalette.cardBg)
            }
            .scrollContentBackground(.hidden)
            .background(TachyonPalette.canvasBg)
            .navigationTitle("Pair Tachyon Host")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
            }
        }
    }

    private func handleScannedCode(_ raw: String) {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        do {
            let payload = try PairingPayload.parse(trimmed)
            try store.addOrUpdateHost(
                alias: payload.alias,
                host: payload.host,
                port: payload.port,
                token: payload.token,
                os: payload.os,
                arch: payload.arch,
                ssh: payload.ssh,
                color: payload.color
            )
            dismiss()
        } catch {
            isError = true
            statusText = error.localizedDescription
        }
    }

    private func pasteFromClipboard() {
        guard let str = UIPasteboard.general.string?.trimmingCharacters(in: .whitespacesAndNewlines),
              !str.isEmpty
        else {
            isError = true
            statusText = "Clipboard is empty."
            return
        }
        handleScannedCode(str)
    }

    private func runConnectionTest() {
        let p = UInt16(port.trimmingCharacters(in: .whitespaces)) ?? defaultMuxPort
        testing = true
        statusText = nil
        Task {
            defer { testing = false }
            do {
                let (info, rtt) = try await store.testConnection(host: host, port: p, token: token)
                isError = false
                statusText = String(format: "Connected to %@ (%@/%@) · %.1f ms", info.hostname, info.os, info.arch, rtt)
            } catch {
                isError = true
                statusText = error.localizedDescription
            }
        }
    }

    private func saveHost() {
        let p = UInt16(port.trimmingCharacters(in: .whitespaces)) ?? defaultMuxPort
        do {
            try store.addOrUpdateHost(
                alias: alias,
                host: host,
                port: p,
                token: token,
                color: colorSpec == "auto" ? nil : colorSpec
            )
            dismiss()
        } catch {
            isError = true
            statusText = error.localizedDescription
        }
    }
}

private struct QRScannerRepresentable: UIViewRepresentable {
    let onDetect: @Sendable @MainActor (String) -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(onDetect: onDetect)
    }

    func makeUIView(context: Context) -> QRPreviewContainerView {
        let view = QRPreviewContainerView()
        context.coordinator.start(inside: view)
        return view
    }

    func updateUIView(_ uiView: QRPreviewContainerView, context: Context) {}

    static func dismantleUIView(_ uiView: QRPreviewContainerView, coordinator: Coordinator) {
        coordinator.stop()
    }

    @MainActor
    final class Coordinator {
        private let controller: QRScannerController
        init(onDetect: @escaping @Sendable @MainActor (String) -> Void) {
            self.controller = QRScannerController(onDetect: onDetect)
        }
        func start(inside view: QRPreviewContainerView) { controller.start(inside: view) }
        func stop() { controller.stop() }
    }
}

private final class QRPreviewContainerView: UIView {
    private let emptyLabel: UILabel = {
        let l = UILabel()
        l.text = "Camera unavailable on Simulator.\nUse Paste or Manual Entry below."
        l.numberOfLines = 0
        l.textAlignment = .center
        l.font = .preferredFont(forTextStyle: .caption1)
        l.textColor = .secondaryLabel
        return l
    }()

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = UIColor(red: 0.09, green: 0.098, blue: 0.114, alpha: 1.0)
        layer.cornerRadius = 10
        clipsToBounds = true
        addSubview(emptyLabel)
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    override func layoutSubviews() {
        super.layoutSubviews()
        emptyLabel.frame = bounds.insetBy(dx: 16, dy: 16)
        layer.sublayers?.forEach { if $0 is AVCaptureVideoPreviewLayer { $0.frame = bounds } }
    }

    func attachPreview(_ previewLayer: AVCaptureVideoPreviewLayer) {
        emptyLabel.isHidden = true
        previewLayer.frame = bounds
        previewLayer.videoGravity = .resizeAspectFill
        layer.insertSublayer(previewLayer, at: 0)
    }
}

private final class QRScannerController: NSObject, AVCaptureMetadataOutputObjectsDelegate, @unchecked Sendable {
    private let queue = DispatchQueue(label: "dev.tachyon.ios.qr")
    private let session = AVCaptureSession()
    private let onDetect: @MainActor (String) -> Void
    private var didEmit = false

    init(onDetect: @escaping @MainActor (String) -> Void) {
        self.onDetect = onDetect
    }

    func start(inside view: QRPreviewContainerView) {
        guard let device = AVCaptureDevice.default(for: .video),
              let input = try? AVCaptureDeviceInput(device: device),
              session.canAddInput(input)
        else { return }
        session.addInput(input)

        let output = AVCaptureMetadataOutput()
        guard session.canAddOutput(output) else { return }
        session.addOutput(output)
        output.setMetadataObjectsDelegate(self, queue: .main)
        if output.availableMetadataObjectTypes.contains(.qr) {
            output.metadataObjectTypes = [.qr]
        }

        let preview = AVCaptureVideoPreviewLayer(session: session)
        DispatchQueue.main.async { view.attachPreview(preview) }
        queue.async { [session] in
            if !session.isRunning { session.startRunning() }
        }
    }

    func stop() {
        queue.async { [session] in
            if session.isRunning { session.stopRunning() }
        }
    }

    func metadataOutput(
        _ output: AVCaptureMetadataOutput,
        didOutput metadataObjects: [AVMetadataObject],
        from connection: AVCaptureConnection
    ) {
        guard !didEmit,
              let obj = metadataObjects.first as? AVMetadataMachineReadableCodeObject,
              let val = obj.stringValue,
              !val.isEmpty
        else { return }
        didEmit = true
        DispatchQueue.main.async { [onDetect] in
            onDetect(val)
        }
    }
}
