//! Zero-hook coding-agent manifest registry and event-driven state machine.
//!
//! Identifies ~24 coding-agent CLIs from live foreground process metadata,
//! spawned command lines, window titles, or terminal screen banners — without
//! requiring shell hooks or OSC 7501 instrumentation — and classifies each
//! agent into `Working`, `Blocked`, `Error`, `Done`, or `Idle` using screen
//! cues and output/input timing.
//!
//! Explicit OSC 7501 (`Term::program_status`) always takes precedence over this
//! detector in callers.

use std::time::{Duration, Instant};

use crate::term::{ProgramState, ProgramStatus, Term};

/// Quiet duration after output before `Working` settles into `Done`/`Idle`.
const QUIET_TO_SETTLE: Duration = Duration::from_millis(1800);
/// Faster settle duration when an explicit idle/ready prompt is visible.
const QUIET_READY_PROMPT: Duration = Duration::from_millis(600);
/// Minimum continuous working time (or user input turn) before emitting `Done`.
const STARTUP_GRACE: Duration = Duration::from_millis(2500);
const MIN_WORKING_FOR_DONE: Duration = Duration::from_millis(1500);
/// Fallback to stop reporting `Working` if a spinner freezes on screen with no output.
const STALE_SPINNER_TIMEOUT: Duration = Duration::from_secs(10);
/// Byte threshold inside `OUTPUT_BURST_WINDOW` to treat streaming output as active work.
const SUBSTANTIAL_OUTPUT_BYTES: usize = 32;
const OUTPUT_BURST_WINDOW: Duration = Duration::from_millis(500);
/// Maximum visible rows scanned from the bottom for screen cues.
pub const MAX_SCREEN_LINES: usize = 24;

/// Foreground OS process occupying a pane's PTY.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForegroundProc {
    pub pid: u32,
    pub name: String,
    pub cmdline: String,
}

/// Static manifest describing how to detect and classify one coding-agent CLI.
#[derive(Clone, Copy, Debug)]
pub struct AgentManifest {
    pub id: &'static str,
    pub display_name: &'static str,
    pub glyph: &'static str,
    pub binaries: &'static [&'static str],
    pub cmdline_tokens: &'static [&'static str],
    pub title_tokens: &'static [&'static str],
    pub screen_banners: &'static [&'static str],
    pub blocked_patterns: &'static [&'static str],
    pub working_patterns: &'static [&'static str],
    pub error_patterns: &'static [&'static str],
    pub ready_patterns: &'static [&'static str],
}

