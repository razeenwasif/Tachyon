//! The training chart: a panel over a pane with a command's metrics, each in
//! a plot of its own, this run's curve solid and the last run's (of the same
//! command, numbers in its command line aside) dashed beside it. It follows
//! the run live.
//!
//! The panel is laid out in cells, so its labels are terminal text drawn by
//! the renderer; the plots themselves are drawn by Core Graphics into a
//! picture of the panel, made again only when a curve or the panel changes.

use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    kCGColorSpaceSRGB, CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo, CGLineCap, CGLineJoin,
};

use super::images::Pixels;
use crate::term::{lens, CommandState, Term};

/// Series colours, one per plot: distinct on dark and light backgrounds.
pub const COLORS: [[u8; 3]; 8] = [
    [76, 155, 232],
    [242, 142, 43],
    [89, 181, 99],
    [200, 120, 210],
    [225, 87, 89],
    [118, 183, 178],
    [237, 201, 72],
    [255, 157, 167],
];

/// Plots a panel shows at most.
const MAX_PLOTS: usize = 4;

pub struct Plot {
    pub name: String,
    pub current: Vec<(f64, f64)>,
    pub previous: Option<Vec<(f64, f64)>>,
    pub by_step: bool,
}

/// What a chart shows: one command's run.
pub struct Chart {
    pub cmd: u32,
    pub title: String,
    pub status: String,
    /// The last run's command line, when it isn't this one's.
    pub previous_cmdline: Option<String>,
    pub has_previous: bool,
    pub plots: Vec<Plot>,
    pub revision: u64,
}

/// Command `cmd`'s chart, or `None` if there's no such command.
pub fn chart_for(term: &Term, cmd: u32) -> Option<Chart> {
    let info = term.command(cmd)?;
    let previous = info.previous.as_ref();
    let plots: Vec<Plot> = info
        .lens
        .chart_curves()
        .into_iter()
        .take(MAX_PLOTS)
        .map(|c| Plot {
            name: c.name.clone(),
            current: c.points(),
            previous: previous.and_then(|p| p.curves.iter().find(|pc| pc.name == c.name)).map(|pc| pc.points()),
            by_step: c.by_step(),
        })
        .collect();
    let status = match info.state {
        CommandState::Running => "running".to_string(),
        CommandState::Done => match (info.elapsed(), info.exit) {
            (Some(e), Some(0)) => format!("finished in {}", duration(e.as_secs_f64())),
            (Some(e), Some(code)) => format!("failed ({code}) after {}", duration(e.as_secs_f64())),
            (Some(e), None) => format!("ended after {}", duration(e.as_secs_f64())),
            _ => "finished".to_string(),
        },
        CommandState::Prompt => "not started".to_string(),
    };
    let title = info.cmdline.clone().unwrap_or_else(|| "this command".to_string());
    let has_previous = plots.iter().any(|p| p.previous.is_some());
    let previous_cmdline = previous.filter(|p| has_previous && p.cmdline.trim() != title.trim()).map(|p| p.cmdline.clone());
    Some(Chart { cmd, title, status, previous_cmdline, has_previous, plots, revision: info.lens.revision })
}

fn duration(s: f64) -> String {
    let s = s.round() as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, s % 3600 / 60),
    }
}

/// The panel's place in a pane of `cols` by `rows` cells: (column, row,
/// columns, rows), at the top right.
pub fn panel_rect(cols: usize, rows: usize) -> (usize, usize, usize, usize) {
    // At least 44 by 14 cells where the pane has them, and a cell short of
    // its edges.
    let fit = |n: usize, share: f32, least: usize| {
        let most = n.saturating_sub(2).max(1);
        ((n as f32 * share) as usize).clamp(least.min(most), most)
    };
    let (w, h) = (fit(cols, 0.62, 44), fit(rows, 0.6, 14));
    (cols.saturating_sub(w + 1), 1.min(rows.saturating_sub(h)), w, h)
}

