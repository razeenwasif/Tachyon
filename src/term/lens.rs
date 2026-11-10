//! The metric lens: numbers in a running command's output that look like
//! training or serving metrics (`loss=0.41`, `acc: 87.1%`, `48.2it/s`,
//! `512 tok/s`, a tqdm bar's `45%|`), kept per command so the prompt line can
//! show the latest values and a sparkline of the main one.
//!
//! Lines come from the raw output stream, not the grid, so tqdm's in-place
//! redraws (carriage returns over the same row) are each seen.
//!
//! Each metric's whole run is kept too, as a curve for the training chart:
//! by the step the line reports (`step=120`, `iter 300`, `'epoch': 0.5`), or
//! failing that, by how many values came before.

use std::collections::VecDeque;

/// How many values a series keeps, and how many the sparkline shows.
const HISTORY: usize = 64;
const SPARK: usize = 12;
/// At most this many named metrics per command.
const MAX_SERIES: usize = 8;

/// What one line of output says.
#[derive(Debug, Default, PartialEq)]
pub struct Reading {
    /// Named metrics, in the order they appear: (`loss`, 0.41).
    pub metrics: Vec<(String, f64)>,
    /// A throughput: (48.2, `it/s`).
    pub rate: Option<(f64, String)>,
    /// A progress bar's percentage.
    pub percent: Option<f32>,
    /// Where in the run this line is: its step or iteration, else its epoch.
    pub step: Option<f64>,
}

impl Reading {
    pub fn is_empty(&self) -> bool {
        self.metrics.is_empty() && self.rate.is_none() && self.percent.is_none()
    }
}

/// Keys that say where in a run a line is, best first.
const STEP_KEYS: [&str; 6] = ["global_step", "step", "iteration", "iter", "update", "epoch"];

/// The step a line reports: `step=120`, `Step 120/1000`, `iter: 300`,
/// `'epoch': 0.5`. A step or iteration wins over an epoch.
fn step_of(line: &str) -> Option<f64> {
    let b = line.as_bytes();
    let lower = line.to_ascii_lowercase();
    let mut best: Option<(usize, f64)> = None;
    for (rank, key) in STEP_KEYS.iter().enumerate() {
        let mut from = 0;
        while let Some(at) = lower[from..].find(key) {
            let start = from + at;
            let end = start + key.len();
            from = end;
            // A whole word: `steps_per_epoch` and `substep` aren't it.
            if start > 0 && (b[start - 1].is_ascii_alphanumeric() || b[start - 1] == b'_') {
                continue;
            }
            let mut v = end;
            if v < b.len() && matches!(b[v], b's' | b'S') {
                v += 1; // `steps`
            }
            if v < b.len() && (b[v].is_ascii_alphanumeric() || b[v] == b'_') {
                continue;
            }
            while v < b.len() && matches!(b[v], b' ' | b'=' | b':' | b'\'' | b'"' | b'[' | b'#') {
                v += 1;
            }
            if let Some((value, _)) = number(&b[v..]) {
                if best.is_none_or(|(r, _)| rank < r) {
                    best = Some((rank, value));
                }
                break;
            }
        }
    }
    best.map(|(_, v)| v)
}

/// Keys worth a sparkline. A metric's key must contain one of these, or be
/// one of the exact names, so `time: 12:30` or `step=4` don't count.
const KEY_PARTS: [&str; 14] = [
    "loss", "acc", "ppl", "perplex", "f1", "bleu", "rouge", "auc", "mse", "mae", "reward", "score", "norm", "precision",
];
const EXACT_KEYS: [&str; 4] = ["lr", "map", "wer", "recall"];
const RATE_UNITS: [&str; 9] = ["it/s", "s/it", "tok/s", "tokens/s", "samples/s", "img/s", "ex/s", "batch/s", "steps/s"];

fn is_metric_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    EXACT_KEYS.contains(&k.as_str()) || KEY_PARTS.iter().any(|p| k.contains(p))
}

/// A number at the start of `s`: digits, a point, an exponent. Returns the
/// value and how many bytes it took.
fn number(s: &[u8]) -> Option<(f64, usize)> {
    let mut i = 0;
    if s.first() == Some(&b'-') {
        i += 1;
    }
    let digits = i;
    while i < s.len() && (s[i].is_ascii_digit() || s[i] == b'.') {
        i += 1;
    }
    if i == digits || !s[digits..i].iter().any(u8::is_ascii_digit) {
        return None;
    }
    // 1e-4, 3.2E+01
    if i + 1 < s.len() && (s[i] == b'e' || s[i] == b'E') {
        let mut j = i + 1;
        if j < s.len() && (s[j] == b'-' || s[j] == b'+') {
            j += 1;
        }
        let start = j;
        while j < s.len() && s[j].is_ascii_digit() {
            j += 1;
        }
        if j > start {
            i = j;
        }
    }
    let v: f64 = std::str::from_utf8(&s[..i]).ok()?.parse().ok()?;
    v.is_finite().then_some((v, i))
}