/// Supported zero-hook coding-agent CLIs (24 agents).
pub const AGENTS: &[AgentManifest] = &[
    AgentManifest {
        id: "claude",
        display_name: "Claude Code",
        glyph: "✳",
        binaries: &["claude", "claude-code"],
        cmdline_tokens: &["@anthropic-ai/claude-code", "claude-code"],
        title_tokens: &["claude code", "claude-code"],
        screen_banners: &["welcome to claude code", "claude code v", "claude code"],
        blocked_patterns: &[
            "do you want to proceed?",
            "do you want to run",
            "allow once",
            "allow for this session",
            "yes, allow",
        ],
        working_patterns: &["esc to interrupt", "thinking…", "thinking...", "compacting conversation"],
        error_patterns: &["credit balance is too low", "anthropic api error", "overloaded_error"],
        ready_patterns: &["try \"", "? for shortcuts", "shift+tab to cycle"],
    },
    AgentManifest {
        id: "antigravity",
        display_name: "Antigravity",
        glyph: "✦",
        binaries: &["antigravity", "agy"],
        cmdline_tokens: &["antigravity", "@google/antigravity"],
        title_tokens: &["antigravity"],
        screen_banners: &["antigravity cli", "welcome to antigravity"],
        blocked_patterns: &["approve action", "waiting for approval"],
        working_patterns: &["esc to interrupt", "generating…", "generating..."],
        error_patterns: &["gemini api error"],
        ready_patterns: &["ask antigravity", "/help for commands"],
    },
    AgentManifest {
        id: "codex",
        display_name: "Codex",
        glyph: "◎",
        binaries: &["codex"],
        cmdline_tokens: &["@openai/codex", "codex-cli"],
        title_tokens: &["openai codex", "codex"],
        screen_banners: &["openai codex", "codex cli"],
        blocked_patterns: &[
            "approve this command",
            "allow execution",
            "would you like to run",
            "yes, proceed",
        ],
        working_patterns: &["working...", "working…", "esc to interrupt"],
        error_patterns: &["openai api error"],
        ready_patterns: &["? for shortcuts", "send message", "100% context left"],
    },
    AgentManifest {
        id: "opencode",
        display_name: "OpenCode",
        glyph: "◈",
        binaries: &["opencode", "open-code"],
        cmdline_tokens: &["opencode-ai", "sst/opencode"],
        title_tokens: &["opencode", "open-code"],
        screen_banners: &["opencode", "ctrl+p commands"],
        blocked_patterns: &["permission required", "allow tool", "deny tool"],
        working_patterns: &["generating...", "generating…", "esc to cancel"],
        error_patterns: &["provider error", "api key missing"],
        ready_patterns: &["ask anything...", "ctrl+p commands"],
    },
    AgentManifest {
        id: "pi",
        display_name: "Pi",
        glyph: "π",
        binaries: &["pi", "pi-coding-agent"],
        cmdline_tokens: &["@mariozechner/pi-coding-agent", "pi-coding-agent"],
        title_tokens: &["pi-coding-agent", "pi coding agent"],
        screen_banners: &["pi coding agent", "@mariozechner/pi-coding-agent"],
        blocked_patterns: &["confirm action", "allow tool"],
        working_patterns: &["streaming...", "streaming…", "esc to abort"],
        error_patterns: &[],
        ready_patterns: &["pi >", "/help"],
    },
    AgentManifest {
        id: "gemini",
        display_name: "Gemini CLI",
        glyph: "✧",
        binaries: &["gemini", "gemini-cli"],
        cmdline_tokens: &["@google/gemini-cli", "gemini-cli"],
        title_tokens: &["gemini cli", "gemini-cli"],
        screen_banners: &["welcome to gemini cli", "gemini cli", "type your message or @path/to/file"],
        blocked_patterns: &[
            "waiting for user confirmation",
            "yes, allow once",
            "yes, allow always",
            "allow execution of",
        ],
        working_patterns: &["esc to cancel", "responding...", "responding…"],
        error_patterns: &["quota exceeded", "gemini error"],
        ready_patterns: &["type your message", "context left"],
    },
    AgentManifest {
        id: "cursor",
        display_name: "Cursor",
        glyph: "▣",
        // Never match bare `agent` so ssh-agent / gpg-agent never false-trigger.
        binaries: &["cursor-agent"],
        cmdline_tokens: &["cursor-agent", "@cursor/agent"],
        title_tokens: &["cursor-agent", "cursor agent"],
        screen_banners: &["cursor agent"],
        blocked_patterns: &["approve tool", "run command?"],
        working_patterns: &["generating...", "esc to interrupt"],
        error_patterns: &[],
        ready_patterns: &["ask anything"],
    },
    AgentManifest {
        id: "aider",
        display_name: "Aider",
        glyph: "◇",
        binaries: &["aider"],
        cmdline_tokens: &["aider-chat", "aider.main"],
        title_tokens: &["aider"],
        screen_banners: &["aider v", "aider-chat"],
        blocked_patterns: &["run shell command?", "add to the chat?", "create new file?"],
        working_patterns: &["waiting for llm", "streaming"],
        error_patterns: &["litellm.", "aider error"],
        ready_patterns: &["tokens:", "> ask", "/help"],
    },
    AgentManifest {
        id: "copilot",
        display_name: "Copilot",
        glyph: "⌘",
        binaries: &["copilot", "github-copilot-cli"],
        cmdline_tokens: &["@github/copilot", "github-copilot-cli", "gh copilot"],
        title_tokens: &["github copilot", "copilot cli"],
        screen_banners: &["github copilot cli", "welcome to github copilot"],
        blocked_patterns: &["confirm command", "run this command?"],
        working_patterns: &["thinking...", "esc to cancel"],
        error_patterns: &["copilot error"],
        ready_patterns: &["ask copilot"],
    },
    AgentManifest {
        id: "grok",
        display_name: "Grok",
        glyph: "⊗",
        binaries: &["grok", "grok-cli"],
        cmdline_tokens: &["@vibe-kit/grok-cli", "grok-cli"],
        title_tokens: &["grok cli", "grok-cli"],
        screen_banners: &["grok cli", "ask grok"],
        blocked_patterns: &["confirm operation"],
        working_patterns: &["thinking...", "esc to interrupt"],
        error_patterns: &[],
        ready_patterns: &["ask grok"],
    },
    AgentManifest {
        id: "goose",
        display_name: "Goose",
        glyph: "◈",
        binaries: &["goose", "goose-cli"],
        cmdline_tokens: &["goose-cli", "block/goose"],
        title_tokens: &["goose cli", "goose session"],
        screen_banners: &["starting session | provider:", "goose is ready"],
        blocked_patterns: &["confirm extension", "allow tool call"],
        working_patterns: &["working on it...", "ctrl+c to cancel"],
        error_patterns: &["provider failed"],
        ready_patterns: &["goose >"],
    },
    AgentManifest {
        id: "amp",
        display_name: "Amp",
        glyph: "⚡",
        binaries: &["amp", "ampcode"],
        cmdline_tokens: &["@sourcegraph/amp", "ampcode"],
        title_tokens: &["sourcegraph amp", "ampcode"],
        screen_banners: &["sourcegraph amp", "amp cli"],
        blocked_patterns: &["approve tool call"],
        working_patterns: &["esc to interrupt", "streaming"],
        error_patterns: &[],
        ready_patterns: &["amp >"],
    },
    AgentManifest {
        id: "cline",
        display_name: "Cline",
        glyph: "◬",
        binaries: &["cline"],
        cmdline_tokens: &["cline-cli", "saoudrizwan/cline"],
        title_tokens: &["cline"],
        screen_banners: &["cline cli", "welcome to cline"],
        blocked_patterns: &["cline wants to", "approve command"],
        working_patterns: &["api request...", "thinking"],
        error_patterns: &[],
        ready_patterns: &["type a task"],
    },
    AgentManifest {
        id: "roo",
        display_name: "Roo Code",
        glyph: "◭",
        binaries: &["roo", "roo-code"],
        cmdline_tokens: &["roo-code", "roocode"],
        title_tokens: &["roo code", "roocode"],
        screen_banners: &["roo code", "roocode"],
        blocked_patterns: &["roo wants to", "approve action"],
        working_patterns: &["working...", "esc to cancel"],
        error_patterns: &[],
        ready_patterns: &["roo >"],
    },
    AgentManifest {
        id: "hermes",
        display_name: "Hermes",
        glyph: "☤",
        binaries: &["hermes", "hermes-agent"],
        cmdline_tokens: &["hermes-agent", "nousresearch/hermes"],
        title_tokens: &["hermes agent", "hermes-agent"],
        screen_banners: &["hermes agent", "nous research hermes"],
        blocked_patterns: &["requires your approval"],
        working_patterns: &["reasoning...", "esc to interrupt"],
        error_patterns: &[],
        ready_patterns: &["hermes >"],
    },
    AgentManifest {
        id: "kiro",
        display_name: "Kiro",
        glyph: "◇",
        // Never match bare `q` so user aliases don't false-trigger.
        binaries: &["kiro", "kiro-cli", "qchat"],
        cmdline_tokens: &["kiro-cli", "amazon-q", "q chat"],
        title_tokens: &["kiro-cli", "amazon q", "kiro cli"],
        screen_banners: &["kiro cli", "amazon q developer cli"],
        blocked_patterns: &["allow tool execution"],
        working_patterns: &["thinking...", "ctrl+c to stop"],
        error_patterns: &[],
        ready_patterns: &["kiro >"],
    },
    AgentManifest {
        id: "crush",
        display_name: "Crush",
        glyph: "✧",
        binaries: &["crush"],
        cmdline_tokens: &["charmbracelet/crush", "@charmland/crush"],
        title_tokens: &["charm crush", "crush cli"],
        screen_banners: &["charmbracelet/crush", "welcome to crush"],
        blocked_patterns: &["allow permission"],
        working_patterns: &["working...", "esc to cancel"],
        error_patterns: &[],
        ready_patterns: &["enter to send"],
    },
    AgentManifest {
        id: "auggie",
        display_name: "Auggie",
        glyph: "◈",
        binaries: &["auggie"],
        cmdline_tokens: &["@augmentcode/auggie", "augment-cli"],
        title_tokens: &["auggie", "augment code"],
        screen_banners: &["augment code", "auggie cli"],
        blocked_patterns: &["approve execution"],
        working_patterns: &["thinking...", "esc to interrupt"],
        error_patterns: &[],
        ready_patterns: &["auggie >"],
    },
    AgentManifest {
        id: "droid",
        display_name: "Droid",
        glyph: "◉",
        binaries: &["droid", "factory-droid"],
        cmdline_tokens: &["@factory-ai/droid", "factory-droid"],
        title_tokens: &["factory droid", "factory-droid"],
        screen_banners: &["factory droid", "droid cli"],
        blocked_patterns: &["approve action"],
        working_patterns: &["working...", "esc to interrupt"],
        error_patterns: &[],
        ready_patterns: &["droid >"],
    },
    AgentManifest {
        id: "qwen",
        display_name: "Qwen Code",
        glyph: "❖",
        binaries: &["qwen", "qwen-code"],
        cmdline_tokens: &["@qwen-code/qwen-code", "qwen-code"],
        title_tokens: &["qwen code", "qwen-code"],
        screen_banners: &["qwen code", "welcome to qwen code"],
        blocked_patterns: &["waiting for user confirmation", "allow once"],
        working_patterns: &["esc to cancel", "responding..."],
        error_patterns: &[],
        ready_patterns: &["type your message"],
    },
    AgentManifest {
        id: "windsurf",
        display_name: "Windsurf",
        glyph: "◈",
        binaries: &["windsurf-cli", "cascade-cli"],
        cmdline_tokens: &["windsurf-cli", "cascade-cli", "codeium-cli"],
        title_tokens: &["windsurf cli", "cascade cli"],
        screen_banners: &["windsurf cli", "cascade cli"],
        blocked_patterns: &["approve step"],
        working_patterns: &["generating...", "esc to cancel"],
        error_patterns: &[],
        ready_patterns: &["cascade >"],
    },
    AgentManifest {
        id: "openhands",
        display_name: "OpenHands",
        glyph: "◎",
        binaries: &["openhands"],
        cmdline_tokens: &["openhands.cli", "all-hands-ai/openhands"],
        title_tokens: &["openhands"],
        screen_banners: &["openhands cli", "starting openhands"],
        blocked_patterns: &["confirm action", "user confirmation"],
        working_patterns: &["running action", "thinking..."],
        error_patterns: &[],
        ready_patterns: &["openhands >"],
    },
    AgentManifest {
        id: "forge",
        display_name: "Forge",
        glyph: "⚒",
        // Never match bare `forge` to avoid colliding with Foundry's `forge`.
        binaries: &["forgecode"],
        cmdline_tokens: &["antinomyhq/forge", "forgecode"],
        title_tokens: &["forgecode"],
        screen_banners: &["forgecode", "antinomy forge"],
        blocked_patterns: &["approve tool"],
        working_patterns: &["thinking...", "esc to interrupt"],
        error_patterns: &[],
        ready_patterns: &["forge >"],
    },
    AgentManifest {
        id: "plandex",
        display_name: "Plandex",
        glyph: "◫",
        binaries: &["plandex"],
        cmdline_tokens: &["plandex-ai", "plandex"],
        title_tokens: &["plandex"],
        screen_banners: &["plandex v", "plandex tell"],
        blocked_patterns: &["apply changes?", "confirm plan"],
        working_patterns: &["building plan", "streaming"],
        error_patterns: &[],
        ready_patterns: &["plandex >"],
    },
];

