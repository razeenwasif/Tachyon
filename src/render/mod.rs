//! Frame construction: terminal state in, draw lists out.
//!
//! Two ideas do most of the work here.
//!
//! **Background run-length coalescing.** A terminal screen is mostly one
//! background colour. Emitting one quad per cell would mean tens of thousands
//! of quads that overdraw each other; instead each row is scanned for runs of
//! equal background and emitted as a single wide rectangle. A typical screen
//! collapses to a handful of rectangles per row, and a plain shell prompt to
//! roughly one.
//!
//! **Nothing is drawn for blank cells.** The glyph list only receives cells
//! that actually have ink, so whitespace -- most of the screen, most of the
//! time -- costs zero vertices and zero fragments.
//!
//! The result is that a full 200x50 screen of text is three draw calls and a
//! few thousand quads.

pub mod atlas;
pub mod d3d;
pub mod font;

use windows::core::Result;
use windows::Win32::Foundation::HWND;

use crate::config::{Antialias, Config};
use crate::term::{Cell, CellFlags, CursorShape, Palette, Term, TermMode};
use atlas::{Atlas, GlyphKey};
use d3d::{GlyphInstance, Globals, Gpu, RectInstance};
use font::{CellMetrics, FontSet};

/// An inclusive text range, addressed by absolute line (0 = oldest scrollback)
/// and column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SelectionRange {
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
}

impl SelectionRange {
    fn contains(&self, line: usize, col: usize) -> bool {
        if line < self.start_line || line > self.end_line {
            return false;
        }
        if self.start_line == self.end_line {
            return col >= self.start_col && col <= self.end_col;
        }
        if line == self.start_line {
            return col >= self.start_col;
        }
        if line == self.end_line {
            return col <= self.end_col;
        }
        true
    }
}

/// One entry in the tab strip.
pub struct TabInfo {
    pub title: String,
}

/// Everything a frame depends on beyond the terminal itself.
pub struct FrameInput<'a> {
    pub term: &'a Term,
    pub selection: Option<SelectionRange>,
    pub focused: bool,
    /// Current phase of the cursor blink; ignored for non-blinking cursors.
    pub cursor_blink_on: bool,
    /// Empty or single-entry means the strip is not drawn.
    pub tabs: &'a [TabInfo],
    pub active_tab: usize,
}

pub struct Renderer {
    gpu: Gpu,
    fonts: FontSet,
    atlas: Atlas,
    palette: Palette,

    cfg: Config,
    dpi_scale: f32,

    // Scratch lists, retained across frames so steady-state rendering does not
    // allocate at all.
    bg: Vec<RectInstance>,
    glyphs: Vec<GlyphInstance>,
    overlay: Vec<RectInstance>,
}

impl Renderer {
    pub fn new(hwnd: HWND, cfg: &Config, dpi_scale: f32) -> Result<Renderer> {
        let gpu = Gpu::new(hwnd, cfg.render.vsync, cfg.translucent())?;
        let fonts = FontSet::new(&cfg.font, dpi_scale)?;
        let atlas = Atlas::new(&gpu.device)?;

        Ok(Renderer {
            gpu,
            fonts,
            atlas,
            palette: cfg.palette(),
            cfg: cfg.clone(),
            dpi_scale,
            bg: Vec::new(),
            glyphs: Vec::new(),
            overlay: Vec::new(),
        })
    }

    pub fn metrics(&self) -> CellMetrics {
        self.fonts.metrics
    }

    /// Height reserved at the top of the client area for the tab strip.
    /// Zero when there is nothing to show.
    pub fn tab_strip_height(&self, tab_count: usize) -> f32 {
        if tab_count <= 1 {
            0.0
        } else {
            (self.fonts.metrics.height * 1.7).round()
        }
    }

    /// Width of one tab, given how many share the strip.
    fn tab_width(&self, width: f32, count: usize) -> f32 {
        let max = (self.fonts.metrics.width * 24.0).round();
        (width / count as f32).min(max)
    }

