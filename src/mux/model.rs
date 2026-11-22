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

/// Portable per-command resource receipt captured inside `tachyon server`
/// across macOS (`rusage`), Linux (`/proc`), and Windows (`Win32`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandReceipt {
    pub cmd_seq: Option<u32>,
    pub cmdline: Option<String>,
    pub exit_code: Option<i32>,
    pub wall_us: u64,
    pub cpu_us: Option<u64>,
    /// Share of CPU time spent on P-cores in basis points (0..=10000).
    pub pcore_bp: Option<u16>,
    /// Energy consumed in millijoules.
    pub energy_mj: Option<u64>,
    /// Peak memory footprint in bytes.
    pub peak_bytes: Option<u64>,
    pub finished_at: u64,
}

impl CommandReceipt {
    pub fn wall_duration(&self) -> std::time::Duration {
        std::time::Duration::from_micros(self.wall_us)
    }

    pub fn cpu_duration(&self) -> Option<std::time::Duration> {
        self.cpu_us.map(std::time::Duration::from_micros)
    }

    pub fn pcore_share(&self) -> Option<f32> {
        self.pcore_bp.map(|bp| (bp.min(10_000) as f32) / 10_000.0)
    }

    pub fn energy_joules(&self) -> Option<f64> {
        self.energy_mj.map(|mj| mj as f64 / 1000.0)
    }

    pub fn from_term_receipt(
        cmd_seq: Option<u32>,
        cmdline: Option<String>,
        exit_code: Option<i32>,
        wall: std::time::Duration,
        r: crate::term::Receipt,
        finished_at: u64,
    ) -> Self {
        Self {
            cmd_seq,
            cmdline,
            exit_code,
            wall_us: wall.as_micros().min(u64::MAX as u128) as u64,
            cpu_us: r.cpu.map(|c| c.as_micros().min(u64::MAX as u128) as u64),
            pcore_bp: r.pcore_share.filter(|v| v.is_finite()).map(|v| (v.clamp(0.0, 1.0) * 10_000.0).round() as u16),
            energy_mj: r.energy_joules.filter(|v| v.is_finite() && *v >= 0.0).map(|v| (v * 1000.0).round() as u64),
            peak_bytes: r.peak_footprint,
            finished_at,
        }
    }

    pub fn to_term_receipt(&self) -> crate::term::Receipt {
        crate::term::Receipt {
            cpu: self.cpu_duration(),
            pcore_share: self.pcore_share(),
            energy_joules: self.energy_joules(),
            peak_footprint: self.peak_bytes,
        }
    }

    pub fn format_duration(d: std::time::Duration) -> String {
        let secs = d.as_secs_f64();
        if secs < 0.001 {
            format!("{} µs", d.as_micros())
        } else if secs < 1.0 {
            format!("{} ms", d.as_millis())
        } else if secs < 60.0 {
            format!("{secs:.2} s")
        } else {
            let total = d.as_secs();
            let h = total / 3600;
            let m = (total % 3600) / 60;
            let s = total % 60;
            if h > 0 {
                format!("{h}h {m:02}m {s:02}s")
            } else {
                format!("{m}m {s:02}s")
            }
        }
    }

    pub fn format_bytes(bytes: u64) -> String {
        const KIB: f64 = 1024.0;
        const MIB: f64 = KIB * 1024.0;
        const GIB: f64 = MIB * 1024.0;
        let b = bytes as f64;
        if b >= GIB {
            format!("{:.2} GiB", b / GIB)
        } else if b >= MIB {
            format!("{:.1} MiB", b / MIB)
        } else if b >= KIB {
            format!("{:.0} KiB", b / KIB)
        } else {
            format!("{bytes} B")
        }
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::with_capacity(5);
        parts.push(format!("wall {}", Self::format_duration(self.wall_duration())));
        if let Some(cpu) = self.cpu_duration() {
            let pct = if self.wall_us > 0 {
                let ratio = (cpu.as_secs_f64() / self.wall_duration().as_secs_f64()) * 100.0;
                if ratio >= 5.0 { format!(" ({ratio:.0}%)") } else { String::new() }
            } else {
                String::new()
            };
            parts.push(format!("cpu {}{pct}", Self::format_duration(cpu)));
        }
        if let Some(p) = self.pcore_share() {
            parts.push(format!("{:.0}% P-core", p * 100.0));
        }
        if let Some(e) = self.energy_joules() {
            if e >= 0.05 {
                parts.push(format!("{e:.1} J"));
            }
        }
        if let Some(mem) = self.peak_bytes.filter(|&b| b > 0) {
            parts.push(format!("peak {}", Self::format_bytes(mem)));
        }
        parts.join(" · ")
    }

