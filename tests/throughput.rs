//! Throughput measurements for the VT pipeline.
//!
//! These are `#[ignore]`d so `cargo test` stays fast; run them with
//!
//! ```text
//! cargo test --release --test throughput -- --ignored --nocapture
//! ```
//!
//! The numbers that matter are the ratio between the vectorised scanner and the
//! byte-at-a-time path, and the absolute bytes/second the terminal can absorb.
//! The second one is what decides whether `cat` of a large file feels instant
//! or feels like a terminal from 1995.

use std::time::Instant;

use tachyon::term::{Processor, Term};

/// Rough stand-in for build output: mostly plain ASCII, a colour escape every
/// few lines, regular newlines.
fn build_log(lines: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(lines * 90);
    for i in 0..lines {
        if i % 7 == 0 {
            out.extend_from_slice(b"\x1b[1;32m");
        }
        out.extend_from_slice(
            format!(
                "   Compiling some-crate-name v0.{}.{} (/home/user/projects/workspace/crate-{})\r\n",
                i % 40,
                i % 13,
                i % 97
            )
            .as_bytes(),
        );
        if i % 7 == 0 {
            out.extend_from_slice(b"\x1b[0m");
        }
    }
    out
}

/// Pathological case for a naive parser: no escapes at all, very long lines.
fn plain_ascii(bytes: usize) -> Vec<u8> {
    let mut line: Vec<u8> = (0..200).map(|i| b' ' + ((i * 7) % 90) as u8).collect();
    line.extend_from_slice(b"\r\n");
    line.iter().cycle().take(bytes).copied().collect()
}

/// Escape-heavy: what a TUI redrawing a full screen emits.
fn tui_redraw(frames: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for f in 0..frames {
        out.extend_from_slice(b"\x1b[H");
        for row in 0..50 {
            out.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());
            out.extend_from_slice(
                format!("\x1b[38;5;{}m\x1b[48;5;{}m", (row + f) % 256, (row * 3) % 256).as_bytes(),
            );
            out.extend_from_slice(b"status line content padded out to a realistic width | ");
            out.extend_from_slice(b"\x1b[K");
        }
        out.extend_from_slice(b"\x1b[0m");
    }
    out
}

fn measure(name: &str, data: &[u8], rounds: usize) -> f64 {
    // Warm the caches and the atlas-free path once before timing.
    {
        let mut term = Term::new(200, 50, 10_000);
        let mut p = Processor::new();
        p.advance(&mut term, data);
    }

    let start = Instant::now();
    let mut total = 0usize;
    for _ in 0..rounds {
        let mut term = Term::new(200, 50, 10_000);
        let mut p = Processor::new();
        // Feed in 64 KiB chunks, the way a real read loop would.
        for chunk in data.chunks(64 * 1024) {
            p.advance(&mut term, chunk);
        }
        total += data.len();
    }
    let secs = start.elapsed().as_secs_f64();
    let mib_s = total as f64 / secs / (1024.0 * 1024.0);
    println!("  {name:<28} {mib_s:>8.1} MiB/s   ({total} bytes in {secs:.3}s)");
    mib_s
}

#[test]
#[ignore = "benchmark; run with --ignored --nocapture"]
fn parser_throughput() {
    println!("\nVT pipeline throughput (parse + apply to a 200x50 grid):");
    let plain = plain_ascii(16 * 1024 * 1024);
    let logs = build_log(120_000);
    let tui = tui_redraw(2_000);

    let a = measure("plain ASCII", &plain, 3);
    let b = measure("build log (SGR every 7)", &logs, 3);
    let c = measure("TUI full redraw", &tui, 3);

    // These are correctness-adjacent guards, not tight bounds: if a change
    // makes the parser an order of magnitude slower, the suite should say so.
    assert!(a > 50.0, "plain ASCII throughput collapsed: {a:.1} MiB/s");
    assert!(b > 20.0, "log throughput collapsed: {b:.1} MiB/s");
    assert!(c > 5.0, "TUI throughput collapsed: {c:.1} MiB/s");
}

#[test]
#[ignore = "benchmark; run with --ignored --nocapture"]
fn scanner_speedup() {
    use tachyon::term::parser::scan_printable_ascii;

    let data = plain_ascii(64 * 1024 * 1024);
    let rounds = 4;

    // Vectorised: one call consumes the whole run.
    let start = Instant::now();
    let mut consumed = 0usize;
    for _ in 0..rounds {
        let mut s = &data[..];
        while !s.is_empty() {
            let n = scan_printable_ascii(s);
            if n == 0 {
                s = &s[1..];
                consumed += 1;
            } else {
                s = &s[n..];
                consumed += n;
            }
        }
    }
    let simd = start.elapsed().as_secs_f64();
    let simd_gib = (consumed as f64) / simd / (1024.0 * 1024.0 * 1024.0);

    // Byte-at-a-time reference, the shape of a classic state machine.
    let start = Instant::now();
    let mut count = 0usize;
    for _ in 0..rounds {
        for &b in &data {
            if (0x20..0x7F).contains(&b) {
                count += 1;
            }
        }
    }
    let scalar = start.elapsed().as_secs_f64();
    let scalar_gib = (data.len() * rounds) as f64 / scalar / (1024.0 * 1024.0 * 1024.0);

    println!("\nGround-state scanner:");
    println!("  vectorised   {simd_gib:>8.2} GiB/s");
    println!("  per-byte     {scalar_gib:>8.2} GiB/s");
    println!("  speedup      {:>8.2}x", simd_gib / scalar_gib);
    assert!(count > 0);
    assert!(
        simd_gib > scalar_gib,
        "vectorised scan should beat the per-byte loop"
    );
}
