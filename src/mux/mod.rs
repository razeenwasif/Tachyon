//! Tachyon Mux v2: portable multi-host protocol, state model, transports,
//! host registry, bearer-token auth helpers, and multiplexed client.

pub mod client;
pub mod model;
pub mod proto;
pub mod transport;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use model::HostEntry;

/// Default TCP port for direct Tailscale / LAN Mux v2 listeners (`--listen`).
pub const DEFAULT_TCP_PORT: u16 = 7501;

/// Local server endpoint path:
/// - Unix (macOS & Linux): `$TACHYON_SOCKET` -> `$XDG_RUNTIME_DIR/tachyon/server.sock`
///   -> `~/.local/state/tachyon/server.sock` -> temp dir.
/// - Windows: `%TACHYON_SOCKET%` -> `\\.\pipe\tachyon-server-<username>`.
pub fn default_socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("TACHYON_SOCKET").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
        let safe: String = user
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        let suffix = if safe.is_empty() { "user" } else { &safe };
        return PathBuf::from(format!(r"\\.\pipe\tachyon-server-{suffix}"));
    }
    #[cfg(not(windows))]
    {
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
            return PathBuf::from(dir).join("tachyon").join("server.sock");
        }
        if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return PathBuf::from(home).join(".local/state/tachyon/server.sock");
        }
        std::env::temp_dir().join("tachyon-server.sock")
    }
}

/// Human-readable URI for the default local endpoint (`unix://...` or `pipe://...`).
pub fn default_local_uri() -> String {
    let p = default_socket_path();
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        let trimmed = s.strip_prefix(r"\\.\pipe\").unwrap_or(&s);
        format!("pipe://{trimmed}")
    }
    #[cfg(not(windows))]
    {
        format!("unix://{}", p.display())
    }
}

/// Default path for the configured remote hosts file (`hosts.json`).
pub fn hosts_config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("TACHYON_HOSTS_FILE").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA").filter(|v| !v.is_empty()) {
            return PathBuf::from(appdata).join("Tachyon").join("hosts.json");
        }
        if let Some(profile) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
            return PathBuf::from(profile).join(".config/Tachyon/hosts.json");
        }
        return PathBuf::from("hosts.json");
    }
    #[cfg(not(windows))]
    {
        if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return PathBuf::from(home).join(".config/Tachyon/hosts.json");
        }
        PathBuf::from("hosts.json")
    }
}

/// Default path for the server's bearer auth token file used by `--listen` TCP mode.
pub fn default_token_path() -> PathBuf {
    if let Some(p) = std::env::var_os("TACHYON_TOKEN_FILE").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            return PathBuf::from(local).join("Tachyon").join("server.token");
        }
        if let Some(profile) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
            return PathBuf::from(profile).join(".local/state/tachyon/server.token");
        }
        return std::env::temp_dir().join("tachyon-server.token");
    }
    #[cfg(not(windows))]
    {
        if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return PathBuf::from(home).join(".local/state/tachyon/server.token");
        }
        std::env::temp_dir().join("tachyon-server.token")
    }
}

/// Generate a cryptographically random 256-bit (64-hex-char) bearer token.
pub fn generate_auth_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    fill_os_random(&mut bytes)?;
    let mut hex = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{b:02x}");
    }
    Ok(hex)
}

#[cfg(unix)]
fn fill_os_random(buf: &mut [u8]) -> io::Result<()> {
    use std::io::Read;
    let mut f = fs::File::open("/dev/urandom")?;
    f.read_exact(buf)
}

#[cfg(windows)]
fn fill_os_random(buf: &mut [u8]) -> io::Result<()> {
    use windows::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    let status = unsafe { BCryptGenRandom(None, buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if status.is_ok() {
        Ok(())
    } else {
        Err(io::Error::other(format!("BCryptGenRandom failed: {status:?}")))
    }
}

#[cfg(not(any(unix, windows)))]
fn fill_os_random(_buf: &mut [u8]) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "OS random generator unavailable on this platform",
    ))
}

/// Write `token` to `path` with owner-only permissions (`0600` on Unix).
pub fn write_token_file(path: &Path, token: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("token.tmp");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(format!("{}\n", token.trim()).as_bytes())?;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        fs::write(&tmp, format!("{}\n", token.trim()))?;
    }
    fs::rename(&tmp, path)
}

/// Load a non-empty bearer token from `path`, or create and persist a new one.
/// Returns `(token, was_created)`.
pub fn load_or_create_token(path: &Path) -> io::Result<(String, bool)> {
    if let Ok(raw) = fs::read_to_string(path) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Ok((trimmed.to_string(), false));
        }
    }
    let token = generate_auth_token()?;
    write_token_file(path, &token)?;
    Ok((token, true))
}