    pub fn to_json(&self) -> Value {
        json!({
            "cmd_seq": self.cmd_seq,
            "cmdline": self.cmdline,
            "exit_code": self.exit_code,
            "wall_us": self.wall_us,
            "cpu_us": self.cpu_us,
            "pcore_bp": self.pcore_bp,
            "energy_mj": self.energy_mj,
            "peak_bytes": self.peak_bytes,
            "finished_at": self.finished_at,
        })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            cmd_seq: v.get("cmd_seq").and_then(Value::as_u64).map(|x| x as u32),
            cmdline: v.get("cmdline").and_then(Value::as_str).map(str::to_string),
            exit_code: v.get("exit_code").and_then(Value::as_i64).map(|x| x as i32),
            wall_us: v.get("wall_us")?.as_u64()?,
            cpu_us: v.get("cpu_us").and_then(Value::as_u64),
            pcore_bp: v.get("pcore_bp").and_then(Value::as_u64).map(|x| (x.min(10_000)) as u16),
            energy_mj: v.get("energy_mj").and_then(Value::as_u64),
            peak_bytes: v.get("peak_bytes").and_then(Value::as_u64),
            finished_at: v.get("finished_at").and_then(Value::as_u64).unwrap_or(0),
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
    /// Live OSC 7501 or zero-hook inferred program/agent status.
    pub status: Option<ProgramStatus>,
    /// True when `status` was derived automatically without explicit OSC 7501 hooks.
    pub status_inferred: bool,
    /// Latest resource receipt captured on the server for this block.
    pub last_receipt: Option<CommandReceipt>,
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
            if let Some(m) = crate::mux::agents::find_manifest(app) {
                return m.display_name.to_string();
            }
            return app.to_string();
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
            "status_inferred": self.status_inferred,
            "last_receipt": self.last_receipt.as_ref().map(CommandReceipt::to_json),
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
            status_inferred: v.get("status_inferred").and_then(Value::as_bool).unwrap_or(false),
            last_receipt: v.get("last_receipt").and_then(CommandReceipt::from_json),
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

// ---------------------------------------------------------------------------
// Cross-host Coding Agent Mission Control model
// ---------------------------------------------------------------------------

/// Urgency filter for Agent Mission Control (GUI card & `tachyon agents` CLI).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AgentFilter {
    #[default]
    All,
    Attention,
    Working,
    Done,
}

impl AgentFilter {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "all" | "*" => Some(Self::All),
            "attention" | "blocked" | "urgent" | "needs-attention" | "error" => Some(Self::Attention),
            "working" | "running" | "active" | "busy" => Some(Self::Working),
            "done" | "finished" | "complete" | "idle" => Some(Self::Done),
            _ => None,
        }
    }

    pub fn matches(self, state: ProgramState) -> bool {
        match self {
            Self::All => true,
            Self::Attention => matches!(state, ProgramState::Blocked | ProgramState::Error),
            Self::Working => matches!(state, ProgramState::Working),
            Self::Done => matches!(state, ProgramState::Done | ProgramState::Idle),
        }
    }
}

/// Summary counts of coding agents across all connected hosts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentCounts {
    pub total: usize,
    pub blocked: usize,
    pub error: usize,
    pub working: usize,
    pub done: usize,
    pub idle: usize,
}

impl AgentCounts {
    pub fn attention(self) -> usize {
        self.blocked + self.error
    }

    pub fn from_entries(entries: &[AgentEntry]) -> Self {
        let mut c = Self::default();
        for e in entries {
            c.total += 1;
            match e.state {
                ProgramState::Blocked => c.blocked += 1,
                ProgramState::Error => c.error += 1,
                ProgramState::Working => c.working += 1,
                ProgramState::Done => c.done += 1,
                ProgramState::Idle => c.idle += 1,
            }
        }
        c
    }
}

