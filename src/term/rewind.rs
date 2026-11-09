//! Rewind: a tab's raw output, kept so the screen can be shown as it was at
//! any recent moment, including full-screen programs (htop, nvtop, vim, agent
//! UIs) that scrollback never keeps.
//!
//! The newest output (up to [`HOT_BUDGET`]) is in memory as reads with their
//! times, plus window resizes, with a copy of the screen state every
//! [`SNAPSHOT_EVERY`] bytes, taken only between escape sequences. Older output
//! moves to disk, LZ4-compressed, in segments of [`COLD_SEGMENT`] bytes that
//! each keep one snapshot, up to [`COLD_BUDGET`]. To show a moment,
//! [`RewindLog::replay`] copies the nearest snapshot before it and replays at
//! most one segment onto it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::cell::Cell;
use super::{Processor, Term};

/// Output kept in memory per tab.
pub const HOT_BUDGET: usize = 32 << 20;
/// Bytes between snapshots in memory: the most a recent seek replays.
pub const SNAPSHOT_EVERY: usize = 2 << 20;
/// Output per segment on disk, with one snapshot each.
pub const COLD_SEGMENT: usize = 8 << 20;
/// Output kept on disk per tab, before compression.
pub const COLD_BUDGET: usize = 512 << 20;
/// Reads closer together than this are one frame, for stepping.
const FRAME_GAP_MS: u128 = 40;

#[derive(Clone)]
enum Data {
    Bytes(Vec<u8>),
    Resize(usize, usize),
}

struct Chunk {
    seq: u64,
    /// Bytes of output before this chunk, over the tab's life.
    offset: u64,
    time: Instant,
    data: Data,
}

/// A copy of the screen state from just before chunk `seq`.
struct Snapshot {
    seq: u64,
    term: Term,
}

/// A segment moved to disk: its snapshot, and an index of its chunks so
/// frames can be listed without reading it.
struct Cold {
    snapshot: Snapshot,
    /// (seq, offset, time, is output) for each chunk.
    index: Vec<(u64, u64, Instant, bool)>,
    file: PathBuf,
    raw: usize,
    /// The chunks themselves until a background writer has compressed and
    /// written them, so the thread reading output never waits on that.
    pending: Arc<Mutex<Option<Vec<Chunk>>>>,
}

/// A segment for the background writer.
struct Job {
    file: PathBuf,
    pending: Arc<Mutex<Option<Vec<Chunk>>>>,
}

/// Where a cell's content came from, for cell provenance.
#[derive(Debug, PartialEq)]
pub struct Provenance {
    /// Bytes of output before the write, over the tab's life.
    pub offset: u64,
    pub time: Instant,
    /// The output around the write, escapes shown as `\e`, ending with it.
    pub context: String,
    /// The cell only arrived here because the screen scrolled.
    pub scrolled: bool,
}

pub struct RewindLog {
    hot: VecDeque<Chunk>,
    snapshots: VecDeque<Snapshot>,
    cold: VecDeque<Cold>,
    /// Where segments go; `None` keeps only what fits in memory.
    dir: Option<PathBuf>,
    writer: Option<Sender<Job>>,
    next_seq: u64,
    next_offset: u64,
    hot_bytes: usize,
    cold_bytes: usize,
    since_snapshot: usize,
}

impl RewindLog {
    /// A log for a terminal that starts out as `initial`, spilling to files
    /// in `dir` (created, private to the user, and removed with the log).
    pub fn new(initial: Term, dir: Option<PathBuf>) -> RewindLog {
        let dir = dir.filter(|d| create_private_dir(d).is_ok());
        let writer = dir.as_ref().and_then(|_| {
            let (tx, rx) = channel::<Job>();
            std::thread::Builder::new()
                .name("tachyon-rewind-writer".into())
                .spawn(move || {
                    for job in rx {
                        let raw = job.pending.lock().ok().and_then(|p| p.as_ref().map(|c| encode(c)));
                        if let Some(raw) = raw {
                            if std::fs::write(&job.file, lz4_flex::compress_prepend_size(&raw)).is_ok() {
                                if let Ok(mut p) = job.pending.lock() {
                                    *p = None;
                                }
                            }
                        }
                    }
                })
                .ok()
                .map(|_| tx)
        });
        RewindLog {
            writer,
            hot: VecDeque::new(),
            snapshots: VecDeque::from([Snapshot { seq: 0, term: initial }]),
            cold: VecDeque::new(),
            dir,
            next_seq: 0,
            next_offset: 0,
            hot_bytes: 0,
            cold_bytes: 0,
            since_snapshot: 0,
        }
    }

