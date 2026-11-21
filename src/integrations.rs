//! Coding-agent integrations (Claude Code, Codex, OpenCode, Pi) and OSC 7501
//! status/hook helpers.
//!
//! Installs a `SKILL.md` teaching coding agents how to orchestrate Tachyon
//! panes (`tachyon split`, `run --wait --capture`, `send`, `capture`, `ls`,
//! `-S <host>`), merges non-destructive Claude Code lifecycle hooks into
//! `~/.claude/settings.json` to emit OSC 7501 status transitions (`working`,
//! `blocked`, `done`), and optionally auto-names generated sessions after the
//! active task.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::config::Config;
use crate::mux::client::MuxClient;
use crate::mux::{load_hosts, resolve_target};
use crate::term::{ProgramState, ProgramStatus};

const HOOK_MARKER: &str = "tachyon hook claude";

/// Supported coding agents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Antigravity,
    Codex,
    OpenCode,
    Pi,
}

impl AgentKind {
    pub const ALL: [Self; 5] = [
        Self::Claude,
        Self::Antigravity,
        Self::Codex,
        Self::OpenCode,
        Self::Pi,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Antigravity => "antigravity",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Pi => "pi",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Antigravity => "Antigravity",
            Self::Codex => "Codex",
            Self::OpenCode => "OpenCode",
            Self::Pi => "Pi",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "claudecode" => Some(Self::Claude),
            "antigravity" | "agy" => Some(Self::Antigravity),
            "codex" => Some(Self::Codex),
            "opencode" | "open-code" => Some(Self::OpenCode),
            "pi" => Some(Self::Pi),
            _ => None,
        }
    }

    fn config_dirs(self, home: &Path) -> Vec<PathBuf> {
        match self {
            Self::Claude => vec![home.join(".claude")],
            Self::Antigravity => vec![
                home.join(".gemini").join("config"),
                home.join(".gemini").join("antigravity"),
            ],
            Self::Codex => vec![home.join(".codex")],
            Self::OpenCode => vec![home.join(".config").join("opencode")],
            Self::Pi => vec![home.join(".pi")],
        }
    }

    fn skill_path(self, home: &Path) -> PathBuf {
        match self {
            Self::Claude => home.join(".claude").join("skills").join("tachyon").join("SKILL.md"),
            Self::Antigravity => home
                .join(".gemini")
                .join("config")
                .join("skills")
                .join("tachyon")
                .join("SKILL.md"),
            Self::Codex => home.join(".codex").join("skills").join("tachyon").join("SKILL.md"),
            Self::OpenCode => home
                .join(".config")
                .join("opencode")
                .join("skills")
                .join("tachyon")
                .join("SKILL.md"),
            Self::Pi => home
                .join(".pi")
                .join("agent")
                .join("skills")
                .join("tachyon")
                .join("SKILL.md"),
        }
    }

    fn settings_path(self, home: &Path) -> Option<PathBuf> {
        match self {
            Self::Claude => Some(home.join(".claude").join("settings.json")),
            Self::Antigravity => Some(home.join(".gemini").join("config").join("hooks.json")),
            _ => None,
        }
    }

    fn binary_names(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["claude"],
            Self::Antigravity => &["agy", "antigravity"],
            Self::Codex => &["codex"],
            Self::OpenCode => &["opencode"],
            Self::Pi => &["pi"],
        }
    }
}

/// Detection & installation status for one coding agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentStatus {
    pub kind: AgentKind,
    pub id: &'static str,
    pub name: &'static str,
    pub detected: bool,
    pub skill_installed: bool,
    pub hooks_installed: bool,
    pub skill_path: PathBuf,
    pub settings_path: Option<PathBuf>,
}

impl AgentStatus {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "detected": self.detected,
            "skill_installed": self.skill_installed,
            "hooks_installed": self.hooks_installed,
            "skill_path": self.skill_path.display().to_string(),
            "settings_path": self.settings_path.as_ref().map(|p| p.display().to_string()),
        })
    }
}

pub fn user_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn on_path(bin: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return true;
        }
    }
    false
}