/// One active or recently reported coding-agent pane across any host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEntry {
    pub host: String,
    pub host_display: String,
    pub host_color: Option<HostColor>,
    pub session_id: String,
    pub session_name: String,
    pub window_id: String,
    pub window_name: String,
    pub block_id: String,
    pub app_id: String,
    pub agent_name: String,
    pub glyph: &'static str,
    pub state: ProgramState,
    pub message: Option<String>,
    pub cwd: Option<String>,
    pub command: Option<String>,
    pub created_at: u64,
    pub inferred: bool,
}

impl AgentEntry {
    pub fn to_json(&self) -> Value {
        json!({
            "host": self.host,
            "host_display": self.host_display,
            "session_id": self.session_id,
            "session_name": self.session_name,
            "window_id": self.window_id,
            "window_name": self.window_name,
            "block_id": self.block_id,
            "app": self.app_id,
            "agent": self.agent_name,
            "state": self.state.as_str(),
            "message": self.message,
            "cwd": self.cwd,
            "command": self.command,
            "created_at": self.created_at,
            "inferred": self.inferred,
        })
    }
}

/// Priority rank for sorting Mission Control inbox entries (highest urgency first).
pub fn agent_state_priority(state: ProgramState) -> u8 {
    match state {
        ProgramState::Blocked => 5,
        ProgramState::Error => 4,
        ProgramState::Working => 3,
        ProgramState::Done => 2,
        ProgramState::Idle => 1,
    }
}

/// Human-friendly agent name & glyph from an OSC 7501 `app` or inferred process name.
pub fn agent_identity(raw: &str) -> (String, String, &'static str) {
    crate::mux::agents::agent_identity(raw)
}

/// Infer whether a block without explicit OSC 7501 status is running a known coding agent.
pub fn infer_agent_from_block(b: &BlockInfo) -> Option<&'static str> {
    if !b.alive {
        return None;
    }
    crate::mux::agents::identify_agent(
        None,
        b.command.as_deref(),
        b.title.as_deref(),
        b.label.as_deref(),
        &[],
        false,
    )
    .map(|m| m.id)
}

/// Collect and urgency-sort coding-agent entries across one or more host snapshots.
///
/// Each tuple is `(host_alias, host_display, host_color, snapshot)`.
pub fn collect_agent_entries(
    hosts: &[(String, String, Option<HostColor>, &ServerSnapshot)],
) -> Vec<AgentEntry> {
    let mut out: Vec<(usize, AgentEntry)> = Vec::new();
    for (host_idx, (alias, display, color, snap)) in hosts.iter().enumerate() {
        for blk in &snap.blocks {
            if !blk.alive && blk.status.is_none() {
                continue;
            }
            let (state, app_raw, msg, inferred) = if let Some(st) = &blk.status {
                let raw = st
                    .app
                    .as_deref()
                    .or(st.kind.as_deref())
                    .or_else(|| infer_agent_from_block(blk))
                    .unwrap_or("agent");
                let msg = st
                    .msg
                    .clone()
                    .filter(|s| !s.is_empty())
                    .or_else(|| st.label.clone().filter(|s| !s.is_empty()));
                (st.state, raw, msg, blk.status_inferred)
            } else if let Some(inferred_app) = infer_agent_from_block(blk) {
                (
                    ProgramState::Working,
                    inferred_app,
                    blk.command.clone().or_else(|| blk.title.clone()),
                    true,
                )
            } else {
                continue;
            };

            let sess_name = snap
                .sessions
                .iter()
                .find(|s| s.id == blk.session_id)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| blk.session_id.clone());
            let win_name = snap
                .windows
                .iter()
                .find(|w| w.id == blk.window_id)
                .and_then(|w| w.name.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| blk.display_name());
            let (app_id, agent_name, glyph) = agent_identity(app_raw);

            out.push((
                host_idx,
                AgentEntry {
                    host: alias.clone(),
                    host_display: display.clone(),
                    host_color: *color,
                    session_id: blk.session_id.clone(),
                    session_name: sess_name,
                    window_id: blk.window_id.clone(),
                    window_name: win_name,
                    block_id: blk.id.clone(),
                    app_id,
                    agent_name,
                    glyph,
                    state,
                    message: msg,
                    cwd: blk.cwd.clone(),
                    command: blk.command.clone(),
                    created_at: blk.created_at,
                    inferred,
                },
            ));
        }
    }
    out.sort_by(|(ha, a), (hb, b)| {
        agent_state_priority(b.state)
            .cmp(&agent_state_priority(a.state))
            .then_with(|| ha.cmp(hb))
            .then_with(|| a.session_name.cmp(&b.session_name))
            .then_with(|| a.window_name.cmp(&b.window_name))
            .then_with(|| a.block_id.cmp(&b.block_id))
    });
    out.into_iter().map(|(_, e)| e).collect()
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

