//! Fonts through Core Text.
//!
//! The configured family is used if it is installed; otherwise the system
//! monospaced face, SF Mono. Fonts are created at *pixel* size (points times the
//! backing scale), so every metric and every rasterised glyph is already in
//! device pixels and the renderer never scales a bitmap.
//!
//! Missing characters fall back through `CTFontCreateForString`, which asks the
//! system for a font that covers them. That replaces the Windows build's fixed
//! fallback chain and covers CJK, symbols, Nerd Font icons and colour emoji.

use std::ptr::{self, NonNull};

use objc2::rc::Retained;
use objc2_app_kit::{NSFont, NSFontWeightRegular};
use objc2_core_foundation::{CFRange, CFRetained, CFString, CGFloat, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    kCGColorSpaceSRGB, CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo,
};
use objc2_core_text::{CTFont, CTFontOrientation, CTFontSymbolicTraits};
use rustc_hash::FxHashMap;

use crate::config::FontConfig;

/// Cell geometry in device pixels.
#[derive(Clone, Copy, Debug)]
pub struct CellMetrics {
    pub width: f32,
    pub height: f32,
    /// Baseline, measured down from the top of the cell.
    pub ascent: f32,
    pub underline_y: f32,
    pub underline_thickness: f32,
    pub strikeout_y: f32,
    pub strikeout_thickness: f32,
}

/// A rasterised glyph: RGBA8, rows top to bottom.
pub struct GlyphBitmap {
    pub width: u32,
    pub height: u32,
    /// Offset from the pen position to the bitmap's left edge.
    pub left: f32,
    /// Offset from the baseline to the bitmap's top edge (negative is up).
    pub top: f32,
    /// For text glyphs every channel holds coverage. For colour glyphs this is
    /// straight (un-premultiplied) sRGB plus alpha.
    pub data: Vec<u8>,
    pub color: bool,
}

struct Face {
    font: CFRetained<CTFont>,
    color: bool,
}

/// Which of the four primary styles a lookup wants.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Style {
    Regular = 0,
    Bold = 1,
    Italic = 2,
    BoldItalic = 3,
}

pub struct FontSet {
    /// Index 0-3 are the styles of the primary family; fallbacks follow.
    faces: Vec<Face>,
    /// Fallback faces by PostScript name, so each is created once.
    by_name: FxHashMap<String, u16>,
    /// Resolved (face, glyph) per character and style.
    lookup: FxHashMap<(char, Style), Option<(u16, u16)>>,
    pub metrics: CellMetrics,
}

impl FontSet {
    pub fn new(cfg: &FontConfig, scale: f32) -> FontSet {
        let px = (cfg.size * scale) as CGFloat;
        let (regular, family) = primary_font(&cfg.family, px);

        let style = |traits: CTFontSymbolicTraits, enabled: bool| -> CFRetained<CTFont> {
            if !enabled {
                return regular.clone();
            }
            unsafe { regular.copy_with_symbolic_traits(0.0, ptr::null(), traits, traits) }
                .unwrap_or_else(|| regular.clone())
        };
        let bold = style(CTFontSymbolicTraits::TraitBold, cfg.use_bold_font);
        let italic = style(CTFontSymbolicTraits::TraitItalic, cfg.use_italic_font);
        let bold_italic = style(
            CTFontSymbolicTraits(
                CTFontSymbolicTraits::TraitBold.0 | CTFontSymbolicTraits::TraitItalic.0,
            ),
            cfg.use_bold_font && cfg.use_italic_font,
        );

        let metrics = measure(&regular, cfg);
        let faces = [regular, bold, italic, bold_italic]
            .into_iter()
            .map(|font| Face {
                color: is_color(&font),
                font,
            })
            .collect();

        crate::log!("font: {family} at {px:.1}px, cell {:.0}x{:.0}", metrics.width, metrics.height);
        FontSet {
            faces,
            by_name: FxHashMap::default(),
            lookup: FxHashMap::default(),
            metrics,
        }
    }

    /// The face and glyph that draw `ch`, falling back to a system font when
    /// the primary family lacks it.
    pub fn glyph_for(&mut self, ch: char, bold: bool, italic: bool) -> Option<(u16, u16)> {
        let style = match (bold, italic) {
            (false, false) => Style::Regular,
            (true, false) => Style::Bold,
            (false, true) => Style::Italic,
            (true, true) => Style::BoldItalic,
        };
        if let Some(hit) = self.lookup.get(&(ch, style)) {
            return *hit;
        }

        let base = style as u16;
        let found = glyph_in(&self.faces[base as usize].font, ch)
            .map(|g| (base, g))
            .or_else(|| self.fallback(base, ch));
        self.lookup.insert((ch, style), found);
        found
    }