/// Constant-time byte-slice comparison for bearer token verification.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Normalize a Windows named-pipe URI/path into `\\.\pipe\<name>`.
pub fn normalize_pipe_path(raw: &str) -> PathBuf {
    let trimmed = raw.trim().trim_start_matches('/');
    if trimmed.starts_with(r"\\.\pipe\") || trimmed.starts_with("//./pipe/") {
        return PathBuf::from(trimmed.replace('/', "\\"));
    }
    PathBuf::from(format!(r"\\.\pipe\{trimmed}"))
}

/// Normalize a TCP host[:port] address, defaulting the port to `7501`.
pub fn normalize_tcp_addr(raw: &str) -> io::Result<String> {
    let s = raw.trim().trim_end_matches('/');
    if s.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty TCP address"));
    }
    // IPv6 bracketed form: `[::1]:7501` or `[::1]`.
    if let Some(rest) = s.strip_prefix('[') {
        let Some((host, after)) = rest.split_once(']') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unclosed IPv6 bracket in `{s}`"),
            ));
        };
        if after.is_empty() {
            return Ok(format!("[{host}]:{DEFAULT_TCP_PORT}"));
        }
        if let Some(port_str) = after.strip_prefix(':') {
            let port: u16 = port_str.parse().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("invalid TCP port `{port_str}`"))
            })?;
            return Ok(format!("[{host}]:{port}"));
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid TCP address `{s}`"),
        ));
    }
    // Hostname or IPv4 with explicit `:port`.
    if let Some((host, port_str)) = s.rsplit_once(':') {
        if !host.contains(':') {
            if host.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("missing host in `{s}`"),
                ));
            }
            let port: u16 = port_str.parse().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("invalid TCP port `{port_str}`"))
            })?;
            return Ok(format!("{host}:{port}"));
        }
    }
    // Bare IPv6 address without brackets (multiple colons).
    if s.contains(':') {
        return Ok(format!("[{s}]:{DEFAULT_TCP_PORT}"));
    }
    Ok(format!("{s}:{DEFAULT_TCP_PORT}"))
}

/// Resolve a `--listen` bind specification into `ip:port`.
///
/// Supports:
/// - `tailscale` or `tailscale:<port>` -> discovers the host's Tailscale IPv4 (`100.x.y.z`)
/// - `<ip>:<port>` or `<ip>` (defaults to port `7501`)
pub fn resolve_listen_addr(spec: &str) -> io::Result<String> {
    let trimmed = spec.trim();
    if trimmed.eq_ignore_ascii_case("tailscale") {
        let ip = detect_tailscale_ipv4()?;
        return Ok(format!("{ip}:{DEFAULT_TCP_PORT}"));
    }
    if let Some(rest) = trimmed
        .strip_prefix("tailscale:")
        .or_else(|| trimmed.strip_prefix("TAILSCALE:"))
    {
        let port: u16 = rest.parse().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("invalid Tailscale port `{rest}`"))
        })?;
        let ip = detect_tailscale_ipv4()?;
        return Ok(format!("{ip}:{port}"));
    }
    normalize_tcp_addr(trimmed)
}

/// Detect the local machine's Tailscale IPv4 address (`tailscale ip -4`).
pub fn detect_tailscale_ipv4() -> io::Result<String> {
    let candidates: &[&str] = &[
        "tailscale",
        #[cfg(target_os = "macos")]
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        #[cfg(target_os = "macos")]
        "/opt/homebrew/bin/tailscale",
        #[cfg(unix)]
        "/usr/local/bin/tailscale",
        #[cfg(windows)]
        r"C:\Program Files\Tailscale\tailscale.exe",
    ];
    for bin in candidates {
        let Ok(out) = std::process::Command::new(bin).args(["ip", "-4"]).output() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            let ip = line.trim();
            if ip.parse::<std::net::Ipv4Addr>().is_ok() {
                return Ok(ip.to_string());
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "could not detect Tailscale IPv4 via `tailscale ip -4` (pass an explicit IP:port instead)",
    ))
}

/// Resolved connection target for `-S` / `--server` / host entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Local { socket: PathBuf },
    Ssh {
        /// Display alias (`kagutsuchi`, etc.).
        alias: String,
        /// SSH destination passed to `ssh` (`kagutsuchi` or `user@host`).
        destination: String,
        /// Optional SSH port parsed from `ssh://host:port`.
        port: Option<u16>,
        /// Optional explicit remote `tachyon` binary path.
        remote_bin: Option<String>,
    },
    Tcp {
        /// Display alias (`linux-pc`, `win-pc`, etc.).
        alias: String,
        /// `host:port` (e.g. `100.88.12.34:7501` or `my-pc.tailnet.ts.net:7501`).
        addr: String,
        /// Bearer authentication token sent in `hello`.
        token: Option<String>,
    },
}

