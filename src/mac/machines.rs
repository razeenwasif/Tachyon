//! The machines this Mac can ssh into, from the `Host` entries in
//! `~/.ssh/config`: listed under Shell > New Tab On, and offered as inline
//! suggestions after `ssh `. A short TCP check on port 22, every minute on a
//! thread of its own, gives each a cached reachability dot.

use std::collections::HashMap;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub struct Machine {
    /// The name given first on its `Host` line; later names are aliases.
    pub name: String,
    /// Where it is (`HostName`), or its name when none is given.
    pub address: String,
    pub port: u16,
}

/// Aliases that name machines already listed under another name.
const ALIASES: [&str; 2] = ["mini", "mini-lan"];

/// The machines in an ssh config: each `Host` line's first plain name (not a
/// pattern), with its `HostName` and `Port`.
pub fn parse(config: &str) -> Vec<Machine> {
    let mut out: Vec<Machine> = Vec::new();
    // Settings after a Host line belong to it; after a pattern-only one, to
    // nothing listed.
    let mut current: Option<usize> = None;
    for line in config.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((k, v)) => (k.to_ascii_lowercase(), v.trim_start_matches(|c: char| c.is_whitespace() || c == '=').trim()),
            None => continue,
        };
        match key.as_str() {
            "host" => {
                current = value
                    .split_whitespace()
                    .find(|n| !n.contains(['*', '?', '!']))
                    .filter(|n| !ALIASES.contains(n))
                    .filter(|n| !out.iter().any(|m| m.name == *n))
                    .map(|n| {
                        out.push(Machine { name: n.to_string(), address: n.to_string(), port: 22 });
                        out.len() - 1
                    });
            }
            "match" => current = None,
            "hostname" => {
                if let Some(i) = current {
                    out[i].address = value.to_string();
                }
            }
            "port" => {
                if let (Some(i), Ok(p)) = (current, value.parse()) {
                    out[i].port = p;
                }
            }
            _ => {}
        }
    }
    out
}

/// The machines in `~/.ssh/config`.
pub fn load() -> Vec<Machine> {
    let Some(home) = std::env::var_os("HOME") else { return Vec::new() };
    let path = std::path::Path::new(&home).join(".ssh/config");
    std::fs::read_to_string(path).map(|t| parse(&t)).unwrap_or_default()
}

/// The machines, read once.
pub fn all() -> &'static [Machine] {
    static ALL: OnceLock<Vec<Machine>> = OnceLock::new();
    ALL.get_or_init(load)
}

/// Whether `m` answers on its ssh port within a second and a half.
pub fn reachable(m: &Machine) -> bool {
    let Ok(addrs) = (m.address.as_str(), m.port).to_socket_addrs() else { return false };
    addrs.into_iter().any(|a| TcpStream::connect_timeout(&a, Duration::from_millis(1500)).is_ok())
}

fn cache() -> &'static Mutex<HashMap<String, bool>> {
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The last check of a machine, if there's been one.
pub fn status(name: &str) -> Option<bool> {
    cache().lock().ok()?.get(name).copied()
}

/// Check every machine now and then each minute, on a thread of its own;
/// `checked` is called after each round.
pub fn start_checks(checked: impl Fn() + Send + 'static) {
    let _ = std::thread::Builder::new().name("tachyon-machines".into()).spawn(move || loop {
        let results: Vec<(String, bool)> = std::thread::scope(|s| {
            let handles: Vec<_> = all().iter().map(|m| s.spawn(move || (m.name.clone(), reachable(m)))).collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        if let Ok(mut c) = cache().lock() {
            c.extend(results);
        }
        checked();
        std::thread::sleep(Duration::from_secs(60));
    });
}

/// A menu title for a machine: a dot for how its last check went.
pub fn title(m: &Machine) -> String {
    let dot = match status(&m.name) {
        Some(true) => "🟢",
        Some(false) => "⚪️",
        None => "…",
    };
    format!("{dot} {}", m.name)
}

/// The rest of a machine's name after `ssh ` and what's typed of it.
pub fn suggest<'a>(typed: &str, machines: &'a [Machine]) -> Option<&'a str> {
    let partial = typed.strip_prefix("ssh ")?;
    if partial.contains(char::is_whitespace) {
        return None;
    }
    machines.iter().find(|m| m.name.len() > partial.len() && m.name.starts_with(partial)).map(|m| &m.name[partial.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "\
# Duet cluster nodes
Host kagutsuchi mini
    HostName 100.97.247.34
    User razeenwasif

Host kagutsuchi-lan mini-lan
    HostName 192.168.68.62

Host forge
    HostName=192.168.68.200
    Port 2222

Host raijin
    HostName 192.168.68.69

Host *
    ServerAliveInterval 30
    HostName nowhere

Host mini
    HostName 10.0.0.1
";

    #[test]
    fn hosts_come_from_the_config_without_aliases_or_patterns() {
        let m = parse(CONFIG);
        let names: Vec<&str> = m.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["kagutsuchi", "kagutsuchi-lan", "forge", "raijin"]);
        assert_eq!(m[0].address, "100.97.247.34");
        assert_eq!(m[2], Machine { name: "forge".into(), address: "192.168.68.200".into(), port: 2222 });
        assert_eq!(m[3].port, 22);
    }

    #[test]
    fn machines_are_suggested_after_ssh() {
        let m = parse(CONFIG);
        assert_eq!(suggest("ssh ", &m), Some("kagutsuchi"));
        assert_eq!(suggest("ssh kagutsuchi-", &m), Some("lan"));
        assert_eq!(suggest("ssh f", &m), Some("orge"));
        assert_eq!(suggest("ssh forge", &m), None);
        assert_eq!(suggest("ssh -p 22 f", &m), None);
        assert_eq!(suggest("sshx f", &m), None);
    }

    #[test]
    fn a_closed_port_is_unreachable() {
        let m = Machine { name: "x".into(), address: "127.0.0.1".into(), port: 1 };
        assert!(!reachable(&m));
    }
}