    pub fn record(&mut self, bytes: &[u8]) {
        self.push(Data::Bytes(bytes.to_vec()));
        self.next_offset += bytes.len() as u64;
        self.hot_bytes += bytes.len();
        self.since_snapshot += bytes.len();
        self.spill();
    }

    pub fn record_resize(&mut self, cols: usize, rows: usize) {
        self.push(Data::Resize(cols, rows));
    }

    fn push(&mut self, data: Data) {
        self.hot.push_back(Chunk { seq: self.next_seq, offset: self.next_offset, time: Instant::now(), data });
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

    /// Past the memory budget, move the oldest output to disk a segment at
    /// a time, keeping the segment's first snapshot; past the disk budget,
    /// drop the oldest segment. Without a folder, old output is dropped.
    fn spill(&mut self) {
        let per_segment = COLD_SEGMENT / SNAPSHOT_EVERY;
        while self.hot_bytes > HOT_BUDGET && self.snapshots.len() > per_segment {
            let end = self.snapshots[per_segment].seq;
            let snapshot = self.snapshots.pop_front().expect("checked above");
            for _ in 1..per_segment {
                self.snapshots.pop_front();
            }
            let mut chunks = Vec::new();
            while self.hot.front().is_some_and(|c| c.seq < end) {
                let c = self.hot.pop_front().expect("checked above");
                if let Data::Bytes(b) = &c.data {
                    self.hot_bytes -= b.len();
                }
                chunks.push(c);
            }
            if let (Some(dir), Some(writer)) = (&self.dir, &self.writer) {
                let cold = cold_segment(dir, snapshot, chunks);
                let _ = writer.send(Job { file: cold.file.clone(), pending: Arc::clone(&cold.pending) });
                self.cold_bytes += cold.raw;
                self.cold.push_back(cold);
            }
        }
        while self.cold_bytes > COLD_BUDGET {
            let Some(old) = self.cold.pop_front() else { break };
            if let Ok(mut p) = old.pending.lock() {
                *p = None;
            }
            let _ = std::fs::remove_file(&old.file);
            self.cold_bytes -= old.raw;
        }
    }

    /// Every chunk's (seq, offset, time, is output), oldest first.
    fn index(&self) -> impl Iterator<Item = (u64, u64, Instant, bool)> + '_ {
        let cold = self.cold.iter().flat_map(|c| c.index.iter().copied());
        let hot = self.hot.iter().map(|c| (c.seq, c.offset, c.time, matches!(c.data, Data::Bytes(_))));
        cold.chain(hot)
    }

    /// The frames that can be shown, oldest first: the last chunk of each
    /// burst of output.
    pub fn frames(&self) -> Vec<(u64, Instant)> {
        let mut out: Vec<(u64, Instant)> = Vec::new();
        for (seq, _, time, output) in self.index() {
            if !output {
                continue;
            }
            match out.last_mut() {
                Some(last) if time.duration_since(last.1).as_millis() < FRAME_GAP_MS => *last = (seq, time),
                _ => out.push((seq, time)),
            }
        }
        out
    }

    /// The snapshot to replay `seq` from, and the chunks after it up to and
    /// including `seq`, reading a segment from disk if need be.
    fn chunks_for(&self, seq: u64) -> Option<(&Term, Vec<Chunk>)> {
        if let Some(snap) = self.snapshots.iter().rev().find(|s| s.seq <= seq) {
            let chunks = self
                .hot
                .iter()
                .filter(|c| c.seq >= snap.seq && c.seq <= seq)
                .map(|c| Chunk { seq: c.seq, offset: c.offset, time: c.time, data: c.data.clone() })
                .collect();
            return Some((&snap.term, chunks));
        }
        let cold = self.cold.iter().rev().find(|c| c.snapshot.seq <= seq)?;
        let chunks = read_segment(cold)?.into_iter().filter(|c| c.seq <= seq).collect();
        Some((&cold.snapshot.term, chunks))
    }

