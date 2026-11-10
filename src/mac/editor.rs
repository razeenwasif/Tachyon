//! Opening a file at a line in your editor, for ⌘-clicks on tracebacks and
//! compiler errors.
//!
//! GUI editors are opened through their own command-line tools, found inside
//! their app bundles (so none needs installing on the PATH): Zed's `cli`, VS
//! Code's and Cursor's `code -g`, Sublime Text's `subl`, Xcode's `xed -l`.
//! Terminal editors (nvim, vim, helix, emacs, nano, micro) run in a new tab,
//! started through the shell so its PATH finds them.

use std::path::{Path, PathBuf};

/// How a file gets opened.
#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    /// Run this program with these arguments, in the background.
    Spawn(PathBuf, Vec<String>),
    /// Type this command in a new tab.
    Tab(String),
}

/// GUI editors: (name in the config, bundle id, its tool inside the bundle,
/// whether it wants `-g` before `file:line:col`).
const GUI: [(&str, &str, &str, bool); 4] = [
    ("zed", "dev.zed.Zed", "Contents/MacOS/cli", false),
    ("code", "com.microsoft.VSCode", "Contents/Resources/app/bin/code", true),
    ("cursor", "com.todesktop.230313mzl4w4u92", "Contents/Resources/app/bin/cursor", true),
    ("subl", "com.sublimetext.4", "Contents/SharedSupport/bin/subl", false),
];

/// Terminal editors Tachyon knows how to give a line to.
const TERMINAL: [&str; 8] = ["nvim", "vim", "vi", "hx", "emacs", "nano", "micro", "kak"];

/// What `setting` (`editor` in the config) opens `path` at `line` and `col`
/// with. `bundle` finds an installed app by bundle id; `env` reads
/// `$VISUAL` and `$EDITOR`; `exists` checks a file.
pub fn launch(
    setting: &str,
    path: &Path,
    line: Option<u32>,
    col: Option<u32>,
    bundle: impl Fn(&str) -> Option<PathBuf>,
    env: impl Fn(&str) -> Option<String>,
    exists: impl Fn(&Path) -> bool,
) -> Option<Launch> {
    let setting = setting.trim();
    let file = path.to_string_lossy().into_owned();
    let at = match (line, col) {
        (Some(l), Some(c)) => format!("{file}:{l}:{c}"),
        (Some(l), None) => format!("{file}:{l}"),
        _ => file.clone(),
    };
    let gui = |name: &str| -> Option<Launch> {
        let (_, id, tool, dash_g) = GUI.iter().find(|(n, ..)| *n == name)?;
        let tool = bundle(id)?.join(tool);
        if !exists(&tool) {
            return None;
        }
        let mut args = Vec::new();
        if *dash_g {
            args.push("-g".to_string());
        }
        args.push(at.clone());
        Some(Launch::Spawn(tool, args))
    };
    let terminal = |program: &str| -> Option<Launch> {
        let base = program.rsplit('/').next().unwrap_or(program);
        let q = quote(&file);
        let (l, c) = (line.unwrap_or(1), col.unwrap_or(1));
        Some(Launch::Tab(match base {
            "nvim" | "vim" | "vi" => match line {
                Some(l) => format!("{program} +{l} {q}"),
                None => format!("{program} {q}"),
            },
            "emacs" => format!("{program} -nw +{l}:{c} {q}"),
            "nano" => format!("{program} +{l},{c} {q}"),
            "hx" | "micro" | "kak" if line.is_some() => format!("{program} {}", quote(&at)),
            _ => format!("{program} {q}"),
        }))
    };
    match setting {
        "" | "auto" => GUI
            .iter()
            .find_map(|(name, ..)| gui(name))
            .or_else(|| {
                // `$VISUAL` / `$EDITOR`, when they name a terminal editor.
                ["VISUAL", "EDITOR"].iter().filter_map(|k| env(k)).map(|v| v.trim().to_string()).find(|v| {
                    let base = v.split_whitespace().next().unwrap_or("").rsplit('/').next().unwrap_or("").to_string();
                    TERMINAL.contains(&base.as_str())
                }).and_then(|v| terminal(&v))
            })
            .or_else(|| {
                let nvim = ["/opt/homebrew/bin/nvim", "/usr/local/bin/nvim"].iter().any(|p| exists(Path::new(p)));
                terminal(if nvim { "nvim" } else { "vim" })
            }),
        "vscode" | "code" => gui("code"),
        "zed" | "cursor" => gui(setting),
        "sublime" | "subl" => gui("subl"),
        "xcode" | "xed" => Some(Launch::Spawn(
            PathBuf::from("/usr/bin/xed"),
            match line {
                Some(l) => vec!["-l".into(), l.to_string(), file],
                None => vec![file],
            },
        )),
        other => {
            let base = other.split_whitespace().next().unwrap_or("").rsplit('/').next().unwrap_or("");
            if TERMINAL.contains(&base) {
                terminal(other)
            } else {
                // A command of your own, typed in a new tab: {file}, {line}
                // and {col} are filled in, or the file goes at the end.
                let (l, c) = (line.unwrap_or(1).to_string(), col.unwrap_or(1).to_string());
                Some(Launch::Tab(if other.contains("{file}") {
                    other.replace("{file}", &quote(&file)).replace("{line}", &l).replace("{col}", &c)
                } else {
                    format!("{other} {}", quote(&file))
                }))
            }
        }
    }
}

