//! Box drawing (U+2500-U+257F) and block elements (U+2580-U+259F), drawn as
//! rectangles instead of font glyphs.
//!
//! Fonts get these wrong in a terminal: SF Mono has none of them, and the
//! fallback font's line weights and vertical metrics differ from the cell, so
//! table borders break apart and tqdm's bars sit at the wrong height. Drawn
//! from the cell's own geometry they always meet edge to edge. Ghostty and
//! kitty do the same.

/// One rectangle in pixels, with the coverage to draw it at (shades are < 1).
pub type Piece = ([f32; 4], f32);

/// Line weight per arm, in the order up, right, down, left.
/// 0 none, 1 light, 2 heavy, 3 double.
const BOX: [&str; 128] = [
    // 2500
    "0101", "0202", "1010", "2020", "0101", "0202", "1010", "2020",
    "0101", "0202", "1010", "2020", "0110", "0210", "0120", "0220",
    // 2510
    "0011", "0012", "0021", "0022", "1100", "1200", "2100", "2200",
    "1001", "1002", "2001", "2002", "1110", "1210", "2110", "1120",
    // 2520
    "2120", "2210", "1220", "2220", "1011", "1012", "2011", "1021",
    "2021", "2012", "1022", "2022", "0111", "0112", "0211", "0212",
    // 2530
    "0121", "0122", "0221", "0222", "1101", "1102", "1201", "1202",
    "2101", "2102", "2201", "2202", "1111", "1112", "1211", "1212",
    // 2540
    "2111", "1121", "2121", "2112", "2211", "1122", "1221", "2212",
    "1222", "2122", "2221", "2222", "0101", "0202", "1010", "2020",
    // 2550
    "0303", "3030", "0310", "0130", "0330", "0013", "0031", "0033",
    "1300", "3100", "3300", "1003", "3001", "3003", "1310", "3130",
    // 2560
    "3330", "1013", "3031", "3033", "0313", "0131", "0333", "1303",
    "3101", "3303", "1313", "3131", "3333", "0110", "0011", "1001",
    // 2570: rounded corner, three diagonals (left to the font), half lines
    "1100", "", "", "", "0001", "1000", "0100", "0010",
    "0002", "2000", "0200", "0020", "0201", "1020", "0102", "2010",
];

/// Rectangles for `ch` in the cell at (x, y) of size (w, h), or `None` if the
/// character is not one we draw. `light` is the light stroke width in pixels.
pub fn pieces(ch: char, x: f32, y: f32, w: f32, h: f32, light: f32) -> Option<Vec<Piece>> {
    let c = ch as u32;
    match c {
        0x2500..=0x257F => box_lines(BOX[(c - 0x2500) as usize], x, y, w, h, light),
        0x2580..=0x259F => Some(block(c, x, y, w, h)),
        _ => None,
    }
}

fn box_lines(spec: &str, x: f32, y: f32, w: f32, h: f32, light: f32) -> Option<Vec<Piece>> {
    if spec.is_empty() {
        return None;
    }
    let arms: Vec<u8> = spec.bytes().map(|b| b - b'0').collect();
    let (up, right, down, left) = (arms[0], arms[1], arms[2], arms[3]);
    let heavy = (light * 2.0).round();
    let gap = light; // between the two strokes of a double line

    // Stroke centre, snapped so a light line lands on whole pixels.
    let cx = (x + (w - light) / 2.0).round();
    let cy = (y + (h - light) / 2.0).round();

    // Where a stroke of each weight sits, as (start, end) offsets from the
    // centre line. A double line is two light strokes either side of the
    // centre with a light-width gap between them.
    let span = |weight: u8| -> (f32, f32) {
        match weight {
            2 => ((light - heavy) / 2.0, (light + heavy) / 2.0),
            3 => (-gap, light + gap),
            _ => (0.0, light),
        }
    };
    // The perpendicular strokes an arm must reach across so the joint is
    // solid. With no perpendicular stroke the arms meet across the centre.
    let widest = |a: u8, b: u8| {
        let (s1, e1) = span(a);
        let (s2, e2) = span(b);
        (s1.min(s2), e1.max(e2))
    };
    let (v_lo, v_hi) = widest(up, down);
    let (h_lo, h_hi) = widest(left, right);

    let strokes = |weight: u8, centre: f32| -> Vec<(f32, f32)> {
        match weight {
            0 => vec![],
            3 => vec![(centre - gap, light), (centre + gap, light)],
            w => {
                let (s, e) = span(w);
                vec![(centre + s, e - s)]
            }
        }
    };

    let mut out = Vec::with_capacity(8);
    for (x0, x1, weight) in [(x, cx + v_hi, left), (cx + v_lo, x + w, right)] {
        for (pos, t) in strokes(weight, cy) {
            out.push(([x0, pos, x1 - x0, t], 1.0));
        }
    }
    for (y0, y1, weight) in [(y, cy + h_hi, up), (cy + h_lo, y + h, down)] {
        for (pos, t) in strokes(weight, cx) {
            out.push(([pos, y0, t, y1 - y0], 1.0));
        }
    }

    // Drop zero-size pieces left by a missing arm.
    out.retain(|(r, _)| r[2] > 0.0 && r[3] > 0.0);
    Some(out)
}