fn is_key_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'/' || b == b'-' || b == b'.'
}

/// Read one line of plain text (escape sequences already removed).
pub fn read_line(line: &str) -> Reading {
    let b = line.as_bytes();
    let mut out = Reading::default();
    let mut i = 0;
    while i < b.len() {
        // key=value, key: value, 'key': value
        if b[i] == b'=' || b[i] == b':' {
            let mut end = i;
            while end > 0 && (b[end - 1] == b'\'' || b[end - 1] == b'"') {
                end -= 1;
            }
            let mut start = end;
            while start > 0 && is_key_byte(b[start - 1]) {
                start -= 1;
            }
            let mut v = i + 1;
            while v < b.len() && (b[v] == b' ' || b[v] == b'\'' || b[v] == b'"') {
                v += 1;
            }
            if start < end {
                let key = &line[start..end];
                if let Some((value, n)) = number(&b[v..]) {
                    // `12:30` is a time, not a metric.
                    let after = b.get(v + n).copied();
                    if is_metric_key(key) && after != Some(b':') {
                        if out.metrics.iter().all(|(k, _)| k != key) {
                            out.metrics.push((key.to_string(), value));
                        }
                        i = v + n;
                        continue;
                    }
                    // Not a metric (`eval: 512 tok/s`): the number may
                    // still be a rate, so read on from it.
                    i = v;
                    continue;
                }
            }
        }
        // 48.2it/s, 512 tok/s, 45%|
        if b[i].is_ascii_digit() && (i == 0 || !is_key_byte(b[i - 1]) || b[i - 1] == b'-') {
            if let Some((value, n)) = number(&b[i..]) {
                let mut j = i + n;
                if b.get(j) == Some(&b'%') && b.get(j + 1) == Some(&b'|') {
                    out.percent = Some(value.clamp(0.0, 100.0) as f32);
                }
                while b.get(j) == Some(&b' ') {
                    j += 1;
                }
                if let Some(unit) = RATE_UNITS.iter().find(|u| b[j..].starts_with(u.as_bytes())) {
                    let next = b.get(j + unit.len()).copied();
                    if next.is_none_or(|c| !c.is_ascii_alphanumeric()) {
                        out.rate = Some((value, unit.to_string()));
                    }
                }
                i += n;
                continue;
            }
        }
        i += 1;
    }
    if !out.metrics.is_empty() {
        out.step = step_of(line);
    }
    out
}

/// Points a curve keeps. Past this, every other one goes and from then on
/// only every second value is kept, so a long run keeps its whole shape,
/// evenly, in bounded memory.
const CURVE_POINTS: usize = 2048;

/// One metric's whole run so far: (step, value) points.
#[derive(Debug, Clone, Default)]
pub struct Curve {
    pub name: String,
    points: Vec<(f64, f64)>,
    /// The newest point, kept even when thinning skips it.
    latest: Option<(f64, f64)>,
    /// One value in this many is kept.
    stride: u64,
    seen: u64,
    /// Points are by reported step, not by count.
    by_step: bool,
}

impl Curve {
    fn new(name: &str, by_step: bool) -> Curve {
        Curve { name: name.to_string(), stride: 1, by_step, ..Curve::default() }
    }

    fn push(&mut self, step: Option<f64>, y: f64) {
        let x = match (self.by_step, step) {
            (true, Some(s)) => s,
            // A line without a step, in a run that has them: where the
            // last one was.
            (true, None) => self.latest.map_or(0.0, |p| p.0),
            (false, _) => (self.seen + 1) as f64,
        };
        // tqdm repeats its postfix on every redraw: the same value at the
        // same place is one point.
        if self.by_step && self.latest == Some((x, y)) {
            return;
        }
        self.seen += 1;
        self.latest = Some((x, y));
        if (self.seen - 1) % self.stride != 0 {
            return;
        }
        self.points.push((x, y));
        if self.points.len() > CURVE_POINTS {
            let mut k = 0;
            self.points.retain(|_| {
                k += 1;
                k % 2 == 1
            });
            self.stride *= 2;
        }
    }

