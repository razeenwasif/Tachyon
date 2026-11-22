//! Away-from-desk coding-agent alert hooks (`agent.alert` events, HTTP webhooks, and shell commands).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::config::ServerConfig;

/// Configuration for server-side away-from-desk agent transition alerts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlertConfig {
    pub webhook: Option<String>,
    pub command: Option<String>,
    pub states: Vec<String>,
    pub cooldown_secs: u64,
    pub when_detached: bool,
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            webhook: None,
            command: None,
            states: vec!["blocked".into(), "error".into()],
            cooldown_secs: 30,
            when_detached: false,
        }
    }
}

/// Validates and normalizes `server.alert_webhook` (empty -> `None`, else `http://` or `https://`).
pub fn normalize_webhook_url(raw: &str) -> Result<Option<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let low = trimmed.to_ascii_lowercase();
    if !low.starts_with("http://") && !low.starts_with("https://") {
        return Err(format!(
            "server.alert_webhook: URL must start with `http://` or `https://`, got `{trimmed}`"
        ));
    }
    Ok(Some(trimmed.to_string()))
}

/// Parses comma/whitespace-separated alert trigger states (`blocked`, `error`, `done`, or `none`/`off`).
pub fn parse_alert_states(raw: &str) -> Result<Vec<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for tok in trimmed.split([',', ' ', '\t']).filter(|s| !s.is_empty()) {
        let low = tok.to_ascii_lowercase();
        match low.as_str() {
            "none" | "off" => return Ok(Vec::new()),
            "blocked" | "error" | "done" => {
                if !out.contains(&low) {
                    out.push(low);
                }
            }
            _ => {
                return Err(format!(
                    "server.alert_states: expected blocked, error, done, or none, got `{tok}`"
                ))
            }
        }
    }
    Ok(out)
}

impl AlertConfig {
    pub fn from_server_config(cfg: &ServerConfig) -> Self {
        let mut out = Self {
            webhook: cfg.alert_webhook.clone(),
            command: cfg.alert_command.clone(),
            states: cfg.alert_states.clone(),
            cooldown_secs: cfg.alert_cooldown_secs,
            when_detached: cfg.alert_when_detached,
        };
        if let Ok(v) = std::env::var("TACHYON_ALERT_WEBHOOK") {
            if let Ok(parsed) = normalize_webhook_url(&v) {
                out.webhook = parsed;
            }
        }
        if let Ok(v) = std::env::var("TACHYON_ALERT_COMMAND") {
            let t = v.trim();
            out.command = (!t.is_empty()).then(|| t.to_string());
        }
        if let Ok(v) = std::env::var("TACHYON_ALERT_STATES") {
            if let Ok(states) = parse_alert_states(&v) {
                out.states = states;
            }
        }
        if let Ok(v) = std::env::var("TACHYON_ALERT_COOLDOWN_SECS") {
            if let Ok(n) = v.trim().parse::<u64>() {
                out.cooldown_secs = n.min(3600);
            }
        }
        out
    }

    pub fn to_json(&self) -> Value {
        json!({
            "webhook": self.webhook,
            "command": self.command,
            "states": self.states,
            "cooldown_secs": self.cooldown_secs,
            "when_detached": self.when_detached,
        })
    }

    pub fn apply_patch(&mut self, v: &Value) -> Result<(), String> {
        if let Some(w) = v.get("webhook") {
            if w.is_null() {
                self.webhook = None;
            } else if let Some(s) = w.as_str() {
                self.webhook = normalize_webhook_url(s)?;
            } else {
                return Err("`webhook` must be a string or null".into());
            }
        }
        if let Some(c) = v.get("command") {
            if c.is_null() {
                self.command = None;
            } else if let Some(s) = c.as_str() {
                let t = s.trim();
                self.command = (!t.is_empty()).then(|| t.to_string());
            } else {
                return Err("`command` must be a string or null".into());
            }
        }
        if let Some(st) = v.get("states") {
            if let Some(s) = st.as_str() {
                self.states = parse_alert_states(s)?;
            } else if let Some(arr) = st.as_array() {
                let joined = arr
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",");
                self.states = parse_alert_states(&joined)?;
            } else {
                return Err("`states` must be a string or array".into());
            }
        }
        if let Some(cd) = v.get("cooldown_secs") {
            let n = cd.as_u64().ok_or("`cooldown_secs` must be an integer")?;
            self.cooldown_secs = n.min(3600);
        }
        if let Some(wd) = v.get("when_detached").and_then(Value::as_bool) {
            self.when_detached = wd;
        }
        Ok(())
    }
}

