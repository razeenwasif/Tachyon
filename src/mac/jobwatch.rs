//! Watching cluster jobs from the session host, so a job's end is told even
//! with Tachyon closed.
//!
//! The host reads every shell's output; a submission it sees (a detached
//! Nexus job, a Slurm batch job, as [`crate::term::jobs`] finds them) is
//! watched until it ends: Nexus jobs with one `nexus jobs --all` for all of
//! them, Slurm jobs with `squeue` and then `sacct`, through the same ssh as
//! the submission when it went over one. When a job ends and no Tachyon
//! window is showing its shell, the host posts the notification itself.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::term::jobs::{JobKind, JobRef};

/// How often jobs are looked at while any is still going.
pub const POLL: Duration = Duration::from_secs(30);
/// Jobs older than this stop being watched (one stuck in a queue for days).
const GIVE_UP: Duration = Duration::from_secs(3 * 24 * 3600);
/// Nexus's ended states.
const NEXUS_ENDED: [&str; 3] = ["finished", "failed", "cancelled"];

/// What became of a job.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Still going: its state (queued, running on forge...).
    Going(String),
    /// Ended: whether it went well, and how to say it.
    Ended { ok: bool, summary: String },
    /// Not found (yet), or the check failed.
    Unknown,
}

/// One line of `nexus jobs`: `j-1a2b3c4d  running  forge  name  reason`.
fn nexus_line(listing: &str, id: &str) -> Option<(String, String, String)> {
    listing.lines().find_map(|line| {
        let mut w = line.split_whitespace();
        (w.next()? == id).then(|| {
            let status = w.next().unwrap_or("").to_string();
            let node = w.next().unwrap_or("-").to_string();
            let rest = w.collect::<Vec<_>>().join(" ");
            (status, node, rest)
        })
    })
}

/// A Nexus job's status, from `nexus jobs --all`.
pub fn nexus_status(listing: &str, id: &str) -> Status {
    let Some((status, node, rest)) = nexus_line(listing, id) else { return Status::Unknown };
    let on = if node == "-" { String::new() } else { format!(" on {node}") };
    if NEXUS_ENDED.contains(&status.as_str()) {
        let why = if status == "finished" || rest.is_empty() { String::new() } else { format!(": {rest}") };
        Status::Ended { ok: status == "finished", summary: format!("{status}{on}{why}") }
    } else {
        Status::Going(format!("{status}{on}"))
    }
}

/// A Slurm job's status: `squeue`'s state while it's queued or running,
/// else `sacct`'s `State|ExitCode|NodeList|Elapsed` line once it's done.
pub fn slurm_status(squeue: &str, sacct: Option<&str>) -> Status {
    let state = squeue.trim().to_lowercase();
    if !state.is_empty() {
        return Status::Going(state);
    }
    let Some(line) = sacct.and_then(|s| s.lines().find(|l| !l.trim().is_empty())) else { return Status::Unknown };
    let f: Vec<&str> = line.trim().split('|').collect();
    let (state, exit, node, elapsed) = (f.first().copied().unwrap_or(""), f.get(1).copied().unwrap_or(""), f.get(2).copied().unwrap_or(""), f.get(3).copied().unwrap_or(""));
    if state.is_empty() {
        return Status::Unknown;
    }
    let ok = state.starts_with("COMPLETED");
    let mut summary = state.split_whitespace().next().unwrap_or(state).to_lowercase();
    if !node.is_empty() && node != "None assigned" {
        summary.push_str(&format!(" on {node}"));
    }
    if !elapsed.is_empty() {
        summary.push_str(&format!(" in {elapsed}"));
    }
    if !ok && !exit.is_empty() {
        summary.push_str(&format!(", exit {exit}"));
    }
    Status::Ended { ok, summary }
}

/// A job being watched.
struct Watched {
    session: u64,
    job: JobRef,
    /// The command that submitted it.
    cmdline: Option<String>,
    since: Instant,
    /// Seen still going at least once.
    seen_going: bool,
}

/// A job that ended, to tell.
#[derive(Clone, Debug, PartialEq)]
pub struct Ended {
    pub session: u64,
    pub job: JobRef,
    pub cmdline: Option<String>,
    pub ok: bool,
    pub summary: String,
    pub took: Duration,
}

#[derive(Default)]
pub struct Watcher {
    jobs: Vec<Watched>,
}

