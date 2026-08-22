//! Font loading, metrics and glyph rasterization via DirectWrite.
//!
//! We work at the glyph level rather than through a text layout object. A
//! terminal has already decided where every character goes, so layout would be
//! wasted work; all we need is codepoint -> glyph id and glyph id -> coverage
//! bitmap, both of which DirectWrite exposes directly.
//!
//! Rasterization always requests `CLEARTYPE_3x1` coverage. That gives three
//! independent samples per pixel, which the subpixel pixel shader consumes as
//! per-channel blend weights; the grayscale path averages them, which is a
//! strictly better estimate of true coverage than asking DirectWrite for a
//! single channel.

use std::collections::HashMap;

use windows::core::{Interface, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::DirectWrite::*;

use crate::config::FontConfig;

/// Which face within a family a cell wants.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FaceKey {
    pub bold: bool,
    pub italic: bool,
    /// 0 is the primary family; higher values index the fallback chain.
    pub fallback: u8,
}

impl FaceKey {
    pub const REGULAR: FaceKey = FaceKey {
        bold: false,
        italic: false,
        fallback: 0,
    };
}

/// A rasterized glyph: 8-bit RGB coverage, tightly packed, plus where it sits
/// relative to the cell origin.
pub struct GlyphBitmap {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes.
    ///
    /// For a monochrome glyph this is RGB coverage with peak coverage in alpha,
    /// and the shader tints it with the cell's foreground colour. For a colour
    /// glyph it is straight-alpha sRGB, and the shader uses it as-is.
    pub pixels: Vec<u8>,
    /// Offset from the pen position, in pixels.
    pub left: i32,
    pub top: i32,
    /// The glyph carries its own colour and must not be tinted.
    pub color: bool,
}

/// Cell geometry derived from the primary face.
#[derive(Clone, Copy, Debug)]
pub struct CellMetrics {
    pub width: f32,
    pub height: f32,
    /// Distance from the top of the cell down to the baseline.
    pub ascent: f32,
    pub underline_y: f32,
    pub underline_thickness: f32,
    pub strikeout_y: f32,
    pub strikeout_thickness: f32,
}

struct Face {
    face: IDWriteFontFace,
    /// Design units per em, needed to scale every metric.
    upem: f32,
}

pub struct FontSet {
    factory: IDWriteFactory,
    faces: HashMap<FaceKey, Option<Face>>,
    /// Family names to consult in order when the primary face lacks a glyph.
    families: Vec<String>,
    /// Index of the colour emoji family within `families`, if installed.
    emoji_family: Option<u8>,
    size_px: f32,
    pub metrics: CellMetrics,
}

/// Families tried, in order, when the configured font has no glyph for a
/// codepoint. Chosen to cover the ranges terminal users actually hit: box
/// drawing and symbols, CJK, and emoji.
const FALLBACK_FAMILIES: &[&str] = &[
    // Nerd Font icon ranges first: prompts like starship and oh-my-posh draw
    // from the private use area, and no stock Windows font covers it. "Symbols
    // Nerd Font" is the icon-only release meant precisely for this fallback
    // role; the patched families are listed because people usually have one of
    // those instead.
    "Symbols Nerd Font Mono",
    "Symbols Nerd Font",
    "CaskaydiaCove Nerd Font Mono",
    "CaskaydiaCove Nerd Font",
    "JetBrainsMono Nerd Font Mono",
    "JetBrainsMono Nerd Font",
    "FiraCode Nerd Font Mono",
    "Hack Nerd Font Mono",
    "MesloLGS NF",
    // Powerline-only patched fonts ship with Windows Terminal installs.
    "Cascadia Mono PL",
    "Cascadia Code PL",
    // Stock coverage.
    "Cascadia Mono",
    "Consolas",
    "Segoe UI Emoji",
    "Segoe UI Symbol",
    "Segoe MDL2 Assets",
    "Microsoft YaHei",
    "Meiryo",
    "Malgun Gothic",
    "Nirmala UI",
    "Segoe UI",
];