/// Alert payload emitted as an `agent.alert` event and delivered to hooks.
#[derive(Clone, Debug)]
pub struct AlertPayload {
    pub host: String,
    pub block_id: String,
    pub session_id: String,
    pub window_id: String,
    pub label: String,
    pub state: String,
    pub app: Option<String>,
    pub message: Option<String>,
    pub cwd: Option<String>,
    pub inferred: bool,
    pub interactive_clients: usize,
    pub timestamp_unix: u64,
}

impl AlertPayload {
    pub fn title(&self) -> String {
        let who = self
            .app
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("agent");
        if self.label.is_empty() {
            format!("Tachyon · {who} {} on {}", self.state, self.host)
        } else {
            format!(
                "Tachyon · {who} {} on {} ({})",
                self.state, self.host, self.label
            )
        }
    }

    pub fn body(&self) -> String {
        match (self.app.as_deref(), self.message.as_deref()) {
            (Some(app), Some(msg)) if !msg.is_empty() => format!("[{app}] {msg}"),
            (_, Some(msg)) if !msg.is_empty() => msg.to_string(),
            (Some(app), _) => format!("{app} is {} in {}", self.state, self.label),
            (None, _) => format!("Agent {} in {}", self.state, self.label),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "event": "agent.alert",
            "host": self.host,
            "block_id": self.block_id,
            "session_id": self.session_id,
            "window_id": self.window_id,
            "label": self.label,
            "state": self.state,
            "app": self.app,
            "message": self.message,
            "cwd": self.cwd,
            "inferred": self.inferred,
            "interactive_clients": self.interactive_clients,
            "timestamp": self.timestamp_unix,
            "title": self.title(),
            "body": self.body(),
        })
    }
}

/// Outcome of a synchronous alert delivery (used by `tachyon server alerts --test`).
#[derive(Clone, Debug, Default)]
pub struct AlertDelivery {
    pub webhook_attempted: bool,
    pub webhook_ok: Option<bool>,
    pub webhook_error: Option<String>,
    pub command_attempted: bool,
    pub command_ok: Option<bool>,
    pub command_error: Option<String>,
}

impl AlertDelivery {
    pub fn to_json(&self) -> Value {
        json!({
            "webhook_attempted": self.webhook_attempted,
            "webhook_ok": self.webhook_ok,
            "webhook_error": self.webhook_error,
            "command_attempted": self.command_attempted,
            "command_ok": self.command_ok,
            "command_error": self.command_error,
        })
    }
}

#[derive(Clone, Debug)]
struct LastAlert {
    state: String,
    message: Option<String>,
    at: Instant,
}

#[derive(Debug)]
struct AlertState {
    config: AlertConfig,
    last_sent: HashMap<String, LastAlert>,
}

/// Stateful per-block deduplication, cooldown gate, and runtime config holder.
pub struct AlertManager {
    inner: Mutex<AlertState>,
}

impl AlertManager {
    pub fn new(config: AlertConfig) -> Self {
        Self {
            inner: Mutex::new(AlertState {
                config,
                last_sent: HashMap::new(),
            }),
        }
    }

    pub fn config(&self) -> AlertConfig {
        self.inner.lock().config.clone()
    }

    pub fn update_config(&self, patch: &Value) -> Result<AlertConfig, String> {
        let mut st = self.inner.lock();
        st.config.apply_patch(patch)?;
        Ok(st.config.clone())
    }

    /// Evaluates whether a block status transition at `now` should fire an alert.
    pub fn evaluate_transition_at(
        &self,
        block_id: &str,
        state: &str,
        message: Option<&str>,
        interactive_clients: usize,
        now: Instant,
    ) -> Option<AlertConfig> {
        let norm_state = state.trim().to_ascii_lowercase();
        let mut st = self.inner.lock();
        if matches!(norm_state.as_str(), "working" | "idle") {
            st.last_sent.remove(block_id);
            return None;
        }
        if !st.config.states.iter().any(|s| s == &norm_state) {
            return None;
        }
        if st.config.when_detached && interactive_clients > 0 {
            return None;
        }
        let msg_norm = message.map(|m| m.trim().to_string()).filter(|m| !m.is_empty());
        if let Some(prev) = st.last_sent.get(block_id) {
            if prev.state == norm_state && prev.message == msg_norm {
                return None;
            }
            if now.saturating_duration_since(prev.at) < Duration::from_secs(st.config.cooldown_secs)
            {
                return None;
            }
        }
        st.last_sent.insert(
            block_id.to_string(),
            LastAlert {
                state: norm_state,
                message: msg_norm,
                at: now,
            },
        );
        Some(st.config.clone())
    }

    pub fn evaluate_transition(
        &self,
        block_id: &str,
        state: &str,
        message: Option<&str>,
        interactive_clients: usize,
    ) -> Option<AlertConfig> {
        self.evaluate_transition_at(block_id, state, message, interactive_clients, Instant::now())
    }
}

