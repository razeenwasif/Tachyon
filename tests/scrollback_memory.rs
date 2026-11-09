//! Heap held by scrollback, per line. Run each case on its own:
//! `cargo test --release --test scrollback_memory scrollback_build_log -- --exact --ignored --nocapture`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use tachyon::term::{Processor, Term};

/// Counts live heap bytes, as the allocator was asked for them.
struct Counting;
static LIVE: AtomicIsize = AtomicIsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size() as isize, Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        LIVE.fetch_add(n as isize - l.size() as isize, Ordering::Relaxed);
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// A build log's lines: mostly short, a colour every few lines.
fn build_log(lines: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..lines {
        if i % 7 == 0 {
            out.extend_from_slice(b"\x1b[1;32m");
        }
        out.extend_from_slice(format!("   Compiling some-crate-name v0.{}.{} (/home/user/projects/workspace/crate-{})\r\n", i % 40, i % 13, i % 97).as_bytes());
        if i % 7 == 0 {
            out.extend_from_slice(b"\x1b[0m");
        }
    }
    out
}

/// The process's memory footprint (what Activity Monitor shows), in bytes.
fn footprint() -> i64 {
    let mut r: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    unsafe { libc::proc_pid_rusage(libc::getpid(), 4, &mut r as *mut _ as *mut libc::rusage_info_t) };
    r.ri_phys_footprint as i64
}

fn per_line(name: &str, data: &[u8], lines: usize) -> f64 {
    let before = LIVE.load(Ordering::Relaxed);
    let fp_before = footprint();
    let mut t = Term::new(200, 50, 100_000);
    let mut p = Processor::new();
    for c in data.chunks(64 * 1024) {
        p.advance(&mut t, c);
    }
    let held = (LIVE.load(Ordering::Relaxed) - before) as f64;
    let real = (footprint() - fp_before) as f64;
    let n = t.grid().history_len() as f64;
    let per = held / n;
    println!(
        "  {name:<36} {:>6.1} MiB asked for, {:>6.1} MiB footprint, {} lines kept: {per:>4.0} B a line asked, {:>4.0} B real",
        held / 1048576.0,
        real / 1048576.0,
        n,
        real / n
    );
    assert!(t.grid().history_len() >= lines.min(100_000) - 100);
    drop(t);
    per
}

// One case per test, and run each in a process of its own (filter by name),
// so the footprint isn't flattered by memory an earlier case freed.

#[test]
#[ignore = "measurement; run with --ignored --nocapture"]
fn scrollback_build_log() {
    per_line("build log, 100k lines", &build_log(100_050), 100_000);
}

#[test]
#[ignore = "measurement; run with --ignored --nocapture"]
fn scrollback_build_log_evicting() {
    // Twice as many lines as are kept: the second 100k evict the first.
    per_line("build log, 200k lines (evicting)", &build_log(200_050), 100_000);
}

#[test]
#[ignore = "measurement; run with --ignored --nocapture"]
fn scrollback_short_lines() {
    let short: Vec<u8> = (0..200_050).flat_map(|i| format!("ok {i}\r\n").into_bytes()).collect();
    per_line("short lines, 200k (evicting)", &short, 100_000);
}