    /// Its points, oldest first, ending with the newest value.
    pub fn points(&self) -> Vec<(f64, f64)> {
        let mut p = self.points.clone();
        if let Some(l) = self.latest {
            if p.last() != Some(&l) {
                p.push(l);
            }
        }
        p
    }

    pub fn last(&self) -> Option<(f64, f64)> {
        self.latest
    }

    /// Its points are by reported step, rather than by count.
    pub fn by_step(&self) -> bool {
        self.by_step
    }

    /// Its value at step `x`, between the points either side; `None` outside
    /// the steps it covers.
    pub fn at(&self, x: f64) -> Option<f64> {
        let p = self.points();
        let i = p.iter().position(|&(px, _)| px >= x)?;
        if i == 0 {
            return (p[0].0 == x).then_some(p[0].1);
        }
        let ((x0, y0), (x1, y1)) = (p[i - 1], p[i]);
        Some(if x1 > x0 { y0 + (y1 - y0) * (x - x0) / (x1 - x0) } else { y1 })
    }
}

/// One command's metrics so far.
#[derive(Debug, Default, Clone)]
pub struct Lens {
    series: Vec<(String, VecDeque<f64>)>,
    /// The same metrics' whole runs, for the chart.
    pub curves: Vec<Curve>,
    pub rate: Option<(f64, String)>,
    pub percent: Option<f32>,
    /// Bumped whenever a curve changes, so a chart is drawn again only then.
    pub revision: u64,
}

impl Lens {
    pub fn record(&mut self, r: &Reading) {
        for (key, value) in &r.metrics {
            let i = match self.series.iter().position(|(k, _)| k == key) {
                Some(i) => i,
                None if self.series.len() < MAX_SERIES => {
                    self.series.push((key.clone(), VecDeque::new()));
                    self.series.len() - 1
                }
                None => continue,
            };
            let values = &mut self.series[i].1;
            // tqdm repeats its postfix on every redraw; only changes count.
            if values.back() != Some(value) {
                if values.len() == HISTORY {
                    values.pop_front();
                }
                values.push_back(*value);
            }
            let c = match self.curves.iter().position(|c| c.name == *key) {
                Some(c) => c,
                None => {
                    self.curves.push(Curve::new(key, r.step.is_some()));
                    self.curves.len() - 1
                }
            };
            let curve = &mut self.curves[c];
            // Without steps, a repeat is tqdm redrawing, as above.
            if curve.by_step || curve.latest.map(|p| p.1) != Some(*value) {
                curve.push(r.step, *value);
                self.revision += 1;
            }
        }
        if r.rate.is_some() {
            self.rate.clone_from(&r.rate);
        }
        if r.percent.is_some() {
            self.percent = r.percent;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.series.is_empty() && self.rate.is_none() && self.percent.is_none()
    }

    /// The curves in the order a chart shows them: the main one first.
    pub fn chart_curves(&self) -> Vec<&Curve> {
        let main = self.primary().map(|(k, _)| k.as_str());
        let mut out: Vec<&Curve> = self.curves.iter().filter(|c| Some(c.name.as_str()) == main).collect();
        out.extend(self.curves.iter().filter(|c| Some(c.name.as_str()) != main));
        out
    }

    /// The series to draw: the first with "loss" in its name, else the first.
    fn primary(&self) -> Option<&(String, VecDeque<f64>)> {
        self.series
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase().contains("loss"))
            .or(self.series.first())
    }

    /// Shorter readouts for a prompt line with less room: the main metric
    /// with its sparkline, then the value alone.
    pub fn compact(&self) -> Vec<String> {
        let Some((key, values)) = self.primary() else {
            return Vec::new();
        };
        let Some(&last) = values.back() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if values.len() >= 2 {
            out.push(format!("{key} {} {}", short(last), sparkline(values)));
        }
        out.push(format!("{key} {}", short(last)));
        out
    }