impl FontSet {
    pub fn new(cfg: &FontConfig, dpi_scale: f32) -> Result<FontSet> {
        let factory: IDWriteFactory =
            unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };

        let mut families = vec![cfg.family.clone()];
        for f in FALLBACK_FAMILIES {
            if !families.iter().any(|x| x.eq_ignore_ascii_case(f)) {
                families.push((*f).to_string());
            }
        }

        let size_px = cfg.size * dpi_scale * 96.0 / 72.0;

        let emoji_family = families
            .iter()
            .position(|f| f.eq_ignore_ascii_case("Segoe UI Emoji"))
            .and_then(|i| u8::try_from(i).ok());

        let mut set = FontSet {
            factory,
            faces: HashMap::new(),
            families,
            emoji_family,
            size_px,
            metrics: CellMetrics {
                width: 8.0,
                height: 16.0,
                ascent: 12.0,
                underline_y: 14.0,
                underline_thickness: 1.0,
                strikeout_y: 8.0,
                strikeout_thickness: 1.0,
            },
        };

        set.compute_metrics(cfg)?;
        Ok(set)
    }

    pub fn size_px(&self) -> f32 {
        self.size_px
    }

    /// Rebuild for a new DPI or point size. Callers must also clear the atlas.
    pub fn rescale(&mut self, cfg: &FontConfig, dpi_scale: f32) -> Result<()> {
        self.size_px = cfg.size * dpi_scale * 96.0 / 72.0;
        self.compute_metrics(cfg)
    }

    fn compute_metrics(&mut self, cfg: &FontConfig) -> Result<()> {
        // Everything is derived from the primary regular face; mixing metrics
        // across faces would make the grid jitter when text turns bold.
        let mut m = DWRITE_FONT_METRICS::default();
        let upem = match self.face(FaceKey::REGULAR) {
            Some(face) => {
                unsafe { face.face.GetMetrics(&mut m) };
                face.upem
            }
            // Nothing installed under that name; keep the fallback metrics so
            // the window still comes up and the user can see the problem.
            None => return Ok(()),
        };
        let scale = self.size_px / upem;

        let ascent = m.ascent as f32 * scale;
        let descent = m.descent as f32 * scale;
        let line_gap = m.lineGap as f32 * scale;

        // Advance width of a reference glyph. A monospace face gives the same
        // answer for any glyph; for a proportional face this at least yields a
        // sane cell.
        let advance = self.advance_of('M').unwrap_or(self.size_px * 0.6);

        let height = ((ascent + descent + line_gap) * cfg.line_height).ceil().max(1.0);
        let width = (advance * cfg.cell_width).round().max(1.0);

        // Centre the text block vertically inside the (possibly taller) cell.
        let leading = (height - (ascent + descent)) * 0.5;
        let baseline = (leading + ascent).round();

        let ul_thick = (m.underlineThickness as f32 * scale).round().max(1.0);
        let ul_y = (baseline - m.underlinePosition as f32 * scale).round();
        let so_thick = (m.strikethroughThickness as f32 * scale).round().max(1.0);
        let so_y = (baseline - m.strikethroughPosition as f32 * scale).round();

        self.metrics = CellMetrics {
            width,
            height,
            ascent: baseline,
            // Keep decorations inside the cell.
            underline_y: ul_y.min(height - ul_thick).max(0.0),
            underline_thickness: ul_thick,
            strikeout_y: so_y.clamp(0.0, height - so_thick),
            strikeout_thickness: so_thick,
        };
        Ok(())
    }

    fn advance_of(&mut self, c: char) -> Option<f32> {
        let size_px = self.size_px;
        let face = self.face(FaceKey::REGULAR)?;
        let upem = face.upem;
        let cp = [c as u32];
        let mut idx = [0u16; 1];
        unsafe {
            face.face
                .GetGlyphIndices(cp.as_ptr(), 1, idx.as_mut_ptr())
                .ok()?;
        }
        if idx[0] == 0 {
            return None;
        }
        let mut gm = [DWRITE_GLYPH_METRICS::default(); 1];
        unsafe {
            face.face
                .GetDesignGlyphMetrics(idx.as_ptr(), 1, gm.as_mut_ptr(), false)
                .ok()?;
        }
        Some(gm[0].advanceWidth as f32 * size_px / upem)
    }

    /// Look up (and cache) a face. `None` means the family is not installed.
    fn face(&mut self, key: FaceKey) -> Option<&Face> {
        if !self.faces.contains_key(&key) {
            let loaded = self.load_face(key);
            self.faces.insert(key, loaded);
        }
        self.faces.get(&key).and_then(|f| f.as_ref())
    }

    fn load_face(&self, key: FaceKey) -> Option<Face> {
        let family_name = self.families.get(key.fallback as usize)?;

        unsafe {
            let mut collection: Option<IDWriteFontCollection> = None;
            self.factory
                .GetSystemFontCollection(&mut collection, false)
                .ok()?;
            let collection = collection?;

            let name = HSTRING::from(family_name.as_str());
            let mut index = 0u32;
            let mut exists = windows::core::BOOL(0);
            collection
                .FindFamilyName(PCWSTR(name.as_ptr()), &mut index, &mut exists)
                .ok()?;
            if !exists.as_bool() {
                return None;
            }

            let family = collection.GetFontFamily(index).ok()?;
            let weight = if key.bold {
                DWRITE_FONT_WEIGHT_BOLD
            } else {
                DWRITE_FONT_WEIGHT_NORMAL
            };
            let style = if key.italic {
                DWRITE_FONT_STYLE_ITALIC
            } else {
                DWRITE_FONT_STYLE_NORMAL
            };
            let font = family
                .GetFirstMatchingFont(weight, DWRITE_FONT_STRETCH_NORMAL, style)
                .ok()?;
            let face = font.CreateFontFace().ok()?;

            let mut m = DWRITE_FONT_METRICS::default();
            face.GetMetrics(&mut m);

            Some(Face {
                face,
                upem: m.designUnitsPerEm as f32,
            })
        }
    }

    /// Resolve a character to a face that actually has a glyph for it, walking
    /// the fallback chain. Returns the face key and the glyph id.
    ///
    /// `prefer_color` jumps the emoji family to the front. This matters more
    /// than it sounds: several fonts on a stock Windows install -- Segoe UI
    /// Symbol, and most Nerd Font patches -- contain *monochrome* outlines for
    /// emoji codepoints. A plain in-order walk finds one of those first and the
    /// emoji renders as a grey glyph even though a colour version exists.
    pub fn glyph_for(
        &mut self,
        c: char,
        bold: bool,
        italic: bool,
        prefer_color: bool,
    ) -> Option<(FaceKey, u16)> {
        if prefer_color || wants_emoji_presentation(c) {
            if let Some(fallback) = self.emoji_family {
                let key = FaceKey {
                    bold,
                    italic,
                    fallback,
                };
                if let Some(gid) = self.glyph_index(key, c) {
                    return Some((key, gid));
                }
            }
        }

        let n = self.families.len().min(u8::MAX as usize) as u8;
        for fallback in 0..n {
            let key = FaceKey {
                bold,
                italic,
                fallback,
            };
            if let Some(gid) = self.glyph_index(key, c) {
                return Some((key, gid));
            }
        }
        None
    }

    /// Glyph id for `c` in one face, or `None` if the face does not cover it.
    fn glyph_index(&mut self, key: FaceKey, c: char) -> Option<u16> {
        let face = self.face(key)?;
        let cp = [c as u32];
        let mut idx = [0u16; 1];
        let ok = unsafe {
            face.face
                .GetGlyphIndices(cp.as_ptr(), 1, idx.as_mut_ptr())
                .is_ok()
        };
        // Glyph 0 is .notdef: the face does not cover this codepoint.
        (ok && idx[0] != 0).then_some(idx[0])
    }

    /// Rasterize one glyph. `None` for glyphs with no ink, such as a space.
    pub fn rasterize(&mut self, key: FaceKey, glyph: u16) -> Option<GlyphBitmap> {
        // Emoji and other COLR/CPAL glyphs decompose into coloured layers.
        // Ask for that first; the call fails cheaply for ordinary glyphs.
        if let Some(bmp) = self.rasterize_color(key, glyph) {
            return Some(bmp);
        }
        self.rasterize_mono(key, glyph)
    }

    fn rasterize_mono(&mut self, key: FaceKey, glyph: u16) -> Option<GlyphBitmap> {
        let size = self.size_px;
        let face = self.face(key)?;

        let mut indices = [glyph];
        let mut advances = [0.0f32];
        let mut offsets = [DWRITE_GLYPH_OFFSET::default()];

        let run = DWRITE_GLYPH_RUN {
            fontFace: unsafe { std::mem::transmute_copy(&face.face) },
            fontEmSize: size,
            glyphCount: 1,
            glyphIndices: indices.as_mut_ptr(),
            glyphAdvances: advances.as_mut_ptr(),
            glyphOffsets: offsets.as_mut_ptr(),
            isSideways: windows::core::BOOL(0),
            bidiLevel: 0,
        };

        unsafe {
            let analysis = self
                .factory
                .CreateGlyphRunAnalysis(
                    &run,
                    1.0,
                    None,
                    // Symmetric natural rendering keeps stem positions honest at
                    // the small sizes terminals use, without the heavy hinting
                    // distortion of the GDI-compatible modes.
                    DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC,
                    DWRITE_MEASURING_MODE_NATURAL,
                    0.0,
                    0.0,
                )
                .ok()?;

            let bounds: RECT = analysis
                .GetAlphaTextureBounds(DWRITE_TEXTURE_CLEARTYPE_3x1)
                .ok()?;

            let w = (bounds.right - bounds.left).max(0) as u32;
            let h = (bounds.bottom - bounds.top).max(0) as u32;
            if w == 0 || h == 0 {
                return None;
            }

            // Three coverage bytes per pixel.
            let mut raw = vec![0u8; (w * h * 3) as usize];
            analysis
                .CreateAlphaTexture(DWRITE_TEXTURE_CLEARTYPE_3x1, &bounds, &mut raw)
                .ok()?;

            // Widen to RGBA for a single atlas format. Alpha carries peak
            // coverage so the grayscale path and any future alpha test agree.
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            for i in 0..(w * h) as usize {
                let (r, g, b) = (raw[i * 3], raw[i * 3 + 1], raw[i * 3 + 2]);
                pixels[i * 4] = r;
                pixels[i * 4 + 1] = g;
                pixels[i * 4 + 2] = b;
                pixels[i * 4 + 3] = r.max(g).max(b);
            }

            Some(GlyphBitmap {
                width: w,
                height: h,
                pixels,
                left: bounds.left,
                // Bounds are relative to the baseline origin; convert to an
                // offset from the top of the cell.
                top: bounds.top,
                color: false,
            })
        }
    }

    /// Build a glyph run for a single glyph at the origin.
    fn single_glyph_run(
        face: &IDWriteFontFace,
        size: f32,
        indices: &mut [u16; 1],
        advances: &mut [f32; 1],
        offsets: &mut [DWRITE_GLYPH_OFFSET; 1],
    ) -> DWRITE_GLYPH_RUN {
        DWRITE_GLYPH_RUN {
            fontFace: unsafe { std::mem::transmute_copy(face) },
            fontEmSize: size,
            glyphCount: 1,
            glyphIndices: indices.as_mut_ptr(),
            glyphAdvances: advances.as_mut_ptr(),
            glyphOffsets: offsets.as_mut_ptr(),
            isSideways: windows::core::BOOL(0),
            bidiLevel: 0,
        }
    }

    /// Rasterize a colour glyph by compositing its COLR layers.
    ///
    /// Returns `None` when the glyph has no colour data, which is the common
    /// case and is signalled by `TranslateColorGlyphRun` failing.
    fn rasterize_color(&mut self, key: FaceKey, glyph: u16) -> Option<GlyphBitmap> {
        let size = self.size_px;
        let factory2: IDWriteFactory2 = self.factory.cast().ok()?;
        let face = self.face(key)?;

        let mut indices = [glyph];
        let mut advances = [0.0f32];
        let mut offsets = [DWRITE_GLYPH_OFFSET::default()];
        let run = Self::single_glyph_run(
            &face.face,
            size,
            &mut indices,
            &mut advances,
            &mut offsets,
        );

        // One rasterized COLR layer.
        struct Layer {
            bounds: RECT,
            coverage: Vec<u8>, // one byte per pixel
            color: [f32; 4],   // linear premultiplied-ready, straight alpha
        }

        let mut layers: Vec<Layer> = Vec::new();

        unsafe {
            let enumerator = factory2
                .TranslateColorGlyphRun(
                    0.0,
                    0.0,
                    &run,
                    None,
                    DWRITE_MEASURING_MODE_NATURAL,
                    None,
                    0,
                )
                .ok()?;

            loop {
                let layer_ptr = enumerator.GetCurrentRun().ok()?;
                let layer = &*layer_ptr;

                let analysis = self
                    .factory
                    .CreateGlyphRunAnalysis(
                        &layer.glyphRun,
                        1.0,
                        None,
                        DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC,
                        DWRITE_MEASURING_MODE_NATURAL,
                        layer.baselineOriginX,
                        layer.baselineOriginY,
                    )
                    .ok();

                if let Some(analysis) = analysis {
                    if let Ok(bounds) = analysis.GetAlphaTextureBounds(DWRITE_TEXTURE_CLEARTYPE_3x1)
                    {
                        let w = (bounds.right - bounds.left).max(0) as u32;
                        let h = (bounds.bottom - bounds.top).max(0) as u32;
                        if w > 0 && h > 0 {
                            let mut raw = vec![0u8; (w * h * 3) as usize];
                            if analysis
                                .CreateAlphaTexture(
                                    DWRITE_TEXTURE_CLEARTYPE_3x1,
                                    &bounds,
                                    &mut raw,
                                )
                                .is_ok()
                            {
                                // Collapse the three subpixel samples: a colour
                                // layer is tinted, so per-channel coverage would
                                // fringe the tint rather than the text.
                                let coverage: Vec<u8> = (0..(w * h) as usize)
                                    .map(|i| {
                                        ((raw[i * 3] as u16
                                            + raw[i * 3 + 1] as u16
                                            + raw[i * 3 + 2] as u16)
                                            / 3) as u8
                                    })
                                    .collect();

                                // paletteIndex 0xFFFF means "use the text
                                // foreground". We have no cell context here, so
                                // the layer stays white and the shader shows it
                                // unchanged; this is rare outside icon fonts.
                                let c = layer.runColor;
                                let color = if layer.paletteIndex == 0xFFFF {
                                    [1.0, 1.0, 1.0, 1.0]
                                } else {
                                    [c.r, c.g, c.b, c.a]
                                };

                                layers.push(Layer {
                                    bounds,
                                    coverage,
                                    color,
                                });
                            }
                        }
                    }
                }

                match enumerator.MoveNext() {
                    Ok(more) if more.as_bool() => continue,
                    _ => break,
                }
            }
        }

        if layers.is_empty() {
            return None;
        }

        // Union of every layer's extent.
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        for l in &layers {
            min_x = min_x.min(l.bounds.left);
            min_y = min_y.min(l.bounds.top);
            max_x = max_x.max(l.bounds.right);
            max_y = max_y.max(l.bounds.bottom);
        }
        let width = (max_x - min_x).max(0) as u32;
        let height = (max_y - min_y).max(0) as u32;
        if width == 0 || height == 0 {
            return None;
        }

        // Composite in linear light with premultiplied alpha, then convert back
        // to straight sRGB for storage.
        //
        // COLR layers are ordered bottom-first, so each one goes *over* what is
        // already accumulated. Getting this backwards is subtle: the base shape
        // still appears, so an emoji looks almost right -- just missing its
        // details, because the face occludes the eyes drawn after it.
        let n = (width * height) as usize;
        let mut acc = vec![[0f32; 4]; n];

        for l in &layers {
            let lw = (l.bounds.right - l.bounds.left) as u32;
            let lh = (l.bounds.bottom - l.bounds.top) as u32;
            let ox = (l.bounds.left - min_x) as u32;
            let oy = (l.bounds.top - min_y) as u32;

            let src_lin = [
                srgb_to_linear(l.color[0]),
                srgb_to_linear(l.color[1]),
                srgb_to_linear(l.color[2]),
            ];

            for y in 0..lh {
                for x in 0..lw {
                    let cov = l.coverage[(y * lw + x) as usize] as f32 / 255.0;
                    if cov <= 0.0 {
                        continue;
                    }
                    let a = cov * l.color[3];
                    let d = &mut acc[((y + oy) * width + (x + ox)) as usize];
                    let inv = 1.0 - a;
                    d[0] = src_lin[0] * a + d[0] * inv;
                    d[1] = src_lin[1] * a + d[1] * inv;
                    d[2] = src_lin[2] * a + d[2] * inv;
                    d[3] = a + d[3] * inv;
                }
            }
        }

        let mut pixels = vec![0u8; n * 4];
        for (i, p) in acc.iter().enumerate() {
            let a = p[3].clamp(0.0, 1.0);
            let (r, g, b) = if a > 0.0001 {
                (p[0] / a, p[1] / a, p[2] / a)
            } else {
                (0.0, 0.0, 0.0)
            };
            let enc = |v: f32| (linear_to_srgb(v.clamp(0.0, 1.0)) * 255.0 + 0.5) as u8;
            pixels[i * 4] = enc(r);
            pixels[i * 4 + 1] = enc(g);
            pixels[i * 4 + 2] = enc(b);
            pixels[i * 4 + 3] = (a * 255.0 + 0.5) as u8;
        }

        Some(GlyphBitmap {
            width,
            height,
            pixels,
            left: min_x,
            top: min_y,
            color: true,
        })
    }
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// Whether a codepoint defaults to emoji (colour) presentation.
///
/// This approximates Unicode's `Emoji_Presentation` property. The supplementary
/// planes are contiguous enough to test by range; the handful of characters in
/// the symbol blocks that default to emoji are listed explicitly, because their
/// neighbours default to *text* presentation and must keep using the text font.
pub fn wants_emoji_presentation(c: char) -> bool {
    let cp = c as u32;
    match cp {
        // Supplementary emoji planes: pictographs, transport, symbols,
        // regional indicators, supplemental and extended-A. Contiguous enough
        // to take as one range.
        0x1F000..=0x1FAFF => true,
        // Individually emoji-by-default in the BMP symbol blocks.
        0x231A | 0x231B => true,
        0x23E9..=0x23EC | 0x23F0 | 0x23F3 => true,
        0x25FD | 0x25FE => true,
        0x2614 | 0x2615 => true,
        0x2648..=0x2653 => true,
        0x267F | 0x2693 | 0x26A1 => true,
        0x26AA | 0x26AB => true,
        0x26BD | 0x26BE => true,
        0x26C4 | 0x26C5 | 0x26CE | 0x26D4 | 0x26EA => true,
        0x26F2 | 0x26F3 | 0x26F5 | 0x26FA | 0x26FD => true,
        0x2705 | 0x270A | 0x270B | 0x2728 => true,
        0x274C | 0x274E => true,
        0x2753..=0x2755 | 0x2757 => true,
        0x2795..=0x2797 => true,
        0x27B0 | 0x27BF => true,
        0x2B1B | 0x2B1C | 0x2B50 | 0x2B55 => true,
        _ => false,
    }
}

/// Characters that control presentation but draw nothing themselves.
pub fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        // Variation selectors 15 and 16 (text / emoji presentation).
        '\u{FE0E}' | '\u{FE0F}'
        // Zero-width joiner and non-joiner.
        | '\u{200D}' | '\u{200C}'
        // Zero-width space and BOM.
        | '\u{200B}' | '\u{FEFF}'
    )
}
