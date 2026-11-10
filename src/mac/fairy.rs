//! Fairy, the assistant on this Mac: asked about what's on the terminal's
//! screen, her answer shown in a card beside it.
//!
//! Fairy listens on a private Unix socket in her data folder (only this user
//! can open it). Each message is a line of JSON: Tachyon sends `hello`,
//! `ask` (a question and the terminal's context), `stop` and `new`; Fairy
//! answers `hello` with her name, and an `ask` with `text` (her reply so far)
//! as she writes it, then `done` or `error`. When she isn't running, she's
//! opened in the background and asked once her socket appears.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Fairy's bundle identifier (her app was called Lyra first).
pub const BUNDLE_ID: &str = "dev.razeen.lyra";

/// Where Fairy listens, unless the config says otherwise.
pub fn default_socket() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/lyra-companion/lyra/tachyon.sock"))
}

/// What the terminal shows, for Fairy to look at.
#[derive(Clone, Debug, Default)]
pub struct Context {
    pub cwd: Option<String>,
    pub command: Option<String>,
    pub exit: Option<i32>,
    pub output: Option<String>,
    pub screen: String,
    pub selection: Option<String>,
}

impl Context {
    fn to_json(&self) -> Value {
        let mut v = json!({ "screen": self.screen });
        let put = |v: &mut Value, k: &str, s: &Option<String>| {
            if let Some(s) = s.as_ref().filter(|s| !s.is_empty()) {
                v[k] = json!(s);
            }
        };
        put(&mut v, "cwd", &self.cwd);
        put(&mut v, "command", &self.command);
        put(&mut v, "output", &self.output);
        put(&mut v, "selection", &self.selection);
        if let Some(e) = self.exit {
            v["exit"] = json!(e);
        }
        v
    }
}

/// What comes back while Fairy answers.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// She wasn't running: she's being woken.
    Waking,
    /// Her name, as she's called in her settings.
    Hello(String),
    /// Her reply so far.
    Text(String),
    Done(String),
    Error(String),
}

/// One message from Fairy.
fn parse(line: &str) -> Option<Event> {
    let v: Value = serde_json::from_str(line).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Some(match v.get("type")?.as_str()? {
        "hello" => Event::Hello(s("name")),
        "text" => Event::Text(s("text")),
        "done" => Event::Done(s("text")),
        "error" => Event::Error(s("message")),
        _ => return None,
    })
}

/// A connection to Fairy for one question, which can be stopped.
pub struct Asking {
    stream: UnixStream,
}

impl Asking {
    /// Stop her answer where it is.
    pub fn stop(&self) {
        let _ = (&self.stream).write_all(b"{\"type\":\"stop\"}\n");
    }
}

/// Open Fairy's socket, waking her if she isn't running. `waking` is
/// called if she has to be opened.
fn connect(path: &Path, waking: &dyn Fn()) -> Result<UnixStream, String> {
    if let Ok(s) = UnixStream::connect(path) {
        return Ok(s);
    }
    waking();
    open_fairy()?;
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        std::thread::sleep(Duration::from_millis(300));
        if let Ok(s) = UnixStream::connect(path) {
            return Ok(s);
        }
        if Instant::now() > deadline {
            return Err("Fairy didn't answer. If she's open, she may need updating to talk to Tachyon.".into());
        }
    }
}