    fn fallback(&mut self, base: u16, ch: char) -> Option<(u16, u16)> {
        // Private-use codepoints are Nerd Font icons in practice (starship,
        // oh-my-posh, eza). Core Text's cascade never looks for them in user
        // fonts, so try the Nerd families by name first.
        if is_private_use(ch) {
            let size = unsafe { self.faces[base as usize].font.size() };
            for family in NERD_FONTS {
                let name = CFString::from_str(family);
                let font = unsafe { CTFont::with_name(&name, size, ptr::null()) };
                let got = unsafe { font.family_name() }.to_string();
                if !got.eq_ignore_ascii_case(family) {
                    continue;
                }
                if let Some(glyph) = glyph_in(&font, ch) {
                    return Some((self.intern(font), glyph));
                }
            }
        }

        let mut buf = [0u16; 2];
        let utf16 = ch.encode_utf16(&mut buf);
        let s = CFString::from_str(&ch.to_string());
        let range = CFRange {
            location: 0,
            length: utf16.len() as _,
        };
        let font = unsafe { self.faces[base as usize].font.for_string(&s, range) };
        let glyph = glyph_in(&font, ch)?;
        Some((self.intern(font), glyph))
    }

    /// Index of `font` in the face list, adding it the first time it is seen.
    fn intern(&mut self, font: CFRetained<CTFont>) -> u16 {
        let name = unsafe { font.post_script_name() }.to_string();
        if let Some(&i) = self.by_name.get(&name) {
            return i;
        }
        let i = self.faces.len() as u16;
        self.faces.push(Face {
            color: is_color(&font),
            font,
        });
        self.by_name.insert(name, i);
        i
    }

    /// Rasterise one glyph with grayscale antialiasing, the way macOS draws
    /// text everywhere else since it dropped subpixel rendering.
    pub fn rasterize(&self, face: u16, glyph: u16) -> Option<GlyphBitmap> {
        let f = self.faces.get(face as usize)?;
        let mut g = glyph;
        let mut rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(0.0, 0.0));
        unsafe {
            f.font.bounding_rects_for_glyphs(
                CTFontOrientation::Default,
                NonNull::from(&mut g),
                &mut rect,
                1,
            );
        }
        if rect.size.width <= 0.0 || rect.size.height <= 0.0 {
            return None;
        }

        // One pixel of slack on every side so antialiased edges are not cut.
        let x0 = rect.origin.x.floor() - 1.0;
        let y0 = rect.origin.y.floor() - 1.0;
        let x1 = (rect.origin.x + rect.size.width).ceil() + 1.0;
        let y1 = (rect.origin.y + rect.size.height).ceil() + 1.0;
        let w = (x1 - x0) as usize;
        let h = (y1 - y0) as usize;

        let mut data = vec![0u8; w * h * 4];
        let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
        let ctx = unsafe {
            CGBitmapContextCreate(
                data.as_mut_ptr().cast(),
                w,
                h,
                8,
                w * 4,
                Some(&space),
                CGImageAlphaInfo::PremultipliedLast.0,
            )
        }?;
        CGContext::set_should_antialias(Some(&ctx), true);
        CGContext::set_allows_font_smoothing(Some(&ctx), true);
        CGContext::set_should_smooth_fonts(Some(&ctx), false);
        if !f.color {
            CGContext::set_rgb_fill_color(Some(&ctx), 1.0, 1.0, 1.0, 1.0);
        }

        // Core Graphics has its origin at the bottom left; the bitmap's first
        // row in memory is the top of the image.
        let pos = CGPoint::new(-x0, -y0);
        unsafe { f.font.draw_glyphs(NonNull::from(&g), NonNull::from(&pos), 1, &ctx) };
        drop(ctx);

        if f.color {
            // Un-premultiply: the shader treats colour texels as straight sRGB.
            for px in data.chunks_exact_mut(4) {
                let a = px[3] as u32;
                if a > 0 && a < 255 {
                    for c in &mut px[..3] {
                        *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                    }
                }
            }
        }