    /// Which tab, if any, a client-area point falls on.
    pub fn tab_at(&self, x: f32, y: f32, count: usize) -> Option<usize> {
        let h = self.tab_strip_height(count);
        if h == 0.0 || y >= h {
            return None;
        }
        let (w, _) = self.gpu.size();
        let tw = self.tab_width(w as f32, count);
        let idx = (x / tw).floor() as usize;
        (idx < count).then_some(idx)
    }

    pub fn gpu(&self) -> &Gpu {
        &self.gpu
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        self.gpu.resize(width, height)
    }

    pub fn set_dpi_scale(&mut self, scale: f32) -> Result<()> {
        if (scale - self.dpi_scale).abs() < f32::EPSILON {
            return Ok(());
        }
        self.dpi_scale = scale;
        self.fonts.rescale(&self.cfg.font, scale)?;
        // Cached bitmaps were rasterized at the old size.
        self.atlas.reset();
        Ok(())
    }

    /// Grid dimensions that fit the current client area, below the tab strip.
    pub fn grid_size_for(&self, width: u32, height: u32, tab_count: usize) -> (usize, usize) {
        let m = self.fonts.metrics;
        let pad_x = self.cfg.window.padding_x * self.dpi_scale;
        let pad_y = self.cfg.window.padding_y * self.dpi_scale;
        let strip = self.tab_strip_height(tab_count);
        let usable_w = (width as f32 - pad_x * 2.0).max(m.width);
        let usable_h = (height as f32 - strip - pad_y * 2.0).max(m.height);
        (
            (usable_w / m.width).floor().max(1.0) as usize,
            (usable_h / m.height).floor().max(1.0) as usize,
        )
    }

    /// Resolve a cell's colours, applying reverse video, dim, bold-is-bright
    /// and selection.
    fn resolve_colors(&self, cell: &Cell, term: &Term, selected: bool) -> ([u8; 3], [u8; 3]) {
        let theme = &self.cfg.theme;

        let mut fg_color = cell.fg;
        if cell.flags.contains(CellFlags::BOLD) && theme.bold_is_bright {
            fg_color = fg_color.brighten();
        }

        let mut fg = fg_color.resolve(&self.palette, theme.foreground);
        let mut bg = cell.bg.resolve(&self.palette, theme.background);

        // A cell's own REVERSE and the screen-wide DECSCNM compose, so two
        // reversals cancel.
        let reverse = cell.flags.contains(CellFlags::REVERSE)
            ^ term.mode.contains(TermMode::REVERSE_VIDEO);
        if reverse {
            std::mem::swap(&mut fg, &mut bg);
        }

        if cell.flags.contains(CellFlags::DIM) {
            // Blend halfway to the background rather than scaling towards
            // black, so dim text stays legible on a light theme.
            for i in 0..3 {
                fg[i] = ((fg[i] as u16 + bg[i] as u16) / 2) as u8;
            }
        }

        if cell.flags.contains(CellFlags::HIDDEN) {
            fg = bg;
        }

        if selected {
            bg = theme.selection_bg;
            if let Some(sfg) = theme.selection_fg {
                fg = sfg;
            }
        }

        (fg, bg)
    }