    /// The screen as it was right after chunk `seq`.
    pub fn replay(&self, seq: u64) -> Option<Term> {
        let (snapshot, chunks) = self.chunks_for(seq)?;
        let mut term = snapshot.rewind_snapshot();
        let mut processor = Processor::new();
        for c in &chunks {
            match &c.data {
                Data::Bytes(b) => processor.advance(&mut term, b),
                Data::Resize(cols, rows) => term.resize(*cols, *rows),
            }
        }
        // Replies to queries were answered live; drop the replay's.
        let _ = term.take_replies();
        Some(term)
    }

    /// Cell provenance: in the screen right after chunk `seq`, which output
    /// last changed the cell at column `x`, row `y`. Replays byte by byte
    /// from the snapshot, so it takes a moment for a long segment.
    pub fn provenance(&self, seq: u64, x: usize, y: usize) -> Option<Provenance> {
        let (snapshot, chunks) = self.chunks_for(seq)?;
        let mut term = snapshot.rewind_snapshot();
        let mut processor = Processor::new();
        let cell = |t: &Term| -> Option<Cell> { t.grid().row(y.min(t.rows().saturating_sub(1))).cells().get(x).copied() };
        let mut last: Option<(u64, Instant, usize, usize, bool)> = None;
        for (ci, c) in chunks.iter().enumerate() {
            match &c.data {
                Data::Bytes(b) => {
                    for i in 0..b.len() {
                        let before = cell(&term);
                        let scroll_before = term.grid().history_len();
                        processor.advance(&mut term, &b[i..=i]);
                        if cell(&term) != before {
                            let scrolled = matches!(b[i], b'\n' | 0x0B | 0x0C) || term.grid().history_len() != scroll_before;
                            last = Some((c.offset + i as u64, c.time, ci, i, scrolled));
                        }
                    }
                }
                Data::Resize(cols, rows) => term.resize(*cols, *rows),
            }
        }
        let (offset, time, ci, i, scrolled) = last?;
        let Data::Bytes(b) = &chunks[ci].data else { return None };
        let from = i.saturating_sub(40);
        let context = escape(&b[from..=i]);
        Some(Provenance { offset, time, context, scrolled })
    }
}

