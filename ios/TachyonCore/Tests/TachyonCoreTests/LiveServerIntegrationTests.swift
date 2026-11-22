import Foundation
import XCTest
@testable import TachyonCore

final class LiveServerIntegrationTests: XCTestCase {
    private func findTachyonBinary() -> String? {
        let fm = FileManager.default
        let fileURL = URL(fileURLWithPath: #filePath)
        // .../<repo>/ios/TachyonCore/Tests/TachyonCoreTests/LiveServerIntegrationTests.swift
        let repoRoot = fileURL
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
        let home = fm.homeDirectoryForCurrentUser.path
        let envTarget = ProcessInfo.processInfo.environment["CARGO_TARGET_DIR"]
        var candidates: [String] = []
        if let envTarget, !envTarget.isEmpty {
            candidates.append("\(envTarget)/debug/tachyon")
        }
        candidates.append("\(home)/.cargo/target-cache/debug/tachyon")
        candidates.append(repoRoot.appendingPathComponent("target/debug/tachyon").path)
        candidates.append("\(home)/.local/bin/tachyon")
        for path in candidates where fm.isExecutableFile(atPath: path) {
            return path
        }
        return nil
    }

    private func runCLI(
        bin: String,
        dir: String,
        sock: String,
        hosts: String,
        tokenFile: String,
        args: [String]
    ) throws -> (code: Int32, stdout: String, stderr: String) {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: bin)
        p.arguments = args
        var env = ProcessInfo.processInfo.environment
        env["HOME"] = dir
        env["TACHYON_SOCKET"] = sock
        env["TACHYON_HOSTS_FILE"] = hosts
        env["TACHYON_TOKEN_FILE"] = tokenFile
        env.removeValue(forKey: "TACHYON_SERVER")
        env.removeValue(forKey: "TACHYON_SESSION")
        env.removeValue(forKey: "TACHYON_WINDOW")
        env.removeValue(forKey: "TACHYON_BLOCK")
        p.environment = env
        let outPipe = Pipe()
        let errPipe = Pipe()
        p.standardOutput = outPipe
        p.standardError = errPipe
        try p.run()
        p.waitUntilExit()
        let outData = outPipe.fileHandleForReading.readDataToEndOfFile()
        let errData = errPipe.fileHandleForReading.readDataToEndOfFile()
        return (
            p.terminationStatus,
            String(decoding: outData, as: UTF8.self),
            String(decoding: errData, as: UTF8.self)
        )
    }

    @MainActor
    func testSwiftMuxClientPairsAuthsAttachesResizesAndReceivesOSC7501FromLiveRustServer() async throws {
        guard let bin = findTachyonBinary() else {
            throw XCTSkip("No compiled tachyon binary found for live Swift->Rust E2E test")
        }
        let fm = FileManager.default
        let dir = "/tmp/ty-ios-e2e-\(ProcessInfo.processInfo.processIdentifier)-\(Int.random(in: 1000...9999))"
        try fm.createDirectory(atPath: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(atPath: dir) }

        let sock = "\(dir)/s.sock"
        let hosts = "\(dir)/hosts.json"
        let tokenFile = "\(dir)/server.token"

        let srv = Process()
        srv.executableURL = URL(fileURLWithPath: bin)
        srv.arguments = [
            "server", "run",
            "--socket", sock,
            "--listen", "127.0.0.1:0",
            "--token-file", tokenFile,
        ]
        var env = ProcessInfo.processInfo.environment
        env["HOME"] = dir
        env["TACHYON_SOCKET"] = sock
        env["TACHYON_HOSTS_FILE"] = hosts
        env["TACHYON_TOKEN_FILE"] = tokenFile
        env.removeValue(forKey: "TACHYON_SERVER")
        env.removeValue(forKey: "TACHYON_SESSION")
        env.removeValue(forKey: "TACHYON_WINDOW")
        env.removeValue(forKey: "TACHYON_BLOCK")
        srv.environment = env
        srv.standardOutput = FileHandle.nullDevice
        srv.standardError = FileHandle.nullDevice
        try srv.run()
        defer {
            _ = try? runCLI(bin: bin, dir: dir, sock: sock, hosts: hosts, tokenFile: tokenFile, args: ["server", "stop"])
            if srv.isRunning { srv.terminate() }
            srv.waitUntilExit()
        }

        // Wait for daemon TCP bind & `listen_addr` reporting.
        var ready = false
        for _ in 0..<80 {
            let res = try runCLI(bin: bin, dir: dir, sock: sock, hosts: hosts, tokenFile: tokenFile, args: ["server", "status", "--json"])
            if res.code == 0,
               let obj = try? JSONSerialization.jsonObject(with: Data(res.stdout.utf8)) as? [String: Any],
               let host = obj["host"] as? [String: Any],
               let listenAddr = host["listen_addr"] as? String,
               !listenAddr.isEmpty {
                ready = true
                break
            }
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        XCTAssertTrue(ready, "Rust daemon failed to report bound listen_addr")

        // Generate real `tachyon://pair?...` URI from `tachyon pair --json` and parse in Swift.
        let pairRes = try runCLI(bin: bin, dir: dir, sock: sock, hosts: hosts, tokenFile: tokenFile, args: ["pair", "--json"])
        XCTAssertEqual(pairRes.code, 0, "stderr: \(pairRes.stderr)")
        let pairObj = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(pairRes.stdout.utf8)) as? [String: Any])
        let pairingURI = try XCTUnwrap(pairObj["pairing_uri"] as? String)
        let pairing = try PairingPayload.parse(pairingURI)
        XCTAssertEqual(pairing.host, "127.0.0.1")
        XCTAssertGreaterThan(pairing.port, 0)