/// Dispatches webhook/command hooks on a one-shot background thread (zero idle wake-ups).
pub fn dispatch_async(cfg: AlertConfig, payload: AlertPayload) {
    if cfg.webhook.is_none() && cfg.command.is_none() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("tachyon-alert-dispatch".into())
        .spawn(move || {
            let _ = dispatch_sync(&cfg, &payload);
        });
}

/// Synchronously delivers webhook and/or command hooks for `payload`.
pub fn dispatch_sync(cfg: &AlertConfig, payload: &AlertPayload) -> AlertDelivery {
    let mut delivery = AlertDelivery::default();
    let body_bytes = serde_json::to_vec(&payload.to_json()).unwrap_or_else(|_| b"{}".to_vec());

    if let Some(url) = cfg.webhook.as_deref() {
        delivery.webhook_attempted = true;
        match send_webhook(url, payload, &body_bytes) {
            Ok(()) => delivery.webhook_ok = Some(true),
            Err(e) => {
                delivery.webhook_ok = Some(false);
                delivery.webhook_error = Some(e);
            }
        }
    }

    if let Some(cmd) = cfg.command.as_deref() {
        delivery.command_attempted = true;
        match run_alert_command(cmd, payload, &body_bytes) {
            Ok(()) => delivery.command_ok = Some(true),
            Err(e) => {
                delivery.command_ok = Some(false);
                delivery.command_error = Some(e);
            }
        }
    }

    delivery
}

fn header_safe(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .collect()
}

fn ntfy_priority_and_tags(state: &str) -> (&'static str, &'static str) {
    match state {
        "blocked" => ("high", "warning,tachyon"),
        "error" => ("high", "rotating_light,tachyon"),
        _ => ("default", "white_check_mark,tachyon"),
    }
}

fn send_webhook(url: &str, payload: &AlertPayload, body: &[u8]) -> Result<(), String> {
    let low = url.to_ascii_lowercase();
    let title = header_safe(&payload.title());
    let (priority, tags) = ntfy_priority_and_tags(&payload.state);
    if low.starts_with("http://") {
        let rest = &url[7..];
        let (authority, path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err("empty host in webhook URL".into());
        }
        let socket_addr = if authority.starts_with('[') {
            if authority.contains("]:") {
                authority.to_string()
            } else {
                format!("{authority}:80")
            }
        } else if authority.contains(':') {
            authority.to_string()
        } else {
            format!("{authority}:80")
        };

        let mut addrs = socket_addr
            .to_socket_addrs()
            .map_err(|e| format!("resolve `{socket_addr}` failed: {e}"))?;
        let first = addrs
            .next()
            .ok_or_else(|| format!("no address for `{socket_addr}`"))?;
        let mut stream = TcpStream::connect_timeout(&first, Duration::from_secs(5))
            .map_err(|e| format!("connect `{socket_addr}` failed: {e}"))?;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

        let req = format!(
            "POST {path} HTTP/1.1\r\n\
             Host: {authority}\r\n\
             User-Agent: tachyon/{}\r\n\
             Content-Type: application/json\r\n\
             Title: {title}\r\n\
             Priority: {priority}\r\n\
             Tags: {tags}\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n",
            crate::APP_VERSION,
            body.len()
        );
        stream
            .write_all(req.as_bytes())
            .and_then(|()| stream.write_all(body))
            .and_then(|()| stream.flush())
            .map_err(|e| format!("webhook write failed: {e}"))?;

        let mut resp = [0u8; 256];
        let n = stream.read(&mut resp).unwrap_or(0);
        let head = String::from_utf8_lossy(&resp[..n]);
        let status_line = head.lines().next().unwrap_or("");
        let code = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse::<u16>().ok())
            .unwrap_or(0);
        if (200..300).contains(&code) {
            Ok(())
        } else {
            Err(format!("webhook returned status `{status_line}`"))
        }
    } else if low.starts_with("https://") {
        let mut child = Command::new("curl")
            .args([
                "-fsS",
                "--max-time",
                "5",
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "-H",
                &format!("Title: {title}"),
                "-H",
                &format!("Priority: {priority}"),
                "-H",
                &format!("Tags: {tags}"),
                "--data-binary",
                "@-",
                url,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to run `curl` for https webhook: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(body);
        }
        let out = child
            .wait_with_output()
            .map_err(|e| format!("curl wait failed: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(if err.is_empty() {
                format!("curl exited with {}", out.status)
            } else {
                err
            })
        }
    } else {
        Err("unsupported webhook URL scheme".into())
    }
}