/// Package/interpreter runtimes whose command-line arguments name the launched
/// agent package or script (unlike outer shells whose `-c` strings may mention
/// agent names as plain text arguments).
const PACKAGE_RUNTIMES: &[&str] = &[
    "node",
    "nodejs",
    "bun",
    "deno",
    "python",
    "python3",
    "python3.11",
    "python3.12",
    "python3.13",
    "npx",
    "pnpx",
    "bunx",
    "uv",
    "uvx",
    "gh",
    "ruby",
    "java",
];

const SHELL_BINARIES: &[&str] = &["sh", "bash", "zsh", "fish", "pwsh", "powershell"];

/// Wrappers that directly exec a trailing command in OSC 133 / spawn command lines.
const CMD_WRAPPERS: &[&str] = &[
    "env",
    "sudo",
    "time",
    "caffeinate",
    "nice",
    "nohup",
    "exec",
    "command",
];

/// Remote/session transports where screen-banner fallback is valuable even
/// when local OS process inspection only sees the transport binary.
const SESSION_TRANSPORTS: &[&str] = &[
    "ssh",
    "mosh",
    "mosh-client",
    "tmux",
    "screen",
    "docker",
    "podman",
    "kubectl",
    "incus",
    "lima",
    "wsl",
];

/// Interactive editors/pagers where alt-screen content should not trigger
/// coding-agent banner heuristics.
const NON_AGENT_ALT_SCREEN: &[&str] = &[
    "vim", "nvim", "vi", "nano", "emacs", "hx", "helix", "micro", "less", "more", "man", "top",
    "htop", "btop", "tig", "lazygit", "gitui", "k9s", "fzf",
];

const GLOBAL_BLOCKED_PATTERNS: &[&str] = &[
    "do you want to proceed?",
    "allow once",
    "allow always",
    "allow for this session",
    "yes, allow",
    "waiting for user confirmation",
    "waiting for approval",
    "permission required",
    "requires approval",
    "needs your approval",
    "approve and run",
    "press enter to confirm",
    "enter to confirm",
    "[y/n]",
    "(y/n)",
    "[y/n/a]",
    "(y)es/(n)o",
    "[y/n/d]",
    "[y/n/t]",
];

const GLOBAL_ERROR_PATTERNS: &[&str] = &[
    "rate limit exceeded",
    "429 too many requests",
    "overloaded_error",
    "credit balance is too low",
    "insufficient_quota",
    "authentication failed",
    "invalid api key",
    "invalid_api_key",
    "unauthorized (401)",
    "api error:",
    "fatal error",
];

const GLOBAL_INTERRUPT_CUES: &[&str] = &[
    "esc to interrupt",
    "esc interrupt",
    "ctrl+c to cancel",
    "ctrl+c to stop",
    "esc to abort",
    "esc to stop",
];

const BRAILLE_AND_SPINNER_CHARS: &[char] = &[
    '⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏', '⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯',
    '⣷', '◐', '◓', '◑', '◒',
];

const THINKING_STAR_CHARS: &[char] = &['✽', '✻', '✶', '✢', '✳'];