/// A plot's place in the panel, in cells: the label row, then the plot
/// area (with a gutter of `gutter` columns for the values on its left),
/// then a row for the steps under it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PlotBox {
    col: usize,
    row: usize,
    cols: usize,
    rows: usize,
    gutter: usize,
}

impl PlotBox {
    /// The plot area's rows: between the label row and the steps row.
    fn area_rows(&self) -> std::ops::Range<usize> {
        self.row + 1..self.row + self.rows - 1
    }
}

/// The ranges a plot is drawn over: its steps, and its values with a
/// little room above and below.
#[derive(Clone, Copy, Debug)]
struct Ranges {
    x: (f64, f64),
    y: (f64, f64),
}

fn ranges(p: &Plot) -> Option<Ranges> {
    let all = p.current.iter().chain(p.previous.iter().flatten());
    let (mut x0, mut x1, mut y0, mut y1) = (f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY);
    for &(x, y) in all {
        x0 = x0.min(x);
        x1 = x1.max(x);
        y0 = y0.min(y);
        y1 = y1.max(y);
    }
    if !x0.is_finite() || !y0.is_finite() {
        return None;
    }
    if x1 <= x0 {
        x1 = x0 + 1.0;
    }
    let pad = if y1 > y0 { (y1 - y0) * 0.06 } else { y0.abs().max(1.0) * 0.1 };
    // Values that are never negative (a loss, an accuracy) don't get an
    // axis that goes below zero.
    let low = if y0 >= 0.0 { (y0 - pad).max(0.0) } else { y0 - pad };
    Some(Ranges { x: (x0, x1), y: (low, y1 + pad) })
}

/// The panel's layout: where each plot goes.
struct Layout {
    boxes: Vec<PlotBox>,
}

/// Rows above the plots: the command line, then how it's going.
const HEADER: usize = 3;

fn layout(chart: &Chart, cols: usize, rows: usize) -> Layout {
    let n = chart.plots.len();
    let (x0, y0) = (1usize, HEADER);
    let (w, h) = (cols.saturating_sub(2), rows.saturating_sub(HEADER + 1));
    let (across, down) = match n {
        0 => (0, 0),
        1 => (1, 1),
        2 if w >= 70 => (2, 1),
        2 => (1, 2),
        _ => (2, 2),
    };
    let mut boxes = Vec::new();
    for i in 0..n {
        let (cx, cy) = (i % across.max(1), i / across.max(1));
        let bw = w / across.max(1);
        let bh = h / down.max(1);
        let gutter = gutter_for(chart.plots.get(i));
        boxes.push(PlotBox { col: x0 + cx * bw, row: y0 + cy * bh, cols: bw.saturating_sub(1), rows: bh, gutter });
    }
    Layout { boxes }
}

/// Columns for a plot's value labels, plus a space.
fn gutter_for(p: Option<&Plot>) -> usize {
    let Some(r) = p.and_then(ranges) else { return 2 };
    [r.y.0, r.y.1, (r.y.0 + r.y.1) / 2.0].iter().map(|v| lens::short(*v).chars().count()).max().unwrap_or(1).min(9) + 1
}

/// A label to draw, at a column and row of the panel.
pub struct Label {
    pub col: usize,
    pub row: usize,
    pub text: String,
    pub color: [u8; 3],
}

/// The panel's colours, from the theme.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Colors {
    pub bg: [u8; 3],
    pub fg: [u8; 3],
}

impl Colors {
    fn mix(&self, t: f32) -> [u8; 3] {
        std::array::from_fn(|i| (self.bg[i] as f32 + (self.fg[i] as f32 - self.bg[i] as f32) * t).round() as u8)
    }
    pub fn panel(&self) -> [u8; 3] {
        self.mix(0.06)
    }
    pub fn dim(&self) -> [u8; 3] {
        self.mix(0.55)
    }
}

/// Text cut to `n` columns, with an ellipsis if it was longer.
fn fit(text: &str, n: usize) -> String {
    if text.chars().count() <= n {
        return text.to_string();
    }
    let mut s: String = text.chars().take(n.saturating_sub(1)).collect();
    s.push('…');
    s
}

