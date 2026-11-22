import Foundation

/// Versioned pairing payload matching `tachyon://pair?v=1&...` from `src/mux/pairing.rs`.
public struct PairingPayload: Sendable, Equatable, Codable {
    public var version: UInt32
    public var alias: String
    public var host: String
    public var port: UInt16
    public var token: String
    public var protocolVersion: UInt32
    public var os: String
    public var arch: String
    public var ssh: String?
    public var color: String?

    public init(
        version: UInt32 = 1,
        alias: String,
        host: String,
        port: UInt16 = defaultMuxPort,
        token: String,
        protocolVersion: UInt32 = muxProtocolVersion,
        os: String = "unknown",
        arch: String = "unknown",
        ssh: String? = nil,
        color: String? = nil
    ) {
        self.version = version
        self.alias = alias
        self.host = host
        self.port = port
        self.token = token
        self.protocolVersion = protocolVersion
        self.os = os
        self.arch = arch
        self.ssh = ssh
        self.color = color
    }

    /// Direct `tcp://host:port` URI (bracketing bare IPv6 literals).
    public var tcpURI: String {
        if host.contains(":") && !host.hasPrefix("[") {
            return "tcp://[\(host)]:\(port)"
        }
        return "tcp://\(host):\(port)"
    }

    /// Encode this payload as a `tachyon://pair?v=1&...` link.
    public func toURI() -> String {
        var params = [
            "v=\(version)",
            "host=\(Self.pctEncode(host))",
            "port=\(port)",
            "token=\(Self.pctEncode(token))",
            "alias=\(Self.pctEncode(alias))",
            "proto=\(protocolVersion)",
            "os=\(Self.pctEncode(os))",
            "arch=\(Self.pctEncode(arch))",
        ]
        if let ssh, !ssh.isEmpty {
            params.append("ssh=\(Self.pctEncode(ssh))")
        }
        if let color, !color.isEmpty {
            params.append("color=\(Self.pctEncode(color))")
        }
        return "tachyon://pair?\(params.joined(separator: "&"))"
    }

    /// Parse and validate a `tachyon://pair?...` pairing link.
    public static func parse(_ rawURI: String) throws -> PairingPayload {
        let trimmed = rawURI.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let range = trimmed.range(of: "tachyon://pair?"),
              range.lowerBound == trimmed.startIndex
        else {
            throw MuxError.invalidPairingURI("pairing URI must start with `tachyon://pair?`")
        }
        let query = String(trimmed[range.upperBound...])

        var version: UInt32?
        var host: String?
        var port: UInt16?
        var token: String?
        var alias: String?
        var proto: UInt32?
        var os: String?
        var arch: String?
        var ssh: String?
        var color: String?

        for pair in query.split(separator: "&", omittingEmptySubsequences: true) {
            guard let eq = pair.firstIndex(of: "=") else { continue }
            let k = String(pair[..<eq])
            let vRaw = String(pair[pair.index(after: eq)...])
            let decoded = try pctDecode(vRaw)
            switch k {
            case "v":
                guard let vNum = UInt32(decoded) else {
                    throw MuxError.invalidPairingURI("invalid pairing version `v`")
                }
                version = vNum
            case "host":
                host = decoded
            case "port":
                guard let pNum = UInt16(decoded) else {
                    throw MuxError.invalidPairingURI("invalid pairing `port`")
                }
                guard pNum != 0 else {
                    throw MuxError.invalidPairingURI("pairing `port` must be non-zero")
                }
                port = pNum
            case "token":
                token = decoded
            case "alias":
                alias = decoded
            case "proto", "protocol":
                guard let p = UInt32(decoded) else {
                    throw MuxError.invalidPairingURI("invalid pairing `proto`")
                }
                proto = p
            case "os":
                os = decoded
            case "arch":
                arch = decoded
            case "ssh" where !decoded.isEmpty:
                ssh = decoded
            case "color" where !decoded.isEmpty:
                color = decoded
            default:
                break
            }
        }

        let ver = version ?? 1
        guard ver == 1 else {
            throw MuxError.invalidPairingURI("unsupported pairing URI version `v=\(ver)`")
        }
        guard let cleanHost = host?.trimmingCharacters(in: .whitespacesAndNewlines),
              !cleanHost.isEmpty,
              let unwrappedHost = host, !unwrappedHost.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else {
            throw MuxError.invalidPairingURI("missing pairing `host`")
        }
        guard let unwrappedToken = token,
              !unwrappedToken.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else {
            throw MuxError.invalidPairingURI("missing pairing `token`")
        }
        let cleanAlias: String
        if let a = alias, !a.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            cleanAlias = a
        } else {
            cleanAlias = unwrappedHost
        }

        return PairingPayload(
            version: ver,
            alias: cleanAlias,
            host: cleanHost.isEmpty ? unwrappedHost : unwrappedHost,
            port: port ?? defaultMuxPort,
            token: unwrappedToken,
            protocolVersion: proto ?? muxProtocolVersion,
            os: os ?? "unknown",
            arch: arch ?? "unknown",
            ssh: ssh,
            color: color
        )
    }

    /// Percent-encode strictly matching RFC 3986 unreserved set (`A-Za-z0-9-_.~`), identical to Rust `pct_encode`.
    public static func pctEncode(_ s: String) -> String {
        var out = ""
        out.reserveCapacity(s.utf8.count)
        for b in s.utf8 {
            let isUnreserved = (b >= 65 && b <= 90)
                || (b >= 97 && b <= 122)
                || (b >= 48 && b <= 57)
                || b == 45 || b == 95 || b == 46 || b == 126
            if isUnreserved {
                out.append(Character(UnicodeScalar(b)))
            } else {
                out.append(String(format: "%%%02X", b))
            }
        }
        return out
    }

    /// Percent-decode `%XX` sequences without altering literal `+`.
    public static func pctDecode(_ s: String) throws -> String {
        let bytes = [UInt8](s.utf8)
        var out: [UInt8] = []
        out.reserveCapacity(bytes.count)
        var i = 0
        while i < bytes.count {
            if bytes[i] == 37 /* '%' */ {
                guard i + 2 < bytes.count,
                      let hi = hexVal(bytes[i + 1]),
                      let lo = hexVal(bytes[i + 2])
                else {
                    throw MuxError.invalidPairingURI("invalid percent-encoding in pairing URI")
                }
                out.append((hi << 4) | lo)
                i += 3
            } else {
                out.append(bytes[i])
                i += 1
            }
        }
        guard let decoded = String(bytes: out, encoding: .utf8) else {
            throw MuxError.invalidPairingURI("pairing parameter is not valid UTF-8")
        }
        return decoded
    }

    private static func hexVal(_ b: UInt8) -> UInt8? {
        switch b {
        case 48...57: return b - 48
        case 65...70: return b - 65 + 10
        case 97...102: return b - 97 + 10
        default: return nil
        }
    }
}