    /// Build and submit one frame.
    pub fn draw(&mut self, input: &FrameInput) -> Result<()> {
        let term = input.term;
        let m = self.fonts.metrics;
        // Copied rather than borrowed: the loop below needs `&mut self` to fault
        // glyphs into the atlas.
        let theme = self.cfg.theme.clone();
        let pad_x = (self.cfg.window.padding_x * self.dpi_scale).round();
        let strip = self.tab_strip_height(input.tabs.len());
        let pad_y = (self.cfg.window.padding_y * self.dpi_scale).round() + strip;

        self.bg.clear();
        self.glyphs.clear();
        self.overlay.clear();

        let rows = term.rows();
        let cols = term.cols();
        let default_bg = theme.background;

        // Absolute line index of the top visible row, used to test selection.
        let first_visible = term
            .grid()
            .history_len()
            .saturating_sub(term.grid().display_offset());

        let cursor = term.cursor_state();
        let cursor_visible = cursor.visible
            && (!cursor.blinking || input.cursor_blink_on || !input.focused);

        for y in 0..rows {
            let row_y = pad_y + y as f32 * m.height;
            let line = first_visible + y;
            let cells = term.grid().row(y).cells();

            // --- backgrounds, run-length coalesced ---
            let mut run_start = 0usize;
            let mut run_color: Option<[u8; 3]> = None;

            for x in 0..cols {
                let selected = input
                    .selection
                    .map_or(false, |s| s.contains(line, x));
                let (_, bg) = self.resolve_colors(&cells[x], term, selected);

                // Only emit runs that differ from the clear colour: the
                // background is already there.
                let effective = if bg == default_bg { None } else { Some(bg) };

                if effective != run_color {
                    if let Some(c) = run_color {
                        self.bg.push(RectInstance {
                            rect: [
                                pad_x + run_start as f32 * m.width,
                                row_y,
                                (x - run_start) as f32 * m.width,
                                m.height,
                            ],
                            color: srgb(c, 1.0),
                        });
                    }
                    run_color = effective;
                    run_start = x;
                }
            }
            if let Some(c) = run_color {
                self.bg.push(RectInstance {
                    rect: [
                        pad_x + run_start as f32 * m.width,
                        row_y,
                        (cols - run_start) as f32 * m.width,
                        m.height,
                    ],
                    color: srgb(c, 1.0),
                });
            }

            // --- cursor, drawn under the glyph so block cursors invert ---
            if cursor_visible && cursor.y == y && term.grid().display_offset() == 0 {
                let cx = pad_x + cursor.x as f32 * m.width;
                let rect = match cursor.shape {
                    CursorShape::Block => [cx, row_y, m.width, m.height],
                    CursorShape::Underline => [
                        cx,
                        row_y + m.height - m.underline_thickness * 2.0,
                        m.width,
                        m.underline_thickness * 2.0,
                    ],
                    CursorShape::Beam => [cx, row_y, (m.width * 0.15).max(1.0), m.height],
                };
                if input.focused {
                    self.bg.push(RectInstance {
                        rect,
                        color: srgb(theme.cursor, 1.0),
                    });
                } else {
                    // Unfocused: hollow outline, so it is visible but clearly
                    // not where keystrokes are going.
                    let t = 1.0f32.max(self.dpi_scale.round());
                    let c = srgb(theme.cursor, 1.0);
                    let (x0, y0, w, h) = (rect[0], rect[1], rect[2], rect[3]);
                    for r in [
                        [x0, y0, w, t],
                        [x0, y0 + h - t, w, t],
                        [x0, y0, t, h],
                        [x0 + w - t, y0, t, h],
                    ] {
                        self.overlay.push(RectInstance { rect: r, color: c });
                    }
                }
            }

            // --- glyphs and decorations ---
            let mut x = 0usize;
            while x < cols {
                let cell = cells[x];
                let width_cells = if cell.flags.contains(CellFlags::WIDE) { 2 } else { 1 };

                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    x += 1;
                    continue;
                }

                let selected = input
                    .selection
                    .map_or(false, |s| s.contains(line, x));
                let (mut fg, _) = self.resolve_colors(&cell, term, selected);

                // A focused block cursor inverts the character under it.
                let under_cursor = cursor_visible
                    && input.focused
                    && cursor.y == y
                    && cursor.x == x
                    && cursor.shape == CursorShape::Block
                    && term.grid().display_offset() == 0;
                if under_cursor {
                    fg = theme.cursor_text;
                }

                let cell_x = pad_x + x as f32 * m.width;

                if !cell.is_empty() && !cell.flags.contains(CellFlags::HIDDEN) {
                    self.push_glyphs_for_cell(&cell, term, cell_x, row_y, fg);
                }

                // Decorations sit above the glyph.
                let deco_w = m.width * width_cells as f32;
                if cell.flags.intersects(CellFlags::ANY_UNDERLINE) {
                    let color = term
                        .underline_color(cell.underline)
                        .map(|c| c.resolve(&self.palette, fg))
                        .unwrap_or(fg);
                    self.push_underline(&cell, cell_x, row_y, deco_w, color, &m);
                }
                if cell.flags.contains(CellFlags::STRIKEOUT) {
                    self.overlay.push(RectInstance {
                        rect: [cell_x, row_y + m.strikeout_y, deco_w, m.strikeout_thickness],
                        color: srgb(fg, 1.0),
                    });
                }
                if cell.flags.contains(CellFlags::OVERLINE) {
                    self.overlay.push(RectInstance {
                        rect: [cell_x, row_y, deco_w, m.strikeout_thickness],
                        color: srgb(fg, 1.0),
                    });
                }

                x += width_cells;
            }
        }

