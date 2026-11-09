//! What a ⌘-click points at: a URL to open, or a file to show in Quick Look.

use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    Url(String),
    Path(PathBuf),
}

/// Resolve the word under a ⌘-click. Relative paths are taken from the tab's
/// folder (`cwd`, from OSC 7); `exists` decides whether a path is real.
///
/// Compiler-style `file:line:col` suffixes are dropped, as are brackets,
/// quotes and sentence punctuation around the word.
pub fn resolve(word: &str, cwd: Option<&Path>, home: Option<&Path>, exists: impl Fn(&Path) -> bool) -> Option<Target> {
    let word = word
        .trim_start_matches(['(', '[', '{', '<', '\'', '"', '`'])
        .trim_end_matches([')', ']', '}', '>', '\'', '"', '`', '.', ',', ';', ':', '!', '?']);
    if word.is_empty() {
        return None;
    }
    if word.starts_with("https://") || word.starts_with("http://") {
        return Some(Target::Url(word.to_string()));
    }
    if word.contains("://") {
        return None;
    }

    // `src/main.rs:42:7` -> `src/main.rs`.
    let mut path = word;
    for _ in 0..2 {
        match path.rsplit_once(':') {
            Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => path = head,
            _ => break,
        }
    }

    let full = if path == "~" {
        home?.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home?.join(rest)
    } else if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        cwd?.join(path)
    };
    exists(&full).then_some(Target::Path(full))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(word: &str) -> Option<Target> {
        let real = ["/proj/src/main.rs", "/proj/loss.png", "/home/me/notes.md", "/etc/hosts"];
        resolve(word, Some(Path::new("/proj")), Some(Path::new("/home/me")), |p| {
            real.iter().any(|r| Path::new(r) == p)
        })
    }

    #[test]
    fn urls_open_as_they_are() {
        assert_eq!(r("https://github.com/razeenwasif/Tachyon."), Some(Target::Url("https://github.com/razeenwasif/Tachyon".into())));
        assert_eq!(r("(http://localhost:8080/x)"), Some(Target::Url("http://localhost:8080/x".into())));
        assert_eq!(r("ftp://host/file"), None);
    }

    #[test]
    fn paths_resolve_from_the_tab_folder_and_home() {
        assert_eq!(r("src/main.rs:42:7"), Some(Target::Path("/proj/src/main.rs".into())));
        assert_eq!(r("'loss.png'"), Some(Target::Path("/proj/loss.png".into())));
        assert_eq!(r("~/notes.md"), Some(Target::Path("/home/me/notes.md".into())));
        assert_eq!(r("/etc/hosts:"), Some(Target::Path("/etc/hosts".into())));
        assert_eq!(r("missing.txt"), None);
        assert_eq!(r("epoch"), None);
    }

    #[test]
    fn relative_paths_need_a_folder() {
        assert_eq!(resolve("loss.png", None, None, |_| true), None);
    }
}
