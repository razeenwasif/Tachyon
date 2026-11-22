//! Markdown command block & receipt export shared by GUI and CLI.

use std::fmt::Write;
use std::time::Duration;

use super::{lens, CommandState, Receipt, Term};

/// Optional host and directory context overriding or supplementing `Term`.
#[derive(Clone, Copy, Debug, Default)]
pub struct MarkdownContext<'a> {
    pub host: Option<&'a str>,
    pub cwd: Option<&'a str>,
}

/// Summary row for a single parsed metric curve.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportMetric {
    pub name: String,
    pub latest: f64,
    pub latest_step: Option<f64>,
    pub min: f64,
    pub max: f64,
    pub samples: u64,
}

/// Self-contained command block metadata rendered to Markdown.
#[derive(Clone, Debug)]
pub struct ExportBlock<'a> {
    pub cmdline: Option<&'a str>,
    pub host: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub elapsed: Option<Duration>,
    pub receipt: Option<Receipt>,
    pub lens_summary: Option<String>,
    pub metrics: Vec<ExportMetric>,
    pub rate: Option<(f64, String)>,
    pub percent: Option<f32>,
    pub rerun_changed_lines: Option<usize>,
    pub job: Option<String>,
    pub output: Option<&'a str>,
}

fn max_backtick_run(s: &str) -> usize {
    let mut best = 0usize;
    let mut cur = 0usize;
    for b in s.bytes() {
        if b == b'`' {
            cur += 1;
            if cur > best {
                best = cur;
            }
        } else {
            cur = 0;
        }
    }
    best
}

/// Formats `s` as an inline CommonMark code span safe against embedded backticks.
pub fn inline_code(s: &str) -> String {
    let fence = "`".repeat(max_backtick_run(s) + 1);
    let pad = s.starts_with('`') || s.ends_with('`') || (s.starts_with(' ') && s.ends_with(' ') && !s.trim().is_empty());
    if pad {
        format!("{fence} {s} {fence}")
    } else {
        format!("{fence}{s}{fence}")
    }
}

/// Returns a fenced-code-block delimiter guaranteed not to collide with `text`.
pub fn fence_for(text: &str) -> String {
    "`".repeat((max_backtick_run(text) + 1).max(3))
}

/// Formats a duration for export, preserving sub-second precision.
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 0.001 {
        format!("{} µs", d.as_micros())
    } else if secs < 1.0 {
        format!("{} ms", d.as_millis())
    } else if secs < 60.0 {
        format!("{secs:.2} s")
    } else {
        let total = d.as_secs();
        let h = total / 3600;
        let m = (total % 3600) / 60;
        let s = total % 60;
        if h > 0 {
            format!("{h}h {m:02}m {s:02}s")
        } else {
            format!("{m}m {s:02}s")
        }
    }
}

/// Formats energy in joules or watt-hours.
pub fn format_energy(joules: f64) -> String {
    if joules >= 360.0 {
        format!("{:.2} Wh", joules / 3600.0)
    } else if joules >= 1.0 {
        format!("{joules:.1} J")
    } else {
        format!("{joules:.2} J")
    }
}