impl Watcher {
    /// Watch `job`, submitted in shell `session` by `cmdline`. A job already
    /// watched is left as it is.
    pub fn add(&mut self, session: u64, job: JobRef, cmdline: Option<String>, now: Instant) {
        if self.jobs.iter().any(|w| w.job.kind == job.kind && w.job.id == job.id) {
            return;
        }
        self.jobs.push(Watched { session, job, cmdline, since: now, seen_going: false });
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    /// Look at every job: `run` runs a shell command (wrapped in the job's
    /// ssh, if any) and gives its output. Returns the jobs that ended, and
    /// stops watching them.
    pub fn poll(&mut self, now: Instant, run: &dyn Fn(&str, Option<&str>) -> Option<String>) -> Vec<Ended> {
        // One listing serves every Nexus job.
        let nexus = self
            .jobs
            .iter()
            .any(|w| w.job.kind == JobKind::Nexus)
            .then(|| run("nexus jobs --all", None))
            .flatten();
        let mut ended = Vec::new();
        self.jobs.retain_mut(|w| {
            let status = match w.job.kind {
                JobKind::Nexus => nexus.as_deref().map_or(Status::Unknown, |l| nexus_status(l, &w.job.id)),
                JobKind::Slurm => {
                    let via = w.job.via.as_deref();
                    match run(&format!("squeue -h -j {} -o %T", w.job.id), via) {
                        // squeue answered: empty means it's no longer queued
                        // or running.
                        Some(q) if q.trim().is_empty() => {
                            let acct = run(&format!("sacct -n -P -X -j {} -o State,ExitCode,NodeList,Elapsed", w.job.id), via);
                            slurm_status("", acct.as_deref())
                        }
                        Some(q) => slurm_status(&q, None),
                        None => Status::Unknown,
                    }
                }
            };
            match status {
                Status::Going(_) => {
                    w.seen_going = true;
                    now.duration_since(w.since) < GIVE_UP
                }
                Status::Ended { ok, summary } => {
                    ended.push(Ended {
                        session: w.session,
                        job: w.job.clone(),
                        cmdline: w.cmdline.clone(),
                        ok,
                        summary,
                        took: now.duration_since(w.since),
                    });
                    false
                }
                Status::Unknown => now.duration_since(w.since) < GIVE_UP,
            }
        });
        ended
    }
}

/// How long something took, briefly.
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        _ => format!("{}h {}m", s / 3600, s % 3600 / 60),
    }
}

/// The notification for an ended job: title, subtitle, body.
pub fn notification(e: &Ended) -> (String, String, String) {
    let title = format!("{} {}", e.job.title(), if e.ok { "finished" } else { "didn't finish" });
    let subtitle = match e.job.kind {
        // Slurm's own elapsed time is in its summary.
        JobKind::Slurm => e.summary.clone(),
        JobKind::Nexus => format!("{} · watched {}", e.summary, duration(e.took)),
    };
    let body = e.cmdline.clone().unwrap_or_else(|| "Submitted from a Tachyon tab.".into());
    (title, subtitle, body)
}

