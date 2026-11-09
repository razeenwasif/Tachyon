//! The latency ledger: where each keystroke's delay goes.
//!
//! A keystroke is timed at five points, all on the same clock (seconds since
//! boot, the clock AppKit's event timestamps and Core Animation use):
//!
//! 1. the key event, when the hardware delivered it;
//! 2. the write to the PTY;
//! 3. the first byte the shell sends back;
//! 4. that output parsed into the terminal;
//! 5. the frame showing it committed to the GPU, then
//! 6. presented on the display.
//!
//! So a keystroke's delay splits into Tachyon taking the key (1→2), the shell
//! and anything between (ssh, a remote machine) echoing it (2→3), Tachyon
//! reading and parsing (3→4), waiting for and building the next frame (4→5),
//! and the GPU and display (5→6). Each stage is also an `os_signpost`
//! interval, so Instruments' Points of Interest track shows them against
//! Tachyon's own frames.

use std::collections::VecDeque;
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;

use parking_lot::Mutex;

/// Now, in nanoseconds since boot (not counting sleep), the clock of
/// `NSEvent.timestamp` and `CACurrentMediaTime`.
pub fn now() -> u64 {
    unsafe { clock_gettime_nsec_np(libc::CLOCK_UPTIME_RAW) }
}

extern "C" {
    fn clock_gettime_nsec_np(clock: libc::clockid_t) -> u64;
}

/// Seconds on that clock, as AppKit and Core Animation give them, in ns.
pub fn from_seconds(s: f64) -> u64 {
    (s * 1e9) as u64
}

/// One pane's keystroke in flight: stamps 1 to 4, set from the main thread
/// and the pane's reader thread. Zero means not yet.
#[derive(Default)]
pub struct Slot {
    key: AtomicU64,
    write: AtomicU64,
    first: AtomicU64,
    parsed: AtomicU64,
    /// Signpost id for this keystroke's intervals.
    id: AtomicU64,
}

impl Slot {
    /// A key was pressed at `key` (its event time) and written now.
    pub fn keystroke(&self, key: u64) {
        let id = signposts().map(|s| s.begin("keystroke")).unwrap_or(0);
        self.first.store(0, Ordering::Release);
        self.parsed.store(0, Ordering::Release);
        self.key.store(key, Ordering::Release);
        self.write.store(now(), Ordering::Release);
        self.id.store(id, Ordering::Release);
        if let Some(s) = signposts() {
            s.begin_with(id, "shell");
        }
    }

    /// Output arrived (reader thread, before parsing it).
    pub fn output_read(&self) {
        if self.write.load(Ordering::Acquire) != 0 && self.first.load(Ordering::Acquire) == 0 {
            self.first.store(now(), Ordering::Release);
            if let Some(s) = signposts() {
                let id = self.id.load(Ordering::Acquire);
                s.end(id, "shell");
                s.begin_with(id, "parse");
            }
        }
    }

    /// That output is parsed (reader thread).
    pub fn output_parsed(&self) {
        if self.first.load(Ordering::Acquire) != 0 && self.parsed.load(Ordering::Acquire) == 0 {
            self.parsed.store(now(), Ordering::Release);
            if let Some(s) = signposts() {
                let id = self.id.load(Ordering::Acquire);
                s.end(id, "parse");
                s.begin_with(id, "frame");
            }
        }
    }

    /// A frame is being committed: if it shows this keystroke's echo, take
    /// the stamps so far for the presented handler to finish, and clear the
    /// slot.
    pub fn take_for_frame(&self) -> Option<Pending> {
        let parsed = self.parsed.load(Ordering::Acquire);
        if parsed == 0 {
            return None;
        }
        let p = Pending {
            key: self.key.swap(0, Ordering::AcqRel),
            write: self.write.swap(0, Ordering::AcqRel),
            first: self.first.swap(0, Ordering::AcqRel),
            parsed: self.parsed.swap(0, Ordering::AcqRel),
            commit: now(),
            id: self.id.load(Ordering::Acquire),
        };
        if let Some(s) = signposts() {
            s.end(p.id, "frame");
            s.begin_with(p.id, "display");
        }
        Some(p)
    }
}

/// Stamps 1 to 5 of a keystroke whose frame is on its way to the display.
#[derive(Clone, Copy, Debug)]
pub struct Pending {
    key: u64,
    write: u64,
    first: u64,
    parsed: u64,
    commit: u64,
    id: u64,
}

/// A keystroke's delay, stage by stage, in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Entry {
    pub take: f64,
    pub shell: f64,
    pub parse: f64,
    pub frame: f64,
    pub display: f64,
}

impl Entry {
    pub fn total(&self) -> f64 {
        self.take + self.shell + self.parse + self.frame + self.display
    }

    /// Tachyon's own work: taking the key, and reading and parsing the
    /// echo. Waiting for the next frame is shown apart.
    pub fn tachyon(&self) -> f64 {
        self.take + self.parse
    }
}