/// Inspect one coding agent's detection & Tachyon integration status.
pub fn inspect_agent(home: &Path, kind: AgentKind) -> AgentStatus {
    let skill_path = kind.skill_path(home);
    let settings_path = kind.settings_path(home);
    let detected = kind.config_dirs(home).iter().any(|d| d.exists())
        || kind.binary_names().iter().any(|b| on_path(b));
    let skill_installed = skill_path.is_file();
    let hooks_installed = match (kind, settings_path.as_deref()) {
        (AgentKind::Claude, Some(p)) => claude_hooks_present(p),
        (AgentKind::Antigravity, Some(p)) => antigravity_hooks_present(p),
        _ => false,
    };

    AgentStatus {
        kind,
        id: kind.id(),
        name: kind.display_name(),
        detected,
        skill_installed,
        hooks_installed,
        skill_path,
        settings_path,
    }
}

/// Inspect all supported coding agents.
pub fn inspect_all(home: &Path) -> Vec<AgentStatus> {
    AgentKind::ALL
        .iter()
        .map(|&k| inspect_agent(home, k))
        .collect()
}

/// Whether at least one supported agent currently has Tachyon integration installed.
pub fn any_installed(home: &Path) -> bool {
    inspect_all(home)
        .iter()
        .any(|s| s.skill_installed || s.hooks_installed)
}

/// Install the Tachyon skill (and lifecycle hooks when applicable) for `kind`.
pub fn setup_agent(home: &Path, kind: AgentKind, auto_name: bool) -> io::Result<AgentStatus> {
    let skill_path = kind.skill_path(home);
    if let Some(parent) = skill_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&skill_path, skill_markdown(auto_name))?;

    match (kind, kind.settings_path(home)) {
        (AgentKind::Claude, Some(p)) => install_claude_hooks(&p)?,
        (AgentKind::Antigravity, Some(p)) => install_antigravity_hooks(&p)?,
        _ => {}
    }
    Ok(inspect_agent(home, kind))
}

/// Install Tachyon skills/hooks for every detected agent (falls back to Claude
/// Code if no agent directories exist yet).
pub fn setup_all_detected(home: &Path, auto_name: bool) -> io::Result<Vec<AgentStatus>> {
    let inspected = inspect_all(home);
    let mut targets: Vec<AgentKind> = inspected
        .iter()
        .filter(|s| s.detected)
        .map(|s| s.kind)
        .collect();
    if targets.is_empty() {
        targets.push(AgentKind::Claude);
    }
    for k in targets {
        setup_agent(home, k, auto_name)?;
    }
    Ok(inspect_all(home))
}

