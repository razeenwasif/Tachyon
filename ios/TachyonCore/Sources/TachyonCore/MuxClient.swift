import Foundation
@preconcurrency import Network

/// Chunk emitted from an attached Mux v2 block stream (`PTY_SNAPSHOT` or `PTY_OUT`).
public enum MuxStreamChunk: Sendable, Equatable {
    case snapshot(Data)
    case output(Data)
}

/// Handle returned by `MuxClient.attach`.
public struct AttachedStreamHandle: Sendable {
    public let streamId: UInt32
    public let blockId: String
    public let cols: UInt16
    public let rows: UInt16
    public let alive: Bool
    public let chunks: AsyncStream<MuxStreamChunk>
}

private final class OnceFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var done = false

    func tryAcquire() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if done { return false }
        done = true
        return true
    }
}

/// Native iOS/macOS TCP client implementing Tachyon's 9-byte Mux v2 protocol (`src/mux/proto.rs`).
public actor MuxClient {
    private var connection: NWConnection?
    private let ioQueue = DispatchQueue(label: "dev.tachyon.mux.tcp", qos: .userInitiated)
    private var rxBuffer = Data()
    private var isClosed = false

    private var nextReqId: UInt32 = 1
    private var nextStreamId: UInt32 = 1
    private var nextPingSeq: UInt32 = 1

    private var pendingRpc: [UInt32: CheckedContinuation<Data, Error>] = [:]
    private var pendingPing: [UInt32: (sentNs: UInt64, cont: CheckedContinuation<Double, Error>)] = [:]
    private var streamContinuations: [UInt32: AsyncStream<MuxStreamChunk>.Continuation] = [:]
    private var eventContinuations: [UUID: AsyncStream<MuxEvent>.Continuation] = [:]
    private var readerTask: Task<Void, Never>?

    public private(set) var hostInfo: HostInfo = HostInfo()
    public private(set) var clientId: String = ""

    public init() {}

    /// Establish a TCP connection to `host:port`, authenticate via bearer `hello`, and return `HostInfo`.
    public func connect(
        host: String,
        port: UInt16,
        token: String,
        clientName: String = "tachyon-ios",
        timeoutSeconds: Double = 6.0
    ) async throws -> HostInfo {
        guard let nwPort = NWEndpoint.Port(rawValue: port) else {
            throw MuxError.connectionFailed("Invalid TCP port \(port)")
        }
        let tcpOpts = NWProtocolTCP.Options()
        tcpOpts.noDelay = true
        tcpOpts.connectionTimeout = max(1, Int(ceil(timeoutSeconds)))
        let params = NWParameters(tls: nil, tcp: tcpOpts)
        let conn = NWConnection(host: NWEndpoint.Host(host), port: nwPort, using: params)
        self.connection = conn
        self.isClosed = false
        self.rxBuffer.removeAll(keepingCapacity: true)

        let (rxStream, rxCont) = AsyncStream<Result<Data, MuxError>>.makeStream()
        let connectFlag = OnceFlag()

        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            conn.stateUpdateHandler = { [weak self] state in
                switch state {
                case .ready:
                    Self.scheduleReceive(on: conn, cont: rxCont)
                    if connectFlag.tryAcquire() {
                        cont.resume()
                    }
                case .waiting(let err):
                    if connectFlag.tryAcquire() {
                        conn.cancel()
                        cont.resume(throwing: MuxError.connectionFailed(err.localizedDescription))
                    }
                case .failed(let err):
                    let muxErr = MuxError.connectionFailed(err.localizedDescription)
                    rxCont.yield(.failure(muxErr))
                    rxCont.finish()
                    if connectFlag.tryAcquire() {
                        cont.resume(throwing: muxErr)
                    } else {
                        Task { await self?.closeWithError(muxErr) }
                    }
                case .cancelled:
                    rxCont.finish()
                    if connectFlag.tryAcquire() {
                        cont.resume(throwing: MuxError.disconnected("connection cancelled"))
                    } else {
                        Task { await self?.closeWithError(.disconnected("connection cancelled")) }
                    }
                default:
                    break
                }
            }

            conn.start(queue: ioQueue)

            let delayNs = UInt64(max(0.5, timeoutSeconds) * 1_000_000_000.0)
            Task {
                try? await Task.sleep(nanoseconds: delayNs)
                if connectFlag.tryAcquire() {
                    conn.cancel()
                    cont.resume(throwing: MuxError.timedOut("Timed out connecting to \(host):\(port)"))
                }
            }
        }

        self.readerTask = Task { [weak self] in
            for await item in rxStream {
                guard let self else { break }
                switch item {
                case .success(let chunk):
                    await self.processIncomingBytes(chunk)
                case .failure(let err):
                    await self.closeWithError(err)
                    return
                }
            }
            await self?.closeWithError(.disconnected("transport stream ended"))
        }

        var helloReq: [String: Any] = [
            "op": "hello",
            "protocol": muxProtocolVersion,
            "client_kind": "ios",
            "client_name": clientName,
        ]
        if !token.isEmpty {
            helloReq["token"] = token
        }

        do {
            let replyData = try await rpcRaw(helloReq)
            let obj = (try? JSONSerialization.jsonObject(with: replyData) as? [String: Any]) ?? [:]
            guard let hostObj = obj["host"] else {
                throw MuxError.invalidPayload("missing `host` in hello response")
            }
            let hostData = try JSONSerialization.data(withJSONObject: hostObj)
            let info = try JSONDecoder().decode(HostInfo.self, from: hostData)
            self.hostInfo = info
            self.clientId = obj["client_id"] as? String ?? ""
            return info
        } catch {
            await disconnect()
            throw error
        }
    }

    public func disconnect() async {
        closeWithError(.disconnected("disconnected"))
    }

    // MARK: - Typed RPC API

    public func ping(timeoutSeconds: Double = 3.0) async throws -> Double {
        guard !isClosed, let conn = connection else {
            throw MuxError.disconnected("not connected")
        }
        let seq = nextPingSeq
        nextPingSeq &+= 1
        let sentNs = DispatchTime.now().uptimeNanoseconds
        let frame = MuxFrame(
            kind: .ping,
            channel: seq,
            payload: MuxCodec.encodeTimestamp(sentNs)
        )
        let bytes = try MuxCodec.encodeFrame(frame)

        return try await withCheckedThrowingContinuation { cont in
            pendingPing[seq] = (sentNs: sentNs, cont: cont)
            conn.send(content: bytes, completion: .contentProcessed { [weak self] err in
                if let err {
                    Task {
                        await self?.failPing(
                            seq: seq,
                            error: MuxError.connectionFailed(err.localizedDescription)
                        )
                    }
                }
            })
            let ns = UInt64(max(0.2, timeoutSeconds) * 1_000_000_000.0)
            Task { [weak self] in
                try? await Task.sleep(nanoseconds: ns)
                await self?.failPing(seq: seq, error: MuxError.timedOut("ping timed out"))
            }
        }
    }

    public func getState() async throws -> ServerSnapshot {
        let data = try await rpcRaw(["op": "state.get"])
        do {
            return try JSONDecoder().decode(ServerSnapshot.self, from: data)
        } catch {
            throw MuxError.invalidPayload("invalid state.get response: \(error.localizedDescription)")
        }
    }

    public func subscribeEvents() async throws -> AsyncStream<MuxEvent> {
        let subId = UUID()
        let (stream, cont) = AsyncStream<MuxEvent>.makeStream()
        eventContinuations[subId] = cont
        cont.onTermination = { [weak self] _ in
            Task { await self?.removeEventSubscriber(id: subId) }
        }
        do {
            _ = try await rpcRaw(["op": "events.subscribe"])
            return stream
        } catch {
            removeEventSubscriber(id: subId)
            throw error
        }
    }

    public func createSession(
        name: String? = nil,
        cwd: String? = nil,
        cmd: [String]? = nil,
        label: String? = nil,
        cols: UInt16 = 80,
        rows: UInt16 = 24
    ) async throws -> (SessionInfo, WindowInfo, BlockInfo) {
        var req: [String: Any] = [
            "op": "session.create",
            "cols": Int(cols),
            "rows": Int(rows),
        ]
        if let name, !name.isEmpty { req["name"] = name }
        if let cwd, !cwd.isEmpty { req["cwd"] = cwd }
        if let cmd, !cmd.isEmpty { req["cmd"] = cmd }
        if let label, !label.isEmpty { req["label"] = label }
        let data = try await rpcRaw(req)
        return try decodeCreatedEntities(data, expectSession: true)
    }

    public func killSession(sessionId: String) async throws {
        _ = try await rpcRaw(["op": "session.kill", "session_id": sessionId])
    }

    public func createWindow(
        sessionId: String,
        name: String? = nil,
        cols: UInt16 = 80,
        rows: UInt16 = 24
    ) async throws -> (WindowInfo, BlockInfo) {
        var req: [String: Any] = [
            "op": "window.create",
            "session_id": sessionId,
            "cols": Int(cols),
            "rows": Int(rows),
        ]
        if let name, !name.isEmpty { req["name"] = name }
        let data = try await rpcRaw(req)
        let (_, win, blk) = try decodeCreatedEntities(data, expectSession: false)
        return (win, blk)
    }

    public func splitBlock(
        blockId: String,
        dir: SplitDir,
        cols: UInt16 = 80,
        rows: UInt16 = 24
    ) async throws -> (WindowInfo, BlockInfo) {
        let data = try await rpcRaw([
            "op": "block.split",
            "block_id": blockId,
            "dir": dir.rawValue,
            "cols": Int(cols),
            "rows": Int(rows),
        ])
        let (_, win, blk) = try decodeCreatedEntities(data, expectSession: false)
        return (win, blk)
    }

    public func killBlock(blockId: String) async throws {
        _ = try await rpcRaw(["op": "block.kill", "block_id": blockId])
    }

    public func sendInput(blockId: String, data: Data) async throws {
        _ = try await rpcRaw([
            "op": "block.send",
            "block_id": blockId,
            "data_b64": data.base64EncodedString(),
        ])
    }

    public func capture(blockId: String, lines: Int? = nil, ansi: Bool = false) async throws -> String {
        var req: [String: Any] = [
            "op": "block.capture",
            "block_id": blockId,
            "ansi": ansi,
        ]
        if let lines { req["lines"] = lines }
        let data = try await rpcRaw(req)
        let obj = (try? JSONSerialization.jsonObject(with: data) as? [String: Any]) ?? [:]
        return obj["text"] as? String ?? ""
    }

    public func attach(
        blockId: String,
        interactive: Bool = true,
        restore: Bool = true
    ) async throws -> AttachedStreamHandle {
        let streamId = nextStreamId
        nextStreamId &+= 1
        let (stream, cont) = AsyncStream<MuxStreamChunk>.makeStream()
        // Register before sending `block.attach` so initial snapshot never races ahead.
        streamContinuations[streamId] = cont

        do {
            let data = try await rpcRaw([
                "op": "block.attach",
                "block_id": blockId,
                "stream_id": Int(streamId),
                "mode": interactive ? "interactive" : "readonly",
                "restore": restore,
            ])
            let obj = (try? JSONSerialization.jsonObject(with: data) as? [String: Any]) ?? [:]
            let cols = UInt16(truncatingIfNeeded: (obj["cols"] as? NSNumber)?.intValue ?? 80)
            let rows = UInt16(truncatingIfNeeded: (obj["rows"] as? NSNumber)?.intValue ?? 24)
            let alive = obj["alive"] as? Bool ?? true
            return AttachedStreamHandle(
                streamId: streamId,
                blockId: blockId,
                cols: cols,
                rows: rows,
                alive: alive,
                chunks: stream
            )
        } catch {
            streamContinuations.removeValue(forKey: streamId)?.finish()
            throw error
        }
    }

    public func detach(streamId: UInt32) async {
        streamContinuations.removeValue(forKey: streamId)?.finish()
        _ = try? await rpcRaw([
            "op": "block.detach",
            "stream_id": Int(streamId),
        ])
    }

    public func writePty(streamId: UInt32, data: Data) async throws {
        guard !isClosed, let conn = connection else {
            throw MuxError.disconnected("not connected")
        }
        let bytes = try MuxCodec.encodeFrame(MuxFrame(kind: .ptyIn, channel: streamId, payload: data))
        try await sendBytes(bytes, on: conn)
    }

    public func resizePty(
        streamId: UInt32,
        cols: UInt16,
        rows: UInt16,
        cellW: UInt16 = 0,
        cellH: UInt16 = 0
    ) async throws {
        guard !isClosed, let conn = connection else {
            throw MuxError.disconnected("not connected")
        }
        let payload = MuxCodec.encodeResize(cols: cols, rows: rows, cellW: cellW, cellH: cellH)
        let bytes = try MuxCodec.encodeFrame(MuxFrame(kind: .ptyResize, channel: streamId, payload: payload))
        try await sendBytes(bytes, on: conn)
    }

    /// Uploads a PNG image via a transient server session (`base64 -d` / PowerShell) and returns the remote path.
    public func uploadScreenshotPNG(
        os: String,
        pngData: Data,
        filename: String = "tachyon-ios-\(Int(Date().timeIntervalSince1970)).png"
    ) async throws -> String {
        let spec = KeyEncoder.screenshotUploadSpec(os: os, filename: filename)
        let (session, _, block) = try await createSession(
            name: "ios-upload-\(UInt32.random(in: 1000...9999))",
            cmd: spec.command,
            cols: 80,
            rows: 24
        )
        defer {
            Task { try? await self.killSession(sessionId: session.id) }
        }
        let b64 = pngData.base64EncodedString()
        let chunkSize = 16_384
        var idx = b64.startIndex
        while idx < b64.endIndex {
            let next = b64.index(idx, offsetBy: chunkSize, limitedBy: b64.endIndex) ?? b64.endIndex
            let slice = String(b64[idx..<next])
            try await sendInput(blockId: block.id, data: Data(slice.utf8))
            idx = next
        }
        // Newline + Ctrl-D closes stdin cleanly on POSIX TTYs.
        try await sendInput(blockId: block.id, data: Data([0x0A, 0x04]))
        try? await Task.sleep(nanoseconds: 250_000_000)
        return spec.remotePath
    }

    // MARK: - Internal Transport & Framing

    private func rpcRaw(_ body: [String: Any]) async throws -> Data {
        guard !isClosed, let conn = connection else {
            throw MuxError.disconnected("not connected")
        }
        let reqId = nextReqId
        nextReqId &+= 1
        let payload = try JSONSerialization.data(withJSONObject: body)
        let frame = MuxFrame(kind: .rpcReq, channel: reqId, payload: payload)
        let bytes = try MuxCodec.encodeFrame(frame)

        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { cont in
                if isClosed {
                    cont.resume(throwing: MuxError.disconnected("not connected"))
                    return
                }
                pendingRpc[reqId] = cont
                conn.send(content: bytes, completion: .contentProcessed { [weak self] err in
                    if let err {
                        Task {
                            await self?.failRpc(
                                reqId: reqId,
                                error: MuxError.connectionFailed(err.localizedDescription)
                            )
                        }
                    }
                })
            }
        } onCancel: {
            Task { await self.failRpc(reqId: reqId, error: CancellationError()) }
        }
    }

    private func sendBytes(_ bytes: Data, on conn: NWConnection) async throws {
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            conn.send(content: bytes, completion: .contentProcessed { err in
                if let err {
                    cont.resume(throwing: MuxError.connectionFailed(err.localizedDescription))
                } else {
                    cont.resume()
                }
            })
        }
    }

    private func processIncomingBytes(_ chunk: Data) {
        rxBuffer.append(chunk)
        while true {
            do {
                guard let frame = try MuxCodec.tryDecodeFrame(from: &rxBuffer) else {
                    break
                }
                handleFrame(frame)
            } catch let muxErr as MuxError {
                closeWithError(muxErr)
                return
            } catch {
                closeWithError(.invalidPayload(error.localizedDescription))
                return
            }
        }
    }

    private func handleFrame(_ frame: MuxFrame) {
        switch frame.kind {
        case .rpcRes:
            guard let cont = pendingRpc.removeValue(forKey: frame.channel) else { return }
            guard let obj = try? JSONSerialization.jsonObject(with: frame.payload) as? [String: Any] else {
                cont.resume(throwing: MuxError.invalidPayload("invalid rpc response json"))
                return
            }
            if obj["ok"] as? Bool == true {
                if let dataObj = obj["data"], !(dataObj is NSNull),
                   let dataBytes = try? JSONSerialization.data(withJSONObject: dataObj) {
                    cont.resume(returning: dataBytes)
                } else {
                    cont.resume(returning: Data("null".utf8))
                }
            } else {
                let msg = obj["error"] as? String ?? "server returned error"
                cont.resume(throwing: MuxError.serverError(msg))
            }

        case .event:
            let ev = MuxEvent.decode(from: frame.payload)
            for (_, cont) in eventContinuations {
                cont.yield(ev)
            }

        case .ptySnapshot:
            if let decoded = try? MuxCodec.decodeSnapshot(frame.payload) {
                streamContinuations[frame.channel]?.yield(.snapshot(decoded))
            }

        case .ptyOut:
            streamContinuations[frame.channel]?.yield(.output(frame.payload))

        case .pong:
            guard let entry = pendingPing.removeValue(forKey: frame.channel),
                  let echoedNs = try? MuxCodec.decodeTimestamp(frame.payload)
            else { return }
            let nowNs = DispatchTime.now().uptimeNanoseconds
            let delta = nowNs >= echoedNs ? (nowNs - echoedNs) : (nowNs - entry.sentNs)
            entry.cont.resume(returning: Double(delta) / 1_000_000.0)

        case .ping:
            guard let conn = connection, !isClosed else { return }
            let pong = MuxFrame(kind: .pong, channel: frame.channel, payload: frame.payload)
            if let bytes = try? MuxCodec.encodeFrame(pong) {
                conn.send(content: bytes, completion: .contentProcessed { _ in })
            }

        case .rpcReq, .ptyIn, .ptyResize:
            break
        }
    }

    private func failRpc(reqId: UInt32, error: Error) {
        pendingRpc.removeValue(forKey: reqId)?.resume(throwing: error)
    }

    private func failPing(seq: UInt32, error: Error) {
        pendingPing.removeValue(forKey: seq)?.cont.resume(throwing: error)
    }

    private func removeEventSubscriber(id: UUID) {
        eventContinuations.removeValue(forKey: id)?.finish()
    }

    private func closeWithError(_ err: MuxError) {
        guard !isClosed else { return }
        isClosed = true
        readerTask?.cancel()
        readerTask = nil
        connection?.stateUpdateHandler = nil
        connection?.cancel()
        connection = nil

        for (_, cont) in pendingRpc {
            cont.resume(throwing: err)
        }
        pendingRpc.removeAll()

        for (_, entry) in pendingPing {
            entry.cont.resume(throwing: err)
        }
        pendingPing.removeAll()

        for (_, cont) in streamContinuations {
            cont.finish()
        }
        streamContinuations.removeAll()

        for (_, cont) in eventContinuations {
            cont.finish()
        }
        eventContinuations.removeAll()
    }

    private func decodeCreatedEntities(
        _ data: Data,
        expectSession: Bool
    ) throws -> (SessionInfo, WindowInfo, BlockInfo) {
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw MuxError.invalidPayload("invalid creation response")
        }
        let decoder = JSONDecoder()
        let sessionInfo: SessionInfo
        if expectSession {
            guard let sObj = obj["session"] else { throw MuxError.invalidPayload("missing session") }
            sessionInfo = try decoder.decode(SessionInfo.self, from: JSONSerialization.data(withJSONObject: sObj))
        } else {
            sessionInfo = SessionInfo(id: "", name: "", windows: [], activeWindow: nil, createdAt: 0)
        }
        guard let wObj = obj["window"], let bObj = obj["block"] else {
            throw MuxError.invalidPayload("missing window or block")
        }
        let windowInfo = try decoder.decode(WindowInfo.self, from: JSONSerialization.data(withJSONObject: wObj))
        let blockInfo = try decoder.decode(BlockInfo.self, from: JSONSerialization.data(withJSONObject: bObj))
        return (sessionInfo, windowInfo, blockInfo)
    }

    private nonisolated static func scheduleReceive(
        on conn: NWConnection,
        cont: AsyncStream<Result<Data, MuxError>>.Continuation
    ) {
        conn.receive(minimumIncompleteLength: 1, maximumLength: 65_536) { data, _, isComplete, error in
            if let data, !data.isEmpty {
                cont.yield(.success(data))
            }
            if let error {
                cont.yield(.failure(.connectionFailed(error.localizedDescription)))
                cont.finish()
                return
            }
            if isComplete {
                cont.yield(.failure(.disconnected("server closed stream")))
                cont.finish()
                return
            }
            scheduleReceive(on: conn, cont: cont)
        }
    }
}
