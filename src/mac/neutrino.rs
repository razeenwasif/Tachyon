//! Neutrino, the user's own git and CI server, in a card beside the terminal.
//!
//! Everything comes from running `ntr`, Neutrino's CLI: it runs git for the
//! commands it doesn't know (so `ntr status --porcelain=v2 --branch` gives
//! the focused shell's repo) and forwards server commands (`inbox`, `runs`,
//! `pr`, `run`) to the server over SSH. When the focused shell is an ssh
//! session on the server's own Mac, the same commands run there, from that
//! shell's folder, through ssh.
//!
//! The card's items never run anything themselves: clicking one types its
//! `ntr` command into the focused shell (an approval after a confirm).
//!
//! The session host also polls `ntr inbox`, so a run that finishes is told
//! with a notification even when Tachyon is closed.

use std::collections::HashMap;
use std::time::Duration;

/// How often the host looks at the inbox.
pub const POLL: Duration = Duration::from_secs(60);

/// Hosts that are the Neutrino server's Mac (`mini` names the same one).
const SERVER_HOSTS: [&str; 4] = ["kagutsuchi", "kagutsuchi-lan", "mini", "mini-lan"];

/// Text without its terminal colours.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// The focused shell's repo: `git status --porcelain=v2 --branch`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RepoState {
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Two-letter status (`M.`, `??`…) and path.
    pub changed: Vec<(String, String)>,
}

pub fn parse_status(text: &str) -> Option<RepoState> {
    let mut r = RepoState::default();
    let mut any = false;
    for line in plain(text).lines() {
        if let Some(h) = line.strip_prefix("# branch.head ") {
            r.branch = if h == "(detached)" { "detached".into() } else { h.to_string() };
            any = true;
        } else if let Some(u) = line.strip_prefix("# branch.upstream ") {
            r.upstream = Some(u.to_string());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for w in ab.split_whitespace() {
                if let Some(n) = w.strip_prefix('+') {
                    r.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = w.strip_prefix('-') {
                    r.behind = n.parse().unwrap_or(0);
                }
            }
        } else if let Some(p) = line.strip_prefix("? ") {
            r.changed.push(("??".into(), p.to_string()));
        } else {
            // `1 XY sub mH mI mW hH hI path`, `2 XY … score path\torig`,
            // `u XY sub m1 m2 m3 mW h1 h2 h3 path`.
            let fields = match line.chars().next() {
                Some('1') => 8,
                Some('2') => 9,
                Some('u') => 10,
                _ => continue,
            };
            let mut parts = line.splitn(fields + 1, ' ');
            let _kind = parts.next();
            let xy = parts.next().unwrap_or("").to_string();
            let Some(path) = parts.nth(fields - 2) else { continue };
            let path = path.split('\t').next().unwrap_or(path).to_string();
            r.changed.push((xy, path));
        }
    }
    any.then_some(r)
}

/// A deploy waiting for approval: `approve REPO run N: JOB to ENV  (ago)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Approval {
    pub repo: String,
    pub number: u32,
    pub what: String,
}

/// A pull request: `ready   REPO #N TITLE` or `open    REPO #N TITLE`.
#[derive(Clone, Debug, PartialEq)]
pub struct Pull {
    pub repo: String,
    pub number: u32,
    pub title: String,
    pub ready: bool,
}

/// A CI run.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    /// None for the focused repo's own runs.
    pub repo: Option<String>,
    pub number: u32,
    pub name: String,
    pub state: String,
}

const RUN_STATES: [&str; 7] = ["queued", "running", "waiting", "success", "failure", "cancelled", "skipped"];

/// What needs you, from `ntr inbox`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inbox {
    pub approvals: Vec<Approval>,
    pub pulls: Vec<Pull>,
    /// Runs not finished yet.
    pub running: Vec<Run>,
}

pub fn parse_inbox(text: &str) -> Inbox {
    let mut inbox = Inbox::default();
    for line in plain(text).lines() {
        let mut w = line.split_whitespace();
        let Some(first) = w.next() else { continue };
        match first {
            "approve" => {
                let (Some(repo), Some("run"), Some(n)) = (w.next(), w.next(), w.next()) else { continue };
                let Ok(number) = n.trim_end_matches(':').parse() else { continue };
                let what = w.collect::<Vec<_>>().join(" ");
                inbox.approvals.push(Approval { repo: repo.into(), number, what });
            }
            "ready" | "open" => {
                let (Some(repo), Some(n)) = (w.next(), w.next()) else { continue };
                let Some(Ok(number)) = n.strip_prefix('#').map(str::parse) else { continue };
                inbox.pulls.push(Pull { repo: repo.into(), number, title: w.collect::<Vec<_>>().join(" "), ready: first == "ready" });
            }
            s if RUN_STATES.contains(&s) => {
                let rest: Vec<&str> = w.collect();
                let (Some((n, middle)), true) = (rest.split_last(), rest.len() >= 2) else { continue };
                let Ok(number) = n.parse() else { continue };
                inbox.running.push(Run { repo: Some(middle[0].into()), number, name: middle[1..].join(" "), state: s.into() });
            }
            _ => {}
        }
    }
    inbox
}

