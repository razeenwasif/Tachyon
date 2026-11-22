//! Mobile pairing URI builder, parser, and QR helper for Tachyon Mux v2.

use serde_json::{json, Value};

use super::model::{HostColor, HostEntry, HostInfo};
use super::proto::PROTOCOL_VERSION;
use super::{detect_tailscale_ipv4, normalize_tcp_addr, DEFAULT_TCP_PORT};

/// Versioned pairing payload encoded into `tachyon://pair?...` links and QR codes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingPayload {
    pub version: u32,
    pub alias: String,
    pub host: String,
    pub port: u16,
    pub token: String,
    pub protocol: u32,
    pub os: String,
    pub arch: String,
    pub ssh: Option<String>,
    pub color: Option<String>,
}

impl PairingPayload {
    /// Returns the `tcp://host:port` connection URI (bracketing IPv6 literals).
    pub fn tcp_uri(&self) -> String {
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("tcp://[{}]:{}", self.host, self.port)
        } else {
            format!("tcp://{}:{}", self.host, self.port)
        }
    }

    /// Encodes the payload as `tachyon://pair?v=1&...`.
    pub fn to_uri(&self) -> String {
        let mut params = vec![
            format!("v={}", self.version),
            format!("host={}", pct_encode(&self.host)),
            format!("port={}", self.port),
            format!("token={}", pct_encode(&self.token)),
            format!("alias={}", pct_encode(&self.alias)),
            format!("proto={}", self.protocol),
            format!("os={}", pct_encode(&self.os)),
            format!("arch={}", pct_encode(&self.arch)),
        ];
        if let Some(ssh) = self.ssh.as_deref().filter(|s| !s.is_empty()) {
            params.push(format!("ssh={}", pct_encode(ssh)));
        }
        if let Some(color) = self.color.as_deref().filter(|s| !s.is_empty()) {
            params.push(format!("color={}", pct_encode(color)));
        }
        format!("tachyon://pair?{}", params.join("&"))
    }

    /// Parses and validates a `tachyon://pair?...` URI.
    pub fn from_uri(uri: &str) -> Result<Self, String> {
        let trimmed = uri.trim();
        let query = trimmed
            .strip_prefix("tachyon://pair?")
            .ok_or_else(|| "pairing URI must start with `tachyon://pair?`".to_string())?;

        let mut version: Option<u32> = None;
        let mut host: Option<String> = None;
        let mut port: Option<u16> = None;
        let mut token: Option<String> = None;
        let mut alias: Option<String> = None;
        let mut protocol: Option<u32> = None;
        let mut os: Option<String> = None;
        let mut arch: Option<String> = None;
        let mut ssh: Option<String> = None;
        let mut color: Option<String> = None;

        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let Some((k, v)) = pair.split_once('=') else {
                continue;
            };
            let decoded = pct_decode(v)?;
            match k {
                "v" => {
                    let vnum: u32 = decoded.parse().map_err(|_| "invalid pairing version `v`")?;
                    version = Some(vnum);
                }
                "host" => host = Some(decoded),
                "port" => {
                    let pnum: u16 = decoded.parse().map_err(|_| "invalid pairing `port`")?;
                    if pnum == 0 {
                        return Err("pairing `port` must be non-zero".into());
                    }
                    port = Some(pnum);
                }
                "token" => token = Some(decoded),
                "alias" => alias = Some(decoded),
                "proto" | "protocol" => {
                    let p: u32 = decoded.parse().map_err(|_| "invalid pairing `proto`")?;
                    protocol = Some(p);
                }
                "os" => os = Some(decoded),
                "arch" => arch = Some(decoded),
                "ssh" if !decoded.is_empty() => ssh = Some(decoded),
                "color" if !decoded.is_empty() => color = Some(decoded),
                _ => {}
            }
        }

        let ver = version.unwrap_or(1);
        if ver != 1 {
            return Err(format!("unsupported pairing URI version `v={ver}`"));
        }
        let host = host.filter(|s| !s.trim().is_empty()).ok_or("missing pairing `host`")?;
        let token = token.filter(|s| !s.trim().is_empty()).ok_or("missing pairing `token`")?;
        let alias = alias
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| host.clone());

        Ok(Self {
            version: ver,
            alias,
            host,
            port: port.unwrap_or(DEFAULT_TCP_PORT),
            token,
            protocol: protocol.unwrap_or(PROTOCOL_VERSION),
            os: os.unwrap_or_else(|| "unknown".into()),
            arch: arch.unwrap_or_else(|| "unknown".into()),
            ssh,
            color,
        })
    }

    /// Converts this pairing payload into a saved `HostEntry` for `hosts.json`.
    pub fn to_host_entry(&self) -> HostEntry {
        let color = self
            .color
            .as_deref()
            .filter(|spec| HostColor::is_valid_spec(spec))
            .map(str::to_string);
        HostEntry {
            alias: self.alias.clone(),
            uri: self.tcp_uri(),
            remote_bin: None,
            token: Some(self.token.clone()),
            color,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "version": self.version,
            "alias": self.alias,
            "host": self.host,
            "port": self.port,
            "tcp_uri": self.tcp_uri(),
            "token": self.token,
            "protocol": self.protocol,
            "os": self.os,
            "arch": self.arch,
            "ssh": self.ssh,
            "color": self.color,
            "pairing_uri": self.to_uri(),
        })
    }
}