/// Zero-subprocess metadata discovered from `.git` for a working directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitDirInfo {
    pub branch: String,
    pub worktree_root: std::path::PathBuf,
    pub common_repo_root: std::path::PathBuf,
    pub repo_name: String,
    pub is_linked_worktree: bool,
}

fn expand_home_path(path: &std::path::Path) -> std::path::PathBuf {
    let raw = path.to_string_lossy();
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .filter(|h| !h.is_empty());
    if let Some(h) = home {
        if raw == "~" {
            return std::path::PathBuf::from(h);
        }
        if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
            return std::path::PathBuf::from(h).join(rest);
        }
    }
    path.to_path_buf()
}

fn resolve_common_repo_root(worktree_root: &std::path::Path, gitdir: &std::path::Path) -> std::path::PathBuf {
    let resolved_gitdir = std::fs::canonicalize(gitdir).unwrap_or_else(|_| gitdir.to_path_buf());
    if let Ok(common_raw) = std::fs::read_to_string(resolved_gitdir.join("commondir")) {
        let rel = common_raw.trim();
        if !rel.is_empty() {
            let candidate = if std::path::Path::new(rel).is_absolute() {
                std::path::PathBuf::from(rel)
            } else {
                resolved_gitdir.join(rel)
            };
            let canon = std::fs::canonicalize(&candidate).unwrap_or(candidate);
            if let Some(parent) = canon.parent() {
                return parent.to_path_buf();
            }
        }
    }
    // Fallback when `commondir` isn't present: `.../<repo>/.git/worktrees/<name>`.
    if let Some(worktrees_dir) = resolved_gitdir.parent() {
        if worktrees_dir.file_name().and_then(|n| n.to_str()) == Some("worktrees") {
            if let Some(common_git) = worktrees_dir.parent() {
                if common_git.file_name().and_then(|n| n.to_str()) == Some(".git") {
                    if let Some(repo) = common_git.parent() {
                        return repo.to_path_buf();
                    }
                }
            }
        }
    }
    std::fs::canonicalize(worktree_root).unwrap_or_else(|_| worktree_root.to_path_buf())
}

fn parse_git_head_branch(head_raw: &str) -> Option<String> {
    let s = head_raw.trim();
    if let Some(branch) = s.strip_prefix("ref: refs/heads/") {
        let b = branch.trim();
        return (!b.is_empty()).then(|| b.to_string());
    }
    if let Some(r) = s.strip_prefix("ref: ") {
        return r
            .trim()
            .rsplit('/')
            .next()
            .filter(|seg| !seg.is_empty())
            .map(str::to_string);
    }
    if s.len() >= 7 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(s[..7].to_string());
    }
    None
}

/// Inspect `.git` ancestry for `path` without spawning a subprocess.
/// Supports normal repositories, nested subdirectories, and linked worktrees.
pub fn git_info_for_dir(path: &std::path::Path) -> Option<GitDirInfo> {
    let expanded = expand_home_path(path);
    for dir in expanded.ancestors() {
        let dot_git = dir.join(".git");
        let Ok(meta) = std::fs::metadata(&dot_git) else {
            continue;
        };
        let worktree_root = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let (head_path, common_repo_root, is_linked_worktree) = if meta.is_dir() {
            (dot_git.join("HEAD"), worktree_root.clone(), false)
        } else if meta.is_file() {
            let content = std::fs::read_to_string(&dot_git).ok()?;
            let line = content.lines().next()?.trim();
            let target = line.strip_prefix("gitdir:")?.trim();
            if target.is_empty() {
                return None;
            }
            let target_path = std::path::Path::new(target);
            let gitdir = if target_path.is_absolute() {
                target_path.to_path_buf()
            } else {
                dir.join(target_path)
            };
            let common = resolve_common_repo_root(&worktree_root, &gitdir);
            (gitdir.join("HEAD"), common, true)
        } else {
            return None;
        };

        let head = std::fs::read_to_string(head_path).ok()?;
        let branch = parse_git_head_branch(&head)?;
        let repo_name = common_repo_root
            .file_name()
            .or_else(|| worktree_root.file_name())
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("repo")
            .to_string();

        return Some(GitDirInfo {
            branch,
            worktree_root,
            common_repo_root,
            repo_name,
            is_linked_worktree,
        });
    }
    None
}

