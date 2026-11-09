//! Inline suggestions: as you type at a prompt, the rest of a command you've
//! run before, in grey after the cursor; → takes it.
//!
//! Commands come from two places. Every command line shell integration
//! reports (OSC 133 C, in any tab) is recorded with its folder, in a private
//! file that outlives the app. And at startup the shell's own history (zsh,
//! bash or fish) is read, so suggestions work from the first launch. The
//! newest command that starts with what's typed wins, one run in the same
//! folder first.
//!
//! Commands typed with a leading space aren't recorded, the shells'
//! convention for keeping one out of history.

use std::io::Write;
use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;

/// Commands kept, newest last.
const KEEP: usize = 20_000;

struct Item {
    cmd: String,
    cwd: Option<String>,
}

#[derive(Default)]
pub struct History {
    /// Oldest first. A command run again moves to the end.
    items: Vec<Option<Item>>,
    /// Where each command is in `items`.
    at: FxHashMap<String, usize>,
    live: usize,
    /// Tachyon's own record, appended to as commands run.
    file: Option<PathBuf>,
}

impl History {
    /// The shell's history, then Tachyon's own on top.
    pub fn load() -> History {
        let mut h = History::default();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        if let Some(home) = &home {
            for (path, kind) in shell_history_files(home) {
                if let Ok(bytes) = std::fs::read(&path) {
                    for cmd in parse_shell_history(&bytes, kind) {
                        h.add(&cmd, None);
                    }
                }
            }
            let file = home.join("Library/Application Support/Tachyon/history");
            if let Ok(text) = std::fs::read_to_string(&file) {
                let lines: Vec<&str> = text.lines().collect();
                for line in &lines {
                    if let Some((cwd, cmd)) = line.split_once('\t') {
                        h.add(cmd, (!cwd.is_empty()).then_some(cwd));
                    }
                }
                // Keep the file from growing without end.
                if lines.len() > 2 * KEEP {
                    let keep = lines[lines.len() - KEEP..].join("\n") + "\n";
                    let _ = std::fs::write(&file, keep);
                }
            }
            h.file = Some(file);
        }
        crate::log!("suggestions: {} commands", h.live);
        h
    }

    fn add(&mut self, cmd: &str, cwd: Option<&str>) {
        if let Some(i) = self.at.remove(cmd) {
            self.items[i] = None;
            self.live -= 1;
        }
        self.at.insert(cmd.to_string(), self.items.len());
        self.items.push(Some(Item { cmd: cmd.to_string(), cwd: cwd.map(str::to_string) }));
        self.live += 1;
        if self.live > KEEP {
            if let Some(i) = self.items.iter().position(Option::is_some) {
                if let Some(old) = self.items[i].take() {
                    self.at.remove(&old.cmd);
                    self.live -= 1;
                }
            }
        }
        // Mostly holes: pack.
        if self.items.len() > 2 * self.live + 1024 {
            self.items.retain(Option::is_some);
            self.at = self.items.iter().enumerate().map(|(i, it)| (it.as_ref().expect("retained").cmd.clone(), i)).collect();
        }
    }

    /// A command finished: remember it, and where it ran.
    pub fn record(&mut self, cmd: &str, cwd: Option<&str>) {
        if !worth_keeping(cmd) {
            return;
        }
        self.add(cmd, cwd);
        if let Some(file) = &self.file {
            let _ = append(file, &format!("{}\t{cmd}\n", cwd.unwrap_or("").replace('\t', " ")));
        }
    }

    /// The rest of the newest command that starts with `typed`: one run in
    /// `cwd` if there is one, otherwise any.
    pub fn suggest(&self, typed: &str, cwd: Option<&str>) -> Option<&str> {
        if typed.trim().is_empty() {
            return None;
        }
        let mut anywhere = None;
        for it in self.items.iter().rev().flatten() {
            if it.cmd.len() > typed.len() && it.cmd.starts_with(typed) {
                if cwd.is_some() && it.cwd.as_deref() == cwd {
                    return Some(&it.cmd[typed.len()..]);
                }
                anywhere.get_or_insert(&it.cmd[typed.len()..]);
            }
        }
        anywhere
    }
}

/// A command worth suggesting again: one line, not hidden with a leading
/// space, not blank.
fn worth_keeping(cmd: &str) -> bool {
    !cmd.trim().is_empty() && !cmd.starts_with(' ') && !cmd.contains(['\n', '\r', '\t'])
}

