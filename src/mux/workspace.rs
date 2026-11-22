//! Multi-host Session Workspaces / Templates (`tachyon workspace ...` & `⌘⇧P`).
//!
//! A workspace is a declarative JSON template stored in
//! `~/.config/Tachyon/workspaces/<name>.json` (or loaded from any file path)
//! describing sessions across `local` and remote Tachyon servers (`kagutsuchi`,
//! `amaterasu`, Linux clusters, etc.), their windows, recursive split layouts
//! (or flat `panes` shorthand), working directories, startup commands, and
//! initial focus target.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::mux::client::MuxClient;
use crate::mux::hosts_config_path;
use crate::mux::model::{BlockInfo, HostEntry, LayoutNode, ServerSnapshot, SplitDir};

/// Directory where named workspace templates are stored (`*.json`).
///
/// Honors `TACHYON_WORKSPACES_DIR` when set; otherwise defaults to the
/// `workspaces/` sibling directory next to `hosts.json`.
pub fn workspaces_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("TACHYON_WORKSPACES_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    match hosts_config_path().parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join("workspaces"),
        _ => PathBuf::from("workspaces"),
    }
}

/// Validate a workspace name (safe filename stem with no path separators).
pub fn validate_workspace_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("workspace name cannot be empty".into());
    }
    if trimmed == "." || trimmed == ".." || trimmed.contains("..") {
        return Err(format!("invalid workspace name `{trimmed}`"));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(format!(
            "invalid workspace name `{trimmed}` (use letters, digits, `-`, `_`, or `.`)"
        ));
    }
    Ok(trimmed.to_string())
}

/// Canonical file path for a named workspace (`<workspaces_dir>/<name>.json`).
pub fn workspace_path(name: &str) -> Result<PathBuf, String> {
    let valid = validate_workspace_name(name)?;
    Ok(workspaces_dir().join(format!("{valid}.json")))
}