/// Parses `tcp://host[:port]`, `[IPv6]:port`, or `host[:port]` into `(host, port)`.
pub fn parse_host_port(addr: &str) -> Result<(String, u16), String> {
    let trimmed = addr.trim();
    let without_scheme = trimmed.strip_prefix("tcp://").unwrap_or(trimmed);
    let clean = without_scheme.split('?').next().unwrap_or(without_scheme);
    let normalized = normalize_tcp_addr(clean).map_err(|e| e.to_string())?;
    if let Some(rest) = normalized.strip_prefix('[') {
        let (h, p_str) = rest
            .split_once("]:")
            .ok_or_else(|| format!("invalid bracketed IPv6 address `{addr}`"))?;
        let port: u16 = p_str
            .parse()
            .map_err(|_| format!("invalid TCP port `{p_str}`"))?;
        return Ok((h.to_string(), port));
    }
    let (h, p_str) = normalized
        .rsplit_once(':')
        .ok_or_else(|| format!("invalid TCP address `{addr}`"))?;
    let port: u16 = p_str
        .parse()
        .map_err(|_| format!("invalid TCP port `{p_str}`"))?;
    Ok((h.to_string(), port))
}

/// Builds a pairing payload for the local host (`This Mac` or current server).
///
/// Endpoint precedence: `explicit_addr` -> `running_listen_addr` -> `detect_tailscale_ipv4()`
/// -> `127.0.0.1:7501`. Returns `(payload, tcp_listener_ready)`.
pub fn build_local_pairing(
    token: &str,
    explicit_addr: Option<&str>,
    running_listen_addr: Option<&str>,
    alias_override: Option<&str>,
    hostname: &str,
) -> (PairingPayload, bool) {
    let explicit = explicit_addr.map(str::trim).filter(|s| !s.is_empty());
    let running = running_listen_addr.map(str::trim).filter(|s| !s.is_empty());

    let (host, port, ready) = if let Some(addr) = explicit {
        match parse_host_port(addr) {
            Ok((h, p)) => (h, p, running.is_some()),
            Err(_) => ("127.0.0.1".into(), DEFAULT_TCP_PORT, false),
        }
    } else if let Some(addr) = running {
        match parse_host_port(addr) {
            Ok((h, p)) => (h, p, true),
            Err(_) => ("127.0.0.1".into(), DEFAULT_TCP_PORT, false),
        }
    } else if let Ok(ts_ip) = detect_tailscale_ipv4() {
        (ts_ip, DEFAULT_TCP_PORT, false)
    } else {
        ("127.0.0.1".into(), DEFAULT_TCP_PORT, false)
    };

    let raw_alias = alias_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(hostname);
    let alias = if raw_alias.is_empty() {
        "local".to_string()
    } else {
        raw_alias.to_string()
    };

    let payload = PairingPayload {
        version: 1,
        alias,
        host,
        port,
        token: token.trim().to_string(),
        protocol: PROTOCOL_VERSION,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        ssh: (!hostname.trim().is_empty()).then(|| hostname.trim().to_string()),
        color: None,
    };
    (payload, ready)
}