/// The panel's text: its header, and each plot's name, values and steps.
pub fn labels(chart: Option<&Chart>, cols: usize, rows: usize, colors: Colors) -> Vec<Label> {
    let dim = colors.dim();
    let mut out = Vec::new();
    let close = "⌥⌘C or Esc closes";
    let inner = cols.saturating_sub(2);
    let Some(chart) = chart else {
        out.push(Label { col: 1, row: 0, text: fit("Training chart", inner), color: colors.fg });
        out.push(Label { col: 1, row: 1, text: fit("No metrics yet: a command that prints loss=…, acc: … or 'epoch': … fills this in, live.", inner), color: dim });
        out.push(Label { col: 1, row: rows.saturating_sub(1).max(2), text: fit(close, inner), color: dim });
        return out;
    };
    let title_room = inner.saturating_sub(close.chars().count() + 2);
    out.push(Label { col: 1, row: 0, text: fit(&chart.title, title_room), color: colors.fg });
    if cols > close.chars().count() + 4 {
        out.push(Label { col: cols - 1 - close.chars().count(), row: 0, text: close.into(), color: dim });
    }
    let mut status = chart.status.clone();
    if chart.has_previous {
        status.push_str("  ── this run  ╌╌ last run");
        if let Some(prev) = &chart.previous_cmdline {
            status.push_str(&format!(" ({prev})"));
        }
    }
    out.push(Label { col: 1, row: 1, text: fit(&status, inner), color: dim });
    if chart.plots.is_empty() {
        out.push(Label { col: 1, row: HEADER, text: fit("No metrics yet.", inner), color: dim });
        return out;
    }
    let l = layout(chart, cols, rows);
    for (i, (p, b)) in chart.plots.iter().zip(&l.boxes).enumerate() {
        let color = COLORS[i % COLORS.len()];
        let Some(r) = ranges(p) else { continue };
        let Some(&(lx, ly)) = p.current.last() else { continue };
        // The name and the latest value, and the last run's at that step.
        let head = format!("{} {}", p.name, lens::short(ly));
        let width = head.chars().count();
        out.push(Label { col: b.col, row: b.row, text: fit(&head, b.cols), color });
        if let Some(prev) = p.previous.as_ref().and_then(|pp| at(pp, lx)) {
            let then = format!("  last run {}", lens::short(prev));
            if width + then.chars().count() <= b.cols {
                out.push(Label { col: b.col + width, row: b.row, text: then, color: dim });
            }
        }
        // Values at the top, middle and bottom of the area.
        let rows_ = b.area_rows();
        let (top, bottom) = (rows_.start, rows_.end.saturating_sub(1));
        let mid = (top + bottom) / 2;
        for (row, frac) in [(top, 1.0), (mid, 0.5), (bottom, 0.0)] {
            if bottom <= top && row != top {
                continue;
            }
            let v = value_at_row(r, top, bottom, row, frac);
            let s = lens::short(v);
            let col = b.col + b.gutter.saturating_sub(s.chars().count() + 1);
            out.push(Label { col, row, text: s, color: dim });
        }
        // Steps under it, first and last.
        let steps_row = b.row + b.rows - 1;
        let unit = if p.by_step { "step " } else { "#" };
        let first = format!("{unit}{}", lens::short(r.x.0));
        let last = lens::short(r.x.1);
        out.push(Label { col: b.col + b.gutter, row: steps_row, text: first.clone(), color: dim });
        let last_col = (b.col + b.cols).saturating_sub(last.chars().count());
        if last_col > b.col + b.gutter + first.chars().count() {
            out.push(Label { col: last_col, row: steps_row, text: last, color: dim });
        }
    }
    out
}

/// The value a row of a plot's area stands for: the area runs from the
/// middle of its top row (the highest value) to the middle of its bottom
/// row (the lowest).
fn value_at_row(r: Ranges, top: usize, bottom: usize, row: usize, frac: f64) -> f64 {
    if bottom <= top {
        return r.y.0 + (r.y.1 - r.y.0) * frac;
    }
    let t = (bottom - row) as f64 / (bottom - top) as f64;
    r.y.0 + (r.y.1 - r.y.0) * t
}