impl Target {
    pub fn display_name(&self) -> String {
        match self {
            Self::Local { .. } => "local".to_string(),
            Self::Ssh { alias, .. } | Self::Tcp { alias, .. } => alias.clone(),
        }
    }
}

/// Load configured hosts from `path` (missing file -> empty list).
pub fn load_hosts_from(path: &Path) -> Vec<HostEntry> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    v.get("hosts")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(HostEntry::from_json).collect())
        .unwrap_or_default()
}

pub fn load_hosts() -> Vec<HostEntry> {
    load_hosts_from(&hosts_config_path())
}

/// Save configured hosts atomically to `path` (`0600` on Unix since entries may
/// contain TCP bearer tokens).
pub fn save_hosts_to(path: &Path, hosts: &[HostEntry]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = json!({
        "hosts": hosts.iter().map(HostEntry::to_json).collect::<Vec<_>>(),
    });
    let text = serde_json::to_string_pretty(&body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(format!("{text}\n").as_bytes())?;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        fs::write(&tmp, format!("{text}\n"))?;
    }
    fs::rename(&tmp, path)
}

pub fn save_hosts(hosts: &[HostEntry]) -> io::Result<()> {
    save_hosts_to(&hosts_config_path(), hosts)
}

/// Parse a URI (`unix:///path`, `pipe://name`, `ssh://[user@]host[:port]`,
/// `tcp://host[:port][?token=...]`) or alias into a `Target`.
pub fn resolve_target(spec: Option<&str>, hosts: &[HostEntry]) -> io::Result<Target> {
    let raw = match spec.map(str::trim).filter(|s| !s.is_empty()) {
        None => {
            if let Ok(env) = std::env::var("TACHYON_SERVER") {
                if !env.trim().is_empty() {
                    return resolve_target(Some(env.trim()), hosts);
                }
            }
            return Ok(Target::Local { socket: default_socket_path() });
        }
        Some(s) => s,
    };

    if raw == "local" || raw == "localhost" {
        return Ok(Target::Local { socket: default_socket_path() });
    }

    if let Some(entry) = hosts.iter().find(|h| h.alias == raw) {
        return parse_uri(&entry.alias, &entry.uri, entry.remote_bin.clone(), entry.token.clone());
    }

    if raw.contains("://") {
        let alias = raw
            .strip_prefix("ssh://")
            .or_else(|| raw.strip_prefix("tcp://"))
            .map(|s| s.split(['/', '?']).next().unwrap_or(s).to_string())
            .unwrap_or_else(|| raw.to_string());
        return parse_uri(&alias, raw, None, None);
    }

    // Bare name not in hosts.json: treat as an SSH host from ~/.ssh/config.
    parse_uri(raw, &format!("ssh://{raw}"), None, None)
}

fn parse_uri(
    alias: &str,
    uri: &str,
    remote_bin: Option<String>,
    token: Option<String>,
) -> io::Result<Target> {
    if uri == "local" {
        return Ok(Target::Local { socket: default_socket_path() });
    }
    if let Some(path) = uri.strip_prefix("unix://") {
        if path.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty unix:// socket path"));
        }
        return Ok(Target::Local { socket: PathBuf::from(path) });
    }
    if let Some(pipe) = uri.strip_prefix("pipe://") {
        if pipe.trim().is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty pipe:// name"));
        }
        return Ok(Target::Local { socket: normalize_pipe_path(pipe) });
    }
    if let Some(rest) = uri.strip_prefix("tcp://") {
        let (addr_part, query) = match rest.split_once('?') {
            Some((a, q)) => (a, Some(q)),
            None => (rest, None),
        };
        let addr = normalize_tcp_addr(addr_part)?;
        let query_token = query.and_then(|q| {
            q.split('&')
                .find_map(|kv| kv.strip_prefix("token="))
                .filter(|t| !t.is_empty())
                .map(str::to_string)
        });
        let resolved_token = query_token
            .or(token)
            .or_else(|| std::env::var("TACHYON_TOKEN").ok().filter(|v| !v.is_empty()));
        return Ok(Target::Tcp {
            alias: alias.to_string(),
            addr,
            token: resolved_token,
        });
    }
    if let Some(rest) = uri.strip_prefix("ssh://") {
        let rest = rest.trim_end_matches('/');
        if rest.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty ssh:// host"));
        }
        // Split optional `:port` from the host portion (preserving `user@`).
        let (destination, port) = if let Some((head, p)) = rest.rsplit_once(':') {
            if !head.contains(']') && p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() {
                let port: u16 = p.parse().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, format!("invalid ssh port: {p}"))
                })?;
                (head.to_string(), Some(port))
            } else {
                (rest.to_string(), None)
            }
        } else {
            (rest.to_string(), None)
        };
        return Ok(Target::Ssh {
            alias: alias.to_string(),
            destination,
            port,
            remote_bin,
        });
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("unsupported host URI `{uri}` (expected ssh://, tcp://, unix://, or pipe://)"),
    ))
}