impl Pending {
    /// The frame reached the display at `presented` (ns on the same clock).
    pub fn finish(self, presented: u64) -> Option<Entry> {
        if let Some(s) = signposts() {
            s.end(self.id, "display");
            s.end(self.id, "keystroke");
        }
        let ms = |a: u64, b: u64| b.saturating_sub(a) as f64 / 1e6;
        // A presented time before the commit means the display didn't report
        // one (or it was dropped): no entry.
        if presented < self.commit || self.key == 0 {
            return None;
        }
        let e = Entry {
            take: ms(self.key, self.write),
            shell: ms(self.write, self.first),
            parse: ms(self.first, self.parsed),
            frame: ms(self.parsed, self.commit),
            display: ms(self.commit, presented),
        };
        record(e);
        Some(e)
    }
}

/// Keystrokes measured recently, newest last.
static RECENT: Mutex<VecDeque<Entry>> = Mutex::new(VecDeque::new());
const KEEP: usize = 200;

fn record(e: Entry) {
    let mut r = RECENT.lock();
    if r.len() == KEEP {
        r.pop_front();
    }
    r.push_back(e);
}

/// Whether the ledger is showing in the window.
pub static SHOWN: AtomicBool = AtomicBool::new(false);

/// The line the window shows: the last keystroke stage by stage, and the
/// median of recent ones.
pub fn summary() -> Option<String> {
    let r = RECENT.lock();
    let last = *r.back()?;
    let mut totals: Vec<f64> = r.iter().map(Entry::total).collect();
    totals.sort_by(f64::total_cmp);
    let median = totals[totals.len() / 2];
    Some(format!(
        "⏱ {:.1} ms (median {:.1} of {}) · Tachyon {:.1} · shell {:.1} · frame wait {:.1} · display {:.1}",
        last.total(),
        median,
        totals.len(),
        last.tachyon(),
        last.shell,
        last.frame,
        last.display
    ))
}

// ---------------------------------------------------------------------------
// os_signpost, through the functions its C macros expand to
// ---------------------------------------------------------------------------

#[allow(non_upper_case_globals)]
extern "C" {
    #[link_name = "__dso_handle"]
    static DSO_HANDLE: c_void;
    fn os_log_create(subsystem: *const c_char, category: *const c_char) -> *mut c_void;
    fn os_signpost_id_generate(log: *mut c_void) -> u64;
    fn os_signpost_enabled(log: *mut c_void) -> bool;
    fn _os_signpost_emit_with_name_impl(
        dso: *const c_void,
        log: *mut c_void,
        kind: u8,
        id: u64,
        name: *const c_char,
        format: *const c_char,
        buf: *mut u8,
        size: u32,
    );
}

const BEGIN: u8 = 1;
const END: u8 = 2;

pub struct Signposts {
    log: usize,
}

fn signposts() -> Option<&'static Signposts> {
    static S: OnceLock<Option<Signposts>> = OnceLock::new();
    S.get_or_init(|| {
        // "PointsOfInterest" puts them in Instruments' Points of Interest
        // track without any setup.
        let log = unsafe { os_log_create(c"dev.tachyon.terminal".as_ptr(), c"PointsOfInterest".as_ptr()) };
        (!log.is_null()).then_some(Signposts { log: log as usize })
    })
    .as_ref()
}

/// Interval names: static C strings, as the signpost API requires.
fn name(n: &str) -> &'static std::ffi::CStr {
    match n {
        "keystroke" => c"keystroke",
        "shell" => c"shell echo",
        "parse" => c"read and parse",
        "frame" => c"frame wait",
        _ => c"display",
    }
}

impl Signposts {
    fn emit(&self, kind: u8, id: u64, n: &str) {
        let log = self.log as *mut c_void;
        if id == 0 || !unsafe { os_signpost_enabled(log) } {
            return;
        }
        // An empty argument buffer: summary byte, then argument count.
        let mut buf = [0u8; 2];
        unsafe {
            _os_signpost_emit_with_name_impl(&DSO_HANDLE, log, kind, id, name(n).as_ptr(), c"".as_ptr(), buf.as_mut_ptr(), 2);
        }
    }

    fn begin(&self, n: &str) -> u64 {
        let id = unsafe { os_signpost_id_generate(self.log as *mut c_void) };
        self.emit(BEGIN, id, n);
        id
    }

    fn begin_with(&self, id: u64, n: &str) {
        self.emit(BEGIN, id, n);
    }

    fn end(&self, id: u64, n: &str) {
        self.emit(END, id, n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keystroke_splits_into_stages() {
        let slot = Slot::default();
        assert!(slot.take_for_frame().is_none(), "nothing typed");
        let key = now();
        slot.keystroke(key);
        assert!(slot.take_for_frame().is_none(), "no echo yet");
        std::thread::sleep(std::time::Duration::from_millis(3));
        slot.output_read();
        slot.output_parsed();
        let p = slot.take_for_frame().expect("echo parsed");
        assert!(slot.take_for_frame().is_none(), "taken once");
        let e = p.finish(now() + 8_000_000).expect("presented");
        assert!(e.shell >= 3.0, "{e:?}");
        assert!(e.display >= 8.0 && e.total() >= 11.0, "{e:?}");
        assert!(summary().unwrap().starts_with("⏱"));
        // Presented "before" it was committed: the display gave no time.
        slot.keystroke(now());
        slot.output_read();
        slot.output_parsed();
        assert!(slot.take_for_frame().unwrap().finish(0).is_none());
    }
}