/// Builds a pairing payload for a configured `tcp://` remote host entry.
pub fn build_remote_pairing(
    entry: &HostEntry,
    live_info: Option<&HostInfo>,
) -> Result<PairingPayload, String> {
    if !entry.uri.trim().starts_with("tcp://") {
        return Err(format!(
            "host `{}` uses `{}` — add it with `tcp://` or pair with `--addr <tailscale-ip>:7501`",
            entry.alias, entry.uri
        ));
    }
    let token = entry
        .token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("host `{}` has no saved bearer token", entry.alias))?;
    let (host, port) = parse_host_port(&entry.uri)?;
    Ok(PairingPayload {
        version: 1,
        alias: entry.alias.clone(),
        host,
        port,
        token: token.to_string(),
        protocol: live_info.map_or(PROTOCOL_VERSION, |i| i.protocol),
        os: live_info.map_or_else(|| "unknown".into(), |i| i.os.clone()),
        arch: live_info.map_or_else(|| "unknown".into(), |i| i.arch.clone()),
        ssh: Some(entry.alias.clone()),
        color: entry.color.clone(),
    })
}

fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(&mut out, "%{b:02X}");
        }
    }
    out
}

fn pct_decode(s: &str) -> Result<String, String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err("incomplete percent-escape in pairing URI".into());
            }
            let hi = from_hex(bytes[i + 1])?;
            let lo = from_hex(bytes[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "non-UTF-8 percent escape in pairing URI".into())
}

fn from_hex(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err("invalid hex digit in pairing URI".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::qr::QrCode;

    #[test]
    fn pairing_uri_round_trips_ipv4_ipv6_special_chars_and_qr() {
        let p = PairingPayload {
            version: 1,
            alias: "mac mini lab+1".into(),
            host: "100.109.107.16".into(),
            port: 7501,
            token: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            protocol: 2,
            os: "macos".into(),
            arch: "aarch64".into(),
            ssh: Some("kagutsuchi".into()),
            color: Some("teal".into()),
        };
        let uri = p.to_uri();
        assert!(uri.starts_with("tachyon://pair?"));
        let decoded = PairingPayload::from_uri(&uri).expect("decode uri");
        assert_eq!(decoded, p);

        let entry = decoded.to_host_entry();
        assert_eq!(entry.alias, "mac mini lab+1");
        assert_eq!(entry.uri, "tcp://100.109.107.16:7501");
        assert_eq!(entry.token.as_deref(), Some(p.token.as_str()));
        assert_eq!(entry.color.as_deref(), Some("teal"));

        let qr = QrCode::encode(uri.as_bytes()).expect("encode pairing QR");
        assert!(qr.size() >= 45);

        // IPv6 host round-trip + tcp_uri bracketing.
        let v6 = PairingPayload {
            version: 1,
            alias: "v6box".into(),
            host: "fd7a:115c:a1e0::42".into(),
            port: 7502,
            token: "tok_abc123".into(),
            protocol: 2,
            os: "linux".into(),
            arch: "x86_64".into(),
            ssh: None,
            color: None,
        };
        let v6_uri = v6.to_uri();
        let v6_dec = PairingPayload::from_uri(&v6_uri).expect("decode v6");
        assert_eq!(v6_dec, v6);
        assert_eq!(v6_dec.tcp_uri(), "tcp://[fd7a:115c:a1e0::42]:7502");
    }

    #[test]
    fn pairing_uri_rejects_invalid_inputs() {
        assert!(PairingPayload::from_uri("https://example.com").is_err());
        assert!(PairingPayload::from_uri("tachyon://pair?v=2&host=1.2.3.4&token=abc").is_err());
        assert!(PairingPayload::from_uri("tachyon://pair?v=1&token=abc").is_err());
        assert!(PairingPayload::from_uri("tachyon://pair?v=1&host=1.2.3.4").is_err());
        assert!(PairingPayload::from_uri("tachyon://pair?v=1&host=1.2.3.4&port=0&token=abc").is_err());
    }
}