/// Run `command` for the watcher: through `via` (an ssh command line) when
/// given, in batch mode so it never waits for a password; with the folders
/// tools like `nexus` live in on the PATH, since the host started from the
/// app has the system's bare one; and given at most 60 seconds.
pub fn run_command(command: &str, via: Option<&str>) -> Option<String> {
    let nexus = std::env::var("TACHYON_NEXUS").ok();
    let command = match &nexus {
        Some(n) => command.replacen("nexus ", &format!("{n} "), 1),
        None => command.to_string(),
    };
    let line = match via {
        Some(ssh) => {
            let ssh = ssh.trim();
            let rest = ssh.strip_prefix("ssh").unwrap_or(ssh);
            format!("ssh -o BatchMode=yes -o ConnectTimeout=10{rest} {}", shell_quote(&command))
        }
        None => command,
    };
    let home = std::env::var("HOME").unwrap_or_default();
    let path = format!("{home}/.local/bin:/opt/homebrew/bin:/usr/local/bin:{}", std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()));
    let mut child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&line)
        .env("PATH", path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                use std::io::Read;
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return status.success().then_some(out);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Ids already notified, so a host restart mid-watch doesn't tell twice.
#[derive(Default)]
pub struct Told(HashMap<String, Instant>);

impl Told {
    /// Whether `job` is new to tell (and remember it).
    pub fn first_time(&mut self, job: &JobRef, now: Instant) -> bool {
        self.0.retain(|_, at| now.duration_since(*at) < Duration::from_secs(7 * 24 * 3600));
        self.0.insert(job.title(), now).is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const LISTING: &str = "\
j-00000001   running    forge    python train.py
j-00000002   queued     -        python eval.py
j-00000003   finished   forge    python train.py
j-00000004   failed     atlas    make bench       exit 2
";

    #[test]
    fn nexus_listings_give_each_jobs_status() {
        assert_eq!(nexus_status(LISTING, "j-00000001"), Status::Going("running on forge".into()));
        assert_eq!(nexus_status(LISTING, "j-00000002"), Status::Going("queued".into()));
        assert_eq!(nexus_status(LISTING, "j-00000003"), Status::Ended { ok: true, summary: "finished on forge".into() });
        assert_eq!(nexus_status(LISTING, "j-00000004"), Status::Ended { ok: false, summary: "failed on atlas: make bench exit 2".into() });
        assert_eq!(nexus_status(LISTING, "j-99999999"), Status::Unknown);
    }

    #[test]
    fn slurm_says_how_a_job_ended() {
        assert_eq!(slurm_status("RUNNING\n", None), Status::Going("running".into()));
        assert_eq!(
            slurm_status("", Some("COMPLETED|0:0|gpu-07|02:41:12\n")),
            Status::Ended { ok: true, summary: "completed on gpu-07 in 02:41:12".into() }
        );
        assert_eq!(
            slurm_status("", Some("FAILED|1:0|gpu-03|00:00:09")),
            Status::Ended { ok: false, summary: "failed on gpu-03 in 00:00:09, exit 1:0".into() }
        );
        assert_eq!(slurm_status("", Some("CANCELLED by 501|0:15|None assigned|00:00:00")).clone(), Status::Ended { ok: false, summary: "cancelled in 00:00:00, exit 0:15".into() });
        assert_eq!(slurm_status("", None), Status::Unknown);
    }

    #[test]
    fn a_job_is_told_once_when_it_ends() {
        let t0 = Instant::now();
        let mut w = Watcher::default();
        let nexus = |id: &str| JobRef { kind: JobKind::Nexus, id: id.into(), via: None };
        let slurm = JobRef { kind: JobKind::Slurm, id: "77".into(), via: Some("ssh hpc".into()) };
        w.add(1, nexus("j-00000001"), Some("nexus gpu --detach python train.py".into()), t0);
        w.add(2, slurm.clone(), Some("sbatch run.sh".into()), t0);
        w.add(1, nexus("j-00000001"), None, t0);
        // First look: both going. The commands run are recorded.
        let ran = RefCell::new(Vec::new());
        let state = RefCell::new(("j-00000001 running forge x\n".to_string(), "RUNNING".to_string()));
        let run = |cmd: &str, via: Option<&str>| {
            ran.borrow_mut().push(format!("{}{cmd}", via.map(|v| format!("[{v}] ")).unwrap_or_default()));
            let s = state.borrow();
            Some(if cmd.starts_with("nexus") { s.0.clone() } else if cmd.starts_with("squeue") { s.1.clone() } else { "COMPLETED|0:0|gpu-07|00:10:00".into() })
        };
        assert!(w.poll(t0, &run).is_empty());
        assert_eq!(*ran.borrow(), vec!["nexus jobs --all", "[ssh hpc] squeue -h -j 77 -o %T"]);
        // Both end.
        *state.borrow_mut() = ("j-00000001 failed forge x oom\n".into(), String::new());
        let ended = w.poll(t0 + Duration::from_secs(600), &run);
        assert_eq!(ended.len(), 2);
        assert_eq!(ended[0].summary, "failed on forge: x oom");
        assert!(!ended[0].ok);
        assert_eq!(ended[1].summary, "completed on gpu-07 in 00:10:00");
        let (title, subtitle, body) = notification(&ended[0]);
        assert_eq!(title, "nexus j-00000001 didn't finish");
        assert_eq!(subtitle, "failed on forge: x oom · watched 10m");
        assert_eq!(body, "nexus gpu --detach python train.py");
        assert!(w.is_empty(), "ended jobs are let go");
        assert!(w.poll(t0, &run).is_empty());
    }

    #[test]
    fn jobs_told_once_across_restarts() {
        let mut told = Told::default();
        let j = JobRef { kind: JobKind::Nexus, id: "j-00000001".into(), via: None };
        let now = Instant::now();
        assert!(told.first_time(&j, now));
        assert!(!told.first_time(&j, now));
    }
}