/// Remove Tachyon skill & hooks for `kind`, leaving all user files and hooks untouched.
pub fn remove_agent(home: &Path, kind: AgentKind) -> io::Result<AgentStatus> {
    let skill_path = kind.skill_path(home);
    if skill_path.is_file() {
        let _ = fs::remove_file(&skill_path);
        if let Some(parent) = skill_path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
    match (kind, kind.settings_path(home)) {
        (AgentKind::Claude, Some(p)) => remove_claude_hooks(&p)?,
        (AgentKind::Antigravity, Some(p)) => remove_antigravity_hooks(&p)?,
        _ => {}
    }
    Ok(inspect_agent(home, kind))
}

/// Remove Tachyon integrations across all supported agents.
pub fn remove_all(home: &Path) -> io::Result<Vec<AgentStatus>> {
    for &k in &AgentKind::ALL {
        let _ = remove_agent(home, k);
    }
    Ok(inspect_all(home))
}

/// Generate the Tachyon agent skill document (`SKILL.md`).
pub fn skill_markdown(auto_name: bool) -> String {
    let naming_block = if auto_name {
        "\
## Naming Sessions

When you start working on a clear user task inside Tachyon (`$TACHYON_SESSION` is set),
give the session a concise 2–4 word kebab-case label so it is easy to spot in the sidebar:

```sh
tachyon session rename <short-task-name>
```
"
    } else {
        ""
    };

    format!(
        "\
---
name: tachyon
description: Control persistent local and remote terminal sessions, split panes, background runs, and live status in Tachyon.
---

# Tachyon Multiplexer Skill

You are running inside **Tachyon**, a persistent multi-host terminal multiplexer.
Every managed pane automatically exports:
- `$TACHYON_SERVER` — host alias (`local` or remote host like `kagutsuchi`)
- `$TACHYON_SESSION` — current session ID
- `$TACHYON_WINDOW` — current window ID
- `$TACHYON_BLOCK` — current pane/block ID

## Common Commands

```sh
# Inspect hosts, sessions, windows, and panes as structured JSON
tachyon hosts ls --json
tachyon ls --json

# Open a split pane beside or below the current pane
tachyon split --right -- <command>
tachyon split --down -- <command>

# Run a command in a managed pane, wait for completion, and capture output
tachyon run --wait --capture -- <command>

# Run on a remote host (e.g. Mac mini `kagutsuchi`)
tachyon -S kagutsuchi run --wait --capture -- <command>

# Send input to a running pane or read its rendered screen
tachyon send -b <block_id> --enter \"<text>\"
tachyon capture -b <block_id> --lines 80

# Report live status to Tachyon's sidebar, pane headers & notifications (OSC 7501)
tachyon status --state working --app claude --msg \"Running test suite\" --tty
tachyon status --state blocked --app claude --msg \"Waiting for approval\" --tty
tachyon status --state done --app claude --msg \"All tests passed\" --tty
```

{naming_block}\
Always prefer `tachyon run --wait --capture` or `tachyon split` over blocking
background subshells when launching servers, builds, or parallel workers so the
user can see and interact with every pane directly in Tachyon.
"
    )
}

fn claude_hook_entries() -> [(&'static str, &'static str); 4] {
    [
        ("UserPromptSubmit", "tachyon hook claude prompt"),
        ("PreToolUse", "tachyon hook claude working"),
        ("Notification", "tachyon hook claude blocked"),
        ("Stop", "tachyon hook claude done"),
    ]
}

fn entry_is_tachyon_hook(entry: &Value) -> bool {
    if let Some(cmd) = entry.get("command").and_then(Value::as_str) {
        if cmd.contains(HOOK_MARKER) {
            return true;
        }
    }
    if let Some(arr) = entry.get("hooks").and_then(Value::as_array) {
        if arr.iter().any(|h| {
            h.get("command")
                .and_then(Value::as_str)
                .is_some_and(|c| c.contains(HOOK_MARKER))
        }) {
            return true;
        }
    }
    false
}

fn claude_hooks_present(path: &Path) -> bool {
    let Ok(raw) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(val) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    let Some(hooks) = val.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    hooks
        .values()
        .filter_map(Value::as_array)
        .any(|arr| arr.iter().any(entry_is_tachyon_hook))
}

/// Non-destructively merge Tachyon's Claude Code OSC 7501 hooks into `settings.json`.
pub fn install_claude_hooks(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut root: Value = match fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid JSON in {}: {e}", path.display())))?,
        _ => json!({}),
    };
    let Some(obj) = root.as_object_mut() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "settings.json root is not a JSON object",
        ));
    };
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks_obj = hooks.as_object_mut().expect("object");

    for (event_name, cmd) in claude_hook_entries() {
        let slot = hooks_obj
            .entry(event_name)
            .or_insert_with(|| json!([]));
        if !slot.is_array() {
            *slot = json!([]);
        }
        let arr = slot.as_array_mut().expect("array");
        // Remove any prior Tachyon hook in this event slot before appending fresh one.
        arr.retain(|e| !entry_is_tachyon_hook(e));
        arr.push(json!({
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": cmd,
            }],
        }));
    }

    let formatted = serde_json::to_string_pretty(&root)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    fs::write(path, format!("{formatted}\n"))
}

/// Remove only Tachyon's hook entries from `~/.claude/settings.json`.
pub fn remove_claude_hooks(path: &Path) -> io::Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let raw = fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(());
    }
    let mut root: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let Some(hooks_obj) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    let mut empty_keys = Vec::new();
    for (k, v) in hooks_obj.iter_mut() {
        if let Some(arr) = v.as_array_mut() {
            arr.retain(|e| !entry_is_tachyon_hook(e));
            if arr.is_empty() {
                empty_keys.push(k.clone());
            }
        }
    }
    for k in empty_keys {
        hooks_obj.remove(&k);
    }
    let formatted = serde_json::to_string_pretty(&root)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    fs::write(path, format!("{formatted}\n"))
}

const ANTIGRAVITY_HOOK_KEY: &str = "tachyon";

fn antigravity_hooks_present(path: &Path) -> bool {
    let Ok(raw) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(val) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    val.get(ANTIGRAVITY_HOOK_KEY).is_some()
}