/// Read the active git branch (or short detached SHA) for `path` without
/// spawning a git subprocess. Supports worktrees whose `.git` is a `gitdir:` file.
pub fn git_branch_for_dir(path: &std::path::Path) -> Option<String> {
    git_info_for_dir(path).map(|info| info.branch)
}

/// Compact 1-character uppercase monogram (or `~` / `>`) for sidebar Space avatars.
pub fn space_monogram(title: &str) -> String {
    let t = title.trim();
    if t.starts_with('~') {
        let after_sep = t.trim_start_matches('~').trim_start_matches(['/', '\\']);
        if !after_sep.chars().any(|c| c.is_ascii_alphanumeric()) {
            return "~".to_string();
        }
    }
    t.chars()
        .find(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase().to_string())
        .unwrap_or_else(|| ">".to_string())
}

/// Compact elapsed duration string for live sidebar agent timers (`42s`, `1m 05s`, `18m`, `1h 05m`).
pub fn format_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Clean project display name for an agent pane (`cwd` basename, falling back to session/window).
pub fn project_name_for_agent(entry: &AgentEntry) -> String {
    if let Some(raw) = entry.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let trimmed = raw.trim_end_matches(['/', '\\']);
        if trimmed.is_empty() && raw.starts_with('/') {
            return "/".to_string();
        }
        if trimmed == "~" {
            return "~".to_string();
        }
        if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
            let home_trim = home.trim_end_matches(['/', '\\']);
            if !home_trim.is_empty() && trimmed == home_trim {
                return "~".to_string();
            }
        }
        if let Some(last) = trimmed.rsplit(['/', '\\']).find(|s| !s.is_empty()) {
            return last.to_string();
        }
    }
    if !entry.session_name.trim().is_empty() {
        return entry.session_name.clone();
    }
    entry.window_name.clone()
}

/// Project-grouped slice of live coding agents for Zone 2 of the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectAgentGroup {
    pub project: String,
    pub host: String,
    pub host_display: String,
    pub host_color: Option<HostColor>,
    pub entries: Vec<AgentEntry>,
}