/// Formats a byte count using binary IEC units.
pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Suggests a sanitized `.md` filename for saving a command block report.
pub fn suggested_filename(cmdline: Option<&str>, exit_code: Option<i32>) -> String {
    let raw = cmdline.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("command");
    let mut slug = String::with_capacity(40);
    let mut last_dash = false;
    for ch in raw.chars() {
        if slug.len() >= 40 {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if ch == '_' {
            slug.push('_');
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    let base = if slug.is_empty() { "command" } else { slug };
    match exit_code {
        Some(code) if code != 0 => format!("{base}-exit-{code}.md"),
        _ => format!("{base}.md"),
    }
}

/// Renders a structured command block, receipt, metrics, and output as Markdown.
pub fn render_markdown(b: &ExportBlock<'_>) -> String {
    let mut out = String::new();
    let title_raw = b.cmdline.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("(command)");
    let single_line: String = title_raw
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let _ = writeln!(out, "### {}\n", inline_code(&single_line));

    let status = if b.running {
        "running".to_string()
    } else {
        match b.exit_code {
            Some(0) => "✓ exit 0".to_string(),
            Some(code) => format!("✗ exit {code}"),
            None => "finished".to_string(),
        }
    };
    let _ = writeln!(out, "- **Status:** {status}");

    if let Some(host) = b.host.map(str::trim).filter(|s| !s.is_empty()) {
        let _ = writeln!(out, "- **Host:** {}", inline_code(host));
    }
    if let Some(cwd) = b.cwd.map(str::trim).filter(|s| !s.is_empty()) {
        let _ = writeln!(out, "- **Directory:** {}", inline_code(cwd));
    }

    let mut receipt_parts: Vec<String> = Vec::new();
    if let Some(wall) = b.elapsed {
        receipt_parts.push(format!("wall {}", format_duration(wall)));
    }
    if let Some(rcpt) = b.receipt {
        if let Some(cpu) = rcpt.cpu {
            match rcpt.pcore_share {
                Some(p) if p.is_finite() => {
                    receipt_parts.push(format!("cpu {} (P {:.0}%)", format_duration(cpu), p.clamp(0.0, 1.0) * 100.0));
                }
                _ => receipt_parts.push(format!("cpu {}", format_duration(cpu))),
            }
        }
        if let Some(j) = rcpt.energy_joules.filter(|j| j.is_finite() && *j > 0.0) {
            receipt_parts.push(format!("energy {}", format_energy(j)));
        }
        if let Some(bytes) = rcpt.peak_footprint.filter(|&bytes| bytes > 0) {
            receipt_parts.push(format!("peak mem {}", format_bytes(bytes)));
        }
    }
    if !receipt_parts.is_empty() {
        let _ = writeln!(out, "- **Receipt:** {}", receipt_parts.join(" · "));
    }

    let mut metrics_line = b.lens_summary.clone();
    if metrics_line.is_none() {
        let mut extra = Vec::new();
        if let Some((rate, unit)) = &b.rate {
            extra.push(format!("{} {unit}", lens::short(*rate)));
        }
        if let Some(p) = b.percent {
            extra.push(format!("{p:.0}%"));
        }
        if !extra.is_empty() {
            metrics_line = Some(extra.join(" · "));
        }
    }
    if let Some(summary) = metrics_line {
        let _ = writeln!(out, "- **Metrics:** {summary}");
    }

    if let Some(changed) = b.rerun_changed_lines {
        let label = match changed {
            0 => "same as last run".to_string(),
            1 => "1 line changed".to_string(),
            n => format!("{n} lines changed"),
        };
        let _ = writeln!(out, "- **Diff vs previous run:** {label}");
    }
    if let Some(job) = b.job.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let _ = writeln!(out, "- **Cluster job:** {}", inline_code(job));
    }

    if !b.metrics.is_empty() {
        out.push_str("\n#### Metrics\n\n");
        out.push_str("| Metric | Latest | Min | Max | Samples |\n");
        out.push_str("| --- | ---: | ---: | ---: | ---: |\n");
        for m in &b.metrics {
            let safe_name = m.name.replace('|', "\\|");
            let latest = match m.latest_step {
                Some(step) => format!("`{}` (step `{}`)", lens::short(m.latest), lens::short(step)),
                None => format!("`{}`", lens::short(m.latest)),
            };
            let _ = writeln!(
                out,
                "| {} | {} | `{}` | `{}` | {} |",
                inline_code(&safe_name),
                latest,
                lens::short(m.min),
                lens::short(m.max),
                m.samples
            );
        }
    }

    match b.output.map(|s| s.trim_end_matches('\n')).filter(|s| !s.is_empty()) {
        Some(body) => {
            let fence = fence_for(body);
            let _ = writeln!(out, "\n{fence}text\n{body}\n{fence}");
        }
        None => out.push_str("\n_No output._\n"),
    }

    out
}

/// Renders command block `id` inside `term` as a self-contained Markdown report.
pub fn command_to_markdown(term: &Term, id: u32, ctx: &MarkdownContext<'_>) -> Option<String> {
    let info = term.command(id)?;
    let cmd_fallback = if info.cmdline.is_none() { term.command_input_line(id) } else { None };
    let cmdline = info.cmdline.as_deref().or(cmd_fallback.as_deref());
    let output = term.output_text(id);

    let mut metrics = Vec::new();
    for c in info.lens.chart_curves() {
        let pts = c.points();
        let Some((last_x, last_y)) = pts.last().copied().or_else(|| c.last()) else {
            continue;
        };
        let mut lo = last_y;
        let mut hi = last_y;
        for &(_, y) in &pts {
            if y < lo {
                lo = y;
            }
            if y > hi {
                hi = y;
            }
        }
        metrics.push(ExportMetric {
            name: c.name.clone(),
            latest: last_y,
            latest_step: c.by_step().then_some(last_x),
            min: lo,
            max: hi,
            samples: c.seen().max(pts.len() as u64),
        });
    }

    let block = ExportBlock {
        cmdline,
        host: ctx.host.or_else(|| term.cwd_host()),
        cwd: ctx.cwd.or(term.cwd.as_deref()),
        running: info.state == CommandState::Running,
        exit_code: info.exit,
        elapsed: info.elapsed(),
        receipt: info.receipt,
        lens_summary: info.lens.describe(),
        metrics,
        rate: info.lens.rate.clone(),
        percent: info.lens.percent,
        rerun_changed_lines: info.rerun.as_ref().map(Vec::len),
        job: info.job.as_ref().map(|j| j.title()),
        output: output.as_deref(),
    };

    Some(render_markdown(&block))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Processor;

    #[test]
    fn command_markdown_includes_command_exit_receipt_metrics_and_folded_output() {
        let mut t = Term::new(60, 12, 500);
        let mut p = Processor::new();
        p.advance(&mut t, b"\x1b]7;file://tsukuyomi/Users/dev/proj\x07");
        p.advance(&mut t, b"\x1b]133;A\x07$ \x1b]133;B\x07train\r\n\x1b]133;C;cmdline=python train.py --epochs 3\x07");
        let cmd = t.current_command().unwrap();

        for step in 1..=14 {
            let line = format!("step={step} loss={:.3} 42.0it/s\r\n", 1.5 - step as f64 * 0.05);
            let r = lens::read_line(line.trim_end());
            if !r.is_empty() {
                t.record_reading(&r);
            }
            p.advance(&mut t, line.as_bytes());
        }
        let fenced_line = "```json\r\n{\"ok\":true}\r\n```\r\n";
        p.advance(&mut t, fenced_line.as_bytes());
        p.advance(&mut t, b"\x1b]133;D;2\x07");

        t.set_receipt_with_wall(
            cmd,
            Receipt {
                cpu: Some(Duration::from_millis(840)),
                pcore_share: Some(0.85),
                energy_joules: Some(12.4),
                peak_footprint: Some(512 * 1024 * 1024),
            },
            Some(Duration::from_secs(3)),
        );

        assert!(t.toggle_fold(cmd), "output long enough to fold");
        assert!(t.is_folded(cmd));

        let md = command_to_markdown(&t, cmd, &MarkdownContext { host: Some("local"), cwd: None })
            .expect("markdown rendered");
        assert!(md.starts_with("### `python train.py --epochs 3`"), "md:\n{md}");
        assert!(md.contains("- **Status:** ✗ exit 2"), "md:\n{md}");
        assert!(md.contains("- **Host:** `local`"), "md:\n{md}");
        assert!(md.contains("- **Directory:** `/Users/dev/proj`"), "md:\n{md}");
        assert!(md.contains("wall 3.00 s"), "md:\n{md}");
        assert!(md.contains("cpu 840 ms (P 85%)"), "md:\n{md}");
        assert!(md.contains("energy 12.4 J"), "md:\n{md}");
        assert!(md.contains("peak mem 512.0 MiB"), "md:\n{md}");
        assert!(md.contains("#### Metrics"), "md:\n{md}");
        assert!(md.contains("| `loss` |"), "md:\n{md}");
        assert!(md.contains("(step `14`)"), "md:\n{md}");
        // Dynamic 4-backtick fence because output contains triple-backtick code block.
        assert!(md.contains("````text\n"), "expected 4-backtick fence:\n{md}");
        assert!(md.contains("step=1 loss=1.450"), "folded lines preserved:\n{md}");
        assert!(md.contains("```json\n{\"ok\":true}\n```"), "md:\n{md}");
        assert!(md.ends_with("\n````\n"), "md:\n{md}");
    }

    #[test]
    fn suggested_filename_sanitizes_and_bounds_slug() {
        assert_eq!(suggested_filename(Some("cargo test --no-default-features"), Some(0)), "cargo-test-no-default-features.md");
        assert_eq!(suggested_filename(Some("python3 train.py --lr 1e-3"), Some(2)), "python3-train-py-lr-1e-3-exit-2.md");
        assert_eq!(suggested_filename(None, None), "command.md");
        let long = format!("python {}", "a".repeat(80));
        let name = suggested_filename(Some(&long), Some(1));
        assert!(name.ends_with("-exit-1.md"));
        assert!(name.len() <= 40 + "-exit-1.md".len());
    }
}