/// Non-destructively merge Tachyon's named hook block into Antigravity's
/// `~/.gemini/config/hooks.json`, preserving all user hooks.
pub fn install_antigravity_hooks(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut root: Value = match fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid JSON in {}: {e}", path.display())))?,
        _ => json!({}),
    };
    let Some(obj) = root.as_object_mut() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "hooks.json root is not a JSON object",
        ));
    };
    obj.insert(
        ANTIGRAVITY_HOOK_KEY.to_string(),
        json!({
            "PreInvocation": [{
                "type": "command",
                "command": "tachyon hook antigravity prompt",
                "timeout": 5
            }],
            "PreToolUse": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": "tachyon hook antigravity working",
                    "timeout": 5
                }]
            }],
            "Stop": [{
                "type": "command",
                "command": "tachyon hook antigravity done",
                "timeout": 5
            }]
        }),
    );
    let formatted = serde_json::to_string_pretty(&root)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    fs::write(path, format!("{formatted}\n"))
}

/// Remove only Tachyon's `"tachyon"` block from Antigravity's `~/.gemini/config/hooks.json`.
pub fn remove_antigravity_hooks(path: &Path) -> io::Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let raw = fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(());
    }
    let mut root: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let Some(obj) = root.as_object_mut() else {
        return Ok(());
    };
    obj.remove(ANTIGRAVITY_HOOK_KEY);
    if obj.is_empty() {
        let _ = fs::remove_file(path);
        return Ok(());
    }
    let formatted = serde_json::to_string_pretty(&root)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    fs::write(path, format!("{formatted}\n"))
}

/// Emit an OSC 7501 escape sequence to `/dev/tty` (when `use_tty` is true and
/// `/dev/tty` is openable) or stdout otherwise.
pub fn emit_osc_7501(status: Option<&ProgramStatus>, use_tty: bool) -> io::Result<()> {
    let seq = match status {
        Some(st) => st.encode_osc_7501(),
        None => "\x1b]7501;state=clear\x07".to_string(),
    };
    if use_tty {
        #[cfg(unix)]
        if let Ok(mut tty) = fs::OpenOptions::new().write(true).open("/dev/tty") {
            tty.write_all(seq.as_bytes())?;
            tty.flush()?;
            return Ok(());
        }
    }
    let mut out = io::stdout().lock();
    out.write_all(seq.as_bytes())?;
    out.flush()
}

/// Convert a natural-language task prompt into a concise kebab-case session
/// label (2–4 meaningful words, max 36 chars). Returns `None` for trivial inputs.
pub fn slugify_prompt(prompt: &str) -> Option<String> {
    const STOP: &[&str] = &[
        "the", "a", "an", "to", "for", "of", "in", "on", "at", "and", "or", "is", "are", "be",
        "can", "you", "please", "could", "would", "help", "me", "my", "this", "that", "it",
        "with", "from", "into", "about", "just", "hey", "hi", "hello",
    ];
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in prompt.chars() {
        if ch.is_ascii_alphanumeric() {
            cur.push(ch.to_ascii_lowercase());
        } else if !cur.is_empty() {
            let w = std::mem::take(&mut cur);
            if w.len() >= 2 && !STOP.contains(&w.as_str()) {
                words.push(w);
                if words.len() >= 4 {
                    break;
                }
            }
        }
    }
    if !cur.is_empty() && words.len() < 4 && cur.len() >= 2 && !STOP.contains(&cur.as_str()) {
        words.push(cur);
    }
    if words.is_empty() {
        return None;
    }
    let mut slug = words.join("-");
    if slug.len() > 36 {
        slug.truncate(36);
        while slug.ends_with('-') {
            slug.pop();
        }
    }
    (slug.len() >= 3).then_some(slug)
}

/// True if `name` matches Tachyon's default generated session names
/// (`adjective-noun` like `misty-sierra`, or `s_1`), so auto-naming never
/// overwrites a session the user already named.
pub fn is_generated_session_name(name: &str) -> bool {
    if name.starts_with("s_") && name[2..].bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    const ADJ: &[&str] = &[
        "amber", "brisk", "clear", "coral", "crisp", "dawn", "dusk", "frost", "lunar", "misty",
        "quiet", "rapid", "solar", "swift", "tidal", "vivid",
    ];
    const NOUN: &[&str] = &[
        "bay", "bluff", "cedar", "cove", "crest", "dune", "fjord", "grove", "mesa", "peak",
        "pine", "ridge", "sierra", "spark", "tundra", "vale",
    ];
    let Some((a, b)) = name.split_once('-') else {
        return false;
    };
    let b_base = b.split_once('-').map_or(b, |(head, _)| head);
    ADJ.contains(&a) && NOUN.contains(&b_base)
}