        if strip > 0.0 {
            self.draw_tab_strip(input, strip);
        }

        let (w, h) = self.gpu.size();
        let globals = Globals {
            viewport: [w as f32, h as f32],
            atlas_inv_size: [1.0 / atlas::ATLAS_SIZE as f32; 2],
            gamma: self.cfg.font.gamma,
            contrast: self.cfg.font.contrast,
            opacity: self.cfg.window.opacity,
            _pad: 0.0,
        };

        // The clear is the terminal's default background, and it is what the
        // DWM backdrop shows through. `ClearRenderTargetView` writes through the
        // sRGB view, so the value is linear; premultiplying there keeps it
        // consistent with what the shaders emit.
        let clear = srgb_linear(theme.background, self.cfg.window.opacity);
        let subpixel = self.cfg.font.antialias == Antialias::Subpixel;

        // Split borrows: `render` needs `&mut self.gpu` alongside `&self.atlas`.
        let Renderer {
            gpu,
            atlas,
            bg,
            glyphs,
            overlay,
            ..
        } = self;
        gpu.render(&globals, clear, bg, glyphs, overlay, &atlas.srv, subpixel)
    }

    /// Draw the tab strip across the top of the window.
    ///
    /// Deliberately plain: flat rectangles and monospaced titles, using the
    /// same two draw passes as the terminal body, so tabs add no pipeline state
    /// and no extra draw calls.
    fn draw_tab_strip(&mut self, input: &FrameInput, strip: f32) {
        let m = self.fonts.metrics;
        let theme = self.cfg.theme.clone();
        let (win_w, _) = self.gpu.size();
        let count = input.tabs.len();
        let tab_w = self.tab_width(win_w as f32, count);

        // The strip sits on the window background so an inactive tab reads as
        // recessed rather than as a separate surface.
        self.bg.push(RectInstance {
            rect: [0.0, 0.0, win_w as f32, strip],
            color: srgb(mix(theme.background, [0, 0, 0], 0.35), 1.0),
        });

        for (i, tab) in input.tabs.iter().enumerate() {
            let x = i as f32 * tab_w;
            let active = i == input.active_tab;

            let fill = if active {
                theme.background
            } else {
                mix(theme.background, [0, 0, 0], 0.2)
            };
            self.bg.push(RectInstance {
                rect: [x, 0.0, tab_w - 1.0, strip],
                color: srgb(fill, 1.0),
            });

            if active {
                // Accent bar along the bottom edge marks the live tab.
                let t = (2.0 * self.dpi_scale).round().max(2.0);
                self.overlay.push(RectInstance {
                    rect: [x, strip - t, tab_w - 1.0, t],
                    color: srgb(theme.cursor, 1.0),
                });
            }

            // Title, clipped to the tab and centred vertically. The font is
            // monospaced, so stepping by the cell width is exact.
            let fg = if active {
                theme.foreground
            } else {
                mix(theme.foreground, theme.background, 0.45)
            };
            let pad = (m.width * 0.75).round();
            let avail = ((tab_w - pad * 2.0) / m.width).floor().max(0.0) as usize;
            let title = elide(&tab.title, avail);
            let text_y = ((strip - m.height) * 0.5).round();

            for (col, ch) in title.chars().enumerate() {
                if ch == ' ' {
                    continue;
                }
                let Some((face, glyph)) = self.fonts.glyph_for(ch, false, false, false) else {
                    continue;
                };
                let Some(entry) =
                    self.atlas
                        .get(&self.gpu.ctx, &mut self.fonts, GlyphKey { face, glyph })
                else {
                    continue;
                };
                self.glyphs.push(GlyphInstance {
                    dst: [
                        x + pad + col as f32 * m.width + entry.left,
                        text_y + m.ascent + entry.top,
                        entry.width,
                        entry.height,
                    ],
                    uv: [entry.u, entry.v],
                    flags: [if entry.color { 1.0 } else { 0.0 }, 0.0],
                    color: srgb(fg, 1.0),
                });
            }
        }
    }

    fn push_underline(
        &mut self,
        cell: &Cell,
        x: f32,
        row_y: f32,
        w: f32,
        color: [u8; 3],
        m: &CellMetrics,
    ) {
        let y = row_y + m.underline_y;
        let t = m.underline_thickness;
        let c = srgb(color, 1.0);
        let f = cell.flags;

        if f.contains(CellFlags::DOUBLE_UNDERLINE) {
            self.overlay.push(RectInstance {
                rect: [x, y - t, w, t],
                color: c,
            });
            self.overlay.push(RectInstance {
                rect: [x, y + t, w, t],
                color: c,
            });
        } else if f.contains(CellFlags::DOTTED_UNDERLINE) {
            // Dots and dashes are drawn as short rectangles rather than a
            // textured line; at terminal sizes the difference is invisible and
            // it avoids another texture binding.
            let step = (t * 3.0).max(2.0);
            let mut dx = 0.0;
            while dx < w {
                self.overlay.push(RectInstance {
                    rect: [x + dx, y, t.min(w - dx), t],
                    color: c,
                });
                dx += step;
            }
        } else if f.contains(CellFlags::DASHED_UNDERLINE) {
            let dash = (w / 3.0).max(2.0);
            let step = dash * 1.6;
            let mut dx = 0.0;
            while dx < w {
                self.overlay.push(RectInstance {
                    rect: [x + dx, y, dash.min(w - dx), t],
                    color: c,
                });
                dx += step;
            }
        } else if f.contains(CellFlags::CURLY_UNDERLINE) {
            // Approximate a sine with a staircase; two steps per cell reads as
            // a curl at any size a terminal actually uses.
            let amp = t;
            let seg = (w / 4.0).max(1.0);
            for i in 0..4 {
                let off = if i % 2 == 0 { -amp } else { amp };
                self.overlay.push(RectInstance {
                    rect: [x + i as f32 * seg, y + off, seg, t],
                    color: c,
                });
            }
        } else {
            self.overlay.push(RectInstance {
                rect: [x, y, w, t],
                color: c,
            });
        }
    }

    fn push_glyphs_for_cell(
        &mut self,
        cell: &Cell,
        term: &Term,
        cell_x: f32,
        row_y: f32,
        fg: [u8; 3],
    ) {
        let bold = cell.flags.contains(CellFlags::BOLD) && self.cfg.font.use_bold_font;
        let italic = cell.flags.contains(CellFlags::ITALIC) && self.cfg.font.use_italic_font;

        // A cluster renders as its base character plus marks stacked on top;
        // all of them share the cell's origin.
        let chars: heapless::Chars = if cell.ch & crate::term::CLUSTER_TAG != 0 {
            match term.cluster(cell.ch) {
                Some(c) => heapless::Chars::from_slice(c),
                None => return,
            }
        } else {
            match char::from_u32(cell.ch) {
                Some(c) => heapless::Chars::one(c),
                None => return,
            }
        };

        // U+FE0F anywhere in the cluster is an explicit request for colour
        // presentation, and U+FE0E for text. Both are invisible themselves.
        let prefer_color = chars.as_slice().contains(&'\u{FE0F}');
        let force_text = chars.as_slice().contains(&'\u{FE0E}');

        for &ch in chars.as_slice() {
            if ch == ' ' || font::is_invisible_format(ch) {
                continue;
            }
            let Some((face, glyph)) =
                self.fonts
                    .glyph_for(ch, bold, italic, prefer_color && !force_text)
            else {
                continue;
            };
            let Some(entry) = self.atlas.get(
                &self.gpu.ctx,
                &mut self.fonts,
                GlyphKey { face, glyph },
            ) else {
                continue;
            };

            let m = self.fonts.metrics;
            self.glyphs.push(GlyphInstance {
                dst: [
                    cell_x + entry.left,
                    row_y + m.ascent + entry.top,
                    entry.width,
                    entry.height,
                ],
                uv: [entry.u, entry.v],
                flags: [if entry.color { 1.0 } else { 0.0 }, 0.0],
                color: srgb(fg, 1.0),
            });
        }
    }

    pub fn present(&self) {
        let _ = self.gpu.present(self.cfg.render.vsync);
    }

    pub fn wait_for_frame(&self, timeout_ms: u32) {
        self.gpu.wait_for_frame(timeout_ms);
    }
}

