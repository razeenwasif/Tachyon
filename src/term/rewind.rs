//! Rewind: a tab's raw output, kept so the screen can be shown as it was at
//! any recent moment, including full-screen programs (htop, nvtop, vim, agent
//! UIs) that scrollback never keeps.
//!
//! The log holds each read from the PTY with its time, and window resizes,
//! up to a byte budget. Every [`SNAPSHOT_EVERY`] bytes it also keeps a copy of
//! the screen state, taken only between escape sequences. To show a moment,
//! [`RewindLog::replay`] copies the nearest snapshot before it and replays at
//! most that many bytes onto it.

use std::collections::VecDeque;
use std::time::Instant;

use super::{Processor, Term};

/// Bytes of output kept per tab.
pub const BUDGET: usize = 32 << 20;
/// Bytes between snapshots: the most a seek replays.
pub const SNAPSHOT_EVERY: usize = 2 << 20;
/// Reads closer together than this are one frame, for stepping.
const FRAME_GAP_MS: u128 = 40;

enum Data {
    Bytes(Vec<u8>),
    Resize(usize, usize),
}

struct Chunk {
    seq: u64,
    time: Instant,
    data: Data,
}

/// A copy of the screen state from just before chunk `seq`.
struct Snapshot {
    seq: u64,
    term: Term,
}

pub struct RewindLog {
    chunks: VecDeque<Chunk>,
    snapshots: VecDeque<Snapshot>,
    next_seq: u64,
    bytes: usize,
    since_snapshot: usize,
}

impl RewindLog {
    /// A log for a terminal that starts out as `initial`.
    pub fn new(initial: Term) -> RewindLog {
        RewindLog {
            chunks: VecDeque::new(),
            snapshots: VecDeque::from([Snapshot { seq: 0, term: initial }]),
            next_seq: 0,
            bytes: 0,
            since_snapshot: 0,
        }
    }

    pub fn record(&mut self, bytes: &[u8]) {
        self.push(Data::Bytes(bytes.to_vec()));
        self.bytes += bytes.len();
        self.since_snapshot += bytes.len();
        self.evict();
    }

    pub fn record_resize(&mut self, cols: usize, rows: usize) {
        self.push(Data::Resize(cols, rows));
    }

    fn push(&mut self, data: Data) {
        self.chunks.push_back(Chunk { seq: self.next_seq, time: Instant::now(), data });
        self.next_seq += 1;
    }

    /// Whether it's time for a snapshot; the caller takes one with
    /// [`RewindLog::snapshot`] once the parser is between sequences.
    pub fn wants_snapshot(&self) -> bool {
        self.since_snapshot >= SNAPSHOT_EVERY
    }

    /// The terminal's state now, after every chunk recorded so far.
    pub fn snapshot(&mut self, term: Term) {
        self.snapshots.push_back(Snapshot { seq: self.next_seq, term });
        self.since_snapshot = 0;
    }

    /// Drop the oldest output past the budget, a snapshot's worth at a time,
    /// so the oldest chunk kept always has a snapshot to replay from.
    fn evict(&mut self) {
        while self.bytes > BUDGET && self.snapshots.len() >= 2 {
            let keep_from = self.snapshots[1].seq;
            while self.chunks.front().is_some_and(|c| c.seq < keep_from) {
                if let Some(Chunk { data: Data::Bytes(b), .. }) = self.chunks.pop_front() {
                    self.bytes -= b.len();
                }
            }
            self.snapshots.pop_front();
        }
    }

    /// The frames that can be shown, oldest first: the last chunk of each
    /// burst of output.
    pub fn frames(&self) -> Vec<(u64, Instant)> {
        let mut out: Vec<(u64, Instant)> = Vec::new();
        for c in &self.chunks {
            let Data::Bytes(_) = c.data else { continue };
            match out.last_mut() {
                Some(last) if c.time.duration_since(last.1).as_millis() < FRAME_GAP_MS => *last = (c.seq, c.time),
                _ => out.push((c.seq, c.time)),
            }
        }
        out
    }

    /// The screen as it was right after chunk `seq`.
    pub fn replay(&self, seq: u64) -> Option<Term> {
        let snap = self.snapshots.iter().rev().find(|s| s.seq <= seq)?;
        let mut term = snap.term.rewind_snapshot();
        let mut processor = Processor::new();
        for c in self.chunks.iter().filter(|c| c.seq >= snap.seq && c.seq <= seq) {
            match &c.data {
                Data::Bytes(b) => processor.advance(&mut term, b),
                Data::Resize(cols, rows) => term.resize(*cols, *rows),
            }
        }
        // Replies to queries were answered live; drop the replay's.
        let _ = term.take_replies();
        Some(term)
    }

