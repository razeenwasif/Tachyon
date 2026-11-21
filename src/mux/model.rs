//! Portable multiplexer data model: Host -> Session -> Window -> Block.
//!
//! Serialized cleanly via `serde_json::Value` so Tachyon keeps a minimal
//! dependency graph (no `serde_derive` proc-macro compile overhead) while
//! exposing stable machine-readable JSON for the CLI, GUI, and coding agents.

use serde_json::{json, Value};

use crate::term::{ProgramState, ProgramStatus};

/// Serialize `ProgramStatus` into Mux v2 JSON.
pub fn status_to_json(st: &ProgramStatus) -> Value {
    json!({
        "state": st.state.as_str(),
        "app": st.app,
        "kind": st.kind,
        "msg": st.msg,
        "label": st.label,
    })
}

/// Deserialize `ProgramStatus` from Mux v2 JSON.
pub fn status_from_json(v: &Value) -> Option<ProgramStatus> {
    let state = ProgramState::parse(v.get("state")?.as_str()?)?;
    Some(ProgramStatus {
        state,
        app: v.get("app").and_then(Value::as_str).map(str::to_string),
        kind: v.get("kind").and_then(Value::as_str).map(str::to_string),
        msg: v.get("msg").and_then(Value::as_str).map(str::to_string),
        label: v.get("label").and_then(Value::as_str).map(str::to_string),
    })
}

/// Direction of a binary pane split.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDir {
    Right,
    Down,
}

impl SplitDir {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Down => "down",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "right" | "r" | "h" | "horizontal" | "vsplit" => Some(Self::Right),
            "down" | "d" | "v" | "vertical" | "hsplit" => Some(Self::Down),
            _ => None,
        }
    }
}

/// Binary split layout tree inside a Window, referencing Blocks by ID.
#[derive(Clone, Debug, PartialEq)]
pub enum LayoutNode {
    Leaf(String),
    Split {
        dir: SplitDir,
        ratio: f32,
        a: Box<LayoutNode>,
        b: Box<LayoutNode>,
    },
}

impl LayoutNode {
    pub fn leaves(&self) -> Vec<String> {
        match self {
            Self::Leaf(id) => vec![id.clone()],
            Self::Split { a, b, .. } => {
                let mut v = a.leaves();
                v.extend(b.leaves());
                v
            }
        }
    }

    pub fn first(&self) -> &str {
        match self {
            Self::Leaf(id) => id,
            Self::Split { a, .. } => a.first(),
        }
    }