/// Current UNIX timestamp in seconds.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Monotonic timestamp in nanoseconds for PING/PONG RTT checks.
pub fn mono_now_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Local hostname for `HostInfo`.
pub fn local_hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(s) = std::str::from_utf8(&buf[..end]) {
                let s = s.trim().trim_end_matches(".local");
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "localhost".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_local_aliases_ssh_tcp_and_pipe_uris() {
        let hosts = vec![
            HostEntry {
                alias: "mini".into(),
                uri: "ssh://razeen@kagutsuchi:2222".into(),
                remote_bin: Some("/opt/bin/tachyon".into()),
                token: None,
                color: Some("blue".into()),
            },
            HostEntry {
                alias: "winpc".into(),
                uri: "tcp://100.88.12.34".into(),
                remote_bin: None,
                token: Some("secret-tok".into()),
                color: None,
            },
        ];

        assert!(matches!(
            resolve_target(Some("local"), &hosts).unwrap(),
            Target::Local { .. }
        ));
        assert_eq!(
            resolve_target(Some("unix:///tmp/t.sock"), &hosts).unwrap(),
            Target::Local { socket: PathBuf::from("/tmp/t.sock") }
        );
        assert_eq!(
            resolve_target(Some("pipe://tachyon-server-me"), &hosts).unwrap(),
            Target::Local { socket: PathBuf::from(r"\\.\pipe\tachyon-server-me") }
        );
        assert_eq!(
            resolve_target(Some("mini"), &hosts).unwrap(),
            Target::Ssh {
                alias: "mini".into(),
                destination: "razeen@kagutsuchi".into(),
                port: Some(2222),
                remote_bin: Some("/opt/bin/tachyon".into()),
            }
        );
        assert_eq!(
            resolve_target(Some("winpc"), &hosts).unwrap(),
            Target::Tcp {
                alias: "winpc".into(),
                addr: "100.88.12.34:7501".into(),
                token: Some("secret-tok".into()),
            }
        );
        assert_eq!(
            resolve_target(Some("tcp://[::1]:7509?token=abc"), &hosts).unwrap(),
            Target::Tcp {
                alias: "[::1]:7509".into(),
                addr: "[::1]:7509".into(),
                token: Some("abc".into()),
            }
        );
        assert_eq!(
            resolve_target(Some("kagutsuchi"), &hosts).unwrap(),
            Target::Ssh {
                alias: "kagutsuchi".into(),
                destination: "kagutsuchi".into(),
                port: None,
                remote_bin: None,
            }
        );
    }

    #[test]
    fn hosts_file_roundtrip_with_token() {
        let dir = std::env::temp_dir().join(format!("tachyon-hosts-test-{}", std::process::id()));
        let path = dir.join("hosts.json");
        let list = vec![
            HostEntry {
                alias: "kagutsuchi".into(),
                uri: "ssh://kagutsuchi".into(),
                remote_bin: None,
                token: None,
                color: Some("blue".into()),
            },
            HostEntry {
                alias: "forge".into(),
                uri: "ssh://forge".into(),
                remote_bin: Some("~/.local/bin/tachyon".into()),
                token: None,
                color: Some("amber".into()),
            },
            HostEntry {
                alias: "amaterasu".into(),
                uri: "tcp://100.64.0.9:7501".into(),
                remote_bin: None,
                token: Some("deadbeef".into()),
                color: Some("purple".into()),
            },
        ];
        save_hosts_to(&path, &list).unwrap();
        assert_eq!(load_hosts_from(&path), list);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_generation_persistence_and_constant_time_compare() {
        let dir = std::env::temp_dir().join(format!("tachyon-tok-test-{}", std::process::id()));
        let path = dir.join("server.token");
        let _ = fs::remove_dir_all(&dir);

        let (t1, created1) = load_or_create_token(&path).unwrap();
        assert!(created1);
        assert_eq!(t1.len(), 64);
        assert!(t1.chars().all(|c| c.is_ascii_hexdigit()));

        let (t2, created2) = load_or_create_token(&path).unwrap();
        assert!(!created2);
        assert_eq!(t1, t2);
        assert!(ct_eq(t1.as_bytes(), t2.as_bytes()));
        assert!(!ct_eq(t1.as_bytes(), b"wrong"));
        let _ = fs::remove_dir_all(&dir);
    }
}