/// Blend two sRGB colours. Approximate (it ignores gamma), which is fine for
/// chrome shading where only the relative step matters.
fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).clamp(0.0, 255.0) as u8;
    [f(a[0], b[0]), f(a[1], b[1]), f(a[2], b[2])]
}

/// Shorten a title to `width` characters, marking the cut with an ellipsis.
fn elide(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= width {
        return s.to_string();
    }
    if width == 1 {
        return "\u{2026}".to_string();
    }
    // Keep the tail: a shell title is usually a path, and the leaf matters most.
    let skip = count - (width - 1);
    let mut out = String::from("\u{2026}");
    out.extend(s.chars().skip(skip));
    out
}

/// sRGB bytes to the 0..1 floats the shader expects (it linearises them).
#[inline]
fn srgb(c: [u8; 3], a: f32) -> [f32; 4] {
    [
        c[0] as f32 / 255.0,
        c[1] as f32 / 255.0,
        c[2] as f32 / 255.0,
        a,
    ]
}

/// Same, but pre-linearised -- `ClearRenderTargetView` writes through the sRGB
/// view without converting, so the clear colour must already be linear.
#[inline]
fn srgb_linear(c: [u8; 3], a: f32) -> [f32; 4] {
    fn to_linear(v: u8) -> f32 {
        let c = v as f32 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    // Premultiplied, to match the blend state and the composition swap chain.
    [
        to_linear(c[0]) * a,
        to_linear(c[1]) * a,
        to_linear(c[2]) * a,
        a,
    ]
}

/// A tiny inline character buffer, so rendering a cell never allocates.
mod heapless {
    pub struct Chars {
        buf: [char; 8],
        len: usize,
    }

    impl Chars {
        pub fn one(c: char) -> Chars {
            let mut buf = [' '; 8];
            buf[0] = c;
            Chars { buf, len: 1 }
        }

        pub fn from_slice(s: &[char]) -> Chars {
            let mut buf = [' '; 8];
            let len = s.len().min(8);
            buf[..len].copy_from_slice(&s[..len]);
            Chars { buf, len }
        }

        pub fn as_slice(&self) -> &[char] {
            &self.buf[..self.len]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SelectionRange;

    #[test]
    fn selection_covers_partial_first_and_last_lines() {
        let s = SelectionRange {
            start_line: 2,
            start_col: 5,
            end_line: 4,
            end_col: 3,
        };
        assert!(!s.contains(1, 100));
        assert!(!s.contains(2, 4));
        assert!(s.contains(2, 5));
        assert!(s.contains(3, 0));
        assert!(s.contains(3, 999));
        assert!(s.contains(4, 3));
        assert!(!s.contains(4, 4));
        assert!(!s.contains(5, 0));
    }

    #[test]
    fn single_line_selection_is_bounded_both_ends() {
        let s = SelectionRange {
            start_line: 7,
            start_col: 2,
            end_line: 7,
            end_col: 6,
        };
        assert!(!s.contains(7, 1));
        assert!(s.contains(7, 2));
        assert!(s.contains(7, 6));
        assert!(!s.contains(7, 7));
    }
}