impl Drop for RewindLog {
    fn drop(&mut self) {
        if let Some(dir) = &self.dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Bytes shown readably: `\e` for ESC, `\r` and `\n`, other controls as
/// `\xNN`; text as itself.
fn escape(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '\x1b' => out.push_str("\\e"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\x07' => out.push_str("\\a"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    // Padding says little: a long run of spaces becomes `␣×N`.
    let mut squeezed = String::new();
    let mut spaces = 0;
    for c in out.chars().chain(std::iter::once('\0')) {
        if c == ' ' {
            spaces += 1;
            continue;
        }
        if spaces > 3 {
            squeezed.push_str(&format!("␣×{spaces}"));
        } else {
            squeezed.extend(std::iter::repeat_n(' ', spaces));
        }
        spaces = 0;
        if c != '\0' {
            squeezed.push(c);
        }
    }
    squeezed
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// A segment on its way to disk: indexed now, written by the writer thread.
fn cold_segment(dir: &Path, snapshot: Snapshot, chunks: Vec<Chunk>) -> Cold {
    let index = chunks.iter().map(|c| (c.seq, c.offset, c.time, matches!(c.data, Data::Bytes(_)))).collect();
    let raw = chunks.iter().map(|c| if let Data::Bytes(b) = &c.data { b.len() } else { 0 }).sum();
    let file = dir.join(format!("{:012}.lz4", snapshot.seq));
    Cold { snapshot, index, file, raw, pending: Arc::new(Mutex::new(Some(chunks))) }
}

/// A segment's bytes: per chunk, a kind byte, then for output a length and
/// the bytes, for a resize the columns and rows.
fn encode(chunks: &[Chunk]) -> Vec<u8> {
    let mut raw = Vec::new();
    for c in chunks {
        match &c.data {
            Data::Bytes(b) => {
                raw.push(0u8);
                raw.extend_from_slice(&(b.len() as u32).to_le_bytes());
                raw.extend_from_slice(b);
            }
            Data::Resize(cols, rows) => {
                raw.push(1u8);
                raw.extend_from_slice(&(*cols as u32).to_le_bytes());
                raw.extend_from_slice(&(*rows as u32).to_le_bytes());
            }
        }
    }
    raw
}

fn read_segment(cold: &Cold) -> Option<Vec<Chunk>> {
    // Not written yet: still in memory.
    if let Some(chunks) = cold.pending.lock().ok()?.as_ref() {
        return Some(
            chunks.iter().map(|c| Chunk { seq: c.seq, offset: c.offset, time: c.time, data: c.data.clone() }).collect(),
        );
    }
    let raw = lz4_flex::decompress_size_prepended(&std::fs::read(&cold.file).ok()?).ok()?;
    let mut out = Vec::with_capacity(cold.index.len());
    let mut p = 0;
    let u32_at = |p: usize| raw.get(p..p + 4).map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")) as usize);
    for &(seq, offset, time, _) in &cold.index {
        let kind = *raw.get(p)?;
        p += 1;
        let data = if kind == 0 {
            let len = u32_at(p)?;
            p += 4;
            let b = raw.get(p..p + len)?.to_vec();
            p += len;
            Data::Bytes(b)
        } else {
            let (cols, rows) = (u32_at(p)?, u32_at(p + 4)?);
            p += 8;
            Data::Resize(cols, rows)
        };
        out.push(Chunk { seq, offset, time, data });
    }
    Some(out)
}

/// Remove the folders of logs whose app has gone without cleaning up (a
/// crash): each is named `<pid>-<tab>`.
pub fn remove_stale(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.split('-').next().and_then(|p| p.parse::<i32>().ok()) else { continue };
        let alive = unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !alive {
            let _ = std::fs::remove_dir_all(e.path());
        }
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

    fn pause() {
        std::thread::sleep(std::time::Duration::from_millis(60));
    }

    #[test]
    fn replays_the_alternate_screen() {
        let mut live = Term::new(20, 4, 100);
        let mut p = Processor::new();
        let mut log = RewindLog::new(live.rewind_snapshot(), None);
        feed(&mut live, &mut p, &mut log, b"$ htop\r\n");
        pause();
        // htop takes the alternate screen and draws.
        feed(&mut live, &mut p, &mut log, b"\x1b[?1049h\x1b[H\x1b[2JCPU 87%\r\nMEM 9.4G");
        pause();
        feed(&mut live, &mut p, &mut log, b"\x1b[HCPU \x1b[1;32m12%");
        pause();
        // ... and quits, putting the shell's screen back.
        feed(&mut live, &mut p, &mut log, b"\x1b[?1049l$ ");
        assert!(screen_text(&live).starts_with("$ htop"));

        let frames = log.frames();
        assert_eq!(frames.len(), 4);
        assert_eq!(screen_text(&log.replay(frames[1].0).unwrap()), "CPU 87%\nMEM 9.4G");
        assert_eq!(screen_text(&log.replay(frames[2].0).unwrap()), "CPU 12%\nMEM 9.4G");
        assert!(screen_text(&log.replay(frames[3].0).unwrap()).starts_with("$ htop"));

        // Cell provenance: the 1 of "12%" came from the green redraw.
        let prov = log.provenance(frames[2].0, 4, 0).unwrap();
        assert!(prov.context.ends_with("\\e[1;32m1"), "{:?}", prov.context);
        assert!(!prov.scrolled);
        // "MEM" was written once, by the first frame.
        let mem = log.provenance(frames[2].0, 0, 1).unwrap();
        assert!(mem.context.ends_with("\\r\\nM"), "{:?}", mem.context);
    }

    #[test]
    fn older_output_spills_to_disk_and_replays() {
        let dir = std::env::temp_dir().join(format!("tachyon-rewind-test-{}", std::process::id()));
        let mut live = Term::new(40, 5, 100);
        let mut p = Processor::new();
        let mut log = RewindLog::new(live.rewind_snapshot(), Some(dir.clone()));
        let mut early = None;
        // About 3x the memory budget, in bursts.
        for i in 0..(HOT_BUDGET * 3 / 40_000) {
            let block: String = (0..1000).map(|j| format!("{:>9} {}\r\n", i * 1000 + j, "y".repeat(28))).collect();
            feed(&mut live, &mut p, &mut log, block.as_bytes());
            if i == 50 {
                early = Some((log.next_seq - 1, screen_text(&live)));
            }
        }
        assert!(log.hot_bytes <= HOT_BUDGET + SNAPSHOT_EVERY, "{} in memory", log.hot_bytes);
        assert!(!log.cold.is_empty(), "older output went to disk");
        // The writer thread catches up.
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while log.cold.iter().any(|c| c.pending.lock().unwrap().is_some()) && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let files = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(files, log.cold.len());
        // A frame from early on, now on disk, replays exactly.
        let (seq, text) = early.unwrap();
        assert_eq!(screen_text(&log.replay(seq).unwrap()), text);
        // So does the newest, from memory.
        assert_eq!(screen_text(&log.replay(log.next_seq - 1).unwrap()), screen_text(&live));
        drop(log);
        assert!(!dir.exists(), "the log's files go with it");
    }

    #[test]
    fn without_a_folder_old_output_is_dropped() {
        let mut live = Term::new(40, 5, 100);
        let mut p = Processor::new();
        let mut log = RewindLog::new(live.rewind_snapshot(), None);
        for i in 0..(HOT_BUDGET * 2 / 40_000) {
            let block: String = (0..1000).map(|j| format!("{:>9} {}\r\n", i * 1000 + j, "z".repeat(28))).collect();
            feed(&mut live, &mut p, &mut log, block.as_bytes());
        }
        assert!(log.hot_bytes <= HOT_BUDGET + SNAPSHOT_EVERY);
        assert!(log.cold.is_empty());
        let first = log.frames().first().unwrap().0;
        assert!(log.replay(first).is_some(), "the oldest kept frame replays");
    }

    #[test]
    fn resizes_replay_too() {
        let mut live = Term::new(20, 4, 100);
        let mut p = Processor::new();
        let mut log = RewindLog::new(live.rewind_snapshot(), None);
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
        let line = b"[00001234] compiling crate_977 v0.12.6 step loss=0.4123 12.5it/s\r\n";
        let chunk: Vec<u8> = line.iter().copied().cycle().take(64 * 1024).collect();
        let total = 256 << 20;
        let dir = std::env::temp_dir().join(format!("tachyon-rewind-bench-{}", std::process::id()));
        let mut best = (f64::MAX, f64::MAX);
        let mut kept = None;
        for run in 0..3 {
            let mut t = Term::new(200, 50, 10_000);
            let mut p = Processor::new();
            let start = Instant::now();
            for _ in 0..total / chunk.len() {
                p.advance(&mut t, &chunk);
            }
            best.0 = best.0.min(start.elapsed().as_secs_f64());

            let mut t = Term::new(200, 50, 10_000);
            let mut p = Processor::new();
            let mut log = RewindLog::new(t.rewind_snapshot(), Some(dir.join(run.to_string())));
            let start = Instant::now();
            for _ in 0..total / chunk.len() {
                p.advance(&mut t, &chunk);
                log.record(&chunk);
                if log.wants_snapshot() && p.is_ground() {
                    log.snapshot(t.rewind_snapshot());
                }
            }
            best.1 = best.1.min(start.elapsed().as_secs_f64());
            kept = Some(log);
        }
        let mib = (total >> 20) as f64;
        // Seeks, worst case: the read just before the next snapshot in
        // memory, and the last read of a segment on disk.
        let log = kept.unwrap();
        while log.cold.iter().any(|c| c.pending.lock().unwrap().is_some()) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let hot = log.snapshots.get(1).map_or(log.next_seq - 1, |s| s.seq - 1);
        let cold = log.cold.front().expect("spilled").index.last().unwrap().0;
        let time = |seq: u64| {
            let start = Instant::now();
            log.replay(seq).expect("kept");
            start.elapsed().as_secs_f64() * 1000.0
        };
        println!("seek in memory: {:.1} ms; seek on disk: {:.1} ms", time(hot), time(cold));
        drop(log);
        let _ = std::fs::remove_dir_all(&dir);
        println!(
            "parse only: {:.0} MiB/s; with rewind (spilling to disk): {:.0} MiB/s ({:+.1}%)",
            mib / best.0,
            mib / best.1,
            (best.0 / best.1 - 1.0) * 100.0
        );
    }
}