    pub fn split(&mut self, target: &str, new_id: String, dir: SplitDir) -> bool {
        match self {
            Self::Leaf(id) if id == target => {
                let old = id.clone();
                *self = Self::Split {
                    dir,
                    ratio: 0.5,
                    a: Box::new(Self::Leaf(old)),
                    b: Box::new(Self::Leaf(new_id)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { a, b, .. } => a.split(target, new_id.clone(), dir) || b.split(target, new_id, dir),
        }
    }

    /// Remove `target` from the tree, returning the first remaining leaf in its
    /// sibling subtree (the natural next focus). Returns `None` if `target` is
    /// the sole remaining leaf or not found.
    pub fn remove(&mut self, target: &str) -> Option<String> {
        let Self::Split { a, b, .. } = self else {
            return None;
        };
        let keep = if matches!(a.as_ref(), Self::Leaf(id) if id == target) {
            Some((**b).clone())
        } else if matches!(b.as_ref(), Self::Leaf(id) if id == target) {
            Some((**a).clone())
        } else {
            None
        };
        match keep {
            Some(sibling) => {
                *self = sibling;
                Some(self.first().to_string())
            }
            None => a.remove(target).or_else(|| b.remove(target)),
        }
    }

    pub fn set_ratio(&mut self, path: &[bool], to: f32) {
        match (self, path.split_first()) {
            (Self::Split { ratio, .. }, None) => *ratio = to.clamp(0.05, 0.95),
            (Self::Split { a, b, .. }, Some((&second, rest))) => {
                if second {
                    b.set_ratio(rest, to);
                } else {
                    a.set_ratio(rest, to);
                }
            }
            _ => {}
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Self::Leaf(id) => json!({ "type": "leaf", "block_id": id }),
            Self::Split { dir, ratio, a, b } => json!({
                "type": "split",
                "dir": dir.as_str(),
                "ratio": ratio,
                "a": a.to_json(),
                "b": b.to_json(),
            }),
        }
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        match v.get("type")?.as_str()? {
            "leaf" => Some(Self::Leaf(v.get("block_id")?.as_str()?.to_string())),
            "split" => Some(Self::Split {
                dir: SplitDir::parse(v.get("dir")?.as_str()?)?,
                ratio: v.get("ratio")?.as_f64()? as f32,
                a: Box::new(Self::from_json(v.get("a")?)?),
                b: Box::new(Self::from_json(v.get("b")?)?),
            }),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostInfo {
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub version: String,
    pub protocol: u32,
    pub pid: u32,
}

impl HostInfo {
    pub fn to_json(&self) -> Value {
        json!({
            "hostname": self.hostname,
            "os": self.os,
            "arch": self.arch,
            "version": self.version,
            "protocol": self.protocol,
            "pid": self.pid,
        })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            hostname: v.get("hostname")?.as_str()?.to_string(),
            os: v.get("os")?.as_str()?.to_string(),
            arch: v.get("arch")?.as_str()?.to_string(),
            version: v.get("version")?.as_str()?.to_string(),
            protocol: v.get("protocol")?.as_u64()? as u32,
            pid: v.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockInfo {
    pub id: String,
    pub session_id: String,
    pub window_id: String,
    pub label: Option<String>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub command: Option<String>,
    pub command_running: bool,
    pub pid: u32,
    pub cols: u16,
    pub rows: u16,
    pub alive: bool,
    pub exit_code: Option<i32>,
    pub awake: bool,
    pub attached_clients: usize,
    pub created_at: u64,
    /// Live OSC 7501 program/agent status, if reported.
    pub status: Option<ProgramStatus>,
}

impl BlockInfo {
    /// Human-readable display title for sidebars, pane headers, and `tachyon ls`.
    pub fn display_name(&self) -> String {
        if let Some(l) = self.label.as_deref().filter(|s| !s.is_empty()) {
            return l.to_string();
        }
        if let Some(t) = self.title.as_deref().filter(|s| !s.is_empty()) {
            return t.to_string();
        }
        if let Some(app) = self.status.as_ref().and_then(|s| s.app.as_deref()).filter(|s| !s.is_empty()) {
            return match app.to_ascii_lowercase().as_str() {
                "claude" | "claude-code" => "Claude Code".into(),
                "antigravity" | "agy" => "Antigravity".into(),
                "codex" => "Codex".into(),
                "opencode" => "OpenCode".into(),
                "pi" => "Pi".into(),
                _ => app.to_string(),
            };
        }
        if let Some(cmd) = self.command.as_deref().filter(|s| !s.is_empty()) {
            return cmd.to_string();
        }
        if let Some(cwd) = self.cwd.as_deref().filter(|s| !s.is_empty()) {
            return short_path(cwd);
        }
        "shell".to_string()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "session_id": self.session_id,
            "window_id": self.window_id,
            "label": self.label,
            "title": self.title,
            "cwd": self.cwd,
            "command": self.command,
            "command_running": self.command_running,
            "pid": self.pid,
            "cols": self.cols,
            "rows": self.rows,
            "alive": self.alive,
            "exit_code": self.exit_code,
            "awake": self.awake,
            "attached_clients": self.attached_clients,
            "created_at": self.created_at,
            "status": self.status.as_ref().map(status_to_json),
        })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            id: v.get("id")?.as_str()?.to_string(),
            session_id: v.get("session_id")?.as_str()?.to_string(),
            window_id: v.get("window_id")?.as_str()?.to_string(),
            label: v.get("label").and_then(Value::as_str).map(str::to_string),
            title: v.get("title").and_then(Value::as_str).map(str::to_string),
            cwd: v.get("cwd").and_then(Value::as_str).map(str::to_string),
            command: v.get("command").and_then(Value::as_str).map(str::to_string),
            command_running: v.get("command_running").and_then(Value::as_bool).unwrap_or(false),
            pid: v.get("pid").and_then(Value::as_u64).unwrap_or(0) as u32,
            cols: v.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16,
            rows: v.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16,
            alive: v.get("alive").and_then(Value::as_bool).unwrap_or(true),
            exit_code: v.get("exit_code").and_then(Value::as_i64).map(|x| x as i32),
            awake: v.get("awake").and_then(Value::as_bool).unwrap_or(false),
            attached_clients: v.get("attached_clients").and_then(Value::as_u64).unwrap_or(0) as usize,
            created_at: v.get("created_at").and_then(Value::as_u64).unwrap_or(0),
            status: v.get("status").and_then(status_from_json),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WindowInfo {
    pub id: String,
    pub session_id: String,
    pub name: Option<String>,
    pub layout: LayoutNode,
    pub focused_block: String,
    pub zoomed: bool,
    pub created_at: u64,
}

impl WindowInfo {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "session_id": self.session_id,
            "name": self.name,
            "layout": self.layout.to_json(),
            "focused_block": self.focused_block,
            "zoomed": self.zoomed,
            "created_at": self.created_at,
        })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            id: v.get("id")?.as_str()?.to_string(),
            session_id: v.get("session_id")?.as_str()?.to_string(),
            name: v.get("name").and_then(Value::as_str).map(str::to_string),
            layout: LayoutNode::from_json(v.get("layout")?)?,
            focused_block: v.get("focused_block")?.as_str()?.to_string(),
            zoomed: v.get("zoomed").and_then(Value::as_bool).unwrap_or(false),
            created_at: v.get("created_at").and_then(Value::as_u64).unwrap_or(0),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub windows: Vec<String>,
    pub active_window: Option<String>,
    pub created_at: u64,
}

impl SessionInfo {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "windows": self.windows,
            "active_window": self.active_window,
            "created_at": self.created_at,
        })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        let windows = v
            .get("windows")?
            .as_array()?
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect();
        Some(Self {
            id: v.get("id")?.as_str()?.to_string(),
            name: v.get("name")?.as_str()?.to_string(),
            windows,
            active_window: v.get("active_window").and_then(Value::as_str).map(str::to_string),
            created_at: v.get("created_at").and_then(Value::as_u64).unwrap_or(0),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientInfo {
    pub id: String,
    pub kind: String,
    pub name: String,
}

impl ClientInfo {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "kind": self.kind,
            "name": self.name,
        })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            id: v.get("id")?.as_str()?.to_string(),
            kind: v.get("kind").and_then(Value::as_str).unwrap_or("cli").to_string(),
            name: v.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ServerSnapshot {
    pub host: HostInfo,
    pub sessions: Vec<SessionInfo>,
    pub windows: Vec<WindowInfo>,
    pub blocks: Vec<BlockInfo>,
    pub clients: Vec<ClientInfo>,
}

impl ServerSnapshot {
    pub fn to_json(&self) -> Value {
        json!({
            "host": self.host.to_json(),
            "sessions": self.sessions.iter().map(SessionInfo::to_json).collect::<Vec<_>>(),
            "windows": self.windows.iter().map(WindowInfo::to_json).collect::<Vec<_>>(),
            "blocks": self.blocks.iter().map(BlockInfo::to_json).collect::<Vec<_>>(),
            "clients": self.clients.iter().map(ClientInfo::to_json).collect::<Vec<_>>(),
        })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        let host = HostInfo::from_json(v.get("host")?)?;
        let sessions = v
            .get("sessions")?
            .as_array()?
            .iter()
            .filter_map(SessionInfo::from_json)
            .collect();
        let windows = v
            .get("windows")?
            .as_array()?
            .iter()
            .filter_map(WindowInfo::from_json)
            .collect();
        let blocks = v
            .get("blocks")?
            .as_array()?
            .iter()
            .filter_map(BlockInfo::from_json)
            .collect();
        let clients = v
            .get("clients")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(ClientInfo::from_json).collect())
            .unwrap_or_default();
        Some(Self { host, sessions, windows, blocks, clients })
    }
}

/// Accent or danger colour assigned to a remote or SSH host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostColor {
    pub rgb: [u8; 3],
    /// True for warning / production / danger presets (`amber`, `orange`, `red`,
    /// `danger`, `warn`, `prod`, or `danger:#RRGGBB`), which use stronger tinting
    /// and a warning badge (`▲ host`).
    pub danger: bool,
}

/// Curated host colour presets offered in the sidebar menu, Settings UI, and CLI.
pub const HOST_COLOR_PRESETS: &[(&str, &str)] = &[
    ("auto", "Auto (by OS)"),
    ("blue", "Blue"),
    ("purple", "Purple"),
    ("teal", "Teal"),
    ("green", "Green"),
    ("amber", "Amber (Warning)"),
    ("orange", "Orange (Caution)"),
    ("red", "Red (Danger)"),
    ("pink", "Pink"),
    ("none", "None"),
];

impl HostColor {
    /// Parse a host colour spec into:
    /// - `Some(Some(color))` for a named preset or `#RRGGBB` / `danger:#RRGGBB`
    /// - `Some(None)` for explicit opt-out (`none` / `off` / `clear`)
    /// - `None` if unrecognised (note: `"auto"` should be handled by caller as fallthrough)
    pub fn parse(spec: &str) -> Option<Option<Self>> {
        let s = spec.trim();
        if s.is_empty() {
            return None;
        }
        let lower = s.to_ascii_lowercase();
        let out = match lower.as_str() {
            "none" | "off" | "clear" => return Some(None),
            "blue" | "sapphire" => Self { rgb: [66, 153, 255], danger: false },
            "purple" | "violet" => Self { rgb: [168, 118, 255], danger: false },
            "teal" | "cyan" => Self { rgb: [45, 200, 195], danger: false },
            "green" | "emerald" => Self { rgb: [56, 204, 118], danger: false },
            "pink" | "rose" => Self { rgb: [245, 110, 170], danger: false },
            "amber" | "yellow" | "warn" | "warning" => Self { rgb: [245, 178, 45], danger: true },
            "orange" | "caution" => Self { rgb: [250, 132, 48], danger: true },
            "red" | "danger" | "crimson" | "prod" => Self { rgb: [242, 74, 74], danger: true },
            _ => {
                if let Some(hex) = lower.strip_prefix("danger:") {
                    Self { rgb: parse_hex_rgb(hex.trim())?, danger: true }
                } else {
                    Self { rgb: parse_hex_rgb(&lower)?, danger: false }
                }
            }
        };
        Some(Some(out))
    }

    /// True if `spec` is a valid host colour setting (including `"auto"` and `"none"`).
    pub fn is_valid_spec(spec: &str) -> bool {
        let s = spec.trim();
        s.eq_ignore_ascii_case("auto") || Self::parse(s).is_some()
    }
}

fn parse_hex_rgb(s: &str) -> Option<[u8; 3]> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some([r, g, b])
}

/// Tasteful default accent colour for connected remote hosts by OS.
pub fn default_os_host_color(os: &str) -> Option<HostColor> {
    match os.to_ascii_lowercase().as_str() {
        "macos" => Some(HostColor { rgb: [66, 153, 255], danger: false }),
        "windows" => Some(HostColor { rgb: [168, 118, 255], danger: false }),
        "linux" => Some(HostColor { rgb: [45, 200, 195], danger: false }),
        _ => None,
    }
}

/// Case-insensitive glob matcher supporting `*` wildcards for `host.color.<pattern>`.
pub fn host_pattern_matches(pattern: &str, host: &str) -> bool {
    let p: Vec<u8> = pattern.trim().to_ascii_lowercase().into_bytes();
    let h: Vec<u8> = host.trim().to_ascii_lowercase().into_bytes();
    if p.is_empty() {
        return false;
    }
    let (mut pi, mut hi) = (0usize, 0usize);
    let mut star_pi: Option<usize> = None;
    let mut star_hi: usize = 0;
    while hi < h.len() {
        if pi < p.len() && p[pi] == h[hi] {
            pi += 1;
            hi += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_pi = Some(pi);
            pi += 1;
            star_hi = hi;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_hi += 1;
            hi = star_hi;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Resolve the effective accent/danger colour for `host`:
/// 1. Explicit `entry_color` on the `HostEntry` (`hosts.json`), unless `"auto"`
/// 2. Matching `host.color.<pattern>` rule in `tachyon.conf` (exact match beats glob)
/// 3. For non-local hosts, automatic OS default (`macos` blue, `windows` purple, `linux` teal)
pub fn resolve_host_color(
    host: &str,
    host_os: Option<&str>,
    entry_color: Option<&str>,
    config_rules: &[(String, String)],
) -> Option<HostColor> {
    let host_clean = host.trim();
    if host_clean.is_empty() {
        return None;
    }
    if let Some(spec) = entry_color.map(str::trim).filter(|s| !s.is_empty()) {
        if !spec.eq_ignore_ascii_case("auto") {
            if let Some(parsed) = HostColor::parse(spec) {
                return parsed;
            }
        }
    }
    if let Some((_, val)) = config_rules
        .iter()
        .find(|(pat, _)| !pat.contains('*') && pat.eq_ignore_ascii_case(host_clean))
    {
        if !val.trim().eq_ignore_ascii_case("auto") {
            if let Some(parsed) = HostColor::parse(val) {
                return parsed;
            }
        }
    }
    if let Some((_, val)) = config_rules
        .iter()
        .find(|(pat, _)| pat.contains('*') && host_pattern_matches(pat, host_clean))
    {
        if !val.trim().eq_ignore_ascii_case("auto") {
            if let Some(parsed) = HostColor::parse(val) {
                return parsed;
            }
        }
    }
    if host_clean.eq_ignore_ascii_case("local") {
        return None;
    }
    host_os.and_then(default_os_host_color)
}

/// Configured remote host entry persisted in `~/.config/Tachyon/hosts.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostEntry {
    pub alias: String,
    /// e.g. `ssh://kagutsuchi`, `ssh://user@host`, `tcp://100.x.y.z:7501`,
    /// `unix:///path`, or `pipe://tachyon-server-user`.
    pub uri: String,
    /// Optional explicit path to `tachyon` on the remote host (SSH targets).
    pub remote_bin: Option<String>,
    /// Optional bearer auth token for direct `tcp://` (Tailscale) targets.
    pub token: Option<String>,
    /// Optional accent / danger colour preset (`blue`, `amber`, `red`, `danger`,
    /// `#RRGGBB`, `none`, or `auto`).
    pub color: Option<String>,
}

impl HostEntry {
    pub fn to_json(&self) -> Value {
        let mut obj = json!({
            "alias": self.alias,
            "uri": self.uri,
        });
        if let Some(bin) = &self.remote_bin {
            obj["remote_bin"] = json!(bin);
        }
        if let Some(tok) = &self.token {
            obj["token"] = json!(tok);
        }
        if let Some(col) = &self.color {
            obj["color"] = json!(col);
        }
        obj
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            alias: v.get("alias")?.as_str()?.to_string(),
            uri: v.get("uri")?.as_str()?.to_string(),
            remote_bin: v.get("remote_bin").and_then(Value::as_str).map(str::to_string),
            token: v.get("token").and_then(Value::as_str).map(str::to_string),
            color: v.get("color").and_then(Value::as_str).map(str::to_string),
        })
    }
}

/// Shorten a home-rooted path for display (`/Users/me/foo` -> `~/foo`,
/// `C:\Users\me\foo` -> `~\foo`).
pub fn short_path(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        if !home.is_empty() {
            if path == home {
                return "~".to_string();
            }
            if let Some(rest) = path.strip_prefix(&home) {
                if rest.starts_with('/') || rest.starts_with('\\') {
                    return format!("~{rest}");
                }
            }
        }
    }
    path.rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// Friendly adjective-noun session names like Rex (`misty-sierra`, `dawn-bluff`).
const ADJECTIVES: &[&str] = &[
    "amber", "arctic", "ashen", "azure", "brisk", "calm", "cedar", "clear",
    "cobalt", "cosmic", "crisp", "dawn", "deep", "drift", "dusk", "ember",
    "frost", "glass", "golden", "hollow", "ivory", "lunar", "misty", "mossy",
    "nimble", "north", "ochre", "quiet", "rapid", "rolling", "silver", "solar",
    "still", "stone", "swift", "tidal", "timber", "velvet", "vivid", "winter",
];

const NOUNS: &[&str] = &[
    "alder", "aspen", "basin", "bay", "birch", "bluff", "brook", "canyon",
    "cedar", "cliff", "cove", "crag", "crest", "delta", "dune", "fjord",
    "glade", "grove", "harbor", "heath", "inlet", "isle", "knoll", "larch",
    "meadow", "mesa", "pass", "peak", "pine", "plain", "point", "ridge",
    "river", "sierra", "sound", "spire", "summit", "tundra", "vale", "willow",
];

/// Generate a human-friendly session name avoiding `taken`.
pub fn generate_session_name(seed: u64, taken: impl Fn(&str) -> bool) -> String {
    let mut x = seed ^ 0x9E37_79B9_7F4A_7C15;
    for attempt in 0..64u64 {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407 ^ attempt);
        let adj = ADJECTIVES[((x >> 32) as usize) % ADJECTIVES.len()];
        let noun = NOUNS[((x >> 16) as usize) % NOUNS.len()];
        let candidate = format!("{adj}-{noun}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    format!("session-{seed}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_split_remove_and_json_roundtrip() {
        let mut tree = LayoutNode::Leaf("b_1".into());
        assert!(tree.split("b_1", "b_2".into(), SplitDir::Right));
        assert!(tree.split("b_2", "b_3".into(), SplitDir::Down));
        tree.set_ratio(&[], 0.35);
        assert_eq!(tree.leaves(), vec!["b_1", "b_2", "b_3"]);

        let v = tree.to_json();
        assert_eq!(LayoutNode::from_json(&v).unwrap(), tree);

        assert_eq!(tree.remove("b_2"), Some("b_3".into()));
        assert_eq!(tree.leaves(), vec!["b_1", "b_3"]);
        assert_eq!(tree.remove("b_1"), Some("b_3".into()));
        assert_eq!(tree, LayoutNode::Leaf("b_3".into()));
        assert_eq!(tree.remove("b_3"), None);
    }

    #[test]
    fn friendly_names_avoid_collisions() {
        let first = generate_session_name(1, |_| false);
        assert!(first.contains('-'));
        let second = generate_session_name(1, |name| name == first);
        assert_ne!(first, second);
    }

    #[test]
    fn host_color_parsing_globs_and_precedence() {
        assert_eq!(HostColor::parse("none"), Some(None));
        let red = HostColor::parse("danger").unwrap().unwrap();
        assert!(red.danger);
        let hex = HostColor::parse("#112233").unwrap().unwrap();
        assert_eq!(hex.rgb, [0x11, 0x22, 0x33]);
        assert!(!hex.danger);
        let dhex = HostColor::parse("danger:#ff0044").unwrap().unwrap();
        assert_eq!(dhex.rgb, [0xff, 0x00, 0x44]);
        assert!(dhex.danger);
        assert_eq!(HostColor::parse("not-a-color"), None);

        assert!(host_pattern_matches("prod*", "prod-us-east"));
        assert!(host_pattern_matches("*.cluster.local", "gpu01.cluster.local"));
        assert!(!host_pattern_matches("prod*", "staging-1"));

        let rules = vec![
            ("prod*".into(), "red".into()),
            ("prod-safe".into(), "green".into()),
        ];
        // Exact match beats glob.
        let safe = resolve_host_color("prod-safe", Some("linux"), None, &rules).unwrap();
        assert!(!safe.danger);
        let prod = resolve_host_color("prod-01", Some("linux"), None, &rules).unwrap();
        assert!(prod.danger);
        // Entry color overrides config rule; "none" disables tinting completely.
        assert_eq!(resolve_host_color("prod-01", Some("linux"), Some("none"), &rules), None);
        // Local stays neutral by default; remote hosts inherit OS colour.
        assert_eq!(resolve_host_color("local", Some("macos"), None, &rules), None);
        assert_eq!(
            resolve_host_color("amaterasu", Some("windows"), None, &rules),
            default_os_host_color("windows")
        );
    }
}