/// Entry point invoked by coding-agent hooks (`tachyon hook <agent> <event>`).
/// Reads optional JSON payload on stdin, emits OSC 7501 to `/dev/tty`, and
/// optionally auto-names a generated session on the first substantive prompt.
pub fn run_hook(agent: &str, event: &str) -> i32 {
    let mut stdin_raw = String::new();
    // Read stdin non-fatally (hooks pass JSON payloads; empty stdin is fine).
    let _ = io::stdin().read_to_string(&mut stdin_raw);
    let payload: Value = serde_json::from_str(&stdin_raw).unwrap_or(Value::Null);

    let is_agy = agent.eq_ignore_ascii_case("antigravity") || agent.eq_ignore_ascii_case("agy");

    let (state, msg, label) = match event.to_ascii_lowercase().as_str() {
        "prompt" => {
            let prompt_text = payload
                .get("prompt")
                .and_then(Value::as_str)
                .or_else(|| payload.get("lastUserInput").and_then(Value::as_str))
                .or_else(|| payload.get("message").and_then(Value::as_str))
                .unwrap_or("");
            maybe_auto_name_session(prompt_text);
            let short = truncate_msg(prompt_text, 48);
            (ProgramState::Working, short, Some("Thinking".to_string()))
        }
        "working" | "pretool" => {
            let tool = payload
                .get("tool_name")
                .and_then(Value::as_str)
                .or_else(|| payload.get("toolName").and_then(Value::as_str))
                .map(|t| format!("Running {t}"));
            (ProgramState::Working, tool, Some("Working".to_string()))
        }
        "blocked" | "notification" | "approval" => {
            let note = payload
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| payload.get("title").and_then(Value::as_str))
                .and_then(|s| truncate_msg(s, 64))
                .or_else(|| Some("Waiting for input".into()));
            (ProgramState::Blocked, note, Some("Needs input".to_string()))
        }
        "done" | "stop" => (
            ProgramState::Done,
            Some("Finished response".into()),
            Some("Done".to_string()),
        ),
        "error" => {
            let err = payload
                .get("error")
                .and_then(Value::as_str)
                .and_then(|s| truncate_msg(s, 64));
            (ProgramState::Error, err, Some("Error".to_string()))
        }
        "idle" => (ProgramState::Idle, None, Some("Idle".to_string())),
        "clear" => {
            let _ = emit_osc_7501_tty_only_if(None, is_agy);
            if is_agy {
                println!("{{}}");
            }
            return 0;
        }
        _ => {
            if is_agy {
                println!("{{}}");
            }
            return 0;
        }
    };

    let status = ProgramStatus {
        state,
        app: Some(agent.to_string()),
        kind: Some("agent".to_string()),
        msg,
        label,
    };
    let _ = emit_osc_7501_tty_only_if(Some(&status), is_agy);
    if is_agy {
        // Antigravity hooks expect a JSON object on stdout.
        println!("{{}}");
    }
    0
}

fn emit_osc_7501_tty_only_if(status: Option<&ProgramStatus>, tty_only: bool) -> io::Result<()> {
    if !tty_only {
        return emit_osc_7501(status, true);
    }
    let seq = match status {
        Some(st) => st.encode_osc_7501(),
        None => "\x1b]7501;state=clear\x07".to_string(),
    };
    #[cfg(unix)]
    if let Ok(mut tty) = fs::OpenOptions::new().write(true).open("/dev/tty") {
        tty.write_all(seq.as_bytes())?;
        tty.flush()?;
    }
    Ok(())
}

fn truncate_msg(s: &str, max_chars: usize) -> Option<String> {
    let first = s.lines().next()?.trim();
    if first.is_empty() {
        return None;
    }
    if first.chars().count() > max_chars {
        let cut: String = first.chars().take(max_chars.saturating_sub(1)).collect();
        Some(format!("{cut}…"))
    } else {
        Some(first.to_string())
    }
}