/// Lookup a manifest by canonical ID or known alias.
pub fn find_manifest(id_or_alias: &str) -> Option<&'static AgentManifest> {
    let raw = id_or_alias.trim();
    if raw.is_empty() {
        return None;
    }
    let low = raw.to_ascii_lowercase();
    if let Some(m) = AGENTS
        .iter()
        .find(|m| m.id == low || m.binaries.iter().any(|&b| b == low))
    {
        return Some(m);
    }
    if low == "claudecode" || low.contains("claude") {
        return AGENTS.iter().find(|m| m.id == "claude");
    }
    if low.contains("antigravity") {
        return AGENTS.iter().find(|m| m.id == "antigravity");
    }
    if low.contains("codex") {
        return AGENTS.iter().find(|m| m.id == "codex");
    }
    if low.contains("opencode") {
        return AGENTS.iter().find(|m| m.id == "opencode");
    }
    if low.contains("copilot") {
        return AGENTS.iter().find(|m| m.id == "copilot");
    }
    if low.contains("gemini") {
        return AGENTS.iter().find(|m| m.id == "gemini");
    }
    if low.contains("cursor") {
        return AGENTS.iter().find(|m| m.id == "cursor");
    }
    if low.contains("aider") {
        return AGENTS.iter().find(|m| m.id == "aider");
    }
    if low.contains("qwen") {
        return AGENTS.iter().find(|m| m.id == "qwen");
    }
    if low.contains("windsurf") {
        return AGENTS.iter().find(|m| m.id == "windsurf");
    }
    if low.contains("openhands") {
        return AGENTS.iter().find(|m| m.id == "openhands");
    }
    if low.contains("plandex") {
        return AGENTS.iter().find(|m| m.id == "plandex");
    }
    AGENTS
        .iter()
        .find(|m| m.display_name.to_ascii_lowercase() == low)
}

/// Resolve canonical `(app_id, display_name, glyph)` for an agent identifier.
pub fn agent_identity(raw: &str) -> (String, String, &'static str) {
    if let Some(m) = find_manifest(raw) {
        return (m.id.to_string(), m.display_name.to_string(), m.glyph);
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return ("agent".into(), "Agent".into(), "●");
    }
    let mut chars = trimmed.chars();
    let cap = match chars.next() {
        Some(c) => format!("{}{}", c.to_uppercase(), chars.as_str()),
        None => "Agent".into(),
    };
    (trimmed.to_ascii_lowercase(), cap, "●")
}

fn path_basename(raw: &str) -> &str {
    let trimmed = raw.trim().trim_matches('"').trim_matches('\'');
    let no_slash = trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed);
    no_slash.strip_suffix(".exe").unwrap_or(no_slash)
}

/// Unwrap `KEY=VAL` assignments and wrapper commands (`env`, `sudo`, `time`, ...)
/// to find `(executable_basename_lower, original_cmdline)`.
fn unwrap_command(cmdline: &str) -> Option<(String, &str)> {
    let tokens: Vec<&str> = cmdline.split_whitespace().collect();
    let mut i = 0usize;
    while i < tokens.len() {
        let tok = tokens[i].trim_matches('"').trim_matches('\'');
        if tok.is_empty() {
            i += 1;
            continue;
        }
        // Skip leading shell variable assignments (FOO=bar cmd ...)
        if let Some((k, _)) = tok.split_once('=') {
            if !k.is_empty() && !k.contains('/') && !k.contains('\\') {
                i += 1;
                continue;
            }
        }
        let base = path_basename(tok).to_ascii_lowercase();
        if CMD_WRAPPERS.contains(&base.as_str()) {
            i += 1;
            // Skip flags belonging to wrappers like `env -i`, `caffeinate -i`, `sudo -u user`
            while i < tokens.len() && tokens[i].starts_with('-') {
                let flag = tokens[i];
                i += 1;
                if matches!(flag, "-u" | "-C" | "-S" | "-n") && i < tokens.len() {
                    i += 1;
                }
            }
            continue;
        }
        return Some((base, cmdline));
    }
    None
}

fn is_package_runtime(bin: &str) -> bool {
    let low = bin.to_ascii_lowercase();
    PACKAGE_RUNTIMES.contains(&low.as_str())
        || low.starts_with("python3.")
        || low.starts_with("python2.")
}

fn is_shell_binary(bin: &str) -> bool {
    let low = bin.to_ascii_lowercase();
    SHELL_BINARIES.contains(&low.as_str())
}

fn match_cmdline_tokens(cmdline: &str) -> Option<&'static AgentManifest> {
    let low = cmdline.to_ascii_lowercase();
    for m in AGENTS {
        if m.cmdline_tokens.iter().any(|tok| low.contains(tok)) {
            return Some(m);
        }
    }
    None
}

fn match_binary(bin: &str) -> Option<&'static AgentManifest> {
    let low = bin.to_ascii_lowercase();
    AGENTS.iter().find(|m| m.binaries.contains(&low.as_str()))
}

fn match_text_identity(text: &str) -> Option<&'static AgentManifest> {
    let low = text.trim().to_ascii_lowercase();
    if low.is_empty() {
        return None;
    }
    for m in AGENTS {
        if m.title_tokens.iter().any(|tok| low.contains(tok)) {
            return Some(m);
        }
    }
    // Check standalone word tokens for short names like `pi`, `amp`, `roo`, `grok`, `kiro`, `droid`, `gemini`.
    for word in low.split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_') {
        if word.is_empty() {
            continue;
        }
        if matches!(word, "agent" | "forge" | "q") {
            continue;
        }
        if let Some(m) = AGENTS.iter().find(|m| m.binaries.contains(&word)) {
            return Some(m);
        }
    }
    None
}

/// Decide whether screen banner scanning should run for the current pane state.
pub fn should_allow_screen_fallback(
    fg: Option<&ForegroundProc>,
    cmdline: Option<&str>,
    alt_screen: bool,
) -> bool {
    let fg_bin = fg.map(|p| path_basename(&p.name).to_ascii_lowercase());
    let cmd_bin = cmdline
        .and_then(unwrap_command)
        .map(|(b, _)| b);

    for bin in fg_bin.iter().chain(cmd_bin.iter()) {
        if SESSION_TRANSPORTS.contains(&bin.as_str()) || is_package_runtime(bin) || is_shell_binary(bin) {
            return true;
        }
    }
    if alt_screen {
        let is_non_agent = fg_bin
            .iter()
            .chain(cmd_bin.iter())
            .any(|b| NON_AGENT_ALT_SCREEN.contains(&b.as_str()));
        if !is_non_agent {
            return true;
        }
    }
    fg.is_none() && cmdline.is_none()
}