/// Open Fairy in the background, without taking focus.
fn open_fairy() -> Result<(), String> {
    let status = std::process::Command::new("/usr/bin/open")
        .args(["-g", "-b", BUNDLE_ID])
        .status()
        .map_err(|e| format!("couldn't open Fairy: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("Fairy isn't installed (no app with her bundle identifier).".into())
    }
}

/// Ask Fairy `question` about `context` (or, with `fresh`, start a new
/// conversation first), on a thread of its own. `on` gets every event, on
/// that thread. Returns the connection, to stop her, once it's open.
pub fn ask(
    path: PathBuf,
    question: String,
    context: Context,
    fresh: bool,
    on: impl Fn(Event) + Send + Sync + 'static,
) -> std::sync::mpsc::Receiver<Asking> {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("tachyon-fairy".into()).spawn(move || {
        let stream = match connect(&path, &|| on(Event::Waking)) {
            Ok(s) => s,
            Err(e) => return on(Event::Error(e)),
        };
        if let Ok(clone) = stream.try_clone() {
            let _ = tx.send(Asking { stream: clone });
        }
        let mut out = String::new();
        out.push_str("{\"type\":\"hello\"}\n");
        if fresh {
            out.push_str("{\"type\":\"new\"}\n");
        }
        out.push_str(&json!({ "type": "ask", "text": question, "context": context.to_json() }).to_string());
        out.push('\n');
        if let Err(e) = (&stream).write_all(out.as_bytes()) {
            return on(Event::Error(format!("couldn't reach Fairy: {e}")));
        }
        for line in BufReader::new(&stream).lines() {
            let Ok(line) = line else { break };
            let Some(event) = parse(&line) else { continue };
            let last = matches!(event, Event::Done(_) | Event::Error(_));
            on(event);
            if last {
                return;
            }
        }
        on(Event::Error("Fairy stopped answering.".into()));
    });
    if spawned.is_err() {
        crate::log!("fairy: could not start a thread");
    }
    rx
}

/// Text cut to its last `n` characters, at a line start where one is near.
pub fn tail(text: &str, n: usize) -> String {
    let count = text.chars().count();
    if count <= n {
        return text.to_string();
    }
    let rest: String = text.chars().skip(count - n).collect();
    match rest.find('\n') {
        Some(i) if i < 200 => rest[i + 1..].to_string(),
        _ => rest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};

    #[test]
    fn messages_from_fairy_parse() {
        assert_eq!(parse(r#"{"type":"text","text":"Hel\nlo ✨"}"#), Some(Event::Text("Hel\nlo ✨".into())));
        assert_eq!(parse(r#"{"type":"hello","name":"Stella"}"#), Some(Event::Hello("Stella".into())));
        assert_eq!(parse(r#"{"type":"error","message":"no"}"#), Some(Event::Error("no".into())));
        assert_eq!(parse(r#"{"type":"surprise"}"#), None);
        assert_eq!(parse("not json"), None);
    }

    #[test]
    fn a_question_goes_out_and_the_answer_streams_back() {
        let dir = std::env::temp_dir().join(format!("tachyon-fairy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        // A stand-in Fairy: reads what's sent, answers in two pieces.
        let fairy = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut lines = BufReader::new(conn.try_clone().unwrap()).lines();
            let mut got = Vec::new();
            for _ in 0..3 {
                got.push(lines.next().unwrap().unwrap());
            }
            let mut w = &conn;
            w.write_all(b"{\"type\":\"hello\",\"name\":\"Stella\"}\n{\"type\":\"text\",\"text\":\"It \"}\n{\"type\":\"text\",\"text\":\"It failed.\"}\n{\"type\":\"done\",\"text\":\"It failed.\"}\n").unwrap();
            got
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&events);
        let ctx = Context { command: Some("make".into()), exit: Some(2), screen: "$ make\nerror".into(), ..Context::default() };
        let rx = ask(path.clone(), "Why?".into(), ctx, true, move |e| seen.lock().unwrap().push(e));
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok(), "connected");
        let sent = fairy.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !events.lock().unwrap().iter().any(|e| matches!(e, Event::Done(_))) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(sent[0], r#"{"type":"hello"}"#);
        assert_eq!(sent[1], r#"{"type":"new"}"#);
        let ask: Value = serde_json::from_str(&sent[2]).unwrap();
        assert_eq!(ask["text"], "Why?");
        assert_eq!(ask["context"]["exit"], 2);
        assert_eq!(ask["context"]["command"], "make");
        assert!(ask["context"].get("cwd").is_none(), "nothing sent for what isn't known");
        assert_eq!(
            *events.lock().unwrap(),
            vec![Event::Hello("Stella".into()), Event::Text("It ".into()), Event::Text("It failed.".into()), Event::Done("It failed.".into())]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_text_keeps_its_end_from_a_line_start() {
        let text = (1..=100).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let t = tail(&text, 30);
        assert!(t.starts_with("line ") && t.ends_with("line 100"), "{t:?}");
        assert_eq!(tail("short", 30), "short");
    }
}