fn maybe_auto_name_session(prompt: &str) {
    let cfg = Config::load_default();
    if !cfg.agents.auto_name_sessions {
        return;
    }
    let Ok(sid) = std::env::var("TACHYON_SESSION") else {
        return;
    };
    if sid.is_empty() {
        return;
    }
    let Some(slug) = slugify_prompt(prompt) else {
        return;
    };
    let server = std::env::var("TACHYON_SERVER").ok();
    let hosts = load_hosts();
    let Ok(target) = resolve_target(server.as_deref(), &hosts) else {
        return;
    };
    let Ok(client) = MuxClient::connect(&target, false, "cli", "tachyon-hook") else {
        return;
    };
    let Ok(snap) = client.state() else {
        return;
    };
    let Some(sess) = snap.sessions.iter().find(|s| s.id == sid) else {
        return;
    };
    if is_generated_session_name(&sess.name) {
        let _ = client.rename_session(&sid, &slug);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tachyon-int-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn claude_skill_and_hooks_install_and_remove_cleanly_preserving_user_settings() {
        let home = temp_home("claude");
        let claude_dir = home.join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        let settings_path = claude_dir.join("settings.json");

        // Pre-existing user settings + custom hook that must survive install & remove.
        let initial = json!({
            "model": "claude-opus-4-6",
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{ "type": "command", "command": "echo user-custom-hook" }]
                }]
            }
        });
        fs::write(&settings_path, serde_json::to_string_pretty(&initial).unwrap()).unwrap();

        let status = setup_agent(&home, AgentKind::Claude, true).unwrap();
        assert!(status.detected);
        assert!(status.skill_installed);
        assert!(status.hooks_installed);
        let skill_text = fs::read_to_string(&status.skill_path).unwrap();
        assert!(skill_text.contains("tachyon session rename"));
        assert!(skill_text.contains("tachyon run --wait --capture"));

        let after_install: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(after_install["model"], "claude-opus-4-6");
        let pre = after_install["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2, "user hook kept alongside Tachyon hook");

        // Idempotent second install does not duplicate hooks.
        setup_agent(&home, AgentKind::Claude, false).unwrap();
        let after_second: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(after_second["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert!(
            !fs::read_to_string(&status.skill_path)
                .unwrap()
                .contains("## Naming Sessions"),
            "auto_name=false omits naming guidance"
        );

        // Removing Tachyon leaves the user's hook and model setting completely intact.
        let after_rm = remove_agent(&home, AgentKind::Claude).unwrap();
        assert!(!after_rm.skill_installed);
        assert!(!after_rm.hooks_installed);
        let final_json: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(final_json["model"], "claude-opus-4-6");
        let pre_final = final_json["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_final.len(), 1);
        assert_eq!(
            pre_final[0]["hooks"][0]["command"],
            "echo user-custom-hook"
        );
        assert!(final_json["hooks"].get("Stop").is_none());

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn prompt_slugifier_and_generated_name_detection() {
        assert_eq!(
            slugify_prompt("Please help me refactor the auth middleware in src/auth.rs").as_deref(),
            Some("refactor-auth-middleware-src")
        );
        assert_eq!(slugify_prompt("hi"), None);
        assert!(is_generated_session_name("misty-sierra"));
        assert!(is_generated_session_name("dawn-bluff-2"));
        assert!(is_generated_session_name("s_12"));
        assert!(!is_generated_session_name("refactor-auth"));
        assert!(!is_generated_session_name("train-gpt"));
    }

    #[test]
    fn antigravity_skill_and_hooks_install_and_remove_preserving_user_hooks() {
        let home = temp_home("agy");
        let cfg_dir = home.join(".gemini").join("config");
        fs::create_dir_all(&cfg_dir).unwrap();
        let hooks_path = cfg_dir.join("hooks.json");
        fs::write(
            &hooks_path,
            serde_json::to_string_pretty(&json!({
                "my-linter": {
                    "PostToolUse": [{ "matcher": "edit_file", "hooks": [{ "command": "echo lint" }] }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let status = setup_agent(&home, AgentKind::Antigravity, true).unwrap();
        assert!(status.detected);
        assert!(status.skill_installed);
        assert!(status.hooks_installed);
        assert_eq!(status.skill_path, cfg_dir.join("skills").join("tachyon").join("SKILL.md"));

        let after_install: Value =
            serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
        assert!(after_install.get("my-linter").is_some());
        assert!(after_install.get("tachyon").is_some());
        assert_eq!(
            after_install["tachyon"]["Stop"][0]["command"],
            "tachyon hook antigravity done"
        );

        let after_rm = remove_agent(&home, AgentKind::Antigravity).unwrap();
        assert!(!after_rm.skill_installed);
        assert!(!after_rm.hooks_installed);
        let final_json: Value =
            serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
        assert!(final_json.get("my-linter").is_some());
        assert!(final_json.get("tachyon").is_none());

        let _ = fs::remove_dir_all(&home);
    }
}