/// 4-tier agent identification without false positives on shell history, args,
/// or ordinary dev tools.
pub fn identify_agent(
    fg: Option<&ForegroundProc>,
    cmdline: Option<&str>,
    title: Option<&str>,
    label: Option<&str>,
    screen_lines: &[String],
    allow_screen_fallback: bool,
) -> Option<&'static AgentManifest> {
    // Tier 1: Foreground OS process inspecting binary first, then package-runtime cmdline.
    if let Some(proc) = fg {
        let bin = path_basename(&proc.name).to_ascii_lowercase();
        if let Some(m) = match_binary(&bin) {
            return Some(m);
        }
        if is_package_runtime(&bin) {
            if let Some(m) = match_cmdline_tokens(&proc.cmdline) {
                return Some(m);
            }
            if let Some(m) = match_binary_in_runtime_args(&proc.cmdline) {
                return Some(m);
            }
        }
    }

    // Tier 2: Command line executable (skipping leading KEY=VAL and wrappers);
    // only scan arguments when the executable is a package runtime so e.g.
    // `git commit -m "claude"` or `sh -c "tachyon status --app claude"` never false-matches.
    if let Some(cmd) = cmdline {
        if let Some((bin, raw)) = unwrap_command(cmd) {
            if let Some(m) = match_binary(&bin) {
                return Some(m);
            }
            if is_package_runtime(&bin) {
                if let Some(m) = match_cmdline_tokens(raw) {
                    return Some(m);
                }
                if let Some(m) = match_binary_in_runtime_args(raw) {
                    return Some(m);
                }
            }
        }
    }

    // Tier 3: Pane label or window title.
    if let Some(l) = label.filter(|s| !s.trim().is_empty()) {
        if let Some(m) = match_text_identity(l) {
            return Some(m);
        }
    }
    if let Some(t) = title.filter(|s| !s.trim().is_empty()) {
        if let Some(m) = match_text_identity(t) {
            return Some(m);
        }
    }

    // Tier 4: Visible screen banners (only when transport/runtime/alt-screen permits).
    if allow_screen_fallback {
        for raw_line in screen_lines {
            let clean = clean_screen_line(raw_line).to_ascii_lowercase();
            if clean.is_empty() {
                continue;
            }
            for m in AGENTS {
                if m.screen_banners.iter().any(|b| clean.contains(b)) {
                    return Some(m);
                }
            }
        }
    }

    None
}

fn match_binary_in_runtime_args(cmdline: &str) -> Option<&'static AgentManifest> {
    let mut skip_next = false;
    for (idx, raw) in cmdline.split_whitespace().enumerate() {
        if idx == 0 {
            continue;
        }
        if skip_next {
            skip_next = false;
            continue;
        }
        let tok = raw.trim_matches('"').trim_matches('\'');
        if tok == "-c" || tok == "-lc" {
            // Inspect nested script tokens for bare binary invocations after quotes.
            continue;
        }
        if tok.starts_with('-') {
            if matches!(tok, "-C" | "--cwd" | "-e") {
                skip_next = true;
            }
            continue;
        }
        let base = path_basename(tok).to_ascii_lowercase();
        if let Some(m) = match_binary(&base) {
            return Some(m);
        }
    }
    None
}

/// Extract up to `max_bottom` non-empty visible rows from `term` (in top-to-bottom order).
pub fn extract_visible_lines(term: &Term, max_bottom: usize) -> Vec<String> {
    let rows = term.rows();
    if rows == 0 || max_bottom == 0 {
        return Vec::new();
    }
    let h = term.grid().history_len();
    let mut non_empty = Vec::with_capacity(rows.min(max_bottom));
    for y in (h..(h + rows)).rev() {
        let line = term.line_text(y);
        if !line.trim().is_empty() {
            non_empty.push(line);
            if non_empty.len() >= max_bottom {
                break;
            }
        }
    }
    non_empty.reverse();
    non_empty
}

/// Strip outer box-drawing borders and collapse whitespace into a concise string.
pub fn clean_screen_line(raw: &str) -> String {
    let trimmed = raw
        .trim()
        .trim_matches(|c: char| matches!(c, '│' | '┃' | '╭' | '╮' | '╰' | '╯' | '─' | '━' | '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼'));
    let mut out = String::with_capacity(trimmed.len());
    let mut prev_ws = false;
    for ch in trimmed.trim().chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out
}

fn truncate_msg(s: &str, max_chars: usize) -> String {
    let clean = clean_screen_line(s);
    if clean.chars().count() <= max_chars {
        return clean;
    }
    let mut out: String = clean.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn has_spinner_prefix(line: &str) -> bool {
    let clean = clean_screen_line(line);
    let Some(first) = clean.chars().next() else {
        return false;
    };
    if BRAILLE_AND_SPINNER_CHARS.contains(&first) {
        return true;
    }
    if THINKING_STAR_CHARS.contains(&first) {
        let rest = clean[first.len_utf8()..].trim();
        // e.g. `✽ Churning…` or `✻ Thinking...`
        return rest.ends_with('…') || rest.ends_with("...") || rest.contains("…");
    }
    false
}

fn title_has_spinner(title: Option<&str>) -> bool {
    let Some(t) = title else { return false };
    t.chars().any(|c| BRAILLE_AND_SPINNER_CHARS.contains(&c))
}

/// Inputs passed into [`AgentTracker::evaluate`].
pub struct AgentObservation<'a> {
    pub fg: Option<&'a ForegroundProc>,
    pub cmdline: Option<&'a str>,
    pub title: Option<&'a str>,
    pub label: Option<&'a str>,
    pub command_running: bool,
    pub alt_screen: bool,
    pub screen_lines: &'a [String],
    pub now: Instant,
}

/// Stateful per-pane detector combining process/banner identification with
/// output/input timing and screen-cue classification.
#[derive(Debug, Clone)]
pub struct AgentTracker {
    agent: Option<&'static AgentManifest>,
    detected_at: Option<Instant>,
    state: ProgramState,
    msg: Option<String>,
    last_output_at: Option<Instant>,
    last_substantial_output_at: Option<Instant>,
    burst_start: Option<Instant>,
    burst_bytes: usize,
    last_input_at: Option<Instant>,
    working_since: Option<Instant>,
    had_explicit_working: bool,
    last_eval_had_cue: bool,
    last_eval_ready_cue: bool,
    had_input_turn: bool,
    turn_reset_on_input: bool,
}