    /// The readout for the prompt line: `loss 0.398 ▇▆▅▄▃▂ · 48.2 it/s · 45%`.
    pub fn describe(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some((key, values)) = self.primary() {
            let last = *values.back()?;
            let mut s = format!("{key} {}", short(last));
            if values.len() >= 2 {
                s.push(' ');
                s.push_str(&sparkline(values));
            }
            parts.push(s);
        }
        if let Some((rate, unit)) = &self.rate {
            parts.push(format!("{} {unit}", short(*rate)));
        }
        if let Some(p) = self.percent.filter(|p| *p < 100.0) {
            parts.push(format!("{p:.0}%"));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

/// A value to about four significant figures, without trailing zeros.
pub fn short(v: f64) -> String {
    let a = v.abs();
    let s = if a != 0.0 && !(1e-3..1e5).contains(&a) {
        format!("{v:.2e}")
    } else if a >= 100.0 {
        format!("{v:.1}")
    } else if a >= 1.0 {
        format!("{v:.3}")
    } else {
        format!("{v:.4}")
    };
    if s.contains('.') && !s.contains('e') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

/// The last values as block characters, low to high.
fn sparkline(values: &VecDeque<f64>) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let tail: Vec<f64> = values.iter().rev().take(SPARK).rev().copied().collect();
    let (lo, hi) = tail.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    let span = hi - lo;
    tail.iter()
        .map(|&v| if span <= 0.0 { BARS[3] } else { BARS[(((v - lo) / span) * 7.0).round() as usize] })
        .collect()
}

/// Splits a raw output stream into plain-text lines: escape sequences are
/// dropped, and a carriage return or newline ends a line (tqdm redraws with
/// `\r`). Bytes from one read may end mid-line; the rest comes with the next.
#[derive(Default)]
pub struct LineSplitter {
    line: Vec<u8>,
    state: Esc,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum Esc {
    #[default]
    Text,
    /// After ESC.
    Esc,
    /// In a CSI, until its final byte.
    Csi,
    /// In an OSC, DCS or similar string, until BEL or ST.
    Str,
    /// ESC inside a string: maybe the start of ST.
    StrEsc,
}

/// Longest line kept; longer ones are cut, since metrics come early.
const MAX_LINE: usize = 512;

impl LineSplitter {
    /// Feed bytes, calling `f` with each line that ends in them.
    pub fn feed(&mut self, bytes: &[u8], mut f: impl FnMut(&str)) {
        for &b in bytes {
            match self.state {
                Esc::Text => match b {
                    0x1B => self.state = Esc::Esc,
                    b'\r' | b'\n' => self.flush(&mut f),
                    0x20..=0xFF if self.line.len() < MAX_LINE => self.line.push(b),
                    _ => {}
                },
                Esc::Esc => {
                    self.state = match b {
                        b'[' => Esc::Csi,
                        b']' | b'P' | b'_' | b'^' | b'X' => Esc::Str,
                        _ => Esc::Text,
                    }
                }
                Esc::Csi => {
                    if (0x40..=0x7E).contains(&b) {
                        self.state = Esc::Text;
                    }
                }
                Esc::Str => match b {
                    0x07 => self.state = Esc::Text,
                    0x1B => self.state = Esc::StrEsc,
                    _ => {}
                },
                Esc::StrEsc => self.state = if b == b'\\' { Esc::Text } else { Esc::Str },
            }
        }
    }

    fn flush(&mut self, f: &mut impl FnMut(&str)) {
        if !self.line.is_empty() {
            f(&String::from_utf8_lossy(&self.line));
            self.line.clear();
        }
    }

    /// The line still being written, which for a progress bar redrawn with
    /// `\r` is the bar as it stands now.
    pub fn pending(&self) -> Option<String> {
        (self.state == Esc::Text && !self.line.is_empty()).then(|| String::from_utf8_lossy(&self.line).into_owned())
    }

    /// Forget a partial line, for when only the end of a big read is scanned.
    pub fn reset(&mut self) {
        self.line.clear();
        self.state = Esc::Text;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_training_logs() {
        let r = read_line("epoch 3/30  loss=0.412  acc: 87.1%  lr=3e-4  step=1200  time: 12:30");
        assert_eq!(r.metrics, vec![("loss".into(), 0.412), ("acc".into(), 87.1), ("lr".into(), 3e-4)]);
        assert_eq!(r.rate, None);
        let r = read_line("{'loss': 0.3981, 'grad_norm': 1.25, 'learning_rate': 2e-05, 'epoch': 0.5}");
        assert_eq!(r.metrics, vec![("loss".into(), 0.3981), ("grad_norm".into(), 1.25)]);
        let r = read_line("val_loss: 0.52 - val_accuracy: 0.81");
        assert_eq!(r.metrics, vec![("val_loss".into(), 0.52), ("val_accuracy".into(), 0.81)]);
    }

    #[test]
    fn reads_tqdm_and_rates() {
        let r = read_line(" 45%|████▌     | 45/100 [00:05<00:06,  8.91it/s, loss=0.412]");
        assert_eq!(r.percent, Some(45.0));
        assert_eq!(r.rate, Some((8.91, "it/s".into())));
        assert_eq!(r.metrics, vec![("loss".into(), 0.412)]);
        assert_eq!(read_line("eval: 512.3 tok/s").rate, Some((512.3, "tok/s".into())));
        assert_eq!(read_line("1.52s/it").rate, Some((1.52, "s/it".into())));
        assert!(read_line("Downloading 45% done").percent.is_none(), "only tqdm's `%|`");
        assert!(read_line("v2 build 3.2 ok").is_empty());
    }

    #[test]
    fn lens_keeps_changes_and_draws_a_sparkline() {
        let mut lens = Lens::default();
        for loss in [0.9, 0.9, 0.7, 0.5, 0.45, 0.4] {
            lens.record(&read_line(&format!("loss={loss} 12.5it/s")));
        }
        assert_eq!(lens.describe().unwrap(), "loss 0.4 █▅▂▂▁ · 12.5 it/s");
        assert_eq!(lens.compact(), ["loss 0.4 █▅▂▂▁", "loss 0.4"]);
    }

    #[test]
    fn primary_is_the_loss() {
        let mut lens = Lens::default();
        lens.record(&read_line("acc=0.5 loss=2.0"));
        lens.record(&read_line("acc=0.6 loss=1.0"));
        assert!(lens.describe().unwrap().starts_with("loss 1 "));
    }

    #[test]
    fn splitter_drops_escapes_and_splits_on_cr() {
        let mut s = LineSplitter::default();
        let mut lines = Vec::new();
        s.feed(b"\x1b[32m 10%|\x1b[0m bar | loss=1.0\r 20%|bar| lo", |l| lines.push(l.to_string()));
        s.feed(b"ss=0.9\r\n\x1b]0;title\x1b\\done\n", |l| lines.push(l.to_string()));
        assert_eq!(lines, [" 10%| bar | loss=1.0", " 20%|bar| loss=0.9", "done"]);
    }

    #[test]
    fn numbers_print_short() {
        assert_eq!(short(0.41230), "0.4123");
        assert_eq!(short(87.1), "87.1");
        assert_eq!(short(3e-4), "3.00e-4");
        assert_eq!(short(1234.0), "1234");
    }

    #[test]
    fn steps_from_lines() {
        assert_eq!(read_line("step=120 loss=0.41").step, Some(120.0));
        assert_eq!(read_line("Step 120/1000 | loss: 0.41").step, Some(120.0));
        assert_eq!(read_line("[iter 300] train_loss=2.1").step, Some(300.0));
        assert_eq!(read_line("{'loss': 2.31, 'learning_rate': 5e-05, 'epoch': 0.5}").step, Some(0.5));
        assert_eq!(read_line("epoch 3 global_step 4500 loss=1.2").step, Some(4500.0), "a step beats an epoch");
        assert_eq!(read_line("steps_per_epoch=100 loss=1").step, None);
        assert_eq!(read_line("step=5").step, None, "no metric on the line");
    }

    #[test]
    fn curves_keep_the_whole_run() {
        let mut lens = Lens::default();
        for i in 0..5000 {
            lens.record(&read_line(&format!("step={i} loss={}", 1.0 / (i + 1) as f64)));
        }
        let c = &lens.curves[0];
        let p = c.points();
        assert!(p.len() <= CURVE_POINTS + 1, "{}", p.len());
        assert_eq!(p[0], (0.0, 1.0), "from the start");
        assert_eq!(*p.last().unwrap(), (4999.0, 1.0 / 5000.0), "to the newest");
        assert!(p.windows(2).all(|w| w[0].0 < w[1].0));
        // Evenly: the gaps are all the same size, bar the newest.
        let gap = p[1].0 - p[0].0;
        assert!(p[..p.len() - 1].windows(2).all(|w| w[1].0 - w[0].0 == gap));
        let (a, b) = (c.at(8.0).unwrap(), c.at(12.0).unwrap());
        let mid = c.at(10.0).unwrap();
        assert!(mid < a && mid > b, "inside: between its neighbours");
        assert!(c.at(1e9).is_none(), "past the end");
    }

    #[test]
    fn curves_without_steps_count_values() {
        let mut lens = Lens::default();
        for line in ["loss=3.0", "loss=3.0", "loss=2.0", "acc=0.5", "loss=1.0"] {
            lens.record(&read_line(line));
        }
        let loss = lens.chart_curves()[0];
        assert_eq!(loss.name, "loss");
        assert_eq!(loss.points(), vec![(1.0, 3.0), (2.0, 2.0), (3.0, 1.0)], "a repeat is a redraw");
        assert_eq!(loss.at(2.5), Some(1.5));
    }
}