/// The runs of a repo, from `ntr runs`: `N  state  name(20)  event  what  sha  ago`.
pub fn parse_runs(text: &str, repo: Option<&str>) -> Vec<Run> {
    plain(text)
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (n, rest) = line.split_once(char::is_whitespace)?;
            let number = n.parse().ok()?;
            let rest = rest.trim_start();
            let (state, rest) = rest.split_once(char::is_whitespace)?;
            if !RUN_STATES.contains(&state) {
                return None;
            }
            // The name is padded to 20 columns, after two spaces.
            let rest = rest.trim_start();
            let name: String = rest.chars().take(20).collect::<String>().trim().to_string();
            Some(Run { repo: repo.map(str::to_string), number, name, state: state.into() })
        })
        .collect()
}

/// What an item in the card does: the `ntr` command typed into the shell.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Logs { repo: Option<String>, number: u32 },
    ShowPull { repo: String, number: u32 },
    Approve { repo: String, number: u32 },
}

impl Action {
    pub fn command(&self) -> String {
        let r = |repo: &Option<String>| repo.as_ref().map(|r| format!("-r {} ", quote_word(r))).unwrap_or_default();
        match self {
            Action::Logs { repo, number } => format!("ntr {}run logs {number} -f", r(repo)),
            Action::ShowPull { repo, number } => format!("ntr {}pr show {number}", r(&Some(repo.clone()))),
            Action::Approve { repo, number } => format!("ntr {}run approve {number}", r(&Some(repo.clone()))),
        }
    }

    /// Whether to ask before typing it.
    pub fn confirm(&self) -> bool {
        matches!(self, Action::Approve { .. })
    }
}

/// A word for the shell: as it is when it's plain, else single-quoted.
fn quote_word(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)) {
        s.to_string()
    } else {
        shell_quote(s)
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// One line of the card.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Header(String),
    Note(String),
    Item(String, Action),
}

/// What the card shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Panel {
    /// The folder the repo was looked at in, and where.
    pub place: Option<String>,
    pub repo: Option<RepoState>,
    pub recent: Vec<Run>,
    pub inbox: Option<Inbox>,
    pub error: Option<String>,
}

fn run_mark(state: &str) -> &'static str {
    match state {
        "success" => "✓",
        "failure" => "✗",
        "running" => "●",
        "waiting" => "⏸",
        "queued" => "○",
        _ => "–",
    }
}

impl Panel {
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        match &self.repo {
            Some(r) => {
                rows.push(Row::Header(format!("⎇ {}", r.branch)));
                if let Some(p) = &self.place {
                    rows.push(Row::Note(p.clone()));
                }
                let sync = match (&r.upstream, r.ahead, r.behind) {
                    (None, _, _) => "no upstream".to_string(),
                    (Some(u), 0, 0) => format!("up to date with {u}"),
                    (Some(u), a, b) => format!("↑{a} ↓{b} against {u}"),
                };
                rows.push(Row::Note(sync));
                if r.changed.is_empty() {
                    rows.push(Row::Note("clean".into()));
                } else {
                    rows.push(Row::Note(format!("{} changed:", r.changed.len())));
                    for (xy, path) in r.changed.iter().take(12) {
                        rows.push(Row::Note(format!("  {xy} {path}")));
                    }
                    if r.changed.len() > 12 {
                        rows.push(Row::Note(format!("  … and {} more", r.changed.len() - 12)));
                    }
                }
                if !self.recent.is_empty() {
                    rows.push(Row::Header("Recent runs".into()));
                    for run in &self.recent {
                        rows.push(Row::Item(
                            format!("{} #{} {} · {}", run_mark(&run.state), run.number, run.name, run.state),
                            Action::Logs { repo: None, number: run.number },
                        ));
                    }
                }
            }
            None => rows.push(Row::Note("This shell's folder isn't a git repository.".into())),
        }
        rows.push(Row::Header("Inbox".into()));
        match &self.inbox {
            None => rows.push(Row::Note(self.error.clone().unwrap_or_else(|| "Reading…".into()))),
            Some(i) if i.approvals.is_empty() && i.pulls.is_empty() && i.running.is_empty() => rows.push(Row::Note("Nothing needs you.".into())),
            Some(i) => {
                for a in &i.approvals {
                    rows.push(Row::Item(
                        format!("⏸ approve {} run {}: {}", a.repo, a.number, a.what),
                        Action::Approve { repo: a.repo.clone(), number: a.number },
                    ));
                }
                for p in &i.pulls {
                    rows.push(Row::Item(
                        format!("{} {} #{} {}", if p.ready { "⇡ ready" } else { "◦ open" }, p.repo, p.number, p.title),
                        Action::ShowPull { repo: p.repo.clone(), number: p.number },
                    ));
                }
                for r in &i.running {
                    let repo = r.repo.clone().unwrap_or_default();
                    rows.push(Row::Item(
                        format!("{} {repo} {} #{} · {}", run_mark(&r.state), r.name, r.number, r.state),
                        Action::Logs { repo: r.repo.clone(), number: r.number },
                    ));
                }
            }
        }
        rows
    }
}

