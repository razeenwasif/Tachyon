//! Atomic on-disk session persistence & agent resume schema for `tachyon server`.
//!
//! Writes snapshots to a sibling temporary file, fsyncs, sets `0600` permissions
//! on Unix, and atomically renames over `state.json` so power cuts, daemon
//! restarts, or OS reboots never leave a half-written state file.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

use crate::mux::model::{LayoutNode, SplitDir};

pub const STATE_VERSION: u32 = 1;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq)]
pub struct PersistedState {
    pub version: u32,
    pub saved_at: u64,
    pub hostname: String,
    pub sessions: Vec<PersistedSession>,
}

impl PersistedState {
    pub fn to_json(&self) -> Value {
        json!({
            "version": self.version,
            "saved_at": self.saved_at,
            "hostname": self.hostname,
            "sessions": self.sessions.iter().map(PersistedSession::to_json).collect::<Vec<_>>(),
        })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        let version = v.get("version")?.as_u64()? as u32;
        if version != STATE_VERSION {
            return None;
        }
        let saved_at = v.get("saved_at").and_then(Value::as_u64).unwrap_or(0);
        let hostname = v
            .get("hostname")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let sessions = v
            .get("sessions")?
            .as_array()?
            .iter()
            .filter_map(PersistedSession::from_json)
            .collect();
        Some(Self {
            version,
            saved_at,
            hostname,
            sessions,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PersistedSession {
    pub name: String,
    pub active_window_index: usize,
    pub windows: Vec<PersistedWindow>,
}

impl PersistedSession {
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "active_window_index": self.active_window_index,
            "windows": self.windows.iter().map(PersistedWindow::to_json).collect::<Vec<_>>(),
        })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        let name = v.get("name")?.as_str()?.to_string();
        let active_window_index = v
            .get("active_window_index")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let windows = v
            .get("windows")?
            .as_array()?
            .iter()
            .filter_map(PersistedWindow::from_json)
            .collect();
        Some(Self {
            name,
            active_window_index,
            windows,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PersistedWindow {
    pub name: Option<String>,
    pub zoomed: bool,
    pub layout: PersistedLayout,
    pub focused_pane: usize,
    pub panes: Vec<PersistedPane>,
}

impl PersistedWindow {
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "zoomed": self.zoomed,
            "layout": self.layout.to_json(),
            "focused_pane": self.focused_pane,
            "panes": self.panes.iter().map(PersistedPane::to_json).collect::<Vec<_>>(),
        })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        let layout = PersistedLayout::from_json(v.get("layout")?)?;
        let panes = v
            .get("panes")?
            .as_array()?
            .iter()
            .filter_map(PersistedPane::from_json)
            .collect();
        Some(Self {
            name: v.get("name").and_then(Value::as_str).map(str::to_string),
            zoomed: v.get("zoomed").and_then(Value::as_bool).unwrap_or(false),
            layout,
            focused_pane: v.get("focused_pane").and_then(Value::as_u64).unwrap_or(0) as usize,
            panes,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PersistedLayout {
    Leaf(usize),
    Split {
        dir: SplitDir,
        ratio: f32,
        a: Box<PersistedLayout>,
        b: Box<PersistedLayout>,
    },
}

impl PersistedLayout {
    pub fn to_json(&self) -> Value {
        match self {
            Self::Leaf(pane_idx) => json!({ "type": "leaf", "pane": pane_idx }),
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
            "leaf" => Some(Self::Leaf(v.get("pane")?.as_u64()? as usize)),
            "split" => Some(Self::Split {
                dir: SplitDir::parse(v.get("dir")?.as_str()?)?,
                ratio: (v.get("ratio")?.as_f64()? as f32).clamp(0.05, 0.95),
                a: Box::new(Self::from_json(v.get("a")?)?),
                b: Box::new(Self::from_json(v.get("b")?)?),
            }),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedPane {
    pub label: Option<String>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub awake: bool,
    pub agent_id: Option<String>,
    pub resume_cmd: Option<String>,
    pub last_state: Option<String>,
}

impl PersistedPane {
    pub fn to_json(&self) -> Value {
        json!({
            "label": self.label,
            "title": self.title,
            "cwd": self.cwd,
            "cols": self.cols,
            "rows": self.rows,
            "awake": self.awake,
            "agent_id": self.agent_id,
            "resume_cmd": self.resume_cmd,
            "last_state": self.last_state,
        })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            label: v.get("label").and_then(Value::as_str).map(str::to_string),
            title: v.get("title").and_then(Value::as_str).map(str::to_string),
            cwd: v.get("cwd").and_then(Value::as_str).map(str::to_string),
            cols: v.get("cols").and_then(Value::as_u64).unwrap_or(120).max(2) as u16,
            rows: v.get("rows").and_then(Value::as_u64).unwrap_or(32).max(1) as u16,
            awake: v.get("awake").and_then(Value::as_bool).unwrap_or(false),
            agent_id: v.get("agent_id").and_then(Value::as_str).map(str::to_string),
            resume_cmd: v.get("resume_cmd").and_then(Value::as_str).map(str::to_string),
            last_state: v.get("last_state").and_then(Value::as_str).map(str::to_string),
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RestoreReport {
    pub sessions: usize,
    pub windows: usize,
    pub panes: usize,
    pub resumed_agents: usize,
}

impl RestoreReport {
    pub fn to_json(self) -> Value {
        json!({
            "sessions": self.sessions,
            "windows": self.windows,
            "panes": self.panes,
            "resumed_agents": self.resumed_agents,
        })
    }

    pub fn from_json(v: &Value) -> Self {
        Self {
            sessions: v.get("sessions").and_then(Value::as_u64).unwrap_or(0) as usize,
            windows: v.get("windows").and_then(Value::as_u64).unwrap_or(0) as usize,
            panes: v.get("panes").and_then(Value::as_u64).unwrap_or(0) as usize,
            resumed_agents: v.get("resumed_agents").and_then(Value::as_u64).unwrap_or(0) as usize,
        }
    }
}

/// Convert a runtime [`LayoutNode`] into an index-based [`PersistedLayout`].
pub fn layout_to_persisted(
    layout: &LayoutNode,
    block_index: &HashMap<String, usize>,
) -> Option<PersistedLayout> {
    match layout {
        LayoutNode::Leaf(bid) => block_index.get(bid).copied().map(PersistedLayout::Leaf),
        LayoutNode::Split { dir, ratio, a, b } => Some(PersistedLayout::Split {
            dir: *dir,
            ratio: *ratio,
            a: Box::new(layout_to_persisted(a, block_index)?),
            b: Box::new(layout_to_persisted(b, block_index)?),
        }),
    }
}

/// Materialize an index-based [`PersistedLayout`] onto freshly spawned block IDs.
pub fn materialize_layout(
    layout: &PersistedLayout,
    block_ids: &[String],
) -> Option<LayoutNode> {
    match layout {
        PersistedLayout::Leaf(idx) => block_ids.get(*idx).cloned().map(LayoutNode::Leaf),
        PersistedLayout::Split { dir, ratio, a, b } => Some(LayoutNode::Split {
            dir: *dir,
            ratio: ratio.clamp(0.05, 0.95),
            a: Box::new(materialize_layout(a, block_ids)?),
            b: Box::new(materialize_layout(b, block_ids)?),
        }),
    }
}

fn is_disabled_spec(s: &str) -> bool {
    let t = s.trim();
    t.eq_ignore_ascii_case("none") || t.eq_ignore_ascii_case("off")
}

/// Resolve the on-disk state file path for `tachyon server`.
///
/// Honors explicit `--state-file` first, then `TACHYON_STATE_FILE`, then
/// isolates custom/test sockets (`TACHYON_SOCKET` or non-default `socket`)
/// alongside the socket so integration tests never touch the user's real state.
/// On Linux production runs, deliberately uses `$XDG_STATE_HOME` /
/// `~/.local/state/tachyon/state.json` (persistent disk, never `$XDG_RUNTIME_DIR`).
pub fn resolve_state_path(socket: &Path, explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        if is_disabled_spec(&p.to_string_lossy()) {
            return None;
        }
        return Some(p.to_path_buf());
    }
    if let Some(env_path) = std::env::var_os("TACHYON_STATE_FILE").filter(|v| !v.is_empty()) {
        if is_disabled_spec(&env_path.to_string_lossy()) {
            return None;
        }
        return Some(PathBuf::from(env_path));
    }

    let custom_socket_env = std::env::var_os("TACHYON_SOCKET").is_some_and(|v| !v.is_empty());
    let standard = crate::mux::default_socket_path();
    let is_custom_socket = custom_socket_env || socket != standard;
    if is_custom_socket {
        let raw = socket.to_string_lossy();
        let stem = socket
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("server");
        if raw.starts_with(r"\\.\pipe\") {
            let safe: String = stem
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
                .collect();
            return Some(std::env::temp_dir().join(format!("tachyon-{safe}.state.json")));
        }
        let parent = socket
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        return Some(parent.join(format!("{stem}.state.json")));
    }

    #[cfg(windows)]
    {
        if let Some(local_app) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(local_app).join("Tachyon").join("state.json"));
        }
        Some(std::env::temp_dir().join("tachyon-state.json"))
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(xdg).join("tachyon").join("state.json"));
        }
        if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(home).join(".local/state/tachyon/state.json"));
        }
        Some(std::env::temp_dir().join("tachyon-state.json"))
    }
}

/// Atomically persist `state` as pretty JSON with `0600` Unix permissions and `sync_all`.
pub fn save_atomic(path: &Path, state: &PersistedState) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let bytes = serde_json::to_vec_pretty(&state.to_json())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("state.json");
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{name}.tmp.{}.{seq}", std::process::id()));

    let write_res = (|| -> io::Result<()> {
        let mut opts = OpenOptions::new();
        opts.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
        }
        f.write_all(&bytes)?;
        f.sync_all()?;
        Ok(())
    })();

    if let Err(e) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Load a persisted snapshot from disk (`Ok(None)` when missing).
pub fn load(path: &Path) -> io::Result<Option<PersistedState>> {
    let raw = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let val: Value = serde_json::from_slice(&raw)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid state JSON: {e}")))?;
    let parsed = PersistedState::from_json(&val).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported or malformed tachyon server state version",
        )
    })?;
    Ok(Some(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_state_round_trips_and_saves_atomically() {
        let dir = std::env::temp_dir().join(format!("tachyon-persist-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("state.json");

        let state = PersistedState {
            version: STATE_VERSION,
            saved_at: 1_763_800_000,
            hostname: "tsukuyomi".into(),
            sessions: vec![PersistedSession {
                name: "dev".into(),
                active_window_index: 0,
                windows: vec![PersistedWindow {
                    name: Some("code".into()),
                    zoomed: true,
                    layout: PersistedLayout::Split {
                        dir: SplitDir::Right,
                        ratio: 0.65,
                        a: Box::new(PersistedLayout::Leaf(0)),
                        b: Box::new(PersistedLayout::Leaf(1)),
                    },
                    focused_pane: 1,
                    panes: vec![
                        PersistedPane {
                            label: Some("editor".into()),
                            title: Some("nvim".into()),
                            cwd: Some("/tmp/repo".into()),
                            cols: 120,
                            rows: 32,
                            awake: false,
                            agent_id: None,
                            resume_cmd: None,
                            last_state: None,
                        },
                        PersistedPane {
                            label: Some("claude".into()),
                            title: Some("Claude Code".into()),
                            cwd: Some("/tmp/repo".into()),
                            cols: 120,
                            rows: 32,
                            awake: true,
                            agent_id: Some("claude".into()),
                            resume_cmd: Some("claude --continue".into()),
                            last_state: Some("idle".into()),
                        },
                    ],
                }],
            }],
        };

        save_atomic(&path, &state).expect("atomic save");
        assert_eq!(load(&path).expect("load").as_ref(), Some(&state));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        // Missing file yields Ok(None); corrupt file yields Err(InvalidData).
        assert_eq!(load(&dir.join("missing.json")).unwrap(), None);
        fs::write(&path, b"{not valid json").unwrap();
        assert!(load(&path).is_err());

        // Custom sockets isolate state alongside the socket rather than user default state.json.
        let test_sock = dir.join("custom.sock");
        let resolved = resolve_state_path(&test_sock, None).unwrap();
        assert_eq!(resolved, dir.join("custom.state.json"));
        assert_eq!(resolve_state_path(&test_sock, Some(Path::new("none"))), None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn layout_conversion_maps_block_ids_and_indices_cleanly() {
        let mut tree = LayoutNode::Leaf("b_1".into());
        assert!(tree.split("b_1", "b_2".into(), SplitDir::Right));
        assert!(tree.split("b_2", "b_3".into(), SplitDir::Down));

        let mut idx = HashMap::new();
        idx.insert("b_1".into(), 0);
        idx.insert("b_2".into(), 1);
        idx.insert("b_3".into(), 2);

        let persisted = layout_to_persisted(&tree, &idx).unwrap();
        let remapped = materialize_layout(&persisted, &["n_1".into(), "n_2".into(), "n_3".into()]).unwrap();
        assert_eq!(remapped.leaves(), vec!["n_1", "n_2", "n_3"]);
    }
}