fn run_alert_command(cmd: &str, payload: &AlertPayload, body: &[u8]) -> Result<(), String> {
    #[cfg(unix)]
    let mut command = {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    #[cfg(windows)]
    let mut command = {
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        c
    };
    #[cfg(not(any(unix, windows)))]
    return Err("alert command unsupported on this platform".into());

    command
        .env("TACHYON_ALERT_HOST", &payload.host)
        .env("TACHYON_ALERT_BLOCK", &payload.block_id)
        .env("TACHYON_ALERT_STATE", &payload.state)
        .env("TACHYON_ALERT_APP", payload.app.as_deref().unwrap_or(""))
        .env("TACHYON_ALERT_MESSAGE", payload.message.as_deref().unwrap_or(""))
        .env("TACHYON_ALERT_TITLE", payload.title())
        .env("TACHYON_ALERT_BODY", payload.body())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|e| format!("alert command failed to start: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(body);
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("alert command wait failed: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() {
            format!("alert command exited with {}", out.status)
        } else {
            err
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn alert_gate_deduplicates_enforces_cooldown_and_resets_on_working() {
        let mgr = AlertManager::new(AlertConfig {
            webhook: Some("http://127.0.0.1:9999/hook".into()),
            command: None,
            states: vec!["blocked".into(), "error".into()],
            cooldown_secs: 30,
            when_detached: false,
        });
        let t0 = Instant::now();
        assert!(mgr.evaluate_transition_at("b1", "blocked", Some("Approve?"), 0, t0).is_some());
        // Duplicate identical state+message suppressed.
        assert!(mgr.evaluate_transition_at("b1", "blocked", Some("Approve?"), 0, t0 + Duration::from_secs(5)).is_none());
        // Cooldown suppresses different state before 30s.
        assert!(mgr.evaluate_transition_at("b1", "error", Some("Boom"), 0, t0 + Duration::from_secs(10)).is_none());
        // Working resets per-block dedup & cooldown.
        assert!(mgr.evaluate_transition_at("b1", "working", None, 0, t0 + Duration::from_secs(12)).is_none());
        assert!(mgr.evaluate_transition_at("b1", "blocked", Some("Approve?"), 0, t0 + Duration::from_secs(13)).is_some());

        // Done ignored unless enabled in states, and when_detached suppresses attached clients.
        assert!(mgr.evaluate_transition_at("b2", "done", Some("Finished"), 0, t0).is_none());
        mgr.update_config(&json!({ "states": "blocked,done", "when_detached": true })).unwrap();
        assert!(mgr.evaluate_transition_at("b2", "done", Some("Finished"), 1, t0).is_none());
        assert!(mgr.evaluate_transition_at("b2", "done", Some("Finished"), 0, t0).is_some());
    }

    #[test]
    fn http_webhook_delivers_json_and_ntfy_headers_to_loopback_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut buf = vec![0u8; 4096];
            let mut total = 0usize;
            loop {
                let n = sock.read(&mut buf[total..]).unwrap_or(0);
                if n == 0 {
                    break;
                }
                total += n;
                let s = String::from_utf8_lossy(&buf[..total]);
                if let Some(idx) = s.find("\r\n\r\n") {
                    let body = &s[idx + 4..];
                    if !body.is_empty() && body.contains("\"event\":\"agent.alert\"") {
                        break;
                    }
                }
            }
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
            String::from_utf8_lossy(&buf[..total]).into_owned()
        });

        let cfg = AlertConfig {
            webhook: Some(format!("http://127.0.0.1:{port}/tachyon")),
            command: None,
            states: vec!["blocked".into()],
            cooldown_secs: 30,
            when_detached: false,
        };
        let payload = AlertPayload {
            host: "tsukuyomi".into(),
            block_id: "blk_1".into(),
            session_id: "ses_1".into(),
            window_id: "win_1".into(),
            label: "train".into(),
            state: "blocked".into(),
            app: Some("claude".into()),
            message: Some("Run cargo test?".into()),
            cwd: Some("/Users/dev/proj".into()),
            inferred: false,
            interactive_clients: 0,
            timestamp_unix: 12345,
        };
        let del = dispatch_sync(&cfg, &payload);
        assert!(del.webhook_attempted);
        assert_eq!(del.webhook_ok, Some(true), "err={:?}", del.webhook_error);

        let raw_req = handle.join().unwrap();
        let low = raw_req.to_ascii_lowercase();
        assert!(raw_req.starts_with("POST /tachyon HTTP/1.1\r\n"), "{raw_req}");
        assert!(low.contains("title: tachyon"), "{raw_req}");
        assert!(low.contains("priority: high"), "{raw_req}");
        assert!(low.contains("tags: warning,tachyon"), "{raw_req}");
        assert!(raw_req.contains("\"block_id\":\"blk_1\""), "{raw_req}");
    }
}