/// Where `ntr` runs for a shell: on this Mac in its folder, or over ssh on
/// the server's Mac in the folder its shell there reported.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub ssh: Option<String>,
    pub dir: Option<String>,
}

impl Place {
    /// The shell line that runs `ntr ARGS` here, with the usual folders for
    /// tools on the PATH (ssh and apps get the system's bare one).
    pub fn command(&self, args: &str) -> String {
        let cd = self.dir.as_deref().map(|d| format!("cd {} && ", shell_quote(d))).unwrap_or_default();
        let inner = format!("{cd}PATH=\"$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH\" ntr {args}");
        match &self.ssh {
            Some(host) => format!("ssh -o BatchMode=yes -o ConnectTimeout=5 {host} {}", shell_quote(&inner)),
            None => inner,
        }
    }
}

/// The host an ssh command line goes to, when it's the Neutrino server's
/// Mac.
pub fn server_host(ssh_cmdline: &str) -> Option<String> {
    let host = ssh_target(ssh_cmdline)?;
    SERVER_HOSTS.contains(&host.as_str()).then_some(host)
}

/// The destination of an `ssh` command line, without its user.
pub fn ssh_target(cmdline: &str) -> Option<String> {
    let mut words = cmdline.split_whitespace();
    if words.next()? != "ssh" {
        return None;
    }
    while let Some(w) = words.next() {
        if let Some(opt) = w.strip_prefix('-') {
            // Options that take a value, given apart from it.
            if opt.len() == 1 && "BbcDEeFIiJLlmOoPpQRSWw".contains(opt) {
                words.next();
            }
            continue;
        }
        let host = w.rsplit_once('@').map_or(w, |(_, h)| h);
        return Some(host.to_string());
    }
    None
}

/// Runs that were going and now aren't, from two looks at the inbox, keyed by
/// repo and number. Their end comes from `ntr -r REPO runs`.
#[derive(Default)]
pub struct RunWatch {
    going: HashMap<(String, u32), String>,
    started: bool,
}

impl RunWatch {
    /// The runs that left the inbox since the last look. The first look only
    /// learns what's going.
    pub fn update(&mut self, inbox: &Inbox) -> Vec<Run> {
        let now: HashMap<(String, u32), String> = inbox
            .running
            .iter()
            .filter_map(|r| Some(((r.repo.clone()?, r.number), r.name.clone())))
            .collect();
        let gone = if self.started {
            self.going
                .iter()
                .filter(|(k, _)| !now.contains_key(*k))
                .map(|((repo, number), name)| Run { repo: Some(repo.clone()), number: *number, name: name.clone(), state: String::new() })
                .collect()
        } else {
            Vec::new()
        };
        self.going = now;
        self.started = true;
        gone
    }
}

/// The notification for a run that ended: title, subtitle, body.
pub fn notification(run: &Run, state: Option<&str>) -> (String, String, String) {
    let repo = run.repo.as_deref().unwrap_or("");
    let how = match state {
        Some("success") => "passed",
        Some("failure") => "failed",
        Some("cancelled") => "was cancelled",
        Some(s) if !s.is_empty() => s,
        _ => "finished",
    };
    (format!("{repo} {} #{} {how}", run.name, run.number), "Neutrino".into(), format!("ntr -r {repo} run logs {}", run.number))
}

#[cfg(test)]
mod tests {
    use super::*;