    /// Bytes held, for tests and diagnostics.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_text(t: &Term) -> String {
        let h = t.grid().history_len();
        t.text_range(h, 0, h + t.rows() - 1, t.cols() - 1).trim_end().to_string()
    }

    /// Feed `bytes` to both the live terminal and the log, the way the reader
    /// thread does.
    fn feed(live: &mut Term, p: &mut Processor, log: &mut RewindLog, bytes: &[u8]) {
        log.record(bytes);
        p.advance(live, bytes);
        if log.wants_snapshot() && p.is_ground() {
            log.snapshot(live.rewind_snapshot());
        }
    }

    #[test]
    fn replays_the_alternate_screen() {
        let mut live = Term::new(20, 4, 100);
        let mut p = Processor::new();
        let mut log = RewindLog::new(live.rewind_snapshot());
        feed(&mut live, &mut p, &mut log, b"$ htop\r\n");
        std::thread::sleep(std::time::Duration::from_millis(60));
        // htop takes the alternate screen and draws.
        feed(&mut live, &mut p, &mut log, b"\x1b[?1049h\x1b[H\x1b[2JCPU 87%\r\nMEM 9.4G");
        std::thread::sleep(std::time::Duration::from_millis(60));
        feed(&mut live, &mut p, &mut log, b"\x1b[HCPU 12%");
        std::thread::sleep(std::time::Duration::from_millis(60));
        // ... and quits, putting the shell's screen back.
        feed(&mut live, &mut p, &mut log, b"\x1b[?1049l$ ");
        assert!(screen_text(&live).starts_with("$ htop"));

        let frames = log.frames();
        assert_eq!(frames.len(), 4);
        assert_eq!(screen_text(&log.replay(frames[1].0).unwrap()), "CPU 87%\nMEM 9.4G");
        assert_eq!(screen_text(&log.replay(frames[2].0).unwrap()), "CPU 12%\nMEM 9.4G");
        assert!(screen_text(&log.replay(frames[3].0).unwrap()).starts_with("$ htop"));
    }

    #[test]
    fn replays_from_snapshots_within_the_budget() {
        let mut live = Term::new(40, 5, 100);
        let mut p = Processor::new();
        let mut log = RewindLog::new(live.rewind_snapshot());
        let line = format!("{}\r\n", "x".repeat(38));
        // Enough to fill the budget twice over, as one burst per line count.
        for i in 0..(BUDGET * 2 / line.len() / 1000) {
            let block: String = (0..1000).map(|j| format!("{:>8} {}\r\n", i * 1000 + j, &line[..20])).collect();
            feed(&mut live, &mut p, &mut log, block.as_bytes());
        }
        assert!(log.bytes() <= BUDGET + SNAPSHOT_EVERY * 2, "kept {} bytes", log.bytes());
        // The newest frame replays to exactly the live screen.
        let last = log.frames().last().unwrap().0;
        assert_eq!(screen_text(&log.replay(last).unwrap()), screen_text(&live));
        // The oldest kept frame still replays (from the oldest snapshot).
        let first = log.frames().first().unwrap().0;
        assert!(log.replay(first).is_some());
    }

    #[test]
    fn resizes_replay_too() {
        let mut live = Term::new(20, 4, 100);
        let mut p = Processor::new();
        let mut log = RewindLog::new(live.rewind_snapshot());
        feed(&mut live, &mut p, &mut log, b"one\r\ntwo");
        live.resize(30, 6);
        log.record_resize(30, 6);
        feed(&mut live, &mut p, &mut log, b"\r\nthree");
        let t = log.replay(log.frames().last().unwrap().0).unwrap();
        assert_eq!((t.cols(), t.rows()), (30, 6));
        assert_eq!(screen_text(&t), screen_text(&live));
    }

    /// What recording costs: the same output parsed alone, and parsed while
    /// recorded with snapshots, as the reader thread does. Run with
    /// `cargo test --release rewind_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn rewind_cost() {
        use std::time::Instant;
        let line = b"[00001234] compiling crate_977 v0.12.6 step loss=0.4123 12.5it/s\r\n";
        let chunk: Vec<u8> = line.iter().copied().cycle().take(64 * 1024).collect();
        let total = 256 << 20;
        let mut best = (f64::MAX, f64::MAX);
        for _ in 0..3 {
            let mut t = Term::new(200, 50, 10_000);
            let mut p = Processor::new();
            let start = Instant::now();
            for _ in 0..total / chunk.len() {
                p.advance(&mut t, &chunk);
            }
            best.0 = best.0.min(start.elapsed().as_secs_f64());

            let mut t = Term::new(200, 50, 10_000);
            let mut p = Processor::new();
            let mut log = RewindLog::new(t.rewind_snapshot());
            let start = Instant::now();
            for _ in 0..total / chunk.len() {
                p.advance(&mut t, &chunk);
                log.record(&chunk);
                if log.wants_snapshot() && p.is_ground() {
                    log.snapshot(t.rewind_snapshot());
                }
            }
            best.1 = best.1.min(start.elapsed().as_secs_f64());
        }
        let mib = (total >> 20) as f64;
        println!("parse only: {:.0} MiB/s; with rewind: {:.0} MiB/s ({:+.1}%)", mib / best.0, mib / best.1, (best.0 / best.1 - 1.0) * 100.0);
    }
}
