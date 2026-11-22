import XCTest
@testable import TachyonCore

final class TerminalBufferAndKeyTests: XCTestCase {
    func testVT100TrueColorWideGlyphsAltScreenAndOSC7501() {
        let buf = TerminalBuffer(cols: 40, rows: 10)
        // SGR truecolor fg, bold, wide CJK chars, OSC 0 title, OSC 7501 status
        let seq = "\u{1B}]0;build-pane\u{07}"
            + "\u{1B}]7501;state=blocked;app=claude;msg=Approve migration\u{1B}\\"
            + "\u{1B}[1;38;2;150;178;215mHello 日本語\u{1B}[0m\r\n"
            + "Line 2"
        buf.feed(Data(seq.utf8))

        let snap = buf.renderSnapshot()
        XCTAssertEqual(snap.title, "build-pane")
        XCTAssertEqual(snap.agentStatus?.state, .blocked)
        XCTAssertEqual(snap.agentStatus?.app, "claude")
        XCTAssertEqual(snap.agentStatus?.msg, "Approve migration")

        XCTAssertEqual(snap.cell(row: 0, col: 0).scalar, "H")
        XCTAssertTrue(snap.cell(row: 0, col: 0).attrs.contains(.bold))
        XCTAssertEqual(snap.cell(row: 0, col: 0).fg, .rgb(150, 178, 215))
        // '日' starts at col 6 and is wide
        XCTAssertEqual(snap.cell(row: 0, col: 6).scalar, "日")
        XCTAssertTrue(snap.cell(row: 0, col: 6).attrs.contains(.wide))
        XCTAssertTrue(snap.cell(row: 0, col: 7).attrs.contains(.wideContinuation))

        XCTAssertEqual(buf.plainText().components(separatedBy: "\n").first, "Hello 日本語")

        // Alt screen preserves primary screen content cleanly.
        buf.feed(Data("\u{1B}[?1049hAlt Content".utf8))
        XCTAssertTrue(buf.inAltScreen)
        XCTAssertTrue(buf.plainText().contains("Alt Content"))
        buf.feed(Data("\u{1B}[?1049l".utf8))
        XCTAssertFalse(buf.inAltScreen)
        XCTAssertEqual(buf.plainText().components(separatedBy: "\n").first, "Hello 日本語")
    }

    func testUTF8SplitAcrossChunksResetsCleanlyOnSnapshot() {
        let buf = TerminalBuffer(cols: 30, rows: 5)
        let bytes = [UInt8]("🚀 Ok".utf8)
        buf.feed(Data(bytes[0..<2])) // split 4-byte emoji
        buf.feed(Data(bytes[2...]))
        XCTAssertTrue(buf.plainText().contains("🚀 Ok"))
    }

    func testKeyEncoderAndDECCKMAndBracketedPaste() {
        XCTAssertEqual(KeyEncoder.encodeSpecialKey(.up, applicationCursor: false), Data("\u{1B}[A".utf8))
        XCTAssertEqual(KeyEncoder.encodeSpecialKey(.up, applicationCursor: true), Data("\u{1B}OA".utf8))
        XCTAssertEqual(KeyEncoder.encodeText("c", ctrlArmed: true), Data([0x03]))
        XCTAssertEqual(KeyEncoder.encodeText("L", ctrlArmed: true), Data([0x0C]))
        XCTAssertEqual(
            KeyEncoder.encodePaste("echo 1\necho 2", bracketedPaste: true),
            Data("\u{1B}[200~echo 1\recho 2\u{1B}[201~".utf8)
        )
    }

    func testAgentEntryCollectSortsBlockedErrorWorkingDone() throws {
        let mkBlock: (String, String, UInt64) throws -> BlockInfo = { id, state, ts in
            let json = """
            {
              "id": "\(id)",
              "session_id": "s1",
              "window_id": "w1",
              "cols": 80,
              "rows": 24,
              "alive": true,
              "created_at": \(ts),
              "status": { "state": "\(state)", "app": "claude", "msg": "note" }
            }
            """
            return try JSONDecoder().decode(BlockInfo.self, from: Data(json.utf8))
        }
        let host = HostProfile(alias: "mini", host: "100.64.0.1", port: 7501, os: "macos")
        let snap = ServerSnapshot(
            host: HostInfo(hostname: "mini", os: "macos", arch: "aarch64", version: "0.1.0", protocolVersion: 2, pid: 1),
            sessions: [SessionInfo(id: "s1", name: "dev", windows: ["w1"], activeWindow: "w1", createdAt: 1)],
            windows: [WindowInfo(id: "w1", sessionId: "s1", name: nil, layout: .leaf("b1"), focusedBlock: "b1", zoomed: false, createdAt: 1)],
            blocks: [
                try mkBlock("b-done", "done", 100),
                try mkBlock("b-work", "working", 101),
                try mkBlock("b-blk", "blocked", 99),
                try mkBlock("b-err", "error", 98),
            ]
        )
        let collected = AgentEntry.collect(hosts: [(host, snap)])
        XCTAssertEqual(collected.map(\.block.id), ["b-blk", "b-err", "b-work", "b-done"])
    }
}
