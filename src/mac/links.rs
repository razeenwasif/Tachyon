//! What a ⌘-click points at: a URL to open, or a file, perhaps at a line
//! (a compiler's `src/main.rs:42:7`, a Python traceback's
//! `File "train.py", line 142`), to show in Quick Look or open in an editor.

use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    Url(String),
    /// A file that exists, and the line (and column) the text gave, if any.
    File { path: PathBuf, line: Option<u32>, col: Option<u32> },
}

/// Resolve the word under a ⌘-click. Relative paths are taken from the tab's
/// folder (`cwd`, from OSC 7); `exists` decides whether a path is real.
///
/// Compiler-style `file:line:col` suffixes give the line and column; brackets,
/// quotes and sentence punctuation around the word are dropped, as is a call
/// before a bracket (`at foo (src/app.js:10:5)`).
pub fn resolve(word: &str, cwd: Option<&Path>, home: Option<&Path>, exists: impl Fn(&Path) -> bool) -> Option<Target> {
    let mut word = word
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
    // `Foo.bar(Foo.java:42)`, `fn(/x/app.js:3:1)`: what's in the brackets.
    if let Some((_, inner)) = word.rsplit_once('(') {
        if inner.contains(':') || inner.contains('/') {
            word = inner;
        }
    }

    // `src/main.rs:42:7` -> `src/main.rs`, line 42, column 7.
    let mut path = word;
    let mut numbers = Vec::new();
    for _ in 0..2 {
        match path.rsplit_once(':') {
            Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => {
                numbers.insert(0, tail.parse::<u32>().ok());
                path = head;
            }
            _ => break,
        }
    }
    let (line, col) = (numbers.first().copied().flatten(), numbers.get(1).copied().flatten());
    let full = full_path(path, cwd, home)?;
    exists(&full).then_some(Target::File { path: full, line, col })
}

fn full_path(path: &str, cwd: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    Some(if path == "~" {
        home?.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home?.join(rest)
    } else if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        cwd?.join(path)
    })
}

/// A traceback line naming a file and a line, from Python (`File "x.py",
/// line 142, in main`), clicked at column `at` of `text`: anywhere from
/// `File` to the line number counts.
pub fn traceback(text: &str, at: usize, cwd: Option<&Path>, home: Option<&Path>, exists: impl Fn(&Path) -> bool) -> Option<Target> {
    let chars: Vec<char> = text.chars().collect();
    let s: String = chars.iter().collect();
    let start = s.find("File \"")?;
    let path_start = start + "File \"".len();
    let path_end = path_start + s[path_start..].find('"')?;
    let rest = &s[path_end..];
    let after = rest.strip_prefix("\", line ")?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    let line: u32 = digits.parse().ok()?;
    // Byte offsets to columns: the click is in characters.
    let col_of = |byte: usize| s[..byte].chars().count();
    let (from, to) = (col_of(start), col_of(path_end + "\", line ".len() + digits.len()));
    if at < from || at >= to {
        return None;
    }
    let full = full_path(&s[path_start..path_end], cwd, home)?;
    exists(&full).then_some(Target::File { path: full, line: Some(line), col: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: [&str; 6] = ["/proj/src/main.rs", "/proj/loss.png", "/home/me/notes.md", "/etc/hosts", "/proj/train.py", "/proj/app.js"];

    fn r(word: &str) -> Option<Target> {
        resolve(word, Some(Path::new("/proj")), Some(Path::new("/home/me")), |p| REAL.iter().any(|r| Path::new(r) == p))
    }

    fn file(path: &str, line: Option<u32>, col: Option<u32>) -> Option<Target> {
        Some(Target::File { path: path.into(), line, col })
    }

    #[test]
    fn urls_open_as_they_are() {
        assert_eq!(r("https://github.com/razeenwasif/Tachyon."), Some(Target::Url("https://github.com/razeenwasif/Tachyon".into())));
        assert_eq!(r("(http://localhost:8080/x)"), Some(Target::Url("http://localhost:8080/x".into())));
        assert_eq!(r("ftp://host/file"), None);
    }

    #[test]
    fn paths_resolve_from_the_tab_folder_and_home() {
        assert_eq!(r("src/main.rs:42:7"), file("/proj/src/main.rs", Some(42), Some(7)));
        assert_eq!(r("'loss.png'"), file("/proj/loss.png", None, None));
        assert_eq!(r("~/notes.md"), file("/home/me/notes.md", None, None));
        assert_eq!(r("/etc/hosts:"), file("/etc/hosts", None, None));
        assert_eq!(r("missing.txt"), None);
        assert_eq!(r("epoch"), None);
    }

    #[test]
    fn lines_come_from_compilers_test_runners_and_stack_traces() {
        assert_eq!(r("train.py:142:"), file("/proj/train.py", Some(142), None), "pytest");
        assert_eq!(r("(/proj/app.js:10:5)"), file("/proj/app.js", Some(10), Some(5)), "node");
        assert_eq!(r("handler(/proj/app.js:3:1)"), file("/proj/app.js", Some(3), Some(1)));
    }

    #[test]
    fn python_tracebacks_name_a_file_and_a_line() {
        let line = r#"  File "/proj/train.py", line 142, in <module>"#;
        let t = |at| traceback(line, at, Some(Path::new("/proj")), None, |p| REAL.iter().any(|r| Path::new(r) == p));
        assert_eq!(t(10), file("/proj/train.py", Some(142), None), "on the path");
        assert_eq!(t(2), file("/proj/train.py", Some(142), None), "on File");
        assert_eq!(t(30), file("/proj/train.py", Some(142), None), "on the line number");
        assert_eq!(t(40), None, "past it: `in <module>`");
        assert_eq!(t(0), None);
        let relative = r#"  File "train.py", line 3"#;
        assert_eq!(traceback(relative, 9, Some(Path::new("/proj")), None, |p| p == Path::new("/proj/train.py")), file("/proj/train.py", Some(3), None));
        assert_eq!(traceback("no traceback here", 3, None, None, |_| true), None);
    }

    #[test]
    fn relative_paths_need_a_folder() {
        assert_eq!(resolve("loss.png", None, None, |_| true), None);
    }
}