        // 1. Wrong bearer token must fail authentication cleanly.
        let badClient = MuxClient()
        do {
            _ = try await badClient.connect(host: pairing.host, port: pairing.port, token: "wrong-token")
            XCTFail("Expected wrong bearer token to be rejected")
        } catch {
            // Expected failure
        }

        // 2. Real token connects, fetches state, pings, and subscribes to events.
        let client = MuxClient()
        let hostInfo = try await client.connect(host: pairing.host, port: pairing.port, token: pairing.token)
        XCTAssertEqual(hostInfo.protocolVersion, 2)
        let rtt = try await client.ping(timeoutSeconds: 3.0)
        XCTAssertGreaterThanOrEqual(rtt, 0.0)
        let events = try await client.subscribeEvents()

        let (sess, _, blk) = try await client.createSession(name: "ios-e2e", cmd: ["/bin/sh"], cols: 80, rows: 24)
        let snap = try await client.getState()
        XCTAssertTrue(snap.sessions.contains(where: { $0.id == sess.id }))

        // 3. Attach live stream, send input via PtyIn + resize, verify TerminalBuffer receives live output.
        let stream = try await client.attach(blockId: blk.id, interactive: true, restore: true)
        let buffer = TerminalBuffer(cols: 80, rows: 24)

        let pump = Task { @MainActor in
            for await chunk in stream.chunks {
                switch chunk {
                case .snapshot(let vt):
                    buffer.reset()
                    buffer.feed(vt)
                case .output(let out):
                    buffer.feed(out)
                }
            }
        }
        defer { pump.cancel() }

        let eventProbe = Task { () -> AlertEvent? in
            for await ev in events {
                if case .agentAlert(let alert) = ev, alert.blockId == blk.id {
                    return alert
                }
            }
            return nil
        }
        defer { eventProbe.cancel() }

        try await client.resizePty(streamId: stream.streamId, cols: 96, rows: 32, cellW: 9, cellH: 18)
        buffer.resize(cols: 96, rows: 32)
        try? await Task.sleep(nanoseconds: 120_000_000)

        let script = "echo IOS_MUX_V2_OK; printf '\\033]7501;state=blocked;app=claude;msg=ios-approve\\007\\n'\n"
        try await client.writePty(streamId: stream.streamId, data: Data(script.utf8))

        var gotOutput = false
        for _ in 0..<60 {
            if buffer.plainText().contains("IOS_MUX_V2_OK") {
                gotOutput = true
                break
            }
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        XCTAssertTrue(gotOutput, "Expected IOS_MUX_V2_OK in TerminalBuffer: \(buffer.plainText())")

        // Verify OSC 7501 alert event arrived over the TCP Mux v2 stream.
        let alert = await withTaskGroup(of: AlertEvent?.self) { group in
            group.addTask { await eventProbe.value }
            group.addTask {
                try? await Task.sleep(nanoseconds: 3_000_000_000)
                return nil
            }
            let first = await group.next() ?? nil
            group.cancelAll()
            return first
        }
        XCTAssertEqual(alert?.state, "blocked")
        XCTAssertEqual(alert?.app, "claude")
        XCTAssertEqual(alert?.message, "ios-approve")

        // Verify `block.send` one-tap approval (`y\n`) reaches the PTY cleanly.
        try await client.sendInput(blockId: blk.id, data: Data("echo REPLY_DONE\n".utf8))
        var gotReply = false
        for _ in 0..<40 {
            if buffer.plainText().contains("REPLY_DONE") {
                gotReply = true
                break
            }
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        XCTAssertTrue(gotReply, "Expected block.send output REPLY_DONE in buffer")

        await client.detach(streamId: stream.streamId)
        await client.disconnect()
    }
}
