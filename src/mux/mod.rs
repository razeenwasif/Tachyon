//! Tachyon Mux v2: portable multi-host protocol, state model, transports,
//! host registry, and multiplexed client.

pub mod client;
pub mod model;
pub mod proto;
pub mod transport;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use model::HostEntry;

/// Local server socket path.
///
/// Priority:
/// 1. `TACHYON_SOCKET` env override (used by tests and custom instances).
/// 2. `$XDG_RUNTIME_DIR/tachyon/server.sock` (Linux systemd user sessions).
/// 3. `~/.local/state/tachyon/server.sock` (macOS and fallback Unix).
pub fn default_socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("TACHYON_SOCKET").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("tachyon").join("server.sock");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(home).join(".local/state/tachyon/server.sock");
    }
    std::env::temp_dir().join("tachyon-server.sock")
}

/// Default path for the configured remote hosts file (`~/.config/Tachyon/hosts.json`).
pub fn hosts_config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("TACHYON_HOSTS_FILE").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(home).join(".config/Tachyon/hosts.json");
    }
    PathBuf::from("hosts.json")
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
}

impl Target {
    pub fn display_name(&self) -> String {
        match self {
            Self::Local { .. } => "local".to_string(),
            Self::Ssh { alias, .. } => alias.clone(),
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

/// Save configured hosts atomically to `path`.
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
    fs::write(&tmp, format!("{text}\n"))?;
    fs::rename(&tmp, path)
}

pub fn save_hosts(hosts: &[HostEntry]) -> io::Result<()> {
    save_hosts_to(&hosts_config_path(), hosts)
}

/// Parse a URI (`unix:///path`, `ssh://[user@]host[:port]`) or alias into a `Target`.
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
        return parse_uri(&entry.alias, &entry.uri, entry.remote_bin.clone());
    }

    if raw.contains("://") {
        let alias = raw
            .strip_prefix("ssh://")
            .map(|s| s.split('/').next().unwrap_or(s).to_string())
            .unwrap_or_else(|| raw.to_string());
        return parse_uri(&alias, raw, None);
    }

    // Bare name not in hosts.json: treat as an SSH host from ~/.ssh/config.
    parse_uri(raw, &format!("ssh://{raw}"), None)
}

fn parse_uri(alias: &str, uri: &str, remote_bin: Option<String>) -> io::Result<Target> {
    if uri == "local" {
        return Ok(Target::Local { socket: default_socket_path() });
    }
    if let Some(path) = uri.strip_prefix("unix://") {
        if path.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty unix:// socket path"));
        }
        return Ok(Target::Local { socket: PathBuf::from(path) });
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
        format!("unsupported host URI `{uri}` (expected ssh://... or unix://...)"),
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
    fn resolves_local_aliases_and_ssh_uris() {
        let hosts = vec![HostEntry {
            alias: "mini".into(),
            uri: "ssh://razeen@kagutsuchi:2222".into(),
            remote_bin: Some("/opt/bin/tachyon".into()),
        }];

        assert!(matches!(
            resolve_target(Some("local"), &hosts).unwrap(),
            Target::Local { .. }
        ));
        assert_eq!(
            resolve_target(Some("unix:///tmp/t.sock"), &hosts).unwrap(),
            Target::Local { socket: PathBuf::from("/tmp/t.sock") }
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
    fn hosts_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("tachyon-hosts-test-{}", std::process::id()));
        let path = dir.join("hosts.json");
        let list = vec![
            HostEntry { alias: "kagutsuchi".into(), uri: "ssh://kagutsuchi".into(), remote_bin: None },
            HostEntry { alias: "forge".into(), uri: "ssh://forge".into(), remote_bin: Some("~/.local/bin/tachyon".into()) },
        ];
        save_hosts_to(&path, &list).unwrap();
        assert_eq!(load_hosts_from(&path), list);
        let _ = fs::remove_dir_all(&dir);
    }
}