impl Default for AgentTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentTracker {
    pub fn new() -> Self {
        Self {
            agent: None,
            detected_at: None,
            state: ProgramState::Idle,
            msg: None,
            last_output_at: None,
            last_substantial_output_at: None,
            burst_start: None,
            burst_bytes: 0,
            last_input_at: None,
            working_since: None,
            had_explicit_working: false,
            last_eval_had_cue: false,
            last_eval_ready_cue: false,
            had_input_turn: false,
            turn_reset_on_input: false,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn note_output(&mut self, bytes: usize, now: Instant) {
        if bytes == 0 {
            return;
        }
        self.last_output_at = Some(now);
        let reset_burst = self
            .burst_start
            .is_none_or(|t| now.saturating_duration_since(t) > OUTPUT_BURST_WINDOW);
        if reset_burst {
            self.burst_start = Some(now);
            self.burst_bytes = bytes;
        } else {
            self.burst_bytes = self.burst_bytes.saturating_add(bytes);
        }
        if self.burst_bytes >= SUBSTANTIAL_OUTPUT_BYTES {
            self.last_substantial_output_at = Some(now);
        }
    }

    pub fn note_input(&mut self, now: Instant) {
        self.last_input_at = Some(now);
        self.had_input_turn = true;
        if self.state != ProgramState::Working {
            self.turn_reset_on_input = true;
        }
    }

    /// Evaluate the current pane state, updating internal state machine and
    /// returning an inferred [`ProgramStatus`] if a coding agent is active.
    pub fn evaluate(&mut self, obs: &AgentObservation<'_>) -> Option<ProgramStatus> {
        // When clearly back at an idle interactive shell prompt, clear any prior agent.
        if !obs.command_running && obs.fg.is_none() && !obs.alt_screen {
            self.reset();
            return None;
        }

        let allow_fallback = should_allow_screen_fallback(obs.fg, obs.cmdline, obs.alt_screen)
            || self.agent.is_some();
        let active_title = (obs.command_running || obs.fg.is_some() || obs.alt_screen)
            .then_some(obs.title)
            .flatten();

        let identified = identify_agent(
            obs.fg,
            obs.cmdline,
            active_title,
            obs.label,
            obs.screen_lines,
            allow_fallback,
        )
        .or_else(|| {
            // Keep locked agent across momentary title/prompt redraws while the
            // same command or foreground group remains active.
            self.agent.filter(|_| allow_fallback)
        });

        let Some(manifest) = identified else {
            self.reset();
            return None;
        };

        if self.agent.is_none_or(|prev| prev.id != manifest.id) {
            self.agent = Some(manifest);
            self.detected_at = Some(obs.now);
            self.state = ProgramState::Idle;
            self.msg = None;
            self.working_since = None;
            self.had_explicit_working = false;
            self.last_eval_had_cue = false;
            self.last_eval_ready_cue = false;
            self.had_input_turn = false;
            self.turn_reset_on_input = false;
        }

        let (next_state, next_msg) = self.classify_state(manifest, obs);
        self.state = next_state;
        self.msg = next_msg.clone();

        Some(ProgramStatus {
            state: next_state,
            app: Some(manifest.id.to_string()),
            kind: Some("agent".into()),
            msg: next_msg,
            label: None,
        })
    }

    fn classify_state(
        &mut self,
        manifest: &'static AgentManifest,
        obs: &AgentObservation<'_>,
    ) -> (ProgramState, Option<String>) {
        let lines = obs.screen_lines;
        let len = lines.len();

        // 1. Blocked / waiting for approval (immediate, zero debounce).
        self.last_eval_had_cue = false;
        self.last_eval_ready_cue = false;
        let blocked_slice = &lines[len.saturating_sub(16)..];
        if let Some(msg) = detect_blocked(manifest, blocked_slice) {
            self.working_since = None;
            self.turn_reset_on_input = false;
            return (ProgramState::Blocked, Some(msg));
        }

        let quiet_for = self
            .last_output_at
            .map(|t| obs.now.saturating_duration_since(t))
            .unwrap_or(Duration::MAX);
        let subst_quiet_for = self
            .last_substantial_output_at
            .map(|t| obs.now.saturating_duration_since(t))
            .unwrap_or(Duration::MAX);

        let bottom_6 = &lines[len.saturating_sub(6)..];
        let bottom_4 = &lines[len.saturating_sub(4)..];
        let has_spinner = title_has_spinner(obs.title) || bottom_4.iter().any(|l| has_spinner_prefix(l));
        let has_working_cue = has_spinner || has_working_pattern(manifest, bottom_6);
        let ready_cue = has_ready_pattern(manifest, bottom_4);
        self.last_eval_had_cue = has_working_cue;
        self.last_eval_ready_cue = ready_cue;

        // 2. High-confidence Error when not actively working with a spinner.
        if !has_working_cue {
            let err_slice = &lines[len.saturating_sub(8)..];
            if let Some(msg) = detect_error(manifest, err_slice) {
                self.working_since = None;
                self.turn_reset_on_input = false;
                return (ProgramState::Error, Some(msg));
            }
        }

        // If user just typed input while Settled (Done/Error/Blocked), begin a fresh turn as Idle.
        if std::mem::take(&mut self.turn_reset_on_input) && !has_working_cue && subst_quiet_for >= QUIET_TO_SETTLE {
            return (ProgramState::Idle, None);
        }

        // 3. Working: explicit cue (spinner / interrupt hint / agent cue) within stale limit,
        // or recent non-trivial streaming output without an idle prompt sitting quietly at bottom.
        let cue_active = has_working_cue && quiet_for < STALE_SPINNER_TIMEOUT;
        let stream_active = subst_quiet_for < QUIET_TO_SETTLE && (!ready_cue || has_working_cue);
        let fast_prompt_settle = ready_cue && !has_working_cue && subst_quiet_for >= QUIET_READY_PROMPT;

        if (cue_active || stream_active) && !fast_prompt_settle {
            if self.working_since.is_none() {
                self.working_since = Some(obs.now);
            }
            if has_working_cue {
                self.had_explicit_working = true;
            }
            return (ProgramState::Working, extract_working_msg(bottom_6));
        }

        // 4. Settled: Done (only after a genuine working turn) or Idle.
        let was_working_dur = self
            .working_since
            .map(|t| obs.now.saturating_duration_since(t))
            .unwrap_or(Duration::ZERO);
        let since_detect = self
            .detected_at
            .map(|t| obs.now.saturating_duration_since(t))
            .unwrap_or(Duration::ZERO);

        if self.state == ProgramState::Working || self.state == ProgramState::Done {
            let genuine_work = self.had_input_turn
                || (self.had_explicit_working && since_detect >= STARTUP_GRACE)
                || (since_detect >= STARTUP_GRACE && was_working_dur >= MIN_WORKING_FOR_DONE);
            self.working_since = None;
            if self.state == ProgramState::Done || genuine_work {
                return (ProgramState::Done, None);
            }
        }

        self.working_since = None;
        (ProgramState::Idle, None)
    }

    /// Returns how long to wait before running one settle re-evaluation when
    /// currently `Working`, or `None` when settled/inactive (zero idle wake-ups!).
    pub fn next_settle_deadline(&self, now: Instant) -> Option<Duration> {
        if self.agent.is_none() || self.state != ProgramState::Working {
            return None;
        }
        let ref_time = self.last_output_at.or(self.detected_at).unwrap_or(now);
        let elapsed = now.saturating_duration_since(ref_time);
        if self.last_eval_had_cue {
            Some(
                STALE_SPINNER_TIMEOUT
                    .saturating_sub(elapsed)
                    .max(Duration::from_millis(200)),
            )
        } else if self.last_eval_ready_cue {
            Some(
                QUIET_READY_PROMPT
                    .saturating_sub(elapsed)
                    .max(Duration::from_millis(100)),
            )
        } else {
            Some(
                QUIET_TO_SETTLE
                    .saturating_sub(elapsed)
                    .max(Duration::from_millis(100)),
            )
        }
    }
}

fn detect_blocked(manifest: &AgentManifest, lines: &[String]) -> Option<String> {
    let bottom_8_start = lines.len().saturating_sub(8);
    let mut saw_choice_1 = false;
    let mut saw_choice_2_or_cancel = false;
    let mut choice_question: Option<String> = None;

    for (idx, raw) in lines.iter().enumerate() {
        let clean = clean_screen_line(raw);
        if clean.is_empty() {
            continue;
        }
        let low = clean.to_ascii_lowercase();

        if manifest.blocked_patterns.iter().any(|p| low.contains(p))
            || GLOBAL_BLOCKED_PATTERNS.iter().any(|p| low.contains(p))
        {
            return Some(truncate_msg(&clean, 72));
        }

        if idx >= bottom_8_start {
            if low.starts_with("1.") && low.contains("yes") {
                saw_choice_1 = true;
            } else if (low.starts_with("2.") && (low.contains("no") || low.contains("yes")))
                || low.contains("esc to cancel")
            {
                saw_choice_2_or_cancel = true;
            } else if clean.ends_with('?') && !low.starts_with("?") {
                choice_question = Some(clean.clone());
            }
        }
    }

    if saw_choice_1 && saw_choice_2_or_cancel {
        return Some(truncate_msg(
            choice_question.as_deref().unwrap_or("Waiting for approval"),
            72,
        ));
    }
    None
}

fn detect_error(manifest: &AgentManifest, lines: &[String]) -> Option<String> {
    for raw in lines.iter().rev() {
        let clean = clean_screen_line(raw);
        if clean.is_empty() {
            continue;
        }
        let low = clean.to_ascii_lowercase();
        if manifest.error_patterns.iter().any(|p| low.contains(p))
            || GLOBAL_ERROR_PATTERNS.iter().any(|p| low.contains(p))
        {
            return Some(truncate_msg(&clean, 72));
        }
    }
    None
}

fn has_working_pattern(manifest: &AgentManifest, lines: &[String]) -> bool {
    lines.iter().any(|raw| {
        let low = clean_screen_line(raw).to_ascii_lowercase();
        if low.is_empty() {
            return false;
        }
        manifest.working_patterns.iter().any(|p| low.contains(p))
            || GLOBAL_INTERRUPT_CUES.iter().any(|p| low.contains(p))
    })
}

fn has_ready_pattern(manifest: &AgentManifest, lines: &[String]) -> bool {
    lines.iter().any(|raw| {
        let clean = clean_screen_line(raw);
        if clean.is_empty() {
            return false;
        }
        let low = clean.to_ascii_lowercase();
        manifest.ready_patterns.iter().any(|p| low.contains(p))
            || low == ">"
            || low.ends_with("❯")
            || clean == "❯"
    })
}

fn extract_working_msg(lines: &[String]) -> Option<String> {
    for raw in lines.iter().rev() {
        let clean = clean_screen_line(raw);
        if clean.is_empty() {
            continue;
        }
        if has_spinner_prefix(&clean) {
            let body = clean
                .trim_start_matches(|c: char| {
                    BRAILLE_AND_SPINNER_CHARS.contains(&c) || THINKING_STAR_CHARS.contains(&c) || c == ' '
                })
                .trim();
            if !body.is_empty() {
                return Some(truncate_msg(body, 60));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_registry_has_24_unique_agents() {
        assert_eq!(AGENTS.len(), 24);
        let mut ids = std::collections::HashSet::new();
        for m in AGENTS {
            assert!(ids.insert(m.id), "duplicate manifest id `{}`", m.id);
            assert!(!m.binaries.is_empty(), "{} needs binaries", m.id);
        }
        assert_eq!(agent_identity("gemini"), ("gemini".into(), "Gemini CLI".into(), "✧"));
        assert_eq!(agent_identity("aider"), ("aider".into(), "Aider".into(), "◇"));
        assert_eq!(agent_identity("pi"), ("pi".into(), "Pi".into(), "π"));
        assert_eq!(agent_identity("hermes"), ("hermes".into(), "Hermes".into(), "☤"));
        assert_eq!(agent_identity("custom-bot"), ("custom-bot".into(), "Custom-bot".into(), "●"));
    }

    #[test]
    fn identification_avoids_false_positives_on_normal_commands_and_tools() {
        let none: &[String] = &[];
        assert!(identify_agent(None, Some("git commit -m \"fix claude hook\""), None, None, none, false).is_none());
        assert!(identify_agent(None, Some("cargo test codex"), None, None, none, false).is_none());
        assert!(identify_agent(None, Some("vim aider.py"), None, None, none, false).is_none());
        assert!(identify_agent(None, Some("ssh-agent -s"), None, None, none, false).is_none());
        assert!(identify_agent(None, Some("forge build"), None, None, none, false).is_none());
        assert!(identify_agent(None, None, Some("root@kagutsuchi:~"), None, none, false).is_none());
        assert!(identify_agent(None, None, Some("agent"), Some("agent"), none, false).is_none());

        // True positives across direct binaries and runtimes.
        assert_eq!(identify_agent(None, Some("/opt/homebrew/bin/claude --resume"), None, None, none, false).unwrap().id, "claude");
        assert_eq!(identify_agent(None, Some("npx @google/gemini-cli"), None, None, none, false).unwrap().id, "gemini");
        assert_eq!(identify_agent(None, Some("FOO=bar env aider --model sonnet"), None, None, none, false).unwrap().id, "aider");
        assert_eq!(identify_agent(None, Some("uvx hermes-agent"), None, None, none, false).unwrap().id, "hermes");

        // Foreground process beats outer shell command.
        let fg = ForegroundProc {
            pid: 4242,
            name: "node".into(),
            cmdline: "node /usr/local/bin/codex --yolo".into(),
        };
        assert_eq!(identify_agent(Some(&fg), Some("zsh"), None, None, none, true).unwrap().id, "codex");

        // Remote SSH session falls back to screen banners safely.
        let banner = vec![
            "╭──────────────────────────────────╮".to_string(),
            "│ Welcome to Claude Code v1.0.21   │".to_string(),
            "╰──────────────────────────────────╯".to_string(),
        ];
        let ssh_fg = ForegroundProc {
            pid: 100,
            name: "ssh".into(),
            cmdline: "ssh kagutsuchi".into(),
        };
        assert!(should_allow_screen_fallback(Some(&ssh_fg), Some("ssh kagutsuchi"), false));
        assert_eq!(
            identify_agent(Some(&ssh_fg), Some("ssh kagutsuchi"), None, None, &banner, true)
                .unwrap()
                .id,
            "claude"
        );
    }

    #[test]
    fn tracker_transitions_idle_working_blocked_error_and_done_without_false_done_on_startup() {
        let mut tr = AgentTracker::new();
        let t0 = Instant::now();
        let fg = ForegroundProc {
            pid: 99,
            name: "gemini".into(),
            cmdline: "gemini".into(),
        };

        // Startup banner output settles to Idle (never spurious Done).
        tr.note_output(240, t0);
        let ready_screen = vec!["Welcome to Gemini CLI".into(), "Type your message".into()];
        let st = tr
            .evaluate(&AgentObservation {
                fg: Some(&fg),
                cmdline: None,
                title: None,
                label: None,
                command_running: true,
                alt_screen: false,
                screen_lines: &ready_screen,
                now: t0,
            })
            .unwrap();
        assert_eq!(st.app.as_deref(), Some("gemini"));
        assert_eq!(st.state, ProgramState::Idle);
        assert_eq!(tr.next_settle_deadline(t0), None);

        // User submits input -> output streams with spinner -> Working!
        let t1 = t0 + Duration::from_secs(3);
        tr.note_input(t1);
        tr.note_output(120, t1 + Duration::from_millis(50));
        let work_screen = vec!["⠋ Thinking through files…".into(), "esc to cancel".into()];
        let st = tr
            .evaluate(&AgentObservation {
                fg: Some(&fg),
                cmdline: None,
                title: None,
                label: None,
                command_running: true,
                alt_screen: false,
                screen_lines: &work_screen,
                now: t1 + Duration::from_millis(60),
            })
            .unwrap();
        assert_eq!(st.state, ProgramState::Working);
        assert_eq!(st.msg.as_deref(), Some("Thinking through files…"));
        assert!(tr.next_settle_deadline(t1 + Duration::from_millis(60)).is_some());

        // Approval prompt triggers Blocked immediately with clean extracted message.
        let blocked_screen = vec![
            "Run shell command: cargo test".into(),
            "Do you want to proceed?".into(),
            "1. Yes".into(),
            "2. No (esc to cancel)".into(),
        ];
        let st = tr
            .evaluate(&AgentObservation {
                fg: Some(&fg),
                cmdline: None,
                title: None,
                label: None,
                command_running: true,
                alt_screen: false,
                screen_lines: &blocked_screen,
                now: t1 + Duration::from_millis(200),
            })
            .unwrap();
        assert_eq!(st.state, ProgramState::Blocked);
        assert_eq!(st.msg.as_deref(), Some("Do you want to proceed?"));
        assert_eq!(tr.next_settle_deadline(t1 + Duration::from_millis(200)), None);

        // User approves -> resumes Working -> finishes -> settles to Done!
        let t2 = t1 + Duration::from_secs(1);
        tr.note_input(t2);
        tr.note_output(160, t2 + Duration::from_millis(50));
        let st = tr
            .evaluate(&AgentObservation {
                fg: Some(&fg),
                cmdline: None,
                title: None,
                label: None,
                command_running: true,
                alt_screen: false,
                screen_lines: &work_screen,
                now: t2 + Duration::from_millis(100),
            })
            .unwrap();
        assert_eq!(st.state, ProgramState::Working);

        let t3 = t2 + Duration::from_secs(3);
        let st = tr
            .evaluate(&AgentObservation {
                fg: Some(&fg),
                cmdline: None,
                title: None,
                label: None,
                command_running: true,
                alt_screen: false,
                screen_lines: &ready_screen,
                now: t3,
            })
            .unwrap();
        assert_eq!(st.state, ProgramState::Done);
        assert_eq!(tr.next_settle_deadline(t3), None);

        // Rate-limit / API error screen -> Error.
        let err_screen = vec!["Error: Rate limit exceeded (429 Too Many Requests)".into()];
        let st = tr
            .evaluate(&AgentObservation {
                fg: Some(&fg),
                cmdline: None,
                title: None,
                label: None,
                command_running: true,
                alt_screen: false,
                screen_lines: &err_screen,
                now: t3 + Duration::from_secs(1),
            })
            .unwrap();
        assert_eq!(st.state, ProgramState::Error);

        // Process exits back to idle shell prompt -> tracker resets to None cleanly.
        let cleared = tr.evaluate(&AgentObservation {
            fg: None,
            cmdline: None,
            title: None,
            label: None,
            command_running: false,
            alt_screen: false,
            screen_lines: &ready_screen,
            now: t3 + Duration::from_secs(2),
        });
        assert_eq!(cleared, None);
    }

    #[test]
    fn runtime_flags_aider_prose_and_fast_settle_deadline_after_spinner_disappears() {
        let none: &[String] = &[];
        assert_eq!(
            identify_agent(None, Some("python3 -m aider --model sonnet"), None, None, none, false)
                .unwrap()
                .id,
            "aider"
        );
        assert_eq!(
            identify_agent(None, Some("npx -p @openai/codex"), None, None, none, false)
                .unwrap()
                .id,
            "codex"
        );

        let mut tr = AgentTracker::new();
        let t0 = Instant::now();
        let fg = ForegroundProc {
            pid: 77,
            name: "aider".into(),
            cmdline: "aider".into(),
        };
        tr.note_input(t0);
        tr.note_output(120, t0 + Duration::from_millis(50));
        let work = vec!["⠋ Waiting for LLM…".into()];
        let st = tr
            .evaluate(&AgentObservation {
                fg: Some(&fg),
                cmdline: None,
                title: None,
                label: None,
                command_running: true,
                alt_screen: false,
                screen_lines: &work,
                now: t0 + Duration::from_millis(60),
            })
            .unwrap();
        assert_eq!(st.state, ProgramState::Working);
        assert!(tr.next_settle_deadline(t0 + Duration::from_millis(60)).unwrap() > Duration::from_secs(5));

        // Spinner finishes and Aider prints prose mentioning "add" and "create new file" — must NOT false-trigger Blocked,
        // and settle deadline must drop to <= 1.8s once the spinner is gone.
        let t1 = t0 + Duration::from_millis(200);
        tr.note_output(200, t1);
        let prose = vec!["I will add a helper and create new file utils.rs later.".into()];
        let st = tr
            .evaluate(&AgentObservation {
                fg: Some(&fg),
                cmdline: None,
                title: None,
                label: None,
                command_running: true,
                alt_screen: false,
                screen_lines: &prose,
                now: t1 + Duration::from_millis(10),
            })
            .unwrap();
        assert_eq!(st.state, ProgramState::Working);
        let dl = tr.next_settle_deadline(t1 + Duration::from_millis(10)).unwrap();
        assert!(dl <= QUIET_TO_SETTLE, "expected fast settle deadline <= 1.8s, got {dl:?}");
    }
}
