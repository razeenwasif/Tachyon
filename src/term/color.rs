//! Colour representation and the 256-entry palette.
//!
//! Colours are packed into a `u32` so that a cell comparison is a single
//! integer compare and a `Cell` stays exactly 16 bytes. That size matters: the
//! renderer walks the grid linearly every frame, so cells-per-cache-line is a
//! first-order performance term.

/// Packed colour.
///
/// ```text
///   0x00_00_00_00                 default (use the theme's fg/bg)
///   0x01_00_00_II                 palette index II
///   0x02_RR_GG_BB                 direct truecolour
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct Color(pub u32);

const TAG_DEFAULT: u32 = 0x0000_0000;
const TAG_INDEXED: u32 = 0x0100_0000;
const TAG_RGB: u32 = 0x0200_0000;

impl Color {
    pub const DEFAULT: Color = Color(TAG_DEFAULT);

    #[inline]
    pub const fn indexed(i: u8) -> Color {
        Color(TAG_INDEXED | i as u32)
    }

    #[inline]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color(TAG_RGB | ((r as u32) << 16) | ((g as u32) << 8) | b as u32)
    }

    #[inline]
    pub const fn is_default(self) -> bool {
        self.0 == TAG_DEFAULT
    }

    /// Resolve to a concrete sRGB triple against `palette`.
    ///
    /// `default` is supplied by the caller so the same routine serves both
    /// foreground and background.
    #[inline]
    pub fn resolve(self, palette: &Palette, default: [u8; 3]) -> [u8; 3] {
        match self.0 & 0xFF00_0000 {
            TAG_RGB => [
                ((self.0 >> 16) & 0xFF) as u8,
                ((self.0 >> 8) & 0xFF) as u8,
                (self.0 & 0xFF) as u8,
            ],
            TAG_INDEXED => palette.0[(self.0 & 0xFF) as usize],
            _ => default,
        }
    }

    /// Palette index, if this colour is one of the 256 indexed slots.
    #[inline]
    pub fn index(self) -> Option<u8> {
        if self.0 & 0xFF00_0000 == TAG_INDEXED {
            Some((self.0 & 0xFF) as u8)
        } else {
            None
        }
    }

    /// Map indices 0-7 onto 8-15. SGR bold historically brightened the eight
    /// ANSI colours; we do it at resolve time so toggling the behaviour off is
    /// a config flag rather than a re-render of stored state.
    #[inline]
    pub fn brighten(self) -> Color {
        match self.index() {
            Some(i) if i < 8 => Color::indexed(i + 8),
            _ => self,
        }
    }
}

/// The 256-colour palette.
#[derive(Clone)]
pub struct Palette(pub [[u8; 3]; 256]);

impl Default for Palette {
    fn default() -> Self {
        Self::new()
    }
}

impl Palette {
    /// Build the standard palette: 16 configurable ANSI colours, a 6x6x6 colour
    /// cube, then a 24-step greyscale ramp.
    pub fn new() -> Palette {
        let mut p = [[0u8; 3]; 256];

        // Slots 0-15. These defaults are a slightly desaturated, higher-contrast
        // take on the classic xterm set; they read well on dark backgrounds
        // without the neon cast of the originals.
        const ANSI: [[u8; 3]; 16] = [
            [0x14, 0x18, 0x1F], // black
            [0xF2, 0x60, 0x6B], // red
            [0x6F, 0xD0, 0x8C], // green
            [0xE8, 0xC1, 0x6B], // yellow
            [0x63, 0xA8, 0xF0], // blue
            [0xB0, 0x84, 0xEB], // magenta
            [0x5E, 0xD2, 0xD8], // cyan
            [0xC4, 0xCB, 0xD8], // white
            [0x46, 0x50, 0x62], // bright black
            [0xFF, 0x7A, 0x85], // bright red
            [0x89, 0xE8, 0xA6], // bright green
            [0xFF, 0xD9, 0x85], // bright yellow
            [0x82, 0xBF, 0xFF], // bright blue
            [0xC9, 0x9C, 0xFF], // bright magenta
            [0x7A, 0xEC, 0xF2], // bright cyan
            [0xE8, 0xEE, 0xF7], // bright white
        ];
        p[..16].copy_from_slice(&ANSI);

        // 6x6x6 cube, slots 16-231.
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let mut i = 16;
        for r in 0..6 {
            for g in 0..6 {
                for b in 0..6 {
                    p[i] = [LEVELS[r], LEVELS[g], LEVELS[b]];
                    i += 1;
                }
            }
        }

        // Greyscale ramp, slots 232-255.
        for j in 0..24 {
            let v = 8 + j as u8 * 10;
            p[232 + j] = [v, v, v];
        }

        Palette(p)
    }
}