/// Group `entries` by `(host, project)` and rank the most urgent project groups first.
pub fn group_agents_by_project(entries: &[AgentEntry]) -> Vec<ProjectAgentGroup> {
    let mut index: std::collections::HashMap<(String, String), usize> = std::collections::HashMap::new();
    let mut groups: Vec<ProjectAgentGroup> = Vec::new();

    for e in entries {
        let proj = project_name_for_agent(e);
        let key = (e.host.clone(), proj.to_ascii_lowercase());
        if let Some(&idx) = index.get(&key) {
            groups[idx].entries.push(e.clone());
        } else {
            let idx = groups.len();
            index.insert(key, idx);
            groups.push(ProjectAgentGroup {
                project: proj,
                host: e.host.clone(),
                host_display: e.host_display.clone(),
                host_color: e.host_color,
                entries: vec![e.clone()],
            });
        }
    }

    for g in &mut groups {
        g.entries.sort_by(|a, b| {
            agent_state_priority(b.state)
                .cmp(&agent_state_priority(a.state))
                .then_with(|| a.window_name.cmp(&b.window_name))
                .then_with(|| a.block_id.cmp(&b.block_id))
        });
    }

    groups.sort_by(|a, b| {
        let top_a = a.entries.iter().map(|e| agent_state_priority(e.state)).max().unwrap_or(0);
        let top_b = b.entries.iter().map(|e| agent_state_priority(e.state)).max().unwrap_or(0);
        top_b
            .cmp(&top_a)
            .then_with(|| (a.host != "local").cmp(&(b.host != "local")))
            .then_with(|| a.project.to_ascii_lowercase().cmp(&b.project.to_ascii_lowercase()))
            .then_with(|| a.host.cmp(&b.host))
    });

    groups
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

    #[test]
    fn collect_agent_entries_ranks_blocked_and_error_first_and_infers_running() {
        let make_block = |id: &str, cmd: Option<&str>, status: Option<ProgramStatus>| BlockInfo {
            id: id.into(),
            session_id: "s1".into(),
            window_id: "w1".into(),
            label: None,
            title: None,
            cwd: Some("/repo".into()),
            command_running: cmd.is_some(),
            command: cmd.map(str::to_string),
            pid: 1,
            cols: 80,
            rows: 24,
            alive: true,
            exit_code: None,
            awake: false,
            attached_clients: 0,
            created_at: 0,
            status,
            status_inferred: false,
            last_receipt: None,
        };
        let snap = ServerSnapshot {
            host: HostInfo {
                hostname: "kagutsuchi".into(),
                os: "macos".into(),
                arch: "aarch64".into(),
                version: "0.1.0".into(),
                protocol: 2,
                pid: 10,
            },
            sessions: vec![SessionInfo {
                id: "s1".into(),
                name: "dev".into(),
                windows: vec!["w1".into()],
                active_window: Some("w1".into()),
                created_at: 0,
            }],
            windows: vec![WindowInfo {
                id: "w1".into(),
                session_id: "s1".into(),
                name: Some("agents".into()),
                layout: LayoutNode::Leaf("b1".into()),
                focused_block: "b1".into(),
                zoomed: false,
                created_at: 0,
            }],
            blocks: vec![
                make_block(
                    "b1",
                    Some("claude"),
                    Some(ProgramStatus {
                        state: ProgramState::Done,
                        app: Some("claude".into()),
                        kind: None,
                        msg: Some("Tests pass".into()),
                        label: None,
                    }),
                ),
                make_block("b2", Some("/opt/homebrew/bin/codex --yolo"), None),
                make_block(
                    "b3",
                    Some("antigravity"),
                    Some(ProgramStatus {
                        state: ProgramState::Blocked,
                        app: Some("antigravity".into()),
                        kind: None,
                        msg: Some("Approve deploy".into()),
                        label: None,
                    }),
                ),
                make_block("b4", Some("cargo test"), None),
            ],
            clients: Vec::new(),
        };

        let entries = collect_agent_entries(&[(
            "kagutsuchi".into(),
            "kagutsuchi".into(),
            default_os_host_color("macos"),
            &snap,
        )]);
        assert_eq!(entries.len(), 3); // plain `cargo test` excluded
        assert_eq!(entries[0].block_id, "b3");
        assert_eq!(entries[0].state, ProgramState::Blocked);
        assert_eq!(entries[0].agent_name, "Antigravity");
        assert_eq!(entries[0].glyph, "✦");
        assert_eq!(entries[1].block_id, "b2");
        assert_eq!(entries[1].state, ProgramState::Working);
        assert!(entries[1].inferred);
        assert_eq!(entries[1].agent_name, "Codex");
        assert_eq!(entries[2].block_id, "b1");
        assert_eq!(entries[2].state, ProgramState::Done);

        let counts = AgentCounts::from_entries(&entries);
        assert_eq!(counts.total, 3);
        assert_eq!(counts.blocked, 1);
        assert_eq!(counts.working, 1);
        assert_eq!(counts.done, 1);
        assert_eq!(counts.attention(), 1);
    }

    #[test]
    fn sidebar_model_helpers_git_branch_monogram_elapsed_and_project_groups() {
        // Monogram edge cases
        assert_eq!(space_monogram("Tachyon"), "T");
        assert_eq!(space_monogram("~"), "~");
        assert_eq!(space_monogram("~/"), "~");
        assert_eq!(space_monogram("✳ claude"), "C");
        assert_eq!(space_monogram(""), ">");
        assert_eq!(space_monogram(">_"), ">");

        // Elapsed duration formatting
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(42), "42s");
        assert_eq!(format_elapsed(65), "1m 05s");
        assert_eq!(format_elapsed(1080), "18m");
        assert_eq!(format_elapsed(3900), "1h 05m");

        // Git branch detection across normal repo, nested subdir, detached SHA, and worktree file
        let base = std::env::temp_dir().join(format!("tachyon-git-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        let sub = repo.join("src").join("nested");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/herdr-sidebar\n").unwrap();
        assert_eq!(git_branch_for_dir(&repo).as_deref(), Some("herdr-sidebar"));
        assert_eq!(git_branch_for_dir(&sub).as_deref(), Some("herdr-sidebar"));

        std::fs::write(repo.join(".git/HEAD"), "abcdef1234567890fedcba0987654321\n").unwrap();
        assert_eq!(git_branch_for_dir(&sub).as_deref(), Some("abcdef1"));

        let wt = base.join("wt");
        let wt_gitdir = repo.join(".git/worktrees/wt1");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&wt_gitdir).unwrap();
        std::fs::write(wt_gitdir.join("HEAD"), "ref: refs/heads/feat/worktree\n").unwrap();
        std::fs::write(wt_gitdir.join("commondir"), "../..\n").unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_gitdir.display())).unwrap();
        assert_eq!(git_branch_for_dir(&wt).as_deref(), Some("feat/worktree"));

        let info_main = git_info_for_dir(&sub).expect("main repo info");
        assert!(!info_main.is_linked_worktree);
        assert_eq!(info_main.repo_name, "repo");
        let info_wt = git_info_for_dir(&wt).expect("worktree info");
        assert!(info_wt.is_linked_worktree);
        assert_eq!(info_wt.common_repo_root, info_main.common_repo_root);
        assert_eq!(info_wt.repo_name, "repo");
        assert_eq!(info_wt.branch, "feat/worktree");

        let plain = base.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        // `/tmp` shouldn't have `.git`, so if base is in `/tmp` or `/var/folders` check specifically:
        if git_branch_for_dir(&base).is_none() {
            assert_eq!(git_branch_for_dir(&plain), None);
        }
        let _ = std::fs::remove_dir_all(&base);

        // Project grouping sorts most urgent group first and separates local vs remote
        let entry = |host: &str, cwd: &str, block: &str, state: ProgramState| AgentEntry {
            host: host.into(),
            host_display: host.into(),
            host_color: None,
            session_id: "s1".into(),
            session_name: "dev".into(),
            window_id: "w1".into(),
            window_name: "win".into(),
            block_id: block.into(),
            app_id: "claude".into(),
            agent_name: "Claude Code".into(),
            glyph: "✳",
            state,
            message: None,
            cwd: Some(cwd.into()),
            command: Some("claude".into()),
            created_at: 100,
            inferred: false,
        };
        let entries = vec![
            entry("local", "/Users/me/alpha/", "b1", ProgramState::Done),
            entry("kagutsuchi", "/srv/Tachyon", "b2", ProgramState::Blocked),
            entry("local", "/Users/me/Tachyon", "b3", ProgramState::Working),
            entry("local", "/Users/me/tachyon", "b4", ProgramState::Done),
        ];
        let groups = group_agents_by_project(&entries);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].host, "kagutsuchi");
        assert_eq!(groups[0].project, "Tachyon");
        assert_eq!(groups[1].host, "local");
        assert_eq!(groups[1].project, "Tachyon");
        assert_eq!(groups[1].entries.len(), 2);
        assert_eq!(groups[1].entries[0].block_id, "b3"); // Working before Done
        assert_eq!(groups[2].project, "alpha");
    }

    #[test]
    fn command_receipt_json_roundtrip_and_summary() {
        let term_r = crate::term::Receipt {
            cpu: Some(std::time::Duration::from_millis(840)),
            pcore_share: Some(0.82),
            energy_joules: Some(4.6),
            peak_footprint: Some(128 * 1024 * 1024),
        };
        let r = CommandReceipt::from_term_receipt(
            Some(7),
            Some("cargo build --release".into()),
            Some(0),
            std::time::Duration::from_millis(1200),
            term_r,
            1_700_000_000,
        );
        let round = CommandReceipt::from_json(&r.to_json()).expect("roundtrip");
        assert_eq!(round, r);
        let back = round.to_term_receipt();
        assert_eq!(back.cpu, term_r.cpu);
        assert_eq!(back.peak_footprint, term_r.peak_footprint);
        assert!((back.pcore_share.unwrap() - 0.82).abs() < 1e-3);
        assert!((back.energy_joules.unwrap() - 4.6).abs() < 1e-3);
        let s = round.summary();
        assert!(s.contains("wall 1.20 s"));
        assert!(s.contains("cpu 840 ms"));
        assert!(s.contains("82% P-core"));
        assert!(s.contains("4.6 J"));
        assert!(s.contains("peak 128.0 MiB"));
    }
}