/// `s` quoted for a POSIX shell.
fn quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+:@%".contains(&b)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(setting: &str, apps: &[&str], env: &[(&str, &str)], line: Option<u32>) -> Option<Launch> {
        let apps: Vec<String> = apps.iter().map(|s| s.to_string()).collect();
        let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        launch(
            setting,
            Path::new("/proj/my train.py"),
            line,
            Some(4).filter(|_| line.is_some()),
            |id| apps.iter().any(|a| a == id).then(|| PathBuf::from(format!("/Applications/{id}.app"))),
            |k| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()),
            |p| !p.to_string_lossy().contains("missing"),
        )
    }

    #[test]
    fn auto_takes_the_first_editor_installed() {
        assert_eq!(
            with("auto", &["dev.zed.Zed", "com.microsoft.VSCode"], &[], Some(142)),
            Some(Launch::Spawn("/Applications/dev.zed.Zed.app/Contents/MacOS/cli".into(), vec!["/proj/my train.py:142:4".into()]))
        );
        assert_eq!(
            with("auto", &["com.microsoft.VSCode"], &[], Some(7)),
            Some(Launch::Spawn("/Applications/com.microsoft.VSCode.app/Contents/Resources/app/bin/code".into(), vec!["-g".into(), "/proj/my train.py:7:4".into()]))
        );
        // No GUI editor: $EDITOR if it's a terminal one, else nvim (here,
        // where every file exists), else vim.
        assert_eq!(with("auto", &[], &[("EDITOR", "hx")], Some(3)), Some(Launch::Tab("hx '/proj/my train.py:3:4'".into())));
        assert_eq!(with("auto", &[], &[], Some(3)), Some(Launch::Tab("nvim +3 '/proj/my train.py'".into())));
    }

    #[test]
    fn named_editors_and_commands_of_your_own() {
        assert_eq!(with("nvim", &[], &[], Some(12)), Some(Launch::Tab("nvim +12 '/proj/my train.py'".into())));
        assert_eq!(with("nvim", &[], &[], None), Some(Launch::Tab("nvim '/proj/my train.py'".into())));
        assert_eq!(
            with("xcode", &[], &[], Some(9)),
            Some(Launch::Spawn("/usr/bin/xed".into(), vec!["-l".into(), "9".into(), "/proj/my train.py".into()]))
        );
        assert_eq!(with("zed", &[], &[], Some(1)), None, "not installed");
        assert_eq!(with("bat --paging=always {file} -H {line}", &[], &[], Some(5)), Some(Launch::Tab("bat --paging=always '/proj/my train.py' -H 5".into())));
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}
