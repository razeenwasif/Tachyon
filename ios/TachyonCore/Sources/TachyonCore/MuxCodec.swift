import Foundation

/// Wire protocol version negotiated during `hello` (`src/mux/proto.rs`).
public let muxProtocolVersion: UInt32 = 2

/// Default Mux v2 TCP port (`DEFAULT_TCP_PORT`).
public let defaultMuxPort: UInt16 = 7501

/// Maximum single frame payload size (64 MiB).
public let maxMuxFrameLength = 64 << 20

private let snapshotRawFlag: UInt8 = 0
private let snapshotLZ4Flag: UInt8 = 1

/// 9-byte Mux v2 frame kind byte (`src/mux/proto.rs`).
public enum MuxFrameKind: UInt8, Sendable, CaseIterable {
    case rpcReq = 0x01
    case rpcRes = 0x02
    case event = 0x03
    case ptyOut = 0x04
    case ptyIn = 0x05
    case ptySnapshot = 0x06
    case ptyResize = 0x07
    case ping = 0x08
    case pong = 0x09
}

/// Errors produced by Mux v2 framing, snapshot decompression, or client RPCs.
public enum MuxError: Error, LocalizedError, Sendable, Equatable {
    case frameTooLarge(Int)
    case unknownFrameKind(UInt8)
    case invalidPayload(String)
    case lz4Corrupt(String)
    case invalidPairingURI(String)
    case connectionFailed(String)
    case timedOut(String)
    case disconnected(String)
    case serverError(String)

    public var errorDescription: String? {
        switch self {
        case .frameTooLarge(let n):
            return "Mux frame exceeds limit (\(n) bytes)"
        case .unknownFrameKind(let k):
            return String(format: "Unknown mux frame kind: 0x%02x", k)
        case .invalidPayload(let msg):
            return msg
        case .lz4Corrupt(let msg):
            return "Corrupt LZ4 snapshot: \(msg)"
        case .invalidPairingURI(let msg):
            return msg
        case .connectionFailed(let msg):
            return msg
        case .timedOut(let msg):
            return msg
        case .disconnected(let msg):
            return msg
        case .serverError(let msg):
            return msg
        }
    }
}

/// A framed Mux v2 message (`len (u32 LE) | kind (u8) | channel (u32 LE) | payload`).
public struct MuxFrame: Sendable, Equatable {
    public let kind: MuxFrameKind
    public let channel: UInt32
    public let payload: Data

    public init(kind: MuxFrameKind, channel: UInt32, payload: Data) {
        self.kind = kind
        self.channel = channel
        self.payload = payload
    }
}

/// Binary encoder/decoder matching `src/mux/proto.rs` byte-for-byte.
public enum MuxCodec {
    /// Encode `frame` with its 9-byte little-endian header.
    public static func encodeFrame(_ frame: MuxFrame) throws -> Data {
        guard frame.payload.count <= maxMuxFrameLength else {
            throw MuxError.frameTooLarge(frame.payload.count)
        }
        var out = Data(capacity: 9 + frame.payload.count)
        appendUInt32LE(&out, UInt32(frame.payload.count))
        out.append(frame.kind.rawValue)
        appendUInt32LE(&out, frame.channel)
        out.append(frame.payload)
        return out
    }

    /// Attempt to parse one complete frame from `buffer`, removing consumed bytes when complete.
    public static func tryDecodeFrame(from buffer: inout Data) throws -> MuxFrame? {
        guard buffer.count >= 9 else { return nil }
        let start = buffer.startIndex
        let len = Int(readUInt32LE(buffer, at: start))
        guard len <= maxMuxFrameLength else {
            throw MuxError.frameTooLarge(len)
        }
        let kindByte = buffer[start + 4]
        guard let kind = MuxFrameKind(rawValue: kindByte) else {
            throw MuxError.unknownFrameKind(kindByte)
        }
        let total = 9 + len
        guard buffer.count >= total else { return nil }
        let channel = readUInt32LE(buffer, at: start + 5)
        let payload = len > 0
            ? Data(buffer[(start + 9)..<(start + total)])
            : Data()
        buffer.removeFirst(total)
        return MuxFrame(kind: kind, channel: channel, payload: payload)
    }

    /// Encode an 8-byte `PTY_RESIZE` payload (`cols, rows, cellW, cellH` little-endian `u16`).
    public static func encodeResize(
        cols: UInt16,
        rows: UInt16,
        cellW: UInt16 = 0,
        cellH: UInt16 = 0
    ) -> Data {
        var out = Data(capacity: 8)
        appendUInt16LE(&out, cols)
        appendUInt16LE(&out, rows)
        appendUInt16LE(&out, cellW)
        appendUInt16LE(&out, cellH)
        return out
    }

    /// Decode an 8-byte `PTY_RESIZE` payload.
    public static func decodeResize(
        _ payload: Data
    ) throws -> (cols: UInt16, rows: UInt16, cellW: UInt16, cellH: UInt16) {
        guard payload.count == 8 else {
            throw MuxError.invalidPayload("resize payload must be 8 bytes, got \(payload.count)")
        }
        let i = payload.startIndex
        return (
            readUInt16LE(payload, at: i),
            readUInt16LE(payload, at: i + 2),
            readUInt16LE(payload, at: i + 4),
            readUInt16LE(payload, at: i + 6)
        )
    }