/// Strip `//` line comments outside quoted JSON strings so hand-edited
/// workspace templates can include comments while staying valid JSON-compatible.
pub fn strip_json_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    let mut in_str = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            continue;
        }
        if c == '/' && chars.peek() == Some(&'/') {
            for next in chars.by_ref() {
                if next == '\n' {
                    out.push('\n');
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------------
// Declarative workspace data model
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct WorkspacePane {
    pub label: Option<String>,
    pub cwd: Option<String>,
    /// Exact process argv to spawn as the pane's command (exits when process exits).
    pub cmd: Option<Vec<String>>,
    /// Interactive command line typed into the pane's shell followed by Return.
    pub run: Option<String>,
    /// Focus this pane inside its window after layout materialization.
    pub focus: bool,
}

impl WorkspacePane {
    fn from_json(v: &Value) -> Result<Self, String> {
        let obj = v.as_object().ok_or("pane must be a JSON object")?;
        let label = opt_nonempty_str(obj, "label");
        let cwd = opt_nonempty_str(obj, "cwd");
        let cmd = match obj.get("cmd") {
            None | Some(Value::Null) => None,
            Some(Value::Array(arr)) => {
                let mut parts = Vec::with_capacity(arr.len());
                for item in arr {
                    let s = item
                        .as_str()
                        .ok_or("pane `cmd` array elements must be strings")?;
                    parts.push(s.to_string());
                }
                if parts.is_empty() { None } else { Some(parts) }
            }
            Some(Value::String(s)) => {
                let parts: Vec<String> = s.split_whitespace().map(str::to_string).collect();
                if parts.is_empty() { None } else { Some(parts) }
            }
            Some(_) => return Err("pane `cmd` must be an array of strings".into()),
        };
        let run = opt_nonempty_str(obj, "run");
        let focus = obj.get("focus").and_then(Value::as_bool).unwrap_or(false);
        Ok(Self { label, cwd, cmd, run, focus })
    }

    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        if let Some(l) = &self.label {
            m.insert("label".into(), json!(l));
        }
        if let Some(c) = &self.cwd {
            m.insert("cwd".into(), json!(c));
        }
        if let Some(cmd) = &self.cmd {
            m.insert("cmd".into(), json!(cmd));
        }
        if let Some(r) = &self.run {
            m.insert("run".into(), json!(r));
        }
        if self.focus {
            m.insert("focus".into(), json!(true));
        }
        Value::Object(m)
    }

    fn inherit_cwd(&self, fallback: Option<&str>) -> Self {
        let mut p = self.clone();
        if p.cwd.is_none() {
            p.cwd = fallback.map(str::to_string);
        }
        p
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceLayout {
    Leaf(WorkspacePane),
    Split {
        dir: SplitDir,
        ratio: f32,
        a: Box<WorkspaceLayout>,
        b: Box<WorkspaceLayout>,
    },
}

impl WorkspaceLayout {
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let obj = v.as_object().ok_or("layout node must be a JSON object")?;
        if let Some(dir_val) = obj.get("split").or_else(|| obj.get("dir")) {
            let dir_s = dir_val
                .as_str()
                .ok_or("layout `split` must be `right` or `down`")?;
            let dir = SplitDir::parse(dir_s)
                .ok_or_else(|| format!("invalid split direction `{dir_s}` (expected right|down)"))?;
            let ratio = obj
                .get("ratio")
                .and_then(Value::as_f64)
                .map(|r| (r as f32).clamp(0.05, 0.95))
                .unwrap_or(0.5);
            let a_v = obj.get("a").ok_or("split layout missing `a` branch")?;
            let b_v = obj.get("b").ok_or("split layout missing `b` branch")?;
            Ok(Self::Split {
                dir,
                ratio,
                a: Box::new(Self::from_json(a_v)?),
                b: Box::new(Self::from_json(b_v)?),
            })
        } else {
            Ok(Self::Leaf(WorkspacePane::from_json(v)?))
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Self::Leaf(p) => p.to_json(),
            Self::Split { dir, ratio, a, b } => {
                // Round ratio to 3 decimals for readable saved templates.
                let r = ((*ratio as f64) * 1000.0).round() / 1000.0;
                json!({
                    "split": dir.as_str(),
                    "ratio": r,
                    "a": a.to_json(),
                    "b": b.to_json(),
                })
            }
        }
    }

    pub fn leftmost(&self) -> &WorkspacePane {
        match self {
            Self::Leaf(p) => p,
            Self::Split { a, .. } => a.leftmost(),
        }
    }

    pub fn pane_count(&self) -> usize {
        match self {
            Self::Leaf(_) => 1,
            Self::Split { a, b, .. } => a.pane_count() + b.pane_count(),
        }
    }

    fn inherit_cwd(&self, fallback: Option<&str>) -> Self {
        match self {
            Self::Leaf(p) => Self::Leaf(p.inherit_cwd(fallback)),
            Self::Split { dir, ratio, a, b } => Self::Split {
                dir: *dir,
                ratio: *ratio,
                a: Box::new(a.inherit_cwd(fallback)),
                b: Box::new(b.inherit_cwd(fallback)),
            },
        }
    }

    /// Build a layout tree from the flat `panes` shorthand array:
    /// pane 0 is the root; each subsequent pane splits the preceding pane
    /// in `split` direction (default `right`) with optional `ratio` (default `0.5`).
    fn from_panes_array(arr: &[Value]) -> Result<Self, String> {
        if arr.is_empty() {
            return Ok(Self::Leaf(WorkspacePane::default()));
        }
        let mut tree = Self::Leaf(WorkspacePane::from_json(&arr[0])?);
        for item in &arr[1..] {
            let obj = item.as_object().ok_or("pane entry must be a JSON object")?;
            let dir = match obj.get("split").or_else(|| obj.get("dir")).and_then(Value::as_str) {
                Some(s) => SplitDir::parse(s)
                    .ok_or_else(|| format!("invalid pane split `{s}` (expected right|down)"))?,
                None => SplitDir::Right,
            };
            let ratio = obj
                .get("ratio")
                .and_then(Value::as_f64)
                .map(|r| (r as f32).clamp(0.05, 0.95))
                .unwrap_or(0.5);
            let pane = WorkspacePane::from_json(item)?;
            tree = Self::Split {
                dir,
                ratio,
                a: Box::new(tree),
                b: Box::new(Self::Leaf(pane)),
            };
        }
        Ok(tree)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceWindow {
    pub name: Option<String>,
    pub layout: WorkspaceLayout,
}

impl WorkspaceWindow {
    fn from_json(v: &Value, default_cwd: Option<&str>) -> Result<Self, String> {
        let obj = v.as_object().ok_or("window must be a JSON object")?;
        let name = opt_nonempty_str(obj, "name");
        let win_cwd = opt_nonempty_str(obj, "cwd");
        let effective_cwd = win_cwd.as_deref().or(default_cwd);
        let raw_layout = if let Some(layout_v) = obj.get("layout") {
            WorkspaceLayout::from_json(layout_v)?
        } else if let Some(panes_v) = obj.get("panes") {
            let arr = panes_v
                .as_array()
                .ok_or("window `panes` must be a JSON array")?;
            WorkspaceLayout::from_panes_array(arr)?
        } else {
            WorkspaceLayout::Leaf(WorkspacePane::from_json(v)?)
        };
        Ok(Self {
            name,
            layout: raw_layout.inherit_cwd(effective_cwd),
        })
    }

    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        if let Some(n) = &self.name {
            m.insert("name".into(), json!(n));
        }
        m.insert("layout".into(), self.layout.to_json());
        Value::Object(m)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceSession {
    pub host: String,
    pub name: String,
    pub cwd: Option<String>,
    pub windows: Vec<WorkspaceWindow>,
}

impl WorkspaceSession {
    fn from_json(v: &Value, fallback_name: &str) -> Result<Self, String> {
        let obj = v.as_object().ok_or("session must be a JSON object")?;
        let host = opt_nonempty_str(obj, "host").unwrap_or_else(|| "local".into());
        let name = opt_nonempty_str(obj, "name").unwrap_or_else(|| fallback_name.to_string());
        let cwd = opt_nonempty_str(obj, "cwd");
        let windows = match obj.get("windows") {
            Some(Value::Array(arr)) if !arr.is_empty() => {
                let mut out = Vec::with_capacity(arr.len());
                for wv in arr {
                    out.push(WorkspaceWindow::from_json(wv, cwd.as_deref())?);
                }
                out
            }
            None | Some(Value::Array(_)) | Some(Value::Null) => {
                // Single-window shorthand directly on the session.
                let pane = WorkspacePane::from_json(v)?.inherit_cwd(cwd.as_deref());
                vec![WorkspaceWindow {
                    name: None,
                    layout: WorkspaceLayout::Leaf(pane),
                }]
            }
            Some(_) => return Err("session `windows` must be an array".into()),
        };
        Ok(Self { host, name, cwd, windows })
    }

    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("host".into(), json!(self.host));
        m.insert("name".into(), json!(self.name));
        if let Some(c) = &self.cwd {
            m.insert("cwd".into(), json!(c));
        }
        m.insert(
            "windows".into(),
            Value::Array(self.windows.iter().map(WorkspaceWindow::to_json).collect()),
        );
        Value::Object(m)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceFocus {
    pub host: String,
    pub session: Option<String>,
    pub window: Option<String>,
}

impl WorkspaceFocus {
    fn from_json(v: &Value) -> Result<Self, String> {
        let obj = v.as_object().ok_or("`focus` must be a JSON object")?;
        let host = opt_nonempty_str(obj, "host").unwrap_or_else(|| "local".into());
        let session = opt_nonempty_str(obj, "session");
        let window = opt_nonempty_str(obj, "window");
        Ok(Self { host, session, window })
    }

    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("host".into(), json!(self.host));
        if let Some(s) = &self.session {
            m.insert("session".into(), json!(s));
        }
        if let Some(w) = &self.window {
            m.insert("window".into(), json!(w));
        }
        Value::Object(m)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceSpec {
    pub name: String,
    pub description: Option<String>,
    pub focus: Option<WorkspaceFocus>,
    pub sessions: Vec<WorkspaceSession>,
}

impl WorkspaceSpec {
    pub fn parse_str(raw: &str, fallback_name: Option<&str>) -> Result<Self, String> {
        let cleaned = strip_json_comments(raw);
        let val: Value = serde_json::from_str(&cleaned)
            .map_err(|e| format!("invalid workspace JSON: {e}"))?;
        Self::from_json(&val, fallback_name)
    }

    pub fn from_json(v: &Value, fallback_name: Option<&str>) -> Result<Self, String> {
        let obj = v.as_object().ok_or("workspace root must be a JSON object")?;
        let raw_name = opt_nonempty_str(obj, "name")
            .or_else(|| fallback_name.map(str::to_string))
            .ok_or("workspace missing `name`")?;
        let name = validate_workspace_name(&raw_name)?;
        let description = opt_nonempty_str(obj, "description");
        let focus = match obj.get("focus") {
            None | Some(Value::Null) => None,
            Some(fv) => Some(WorkspaceFocus::from_json(fv)?),
        };
        let sess_arr = obj
            .get("sessions")
            .and_then(Value::as_array)
            .ok_or("workspace missing `sessions` array")?;
        if sess_arr.is_empty() {
            return Err("workspace `sessions` array cannot be empty".into());
        }
        let mut sessions = Vec::with_capacity(sess_arr.len());
        for sv in sess_arr {
            sessions.push(WorkspaceSession::from_json(sv, &name)?);
        }
        Ok(Self { name, description, focus, sessions })
    }

    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("name".into(), json!(self.name));
        if let Some(d) = &self.description {
            m.insert("description".into(), json!(d));
        }
        if let Some(f) = &self.focus {
            m.insert("focus".into(), f.to_json());
        }
        m.insert(
            "sessions".into(),
            Value::Array(self.sessions.iter().map(WorkspaceSession::to_json).collect()),
        );
        Value::Object(m)
    }

    /// Unique host aliases referenced by this workspace, in declaration order.
    pub fn hosts(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for s in &self.sessions {
            if seen.insert(s.host.clone()) {
                out.push(s.host.clone());
            }
        }
        out
    }

    pub fn window_count(&self) -> usize {
        self.sessions.iter().map(|s| s.windows.len()).sum()
    }

    pub fn pane_count(&self) -> usize {
        self.sessions
            .iter()
            .flat_map(|s| &s.windows)
            .map(|w| w.layout.pane_count())
            .sum()
    }
}

#[derive(Clone, Debug)]
pub struct WorkspaceSummary {
    pub name: String,
    pub path: PathBuf,
    pub description: Option<String>,
    pub hosts: Vec<String>,
    pub session_names: Vec<(String, String)>, // (host, session_name)
    pub session_count: usize,
    pub window_count: usize,
    pub pane_count: usize,
}

impl WorkspaceSummary {
    pub fn from_spec(path: PathBuf, spec: &WorkspaceSpec) -> Self {
        Self {
            name: spec.name.clone(),
            path,
            description: spec.description.clone(),
            hosts: spec.hosts(),
            session_names: spec
                .sessions
                .iter()
                .map(|s| (s.host.clone(), s.name.clone()))
                .collect(),
            session_count: spec.sessions.len(),
            window_count: spec.window_count(),
            pane_count: spec.pane_count(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "path": self.path.to_string_lossy(),
            "description": self.description,
            "hosts": self.hosts,
            "sessions": self.session_count,
            "windows": self.window_count,
            "panes": self.pane_count,
        })
    }
}

// ---------------------------------------------------------------------------
// File I/O & template helpers
// ---------------------------------------------------------------------------

/// Load a workspace by saved template name (from `workspaces_dir()`) or by file path.
pub fn load_workspace(name_or_path: &str) -> Result<(PathBuf, WorkspaceSpec), String> {
    let trimmed = name_or_path.trim();
    if trimmed.is_empty() {
        return Err("workspace name or path cannot be empty".into());
    }
    let as_path = PathBuf::from(trimmed);
    let explicit_path = trimmed.ends_with(".json")
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || as_path.is_file();
    let path = if explicit_path {
        as_path
    } else {
        workspace_path(trimmed)?
    };
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("could not read workspace `{}`: {e}", path.display()))?;
    let stem = path.file_stem().and_then(|s| s.to_str());
    let spec = WorkspaceSpec::parse_str(&raw, stem)?;
    Ok((path, spec))
}

/// Atomically persist `spec` into `<workspaces_dir>/<spec.name>.json`.
pub fn save_workspace(spec: &WorkspaceSpec, overwrite: bool) -> Result<PathBuf, String> {
    let path = workspace_path(&spec.name)?;
    if path.exists() && !overwrite {
        return Err(format!(
            "workspace `{}` already exists at {} (pass --force to overwrite)",
            spec.name,
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(&spec.to_json())
        .map_err(|e| format!("serialize workspace: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, format!("{body}\n"))
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .map_err(|e| format!("rename {} -> {}: {e}", tmp.display(), path.display()))?;
    Ok(path)
}

/// Delete a saved workspace template by name.
pub fn remove_workspace(name: &str) -> Result<PathBuf, String> {
    let path = workspace_path(name)?;
    if !path.exists() {
        return Err(format!("workspace `{name}` not found"));
    }
    fs::remove_file(&path).map_err(|e| format!("could not remove {}: {e}", path.display()))?;
    Ok(path)
}

/// List all valid saved workspace templates in `workspaces_dir()`, sorted by name.
pub fn list_workspaces() -> Vec<WorkspaceSummary> {
    let dir = workspaces_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(raw) = fs::read_to_string(&path) {
            let stem = path.file_stem().and_then(|s| s.to_str());
            if let Ok(spec) = WorkspaceSpec::parse_str(&raw, stem) {
                out.push(WorkspaceSummary::from_spec(path, &spec));
            }
        }
    }
    out.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
    out
}

/// Generate a starter multi-host workspace template using `local` plus any
/// configured remote hosts from `hosts.json`.
pub fn starter_workspace(name: &str, configured_hosts: &[HostEntry]) -> Result<WorkspaceSpec, String> {
    let valid = validate_workspace_name(name)?;
    let mut sessions = vec![WorkspaceSession {
        host: "local".into(),
        name: valid.clone(),
        cwd: Some("~".into()),
        windows: vec![WorkspaceWindow {
            name: Some("dev".into()),
            layout: WorkspaceLayout::Split {
                dir: SplitDir::Right,
                ratio: 0.6,
                a: Box::new(WorkspaceLayout::Leaf(WorkspacePane {
                    label: Some("main".into()),
                    cwd: Some("~".into()),
                    cmd: None,
                    run: None,
                    focus: true,
                })),
                b: Box::new(WorkspaceLayout::Leaf(WorkspacePane {
                    label: Some("shell".into()),
                    cwd: Some("~".into()),
                    cmd: None,
                    run: None,
                    focus: false,
                })),
            },
        }],
    }];

    for h in configured_hosts {
        if h.alias == "local" {
            continue;
        }
        sessions.push(WorkspaceSession {
            host: h.alias.clone(),
            name: valid.clone(),
            cwd: Some("~".into()),
            windows: vec![WorkspaceWindow {
                name: Some(h.alias.clone()),
                layout: WorkspaceLayout::Leaf(WorkspacePane {
                    label: Some(h.alias.clone()),
                    cwd: Some("~".into()),
                    cmd: None,
                    run: None,
                    focus: true,
                }),
            }],
        });
    }

    let desc = if sessions.len() > 1 {
        format!("Multi-host starter workspace across {} hosts", sessions.len())
    } else {
        "Local split starter workspace".into()
    };

    Ok(WorkspaceSpec {
        name: valid.clone(),
        description: Some(desc),
        focus: Some(WorkspaceFocus {
            host: "local".into(),
            session: Some(valid),
            window: Some("dev".into()),
        }),
        sessions,
    })
}

/// Snapshot live sessions from one or more host `ServerSnapshot`s into a reusable
/// `WorkspaceSpec`, preserving exact split trees, ratios, window names, pane `cwd`s,
/// pane labels, and focused panes.
pub fn snapshot_workspace(
    name: &str,
    description: Option<String>,
    host_snapshots: &[(String, ServerSnapshot)],
    session_filter: Option<&str>,
    preferred_focus: Option<WorkspaceFocus>,
) -> Result<WorkspaceSpec, String> {
    let valid = validate_workspace_name(name)?;
    let mut sessions = Vec::new();
    for (host, snap) in host_snapshots {
        let blocks_by_id: HashMap<&str, &BlockInfo> =
            snap.blocks.iter().map(|b| (b.id.as_str(), b)).collect();
        for sess in &snap.sessions {
            if let Some(f) = session_filter {
                if sess.id != f && sess.name != f {
                    continue;
                }
            }
            let mut windows = Vec::new();
            for wid in &sess.windows {
                let Some(win) = snap.windows.iter().find(|w| &w.id == wid) else {
                    continue;
                };
                let layout = layout_from_live(&win.layout, &win.focused_block, &blocks_by_id);
                windows.push(WorkspaceWindow {
                    name: win.name.clone().filter(|s| !s.is_empty()),
                    layout,
                });
            }
            if windows.is_empty() {
                continue;
            }
            sessions.push(WorkspaceSession {
                host: host.clone(),
                name: sess.name.clone(),
                cwd: None,
                windows,
            });
        }
    }
    if sessions.is_empty() {
        return Err("no live sessions matched to save into workspace".into());
    }
    let focus = preferred_focus.or_else(|| {
        sessions.first().map(|s| WorkspaceFocus {
            host: s.host.clone(),
            session: Some(s.name.clone()),
            window: s.windows.first().and_then(|w| w.name.clone()),
        })
    });
    Ok(WorkspaceSpec {
        name: valid,
        description,
        focus,
        sessions,
    })
}

fn layout_from_live(
    node: &LayoutNode,
    focused_block: &str,
    blocks: &HashMap<&str, &BlockInfo>,
) -> WorkspaceLayout {
    match node {
        LayoutNode::Leaf(bid) => {
            let blk = blocks.get(bid.as_str()).copied();
            WorkspaceLayout::Leaf(WorkspacePane {
                label: blk.and_then(|b| b.label.clone()).filter(|s| !s.is_empty()),
                cwd: blk.and_then(|b| b.cwd.clone()).filter(|s| !s.is_empty()),
                cmd: None,
                run: None,
                focus: bid == focused_block,
            })
        }
        LayoutNode::Split { dir, ratio, a, b } => WorkspaceLayout::Split {
            dir: *dir,
            ratio: *ratio,
            a: Box::new(layout_from_live(a, focused_block, blocks)),
            b: Box::new(layout_from_live(b, focused_block, blocks)),
        },
    }
}

// ---------------------------------------------------------------------------
// Execution engine (`up` / `down`)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct WorkspaceUpOptions {
    pub recreate: bool,
    pub dry_run: bool,
    pub cols: u16,
    pub rows: u16,
}

impl Default for WorkspaceUpOptions {
    fn default() -> Self {
        Self { recreate: false, dry_run: false, cols: 120, rows: 32 }
    }
}

#[derive(Clone, Debug)]
pub struct WindowOutcome {
    pub window_id: String,
    pub name: Option<String>,
    pub blocks: Vec<String>,
    pub focused_block: String,
}

#[derive(Clone, Debug)]
pub struct SessionOutcome {
    pub host: String,
    pub session_id: String,
    pub session_name: String,
    pub reused: bool,
    pub windows: Vec<WindowOutcome>,
}

#[derive(Clone, Debug)]
pub struct FocusTarget {
    pub host: String,
    pub session_id: String,
    pub window_id: String,
}

#[derive(Clone, Debug)]
pub struct HostError {
    pub host: String,
    pub error: String,
}

#[derive(Clone, Debug)]
pub struct WorkspaceUpReport {
    pub name: String,
    pub dry_run: bool,
    pub sessions: Vec<SessionOutcome>,
    pub errors: Vec<HostError>,
    pub focus: Option<FocusTarget>,
}

impl WorkspaceUpReport {
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "dry_run": self.dry_run,
            "sessions": self.sessions.iter().map(|s| json!({
                "host": s.host,
                "session_id": s.session_id,
                "name": s.session_name,
                "reused": s.reused,
                "windows": s.windows.iter().map(|w| json!({
                    "window_id": w.window_id,
                    "name": w.name,
                    "blocks": w.blocks,
                    "focused_block": w.focused_block,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "focus": self.focus.as_ref().map(|f| json!({
                "host": f.host,
                "session_id": f.session_id,
                "window_id": f.window_id,
            })),
            "errors": self.errors.iter().map(|e| json!({
                "host": e.host,
                "error": e.error,
            })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct StoppedSession {
    pub host: String,
    pub session_name: String,
    pub session_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct WorkspaceDownReport {
    pub name: String,
    pub stopped: Vec<StoppedSession>,
    pub not_found: Vec<StoppedSession>,
    pub errors: Vec<HostError>,
}

impl WorkspaceDownReport {
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "stopped": self.stopped.iter().map(|s| json!({
                "host": s.host,
                "name": s.session_name,
                "session_id": s.session_id,
            })).collect::<Vec<_>>(),
            "not_found": self.not_found.iter().map(|s| json!({
                "host": s.host,
                "name": s.session_name,
            })).collect::<Vec<_>>(),
            "errors": self.errors.iter().map(|e| json!({
                "host": e.host,
                "error": e.error,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Launch (or idempotently reuse) every session/window/split defined in `spec`.
///
/// `get_client(host_alias)` is called at most once per unique host alias so CLI
/// runs can open fresh connections and the macOS app can reuse live `HostManager`
/// connections without duplicating orchestration logic.
pub fn workspace_up<F>(
    spec: &WorkspaceSpec,
    opts: &WorkspaceUpOptions,
    mut get_client: F,
) -> WorkspaceUpReport
where
    F: FnMut(&str) -> Result<Arc<MuxClient>, String>,
{
    let mut report = WorkspaceUpReport {
        name: spec.name.clone(),
        dry_run: opts.dry_run,
        sessions: Vec::new(),
        errors: Vec::new(),
        focus: None,
    };

    if opts.dry_run {
        for s in &spec.sessions {
            let windows = s
                .windows
                .iter()
                .enumerate()
                .map(|(i, w)| WindowOutcome {
                    window_id: format!("dry-w{}", i + 1),
                    name: w.name.clone(),
                    blocks: (0..w.layout.pane_count())
                        .map(|p| format!("dry-b{}", p + 1))
                        .collect(),
                    focused_block: "dry-b1".into(),
                })
                .collect();
            report.sessions.push(SessionOutcome {
                host: s.host.clone(),
                session_id: "dry-s".into(),
                session_name: s.name.clone(),
                reused: false,
                windows,
            });
        }
        return report;
    }

    let mut clients: HashMap<String, Result<Arc<MuxClient>, String>> = HashMap::new();
    for s in &spec.sessions {
        if !clients.contains_key(&s.host) {
            clients.insert(s.host.clone(), get_client(&s.host));
        }
    }

    let mut failed_hosts: HashSet<String> = HashSet::new();
    for sess_spec in &spec.sessions {
        let client = match clients.get(&sess_spec.host) {
            Some(Ok(c)) => Arc::clone(c),
            Some(Err(e)) => {
                if failed_hosts.insert(sess_spec.host.clone()) {
                    report.errors.push(HostError {
                        host: sess_spec.host.clone(),
                        error: e.clone(),
                    });
                }
                continue;
            }
            None => continue,
        };

        match bring_up_session(&client, sess_spec, opts) {
            Ok(outcome) => report.sessions.push(outcome),
            Err(e) => report.errors.push(HostError {
                host: sess_spec.host.clone(),
                error: format!("session `{}`: {e}", sess_spec.name),
            }),
        }
    }

    report.focus = resolve_focus_target(spec, &report.sessions);
    report
}

/// Stop every live session referenced in `spec` across all hosts.
pub fn workspace_down<F>(spec: &WorkspaceSpec, mut get_client: F) -> WorkspaceDownReport
where
    F: FnMut(&str) -> Result<Arc<MuxClient>, String>,
{
    let mut report = WorkspaceDownReport {
        name: spec.name.clone(),
        stopped: Vec::new(),
        not_found: Vec::new(),
        errors: Vec::new(),
    };

    let mut clients: HashMap<String, Result<Arc<MuxClient>, String>> = HashMap::new();
    for s in &spec.sessions {
        if !clients.contains_key(&s.host) {
            clients.insert(s.host.clone(), get_client(&s.host));
        }
    }

    let mut failed_hosts: HashSet<String> = HashSet::new();
    for sess_spec in &spec.sessions {
        let client = match clients.get(&sess_spec.host) {
            Some(Ok(c)) => Arc::clone(c),
            Some(Err(e)) => {
                if failed_hosts.insert(sess_spec.host.clone()) {
                    report.errors.push(HostError {
                        host: sess_spec.host.clone(),
                        error: e.clone(),
                    });
                }
                continue;
            }
            None => continue,
        };

        let snap = match client.state() {
            Ok(s) => s,
            Err(e) => {
                report.errors.push(HostError {
                    host: sess_spec.host.clone(),
                    error: format!("state query failed: {e}"),
                });
                continue;
            }
        };

        if let Some(existing) = snap.sessions.iter().find(|s| s.name == sess_spec.name) {
            match client.kill_session(&existing.id) {
                Ok(()) => report.stopped.push(StoppedSession {
                    host: sess_spec.host.clone(),
                    session_name: sess_spec.name.clone(),
                    session_id: Some(existing.id.clone()),
                }),
                Err(e) => report.errors.push(HostError {
                    host: sess_spec.host.clone(),
                    error: format!("kill session `{}`: {e}", sess_spec.name),
                }),
            }
        } else {
            report.not_found.push(StoppedSession {
                host: sess_spec.host.clone(),
                session_name: sess_spec.name.clone(),
                session_id: None,
            });
        }
    }

    report
}

fn bring_up_session(
    client: &MuxClient,
    spec: &WorkspaceSession,
    opts: &WorkspaceUpOptions,
) -> Result<SessionOutcome, String> {
    let snap = client.state().map_err(|e| format!("state failed: {e}"))?;
    if let Some(existing) = snap.sessions.iter().find(|s| s.name == spec.name) {
        if !opts.recreate {
            let windows = existing
                .windows
                .iter()
                .filter_map(|wid| snap.windows.iter().find(|w| &w.id == wid))
                .map(|w| WindowOutcome {
                    window_id: w.id.clone(),
                    name: w.name.clone(),
                    blocks: w.layout.leaves(),
                    focused_block: w.focused_block.clone(),
                })
                .collect();
            return Ok(SessionOutcome {
                host: spec.host.clone(),
                session_id: existing.id.clone(),
                session_name: existing.name.clone(),
                reused: true,
                windows,
            });
        }
        client
            .kill_session(&existing.id)
            .map_err(|e| format!("recreate kill failed: {e}"))?;
    }

    let first_win_spec = spec
        .windows
        .first()
        .ok_or("session has no windows")?;
    let first_leaf = first_win_spec.layout.leftmost();
    let (sess, first_win, first_blk) = client
        .create_session(
            Some(&spec.name),
            first_leaf.cwd.as_deref(),
            first_leaf.cmd.as_deref(),
            first_leaf.label.as_deref(),
            opts.cols,
            opts.rows,
        )
        .map_err(|e| format!("create_session failed: {e}"))?;

    if let Some(wname) = &first_win_spec.name {
        let _ = client.rename_window(&first_win.id, wname);
    }

    let mut windows = Vec::with_capacity(spec.windows.len());
    let first_outcome = materialize_window_layout(
        client,
        &first_win.id,
        first_win_spec.name.clone(),
        &first_blk.id,
        &first_win_spec.layout,
        opts,
    )?;
    windows.push(first_outcome);

    for win_spec in spec.windows.iter().skip(1) {
        let leaf = win_spec.layout.leftmost();
        let (win, blk) = client
            .create_window(
                &sess.id,
                win_spec.name.as_deref(),
                leaf.cwd.as_deref(),
                leaf.cmd.as_deref(),
                leaf.label.as_deref(),
                opts.cols,
                opts.rows,
            )
            .map_err(|e| format!("create_window failed: {e}"))?;
        let outcome = materialize_window_layout(
            client,
            &win.id,
            win_spec.name.clone(),
            &blk.id,
            &win_spec.layout,
            opts,
        )?;
        windows.push(outcome);
    }

    Ok(SessionOutcome {
        host: spec.host.clone(),
        session_id: sess.id,
        session_name: sess.name,
        reused: false,
        windows,
    })
}

/// Materialize an arbitrary binary `WorkspaceLayout` tree onto an existing window
/// whose root leaf is already created as `root_block_id` (corresponding to
/// `layout.leftmost()`).
///
/// Uses pre-order top-down splitting so every split replaces its leaf anchor in
/// exact tree order and sets the split ratio at the exact `path: Vec<bool>`.
fn materialize_window_layout(
    client: &MuxClient,
    window_id: &str,
    window_name: Option<String>,
    root_block_id: &str,
    layout: &WorkspaceLayout,
    opts: &WorkspaceUpOptions,
) -> Result<WindowOutcome, String> {
    let mut created_blocks = vec![root_block_id.to_string()];
    let mut focus_block: Option<String> = None;
    let mut runs: Vec<(String, String)> = Vec::new();

    let first_leaf = layout.leftmost();
    if first_leaf.focus {
        focus_block = Some(root_block_id.to_string());
    }
    if let Some(cmd) = &first_leaf.run {
        runs.push((root_block_id.to_string(), cmd.clone()));
    }

    let mut path = Vec::new();
    materialize_subtree(
        client,
        window_id,
        root_block_id,
        layout,
        &mut path,
        opts,
        &mut created_blocks,
        &mut focus_block,
        &mut runs,
    )?;

    for (bid, cmd) in runs {
        let mut line = cmd;
        if !line.ends_with('\r') && !line.ends_with('\n') {
            line.push('\r');
        }
        client
            .send_input(&bid, line.as_bytes())
            .map_err(|e| format!("send `run` input to {bid}: {e}"))?;
    }

    let focused = focus_block.unwrap_or_else(|| root_block_id.to_string());
    let _ = client.focus_window(window_id, Some(&focused));

    Ok(WindowOutcome {
        window_id: window_id.to_string(),
        name: window_name,
        blocks: created_blocks,
        focused_block: focused,
    })
}

#[allow(clippy::too_many_arguments)]
fn materialize_subtree(
    client: &MuxClient,
    window_id: &str,
    anchor_block_id: &str,
    node: &WorkspaceLayout,
    path: &mut Vec<bool>,
    opts: &WorkspaceUpOptions,
    created_blocks: &mut Vec<String>,
    focus_block: &mut Option<String>,
    runs: &mut Vec<(String, String)>,
) -> Result<(), String> {
    let WorkspaceLayout::Split { dir, ratio, a, b } = node else {
        return Ok(());
    };

    let b_leaf = b.leftmost();
    let (_win, b_blk) = client
        .split_block(
            anchor_block_id,
            *dir,
            b_leaf.cwd.as_deref(),
            b_leaf.cmd.as_deref(),
            b_leaf.label.as_deref(),
            opts.cols,
            opts.rows,
        )
        .map_err(|e| format!("split_block failed: {e}"))?;

    created_blocks.push(b_blk.id.clone());
    if b_leaf.focus {
        *focus_block = Some(b_blk.id.clone());
    }
    if let Some(cmd) = &b_leaf.run {
        runs.push((b_blk.id.clone(), cmd.clone()));
    }

    if (*ratio - 0.5).abs() > 0.005 {
        let _ = client.set_window_ratio(window_id, path, *ratio);
    }

    path.push(false);
    materialize_subtree(
        client,
        window_id,
        anchor_block_id,
        a,
        path,
        opts,
        created_blocks,
        focus_block,
        runs,
    )?;
    path.pop();

    path.push(true);
    materialize_subtree(
        client,
        window_id,
        &b_blk.id,
        b,
        path,
        opts,
        created_blocks,
        focus_block,
        runs,
    )?;
    path.pop();

    Ok(())
}

fn resolve_focus_target(spec: &WorkspaceSpec, outcomes: &[SessionOutcome]) -> Option<FocusTarget> {
    if let Some(want) = &spec.focus {
        for s in outcomes {
            if s.host != want.host {
                continue;
            }
            if let Some(sn) = &want.session {
                if &s.session_name != sn {
                    continue;
                }
            }
            if let Some(wn) = &want.window {
                if let Some(w) = s.windows.iter().find(|w| w.name.as_deref() == Some(wn.as_str())) {
                    return Some(FocusTarget {
                        host: s.host.clone(),
                        session_id: s.session_id.clone(),
                        window_id: w.window_id.clone(),
                    });
                }
            }
            if let Some(w) = s.windows.first() {
                return Some(FocusTarget {
                    host: s.host.clone(),
                    session_id: s.session_id.clone(),
                    window_id: w.window_id.clone(),
                });
            }
        }
    }
    outcomes.first().and_then(|s| {
        s.windows.first().map(|w| FocusTarget {
            host: s.host.clone(),
            session_id: s.session_id.clone(),
            window_id: w.window_id.clone(),
        })
    })
}

fn opt_nonempty_str(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Format a path nicely with `~` if under `$HOME`.
pub fn display_path(path: &Path) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Ok(rel) = path.strip_prefix(&home) {
            return format!("~/{}", rel.display());
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::model::{HostInfo, SessionInfo, WindowInfo};

    #[test]
    fn workspace_parses_comments_layout_tree_and_panes_shorthand() {
        let raw = r#"
        {
          // Multi-host ML workspace
          "name": "ml-dev",
          "description": "Local code + kagutsuchi runner",
          "focus": { "host": "local", "session": "ml-dev", "window": "code" },
          "sessions": [
            {
              "host": "local",
              "cwd": "~/Github/Tachyon",
              "windows": [
                {
                  "name": "code",
                  "layout": {
                    "split": "right",
                    "ratio": 0.65,
                    "a": { "label": "editor", "focus": true },
                    "b": {
                      "split": "down",
                      "ratio": 0.4,
                      "a": { "label": "tests", "run": "cargo test" },
                      "b": { "label": "logs", "cmd": ["tail", "-f", "http://example.com//ok"] }
                    }
                  }
                }
              ]
            },
            {
              "host": "kagutsuchi",
              "name": "mini-build",
              "cwd": "~/work",
              "windows": [
                {
                  "name": "build",
                  "panes": [
                    { "label": "shell" },
                    { "label": "monitor", "split": "down", "ratio": 0.3, "run": "htop" }
                  ]
                }
              ]
            }
          ]
        }
        "#;

        let spec = WorkspaceSpec::parse_str(raw, None).expect("parse workspace");
        assert_eq!(spec.name, "ml-dev");
        assert_eq!(spec.hosts(), vec!["local", "kagutsuchi"]);
        assert_eq!(spec.window_count(), 2);
        assert_eq!(spec.pane_count(), 5);

        // Inherited cwd flows to leaves without overriding explicit fields.
        assert_eq!(
            spec.sessions[0].windows[0].layout.leftmost().cwd.as_deref(),
            Some("~/Github/Tachyon")
        );
        // `//` inside a JSON string must NOT be stripped as a comment.
        let json_rt = spec.to_json();
        let rt = WorkspaceSpec::from_json(&json_rt, None).expect("round-trip");
        assert_eq!(rt, spec);
        assert!(json_rt.to_string().contains("http://example.com//ok"));

        // Second session used `panes` shorthand -> materialized into Split::Down(0.3).
        match &spec.sessions[1].windows[0].layout {
            WorkspaceLayout::Split { dir, ratio, a, b } => {
                assert_eq!(*dir, SplitDir::Down);
                assert!((*ratio - 0.3).abs() < 0.001);
                assert_eq!(a.leftmost().cwd.as_deref(), Some("~/work"));
                assert_eq!(b.leftmost().run.as_deref(), Some("htop"));
            }
            other => panic!("expected split layout, got {other:?}"),
        }
    }

    #[test]
    fn workspace_name_validation_rejects_traversal_and_empty() {
        assert!(validate_workspace_name("ml-dev_1.2").is_ok());
        assert!(validate_workspace_name("").is_err());
        assert!(validate_workspace_name("../evil").is_err());
        assert!(validate_workspace_name("a/b").is_err());
        assert!(validate_workspace_name("a b").is_err());
    }

    #[test]
    fn snapshot_workspace_captures_live_split_trees_and_ratios() {
        let snap = ServerSnapshot {
            host: HostInfo {
                hostname: "macbook".into(),
                os: "macos".into(),
                arch: "aarch64".into(),
                version: "0.1.0".into(),
                protocol: 2,
                pid: 1,
            },
            sessions: vec![SessionInfo {
                id: "s1".into(),
                name: "research".into(),
                windows: vec!["w1".into()],
                active_window: Some("w1".into()),
                created_at: 0,
            }],
            windows: vec![WindowInfo {
                id: "w1".into(),
                session_id: "s1".into(),
                name: Some("lab".into()),
                layout: LayoutNode::Split {
                    dir: SplitDir::Right,
                    ratio: 0.62,
                    a: Box::new(LayoutNode::Leaf("b1".into())),
                    b: Box::new(LayoutNode::Split {
                        dir: SplitDir::Down,
                        ratio: 0.4,
                        a: Box::new(LayoutNode::Leaf("b2".into())),
                        b: Box::new(LayoutNode::Leaf("b3".into())),
                    }),
                },
                focused_block: "b2".into(),
                zoomed: false,
                created_at: 0,
            }],
            blocks: vec![
                BlockInfo {
                    id: "b1".into(),
                    session_id: "s1".into(),
                    window_id: "w1".into(),
                    label: Some("editor".into()),
                    title: None,
                    cwd: Some("/tmp/repo".into()),
                    command_running: false,
                    command: None,
                    pid: 100,
                    cols: 80,
                    rows: 24,
                    alive: true,
                    exit_code: None,
                    awake: false,
                    attached_clients: 1,
                    created_at: 0,
                    status: None,
                    status_inferred: false,
                },
                BlockInfo {
                    id: "b2".into(),
                    session_id: "s1".into(),
                    window_id: "w1".into(),
                    label: Some("agent".into()),
                    title: None,
                    cwd: Some("/tmp/repo".into()),
                    command_running: false,
                    command: None,
                    pid: 101,
                    cols: 80,
                    rows: 12,
                    alive: true,
                    exit_code: None,
                    awake: false,
                    attached_clients: 1,
                    created_at: 0,
                    status: None,
                    status_inferred: false,
                },
                BlockInfo {
                    id: "b3".into(),
                    session_id: "s1".into(),
                    window_id: "w1".into(),
                    label: None,
                    title: None,
                    cwd: Some("/tmp/logs".into()),
                    command_running: false,
                    command: None,
                    pid: 102,
                    cols: 80,
                    rows: 12,
                    alive: true,
                    exit_code: None,
                    awake: false,
                    attached_clients: 1,
                    created_at: 0,
                    status: None,
                    status_inferred: false,
                },
            ],
            clients: Vec::new(),
        };

        let spec = snapshot_workspace("saved-lab", Some("Captured".into()), &[("local".into(), snap)], None, None)
            .expect("snapshot workspace");
        assert_eq!(spec.name, "saved-lab");
        assert_eq!(spec.pane_count(), 3);
        let win = &spec.sessions[0].windows[0];
        assert_eq!(win.name.as_deref(), Some("lab"));
        match &win.layout {
            WorkspaceLayout::Split { dir, ratio, a, b } => {
                assert_eq!(*dir, SplitDir::Right);
                assert!((*ratio - 0.62).abs() < 0.001);
                assert_eq!(a.leftmost().label.as_deref(), Some("editor"));
                assert!(!a.leftmost().focus);
                match b.as_ref() {
                    WorkspaceLayout::Split { dir, ratio, a: ba, b: bb } => {
                        assert_eq!(*dir, SplitDir::Down);
                        assert!((*ratio - 0.4).abs() < 0.001);
                        assert!(ba.leftmost().focus);
                        assert_eq!(bb.leftmost().cwd.as_deref(), Some("/tmp/logs"));
                    }
                    other => panic!("expected nested split, got {other:?}"),
                }
            }
            other => panic!("expected top split, got {other:?}"),
        }
    }
}
