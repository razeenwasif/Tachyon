import XCTest
@testable import TachyonCore

final class MuxCodecAndPairingTests: XCTestCase {
    func testFrameRoundTripAllKindsAndFragmentedStreamingDecode() throws {
        var combined = Data()
        let cases: [(MuxFrameKind, UInt32, Data)] = [
            (.rpcReq, 1, Data("{\"op\":\"hello\"}".utf8)),
            (.rpcRes, 42, Data("{\"ok\":true}".utf8)),
            (.event, 0, Data("{\"event\":\"session.created\"}".utf8)),
            (.ptyOut, 7, Data([0x1B, 0x5B, 0x33, 0x31, 0x6D, 0x41, 0x42])),
            (.ptyIn, 7, Data("echo hi\n".utf8)),
            (.ptySnapshot, 7, MuxCodec.encodeRawSnapshot(Data("screen".utf8))),
            (.ptyResize, 7, MuxCodec.encodeResize(cols: 96, rows: 34, cellW: 10, cellH: 22)),
            (.ping, 9, MuxCodec.encodeTimestamp(123_456_789_012)),
            (.pong, 9, MuxCodec.encodeTimestamp(123_456_789_999)),
        ]

        for (kind, ch, payload) in cases {
            let encoded = try MuxCodec.encodeFrame(MuxFrame(kind: kind, channel: ch, payload: payload))
            combined.append(encoded)
        }

        // Feed 3 bytes at a time to exercise streaming partial header/payload accumulation.
        var rx = Data()
        var decoded: [MuxFrame] = []
        var offset = 0
        while offset < combined.count {
            let end = min(combined.count, offset + 3)
            rx.append(combined[offset..<end])
            offset = end
            while let f = try MuxCodec.tryDecodeFrame(from: &rx) {
                decoded.append(f)
            }
        }

        XCTAssertTrue(rx.isEmpty)
        XCTAssertEqual(decoded.count, cases.count)
        for (idx, (kind, ch, payload)) in cases.enumerated() {
            XCTAssertEqual(decoded[idx], MuxFrame(kind: kind, channel: ch, payload: payload))
        }

        let rsz = try MuxCodec.decodeResize(cases[6].2)
        XCTAssertEqual(rsz.cols, 96)
        XCTAssertEqual(rsz.rows, 34)
        XCTAssertEqual(rsz.cellW, 10)
        XCTAssertEqual(rsz.cellH, 22)
        XCTAssertEqual(try MuxCodec.decodeTimestamp(cases[7].2), 123_456_789_012)
    }

    func testDecodeSnapshotRawAndLZ4IncludingOverlappingRLEMatch() throws {
        let rawPayload = MuxCodec.encodeRawSnapshot(Data("hello vt".utf8))
        XCTAssertEqual(try MuxCodec.decodeSnapshot(rawPayload), Data("hello vt".utf8))

        // Hand-encoded LZ4 block reproducing `lz4_flex::compress_prepend_size`:
        // Original: "ABCD" + ("A" repeated 20 times via offset=4 overlapping RLE? Use "XY" + ("XY"x10) = 22 bytes "XYXYXY...").
        //Wait: 1 byte literal 'A' + match offset=1, matchLen=20 -> "A" * 21 (21 bytes = 0x15 LE).
        let expected = Data(repeating: 0x41, count: 21)
        // Token: litLen=1 (high 4 bits = 0x10), matchLen_code = 20 - 4 = 16 -> 0x0F + ext 0x01 => token = 0x1F
        // Literal byte: 0x41 ('A')
        // Offset LE: 0x01, 0x00
        // Match ext: 0x01 (since 15 + 1 + 4 = 20)
        var lz4Payload = Data([0x01]) // SNAPSHOT_LZ4
        lz4Payload.append(contentsOf: [21, 0, 0, 0]) // uncompressed size 21 LE
        lz4Payload.append(contentsOf: [0x1F, 0x41, 0x01, 0x00, 0x01])
        let out = try MuxCodec.decodeSnapshot(lz4Payload)
        XCTAssertEqual(out, expected)
    }

    func testPairingURIRoundTripAndValidation() throws {
        let original = PairingPayload(
            version: 1,
            alias: "mac mini+dev",
            host: "fd7a:115c:a1e0::42",
            port: 7501,
            token: "0123456789abcdef0123456789abcdef",
            protocolVersion: 2,
            os: "macos",
            arch: "aarch64",
            ssh: "kagutsuchi",
            color: "danger:#ef4444"
        )
        let uri = original.toURI()
        XCTAssertTrue(uri.hasPrefix("tachyon://pair?"))
        XCTAssertEqual(original.tcpURI, "tcp://[fd7a:115c:a1e0::42]:7501")

        let decoded = try PairingPayload.parse(uri)
        XCTAssertEqual(decoded, original)

        XCTAssertThrowsError(try PairingPayload.parse("https://example.com"))
        XCTAssertThrowsError(try PairingPayload.parse("tachyon://pair?v=2&host=1.2.3.4&token=abc"))
        XCTAssertThrowsError(try PairingPayload.parse("tachyon://pair?v=1&port=0&host=1.2.3.4&token=abc"))
        XCTAssertThrowsError(try PairingPayload.parse("tachyon://pair?v=1&host=1.2.3.4"))
    }

    func testHostColorPresetsAndDefaults() {
        XCTAssertEqual(HostColor.resolve(spec: nil, os: "macos"), HostColor(r: 66, g: 153, b: 255))
        XCTAssertEqual(HostColor.resolve(spec: "red", os: "linux"), HostColor(r: 242, g: 74, b: 74, danger: true))
        XCTAssertEqual(HostColor.resolve(spec: "danger:#112233", os: "windows"), HostColor(r: 0x11, g: 0x22, b: 0x33, danger: true))
        XCTAssertNil(HostColor.resolve(spec: "none", os: "macos"))
    }
}