/// A curve's value at step `x`, between its points either side.
fn at(points: &[(f64, f64)], x: f64) -> Option<f64> {
    let i = points.iter().position(|&(px, _)| px >= x)?;
    if i == 0 {
        return (points[0].0 == x).then_some(points[0].1);
    }
    let ((x0, y0), (x1, y1)) = (points[i - 1], points[i]);
    Some(if x1 > x0 { y0 + (y1 - y0) * (x - x0) / (x1 - x0) } else { y1 })
}

/// The panel drawn: background, border, and each plot's grid lines and
/// curves, at `cell` pixels a cell.
pub fn pixels(chart: Option<&Chart>, cols: usize, rows: usize, cell: (f32, f32), scale: f32, colors: Colors) -> Option<Pixels> {
    let (cw, ch) = (cell.0 as f64, cell.1 as f64);
    let (w, h) = ((cols as f64 * cw).round().max(1.0) as usize, (rows as f64 * ch).round().max(1.0) as usize);
    let mut data = vec![0u8; w * h * 4];
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    let ctx = unsafe { CGBitmapContextCreate(data.as_mut_ptr().cast(), w, h, 8, w * 4, Some(&space), CGImageAlphaInfo::PremultipliedLast.0) }?;
    let c = Some(&*ctx);
    // y down, like the grid.
    CGContext::translate_ctm(c, 0.0, h as f64);
    CGContext::scale_ctm(c, 1.0, -1.0);
    let rgb = |v: [u8; 3]| (v[0] as f64 / 255.0, v[1] as f64 / 255.0, v[2] as f64 / 255.0);
    let fill = |v: [u8; 3], a: f64| {
        let (r, g, b) = rgb(v);
        CGContext::set_rgb_fill_color(c, r, g, b, a);
    };
    let stroke = |v: [u8; 3], a: f64| {
        let (r, g, b) = rgb(v);
        CGContext::set_rgb_stroke_color(c, r, g, b, a);
    };
    let s = scale as f64;
    // Background and a hairline border.
    fill(colors.panel(), 1.0);
    CGContext::fill_rect(c, CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64)));
    stroke(colors.mix(0.25), 1.0);
    CGContext::stroke_rect_with_width(c, CGRect::new(CGPoint::new(0.5 * s, 0.5 * s), CGSize::new(w as f64 - s, h as f64 - s)), s);

    let Some(chart) = chart else {
        drop(ctx);
        return Some(Pixels { width: w as u32, height: h as u32, data });
    };
    let l = layout(chart, cols, rows);
    CGContext::set_line_join(c, CGLineJoin::Round);
    CGContext::set_line_cap(c, CGLineCap::Round);
    for (i, (p, b)) in chart.plots.iter().zip(&l.boxes).enumerate() {
        let Some(r) = ranges(p) else { continue };
        let color = COLORS[i % COLORS.len()];
        let rows_ = b.area_rows();
        let (top, bottom) = (rows_.start, rows_.end.saturating_sub(1).max(rows_.start));
        // The area: from the gutter to the box's right edge, and from the
        // middle of its top row to the middle of its bottom one.
        let left = (b.col + b.gutter) as f64 * cw + cw * 0.5;
        let right = (b.col + b.cols) as f64 * cw - cw * 0.5;
        let y_top = top as f64 * ch + ch * 0.5;
        let y_bottom = if bottom > top { bottom as f64 * ch + ch * 0.5 } else { y_top + ch };
        if right <= left {
            continue;
        }
        let px = |x: f64| left + (x - r.x.0) / (r.x.1 - r.x.0) * (right - left);
        let py = |y: f64| y_bottom - (y - r.y.0) / (r.y.1 - r.y.0) * (y_bottom - y_top);
        // Grid lines at the labelled values.
        stroke(colors.mix(0.14), 1.0);
        CGContext::set_line_width(c, s);
        for y in [y_top, (y_top + y_bottom) / 2.0, y_bottom] {
            CGContext::begin_path(c);
            CGContext::move_to_point(c, left, y.round() + 0.5 * s);
            CGContext::add_line_to_point(c, right, y.round() + 0.5 * s);
            CGContext::stroke_path(c);
        }
        CGContext::save_g_state(c);
        CGContext::clip_to_rect(c, CGRect::new(CGPoint::new(left - 3.0 * s, y_top - 3.0 * s), CGSize::new(right - left + 6.0 * s, y_bottom - y_top + 6.0 * s)));
        let path = |pts: &[(f64, f64)]| {
            CGContext::begin_path(c);
            for (k, &(x, y)) in thin(pts, (right - left) as usize).iter().enumerate() {
                if k == 0 {
                    CGContext::move_to_point(c, px(x), py(y));
                } else {
                    CGContext::add_line_to_point(c, px(x), py(y));
                }
            }
            CGContext::stroke_path(c);
        };
        // The last run, dashed and fainter, under this one.
        if let Some(prev) = &p.previous {
            stroke(color, 0.5);
            CGContext::set_line_width(c, 1.5 * s);
            let dash: [CGFloat; 2] = [5.0 * s, 4.0 * s];
            unsafe { CGContext::set_line_dash(c, 0.0, dash.as_ptr(), 2) };
            path(prev);
            unsafe { CGContext::set_line_dash(c, 0.0, std::ptr::null(), 0) };
        }
        stroke(color, 1.0);
        CGContext::set_line_width(c, 2.0 * s);
        path(&p.current);
        // A dot on the newest value.
        if let Some(&(x, y)) = p.current.last() {
            fill(color, 1.0);
            let d = 3.0 * s;
            CGContext::begin_path(c);
            CGContext::add_rect(c, CGRect::new(CGPoint::new(px(x) - d, py(y) - d), CGSize::new(2.0 * d, 2.0 * d)));
            CGContext::fill_path(c);
        }
        CGContext::restore_g_state(c);
    }
    drop(ctx);
    Some(Pixels { width: w as u32, height: h as u32, data })
}