    /// Encode an 8-byte little-endian timestamp used in `PING`/`PONG`.
    public static func encodeTimestamp(_ ns: UInt64) -> Data {
        var out = Data(capacity: 8)
        var le = ns.littleEndian
        withUnsafeBytes(of: &le) { out.append(contentsOf: $0) }
        return out
    }

    /// Decode an 8-byte `PING`/`PONG` payload.
    public static func decodeTimestamp(_ payload: Data) throws -> UInt64 {
        guard payload.count == 8 else {
            throw MuxError.invalidPayload("timestamp payload must be 8 bytes, got \(payload.count)")
        }
        let i = payload.startIndex
        var v: UInt64 = 0
        for shift in 0..<8 {
            v |= UInt64(payload[i + shift]) << (shift * 8)
        }
        return v
    }

    /// Encode raw `PTY_SNAPSHOT` payload (`SNAPSHOT_RAW` header byte + bytes).
    public static func encodeRawSnapshot(_ vt: Data) -> Data {
        var out = Data(capacity: 1 + vt.count)
        out.append(snapshotRawFlag)
        out.append(vt)
        return out
    }

    /// Decode a `PTY_SNAPSHOT` frame payload (`0` raw, `1` LZ4 + 4-byte LE uncompressed size).
    public static func decodeSnapshot(_ payload: Data) throws -> Data {
        guard let flags = payload.first else { return Data() }
        let rest = Data(payload.dropFirst())
        switch flags {
        case snapshotRawFlag:
            return rest
        case snapshotLZ4Flag:
            guard rest.count >= 4 else {
                throw MuxError.lz4Corrupt("missing 4-byte uncompressed size header")
            }
            let expectedSize = Int(readUInt32LE(rest, at: rest.startIndex))
            guard expectedSize <= maxMuxFrameLength else {
                throw MuxError.lz4Corrupt("uncompressed size exceeds limit: \(expectedSize)")
            }
            return try decompressLZ4Block(
                Data(rest.dropFirst(4)),
                expectedSize: expectedSize
            )
        default:
            throw MuxError.invalidPayload("unknown snapshot flags: \(flags)")
        }
    }

    /// Pure-Swift raw LZ4 block decompressor matching `lz4_flex::decompress_size_prepended`.
    public static func decompressLZ4Block(_ src: Data, expectedSize: Int) throws -> Data {
        if expectedSize == 0 {
            return Data()
        }
        let bytes = [UInt8](src)
        var out: [UInt8] = []
        out.reserveCapacity(expectedSize)
        var i = 0

        while i < bytes.count {
            let token = bytes[i]
            i += 1

            var litLen = Int(token >> 4)
            if litLen == 15 {
                while true {
                    guard i < bytes.count else {
                        throw MuxError.lz4Corrupt("truncated literal length")
                    }
                    let ext = Int(bytes[i])
                    i += 1
                    litLen += ext
                    if ext != 255 { break }
                }
            }

            guard i + litLen <= bytes.count else {
                throw MuxError.lz4Corrupt("truncated literal run")
            }
            if litLen > 0 {
                out.append(contentsOf: bytes[i..<(i + litLen)])
                i += litLen
            }

            if i >= bytes.count {
                break
            }

            guard i + 2 <= bytes.count else {
                throw MuxError.lz4Corrupt("truncated match offset")
            }
            let offset = Int(bytes[i]) | (Int(bytes[i + 1]) << 8)
            i += 2
            guard offset >= 1 && offset <= out.count else {
                throw MuxError.lz4Corrupt("invalid match offset \(offset) at out \(out.count)")
            }

            var matchLen = Int(token & 0x0F) + 4
            if (token & 0x0F) == 15 {
                while true {
                    guard i < bytes.count else {
                        throw MuxError.lz4Corrupt("truncated match length")
                    }
                    let ext = Int(bytes[i])
                    i += 1
                    matchLen += ext
                    if ext != 255 { break }
                }
            }

            guard out.count + matchLen <= maxMuxFrameLength else {
                throw MuxError.lz4Corrupt("match exceeds max frame length")
            }
            // Copy byte-by-byte so overlapping RLE matches (offset < matchLen) replicate cleanly.
            for _ in 0..<matchLen {
                let idx = out.count - offset
                out.append(out[idx])
            }
        }

        guard out.count == expectedSize else {
            throw MuxError.lz4Corrupt("expected \(expectedSize) bytes, decoded \(out.count)")
        }
        return Data(out)
    }

    private static func appendUInt16LE(_ data: inout Data, _ v: UInt16) {
        data.append(UInt8(v & 0xFF))
        data.append(UInt8((v >> 8) & 0xFF))
    }

    private static func appendUInt32LE(_ data: inout Data, _ v: UInt32) {
        data.append(UInt8(v & 0xFF))
        data.append(UInt8((v >> 8) & 0xFF))
        data.append(UInt8((v >> 16) & 0xFF))
        data.append(UInt8((v >> 24) & 0xFF))
    }

    private static func readUInt16LE(_ data: Data, at i: Data.Index) -> UInt16 {
        UInt16(data[i]) | (UInt16(data[i + 1]) << 8)
    }

    private static func readUInt32LE(_ data: Data, at i: Data.Index) -> UInt32 {
        UInt32(data[i])
            | (UInt32(data[i + 1]) << 8)
            | (UInt32(data[i + 2]) << 16)
            | (UInt32(data[i + 3]) << 24)
    }
}