fn block(c: u32, x: f32, y: f32, w: f32, h: f32) -> Vec<Piece> {
    let eighth_h = |n: u32| (h * n as f32 / 8.0).round();
    let eighth_w = |n: u32| (w * n as f32 / 8.0).round();
    let hw = (w / 2.0).round();
    let hh = (h / 2.0).round();
    let full = 1.0;
    let quad = |ul: bool, ur: bool, ll: bool, lr: bool| {
        let mut v = Vec::new();
        if ul {
            v.push(([x, y, hw, hh], full));
        }
        if ur {
            v.push(([x + hw, y, w - hw, hh], full));
        }
        if ll {
            v.push(([x, y + hh, hw, h - hh], full));
        }
        if lr {
            v.push(([x + hw, y + hh, w - hw, h - hh], full));
        }
        v
    };
    match c {
        0x2580 => vec![([x, y, w, hh], full)],
        0x2581..=0x2587 => {
            let t = eighth_h(c - 0x2580);
            vec![([x, y + h - t, w, t], full)]
        }
        0x2588 => vec![([x, y, w, h], full)],
        0x2589..=0x258F => vec![([x, y, eighth_w(0x2590 - c), h], full)],
        0x2590 => vec![([x + hw, y, w - hw, h], full)],
        0x2591 => vec![([x, y, w, h], 0.25)],
        0x2592 => vec![([x, y, w, h], 0.5)],
        0x2593 => vec![([x, y, w, h], 0.75)],
        0x2594 => vec![([x, y, w, eighth_h(1)], full)],
        0x2595 => {
            let t = eighth_w(1);
            vec![([x + w - t, y, t, h], full)]
        }
        0x2596 => quad(false, false, true, false),
        0x2597 => quad(false, false, false, true),
        0x2598 => quad(true, false, false, false),
        0x2599 => quad(true, false, true, true),
        0x259A => quad(true, false, false, true),
        0x259B => quad(true, true, true, false),
        0x259C => quad(true, true, false, true),
        0x259D => quad(false, true, false, false),
        0x259E => quad(false, true, true, false),
        _ => quad(false, true, true, true), // 0x259F
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(p: &[Piece]) -> (f32, f32, f32, f32) {
        let x0 = p.iter().map(|(r, _)| r[0]).fold(f32::MAX, f32::min);
        let y0 = p.iter().map(|(r, _)| r[1]).fold(f32::MAX, f32::min);
        let x1 = p.iter().map(|(r, _)| r[0] + r[2]).fold(f32::MIN, f32::max);
        let y1 = p.iter().map(|(r, _)| r[1] + r[3]).fold(f32::MIN, f32::max);
        (x0, y0, x1, y1)
    }

    #[test]
    fn horizontal_line_spans_the_cell_so_neighbours_join() {
        let p = pieces('─', 10.0, 20.0, 16.0, 34.0, 2.0).unwrap();
        let (x0, _, x1, _) = bounds(&p);
        assert_eq!((x0, x1), (10.0, 26.0));
    }

    #[test]
    fn lines_have_no_gap_at_the_centre() {
        for ch in ['─', '━', '═', '│', '┼', '╋', '╬'] {
            let p = pieces(ch, 0.0, 0.0, 16.0, 34.0, 2.0).unwrap();
            // Every horizontal piece must touch or overlap another, so the
            // stroke is continuous from edge to edge.
            let mut spans: Vec<(f32, f32, f32)> =
                p.iter().filter(|(r, _)| r[2] > r[3]).map(|(r, _)| (r[1], r[0], r[0] + r[2])).collect();
            spans.sort_by(|a, b| a.partial_cmp(b).unwrap());
            for pair in spans.windows(2) {
                if pair[0].0 == pair[1].0 {
                    assert!(pair[1].1 <= pair[0].2, "{ch}: gap between {:?} and {:?}", pair[0], pair[1]);
                }
            }
        }
    }

    #[test]
    fn vertical_line_spans_the_row() {
        let p = pieces('│', 0.0, 0.0, 16.0, 34.0, 2.0).unwrap();
        let (_, y0, _, y1) = bounds(&p);
        assert_eq!((y0, y1), (0.0, 34.0));
    }

    #[test]
    fn corner_reaches_right_and_bottom_edges_only() {
        let p = pieces('┌', 0.0, 0.0, 16.0, 34.0, 2.0).unwrap();
        let (x0, y0, x1, y1) = bounds(&p);
        assert!(x0 > 0.0 && y0 > 0.0);
        assert_eq!((x1, y1), (16.0, 34.0));
    }

    #[test]
    fn lower_blocks_sit_on_the_cell_bottom() {
        for ch in ['▁', '▄', '▇', '█'] {
            let p = pieces(ch, 0.0, 0.0, 16.0, 32.0, 2.0).unwrap();
            let (_, _, _, y1) = bounds(&p);
            assert_eq!(y1, 32.0, "{ch}");
        }
    }

    #[test]
    fn diagonals_are_left_to_the_font() {
        assert!(pieces('╱', 0.0, 0.0, 16.0, 32.0, 2.0).is_none());
        assert!(pieces('a', 0.0, 0.0, 16.0, 32.0, 2.0).is_none());
    }
}