    const INBOX: &str = "\
\x1b[35mapprove\x1b[0m oracle run 4: deploy to production  (3m ago)
\x1b[32mready  \x1b[0m vector #1 CI on Neutrino
\x1b[2mopen   \x1b[0m council #2 Rework the prompts

running   council PR Checks 12
queued    onyx CI 2
";

    #[test]
    fn the_inbox_parses() {
        let i = parse_inbox(INBOX);
        assert_eq!(i.approvals, vec![Approval { repo: "oracle".into(), number: 4, what: "deploy to production (3m ago)".into() }]);
        assert_eq!(i.pulls.len(), 2);
        assert_eq!(i.pulls[0], Pull { repo: "vector".into(), number: 1, title: "CI on Neutrino".into(), ready: true });
        assert!(!i.pulls[1].ready);
        assert_eq!(i.running[0], Run { repo: Some("council".into()), number: 12, name: "PR Checks".into(), state: "running".into() });
        assert_eq!(i.running[1].name, "CI");
        assert_eq!(parse_inbox("Nothing needs you.\n"), Inbox::default());
    }

    #[test]
    fn status_gives_branch_sync_and_changes() {
        let text = "# branch.oid abc\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n\
1 .M N... 100644 100644 100644 aaa bbb src/mac/mod.rs\n\
2 R. N... 100644 100644 100644 aaa bbb R100 new name.rs\told.rs\n\
? notes.txt\n";
        let r = parse_status(text).unwrap();
        assert_eq!(r.branch, "main");
        assert_eq!(r.upstream.as_deref(), Some("origin/main"));
        assert_eq!((r.ahead, r.behind), (2, 1));
        assert_eq!(
            r.changed,
            vec![(".M".into(), "src/mac/mod.rs".into()), ("R.".into(), "new name.rs".into()), ("??".into(), "notes.txt".into())]
        );
        assert_eq!(parse_status("fatal: not a git repository"), None);
    }

    #[test]
    fn runs_parse() {
        let text = "12    success    PR Checks            pull_request       #2           1a2b3c4  5m ago\n3     \x1b[31mfailure  \x1b[0m  CI                   push               main         9f8e7d6  2d ago\nNo runs yet.\n";
        let runs = parse_runs(text, None);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0], Run { repo: None, number: 12, name: "PR Checks".into(), state: "success".into() });
        assert_eq!(runs[1].state, "failure");
        assert_eq!(runs[1].name, "CI");
    }

    #[test]
    fn items_type_their_ntr_command() {
        let panel = Panel {
            place: Some("~/Github/Tachyon".into()),
            repo: Some(RepoState { branch: "main".into(), upstream: Some("origin/main".into()), ahead: 1, ..Default::default() }),
            recent: vec![Run { repo: None, number: 7, name: "CI".into(), state: "running".into() }],
            inbox: Some(parse_inbox(INBOX)),
            error: None,
        };
        let rows = panel.rows();
        assert_eq!(rows[0], Row::Header("⎇ main".into()));
        assert!(rows.contains(&Row::Note("↑1 ↓0 against origin/main".into())));
        let commands: Vec<(String, bool)> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Item(_, a) => Some((a.command(), a.confirm())),
                _ => None,
            })
            .collect();
        assert_eq!(
            commands,
            vec![
                ("ntr run logs 7 -f".into(), false),
                ("ntr -r oracle run approve 4".into(), true),
                ("ntr -r vector pr show 1".into(), false),
                ("ntr -r council pr show 2".into(), false),
                ("ntr -r council run logs 12 -f".into(), false),
                ("ntr -r onyx run logs 2 -f".into(), false),
            ]
        );
        let empty = Panel { inbox: Some(Inbox::default()), ..Default::default() };
        assert!(empty.rows().contains(&Row::Note("Nothing needs you.".into())));
        assert!(empty.rows().contains(&Row::Note("This shell's folder isn't a git repository.".into())));
    }

    #[test]
    fn ssh_sessions_on_the_server_run_ntr_there() {
        assert_eq!(server_host("ssh kagutsuchi").as_deref(), Some("kagutsuchi"));
        assert_eq!(server_host("ssh -p 22 -A razeenwasif@mini").as_deref(), Some("mini"));
        assert_eq!(server_host("ssh forge"), None);
        assert_eq!(ssh_target("ssh -o ConnectTimeout=5 forge uptime").as_deref(), Some("forge"));
        let place = Place { ssh: Some("kagutsuchi".into()), dir: Some("/Users/r/repo".into()) };
        assert_eq!(
            place.command("inbox"),
            r#"ssh -o BatchMode=yes -o ConnectTimeout=5 kagutsuchi 'cd '\''/Users/r/repo'\'' && PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH" ntr inbox'"#
        );
        let here = Place { ssh: None, dir: None };
        assert!(here.command("inbox").ends_with("ntr inbox"));
    }

    #[test]
    fn a_run_that_leaves_the_inbox_is_told() {
        let mut w = RunWatch::default();
        let i1 = parse_inbox("running   council PR Checks 12\n");
        assert!(w.update(&i1).is_empty(), "the first look only learns");
        assert!(w.update(&i1).is_empty());
        let gone = w.update(&Inbox::default());
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].number, 12);
        let (title, _, body) = notification(&gone[0], Some("failure"));
        assert_eq!(title, "council PR Checks #12 failed");
        assert_eq!(body, "ntr -r council run logs 12");
        assert!(w.update(&Inbox::default()).is_empty());
    }
}