        Some(GlyphBitmap {
            width: w as u32,
            height: h as u32,
            left: x0 as f32,
            top: -(y1 as f32),
            data,
            color: f.color,
        })
    }
}

/// The configured family, or SF Mono when it is not installed. Core Text never
/// fails a name lookup -- it substitutes the closest match, usually Helvetica --
/// so the family name it actually returned is checked.
fn primary_font(family: &str, px: CGFloat) -> (CFRetained<CTFont>, String) {
    if !family.is_empty() {
        let name = CFString::from_str(family);
        let font = unsafe { CTFont::with_name(&name, px, ptr::null()) };
        let got = unsafe { font.family_name() }.to_string();
        let ps = unsafe { font.post_script_name() }.to_string();
        if got.eq_ignore_ascii_case(family) || ps.eq_ignore_ascii_case(family) {
            return (font, got);
        }
        crate::log!("font: `{family}` is not installed (got `{got}`); using SF Mono");
    }

    let ns: Retained<NSFont> =
        NSFont::monospacedSystemFontOfSize_weight(px, unsafe { NSFontWeightRegular });
    // NSFont and CTFont are toll-free bridged: the same object under two names.
    let font = unsafe {
        CFRetained::retain(NonNull::new_unchecked(Retained::as_ptr(&ns) as *mut CTFont))
    };
    let got = unsafe { font.family_name() }.to_string();
    (font, got)
}

/// Nerd Font families to try for private-use icons, the icon-only release first.
/// Same list as the Windows backend.
const NERD_FONTS: [&str; 10] = [
    "Symbols Nerd Font Mono",
    "Symbols Nerd Font",
    "JetBrainsMono Nerd Font Mono",
    "JetBrainsMono Nerd Font",
    "CaskaydiaCove Nerd Font Mono",
    "CaskaydiaCove Nerd Font",
    "FiraCode Nerd Font Mono",
    "Hack Nerd Font Mono",
    "MesloLGS Nerd Font Mono",
    "MesloLGS NF",
];

fn is_private_use(ch: char) -> bool {
    matches!(ch, '\u{E000}'..='\u{F8FF}' | '\u{F0000}'..='\u{FFFFD}' | '\u{100000}'..='\u{10FFFD}')
}

fn glyph_in(font: &CTFont, ch: char) -> Option<u16> {
    let mut chars = [0u16; 2];
    let n = ch.encode_utf16(&mut chars).len();
    let mut glyphs = [0u16; 2];
    let ok = unsafe {
        font.glyphs_for_characters(
            NonNull::new_unchecked(chars.as_mut_ptr()),
            NonNull::new_unchecked(glyphs.as_mut_ptr()),
            n as _,
        )
    };
    (ok && glyphs[0] != 0).then_some(glyphs[0])
}

fn is_color(font: &CTFont) -> bool {
    let traits = unsafe { font.symbolic_traits() };
    traits.0 & CTFontSymbolicTraits::TraitColorGlyphs.0 != 0
}

fn measure(font: &CTFont, cfg: &FontConfig) -> CellMetrics {
    let (ascent, descent, leading) =
        unsafe { (font.ascent(), font.descent(), font.leading()) };

    let advance = glyph_in(font, '0')
        .map(|mut g| unsafe {
            font.advances_for_glyphs(
                CTFontOrientation::Horizontal,
                NonNull::from(&mut g),
                ptr::null_mut(),
                1,
            )
        })
        .unwrap_or(ascent * 0.6);

    let width = (advance * cfg.cell_width as CGFloat).round().max(1.0);
    let natural = ascent + descent + leading;
    let height = (natural * cfg.line_height as CGFloat).round().max(1.0);
    // Centre the font's own box in the cell, so a taller line height adds
    // space evenly above and below rather than all at the bottom.
    let baseline = ((height - (ascent + descent)) / 2.0 + ascent).round();

    let (ul_pos, ul_thick, x_height) =
        unsafe { (font.underline_position(), font.underline_thickness(), font.x_height()) };
    let thickness = ul_thick.round().max(1.0);

    CellMetrics {
        width: width as f32,
        height: height as f32,
        ascent: baseline as f32,
        underline_y: (baseline - ul_pos).round().min(height - thickness) as f32,
        underline_thickness: thickness as f32,
        strikeout_y: (baseline - x_height / 2.0).round() as f32,
        strikeout_thickness: thickness as f32,
    }
}