fn append(file: &Path, line: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Command lines can hold anything typed: only this user reads them.
    let mut f = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(file)?;
    f.write_all(line.as_bytes())
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Shell {
    Zsh,
    Bash,
    Fish,
}

fn shell_history_files(home: &Path) -> Vec<(PathBuf, Shell)> {
    let zdot = std::env::var_os("ZDOTDIR").map(PathBuf::from).unwrap_or_else(|| home.to_path_buf());
    vec![
        (home.join(".bash_history"), Shell::Bash),
        (home.join(".local/share/fish/fish_history"), Shell::Fish),
        (zdot.join(".zsh_history"), Shell::Zsh),
    ]
}

/// The commands in a shell's history file, oldest first, one-line ones only.
fn parse_shell_history(bytes: &[u8], kind: Shell) -> Vec<String> {
    let mut out = Vec::new();
    match kind {
        Shell::Zsh => {
            // zsh "metafies" bytes it stores: 0x83 then the byte XOR 32.
            let mut raw = Vec::with_capacity(bytes.len());
            let mut it = bytes.iter();
            while let Some(&b) = it.next() {
                if b == 0x83 {
                    if let Some(&n) = it.next() {
                        raw.push(n ^ 32);
                    }
                } else {
                    raw.push(b);
                }
            }
            let text = String::from_utf8_lossy(&raw);
            let mut continued = false;
            for line in text.lines() {
                // A multi-line command: skip it, and its continuation lines.
                if continued {
                    continued = line.ends_with('\\');
                    continue;
                }
                if line.ends_with('\\') {
                    continued = true;
                    continue;
                }
                // Extended history: ": 1699999999:0;command".
                let cmd = match line.strip_prefix(": ").and_then(|l| l.split_once(';')) {
                    Some((_, cmd)) => cmd,
                    None => line,
                };
                out.push(cmd.to_string());
            }
        }
        Shell::Bash => {
            for line in String::from_utf8_lossy(bytes).lines() {
                // Timestamps, with HISTTIMEFORMAT set.
                if line.starts_with('#') && line[1..].chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                out.push(line.to_string());
            }
        }
        Shell::Fish => {
            for line in String::from_utf8_lossy(bytes).lines() {
                if let Some(cmd) = line.strip_prefix("- cmd: ") {
                    if cmd.contains("\\n") {
                        continue;
                    }
                    out.push(cmd.replace("\\\\", "\\"));
                }
            }
        }
    }
    out.retain(|c| worth_keeping(c));
    out
}

/// The next word of a suggestion, with the spaces before it, for ⌥→.
pub fn next_word(rest: &str) -> &str {
    let start = rest.len() - rest.trim_start().len();
    let end = rest[start..].find(char::is_whitespace).map_or(rest.len(), |i| start + i);
    &rest[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_match_wins_this_folder_first() {
        let mut h = History::default();
        h.record("cargo test", Some("/a"));
        h.record("cargo build --release", Some("/b"));
        h.record("cargo test --release", Some("/c"));
        assert_eq!(h.suggest("cargo t", Some("/a")), Some("est"), "run here");
        assert_eq!(h.suggest("cargo t", Some("/z")), Some("est --release"), "newest anywhere");
        assert_eq!(h.suggest("cargo b", None), Some("uild --release"));
        assert_eq!(h.suggest("cargo test --release", None), None, "nothing more to add");
        assert_eq!(h.suggest("", None), None);
        assert_eq!(h.suggest("ls", None), None);
        // Run again: it moves to the front.
        h.record("cargo test", Some("/c"));
        assert_eq!(h.suggest("cargo t", Some("/z")), Some("est"));
        assert_eq!(h.live, 3);
    }

    #[test]
    fn hidden_and_multi_line_commands_are_not_kept() {
        let mut h = History::default();
        h.record(" export TOKEN=secret", None);
        h.record("for i in 1 2\ndo echo $i\ndone", None);
        assert_eq!(h.live, 0);
    }

    #[test]
    fn shell_history_files_parse() {
        let zsh = b": 1700000000:0;git status\n: 1700000001:0;echo one \\\n two\n: 1700000002:0;cd ~/proj\nplain line\n";
        assert_eq!(parse_shell_history(zsh, Shell::Zsh), ["git status", "cd ~/proj", "plain line"]);
        // Metafied: "é" is stored as 0xC3, 0x83 (0xA9 ^ 32).
        let meta = [b"echo caf".as_slice(), &[0xC3, 0x83, 0xA9 ^ 32], b"\n"].concat();
        assert_eq!(parse_shell_history(&meta, Shell::Zsh), ["echo café"]);
        let bash = b"#1700000000\nmake\nls -la\n";
        assert_eq!(parse_shell_history(bash, Shell::Bash), ["make", "ls -la"]);
        let fish = b"- cmd: git push\n  when: 1700000000\n- cmd: echo a\\nb\n- cmd: echo \\\\x\n";
        assert_eq!(parse_shell_history(fish, Shell::Fish), ["git push", "echo \\x"]);
    }

    #[test]
    fn words_one_at_a_time() {
        assert_eq!(next_word("est --release"), "est");
        assert_eq!(next_word(" --release"), " --release");
        assert_eq!(next_word("  a b"), "  a");
    }
}