/// At most about two points a pixel column: a long curve drawn whole is
/// slow and no sharper. Each bucket keeps its lowest and highest values, in
/// order, so spikes survive.
fn thin(points: &[(f64, f64)], width: usize) -> Vec<(f64, f64)> {
    let buckets = width.max(1);
    if points.len() <= 2 * buckets {
        return points.to_vec();
    }
    let per = points.len().div_ceil(buckets);
    let mut out = Vec::with_capacity(2 * buckets + 1);
    for chunk in points.chunks(per) {
        let lo = chunk.iter().min_by(|a, b| a.1.total_cmp(&b.1)).copied().expect("non-empty");
        let hi = chunk.iter().max_by(|a, b| a.1.total_cmp(&b.1)).copied().expect("non-empty");
        if lo.0 <= hi.0 {
            out.push(lo);
            out.push(hi);
        } else {
            out.push(hi);
            out.push(lo);
        }
    }
    if let Some(&last) = points.last() {
        out.push(last);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chart(n: usize, previous: bool) -> Chart {
        let curve: Vec<(f64, f64)> = (0..100).map(|i| (i as f64, 2.0 / (1.0 + i as f64))).collect();
        Chart {
            cmd: 1,
            title: "python train.py --lr 1e-3".into(),
            status: "running".into(),
            previous_cmdline: previous.then(|| "python train.py --lr 3e-4".into()),
            has_previous: previous,
            plots: (0..n)
                .map(|i| Plot {
                    name: ["loss", "acc", "lr", "grad_norm"][i].into(),
                    current: curve.clone(),
                    previous: previous.then(|| curve.iter().map(|&(x, y)| (x, y * 1.1)).collect()),
                    by_step: true,
                })
                .collect(),
            revision: 1,
        }
    }

    #[test]
    fn panels_fit_their_pane() {
        assert_eq!(panel_rect(100, 40), (37, 1, 62, 24));
        let (c, r, w, h) = panel_rect(30, 10);
        assert!(c + w <= 30 && r + h <= 10 && w >= 1 && h >= 1);
    }

    #[test]
    fn plots_share_the_panel() {
        let one = layout(&chart(1, false), 62, 24);
        assert_eq!(one.boxes.len(), 1);
        let four = layout(&chart(4, false), 80, 30);
        assert_eq!(four.boxes.len(), 4);
        // Side by side, then below, none overlapping or past the panel.
        assert!(four.boxes[1].col > four.boxes[0].col && four.boxes[2].row > four.boxes[0].row);
        assert!(four.boxes.iter().all(|b| b.col + b.cols <= 79 && b.row + b.rows <= 30));
    }

    #[test]
    fn labels_say_what_the_plots_show() {
        let colors = Colors { bg: [0, 0, 0], fg: [255, 255, 255] };
        let l = labels(Some(&chart(1, true)), 90, 24, colors);
        let text: Vec<&str> = l.iter().map(|l| l.text.as_str()).collect();
        assert!(text[0].starts_with("python train.py"));
        assert!(text.iter().any(|t| t.contains("last run (python train.py --lr 3e-4)")));
        assert!(text.contains(&"loss 0.02"), "{text:?}");
        assert!(text.iter().any(|t| t.contains("last run 0.022")), "at the same step");
        assert!(text.contains(&"0"), "a loss's axis stops at zero: {text:?}");
        assert!(text.contains(&"step 0") && text.contains(&"99"));
        // Every label stays inside the panel.
        assert!(l.iter().all(|l| l.col + l.text.chars().count() <= 90 && l.row < 24), "{text:?}");
        let narrow = labels(Some(&chart(4, true)), 44, 14, colors);
        assert!(narrow.iter().all(|l| l.col + l.text.chars().count() <= 44 && l.row < 14));
        let empty = labels(None, 62, 24, colors);
        assert!(empty[1].text.starts_with("No metrics yet"));
    }

    #[test]
    fn drawing_makes_a_picture_of_the_panel() {
        let colors = Colors { bg: [0, 0, 0], fg: [255, 255, 255] };
        let px = pixels(Some(&chart(2, true)), 62, 24, (14.0, 28.0), 2.0, colors).unwrap();
        assert_eq!((px.width, px.height), (62 * 14, 24 * 28));
        // Some pixel has the first series' colour, drawn as a line.
        let blue = px.data.chunks_exact(4).any(|p| p[0] == COLORS[0][0] && p[1] == COLORS[0][1] && p[2] == COLORS[0][2]);
        assert!(blue);
    }

    #[test]
    fn long_curves_are_thinned_keeping_spikes() {
        let mut pts: Vec<(f64, f64)> = (0..10_000).map(|i| (i as f64, 1.0)).collect();
        pts[5000].1 = 50.0;
        let t = thin(&pts, 300);
        assert!(t.len() <= 601 + 1);
        assert!(t.iter().any(|p| p.1 == 50.0));
        assert_eq!(t.last(), pts.last());
    }

    /// Writes a chart's picture as raw RGBA to `TACHYON_CHART_DUMP`, to look at.
    #[test]
    #[ignore]
    fn dump_picture() {
        let Some(path) = std::env::var_os("TACHYON_CHART_DUMP") else { return };
        let colors = Colors { bg: [16, 17, 22], fg: [220, 223, 228] };
        let mut c = chart(4, true);
        for (i, p) in c.plots.iter_mut().enumerate() {
            let noisy = |k: usize, f: f64| (0..300).map(move |j| (j as f64 * 10.0, f * (2.5 / (1.0 + j as f64 / 30.0) + 0.05 * ((j * 7919 + k) % 13) as f64 / 13.0)));
            p.current = noisy(i, 1.0).take(220).collect();
            p.previous = Some(noisy(i + 5, 1.15).collect());
        }
        let px = pixels(Some(&c), 90, 30, (14.0, 28.0), 2.0, colors).unwrap();
        std::fs::write(path, &px.data).unwrap();
        for l in labels(Some(&c), 90, 30, colors) {
            println!("{} {} {}", l.col, l.row, l.text);
        }
    }
}
