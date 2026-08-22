//! The grid cell.
//!
//! `Cell` is deliberately exactly 16 bytes. Four cells per cache line means the
//! renderer's per-frame walk over the visible grid is bandwidth-bound at the
//! theoretical minimum, and the whole 200x50 viewport fits in ~160 KB -- L2
//! resident on every machine we care about.

use bitflags::bitflags;

use super::color::Color;

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct CellFlags: u16 {
        const BOLD             = 1 << 0;
        const DIM              = 1 << 1;
        const ITALIC           = 1 << 2;
        const UNDERLINE        = 1 << 3;
        const DOUBLE_UNDERLINE = 1 << 4;
        const CURLY_UNDERLINE  = 1 << 5;
        const DOTTED_UNDERLINE = 1 << 6;
        const DASHED_UNDERLINE = 1 << 7;
        const BLINK            = 1 << 8;
        const REVERSE          = 1 << 9;
        const HIDDEN           = 1 << 10;
        const STRIKEOUT        = 1 << 11;
        const OVERLINE         = 1 << 12;
        /// The left half of a double-width character.
        const WIDE             = 1 << 13;
        /// The right half of a double-width character; carries no glyph.
        const WIDE_SPACER      = 1 << 14;

        const ANY_UNDERLINE = Self::UNDERLINE.bits()
            | Self::DOUBLE_UNDERLINE.bits()
            | Self::CURLY_UNDERLINE.bits()
            | Self::DOTTED_UNDERLINE.bits()
            | Self::DASHED_UNDERLINE.bits();
    }
}

/// Marks `Cell::ch` as an index into the grapheme-cluster arena rather than a
/// literal scalar value. Scalars never exceed U+10FFFF, so the top bit is free.
pub const CLUSTER_TAG: u32 = 0x8000_0000;

/// A single grid position. `#[repr(C)]` pins the layout so the renderer can
/// rely on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Cell {
    /// Unicode scalar, or `CLUSTER_TAG | arena_index`. Zero means empty.
    pub ch: u32,
    pub fg: Color,
    pub bg: Color,
    pub flags: CellFlags,
    /// Index into the terminal's underline-colour side table, biased by one;
    /// zero means "same as foreground". Kept out of band because styled
    /// underline colours are rare and we refuse to grow past 16 bytes.
    pub underline: u16,
}

const _: () = assert!(core::mem::size_of::<Cell>() == 16);

impl Default for Cell {
    #[inline]
    fn default() -> Self {
        Cell::EMPTY
    }
}

impl Cell {
    pub const EMPTY: Cell = Cell {
        ch: 0,
        fg: Color::DEFAULT,
        bg: Color::DEFAULT,
        flags: CellFlags::empty(),
        underline: 0,
    };

    /// An empty cell carrying `pen`'s colours. Erasure (`ED`, `EL`) fills with
    /// this rather than `EMPTY`, which is what makes `clear` preserve a themed
    /// background.
    #[inline]
    pub fn blank(pen: &Pen) -> Cell {
        Cell {
            ch: 0,
            fg: pen.fg,
            bg: pen.bg,
            // Only background-affecting attributes survive an erase.
            flags: pen.flags & CellFlags::REVERSE,
            underline: 0,
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ch == 0
    }

    /// The scalar to render, or `None` if this cell holds a cluster index or is
    /// blank.
    #[inline]
    pub fn scalar(&self) -> Option<char> {
        if self.ch == 0 || self.ch & CLUSTER_TAG != 0 {
            None
        } else {
            char::from_u32(self.ch)
        }
    }
}

/// The current graphic rendition -- everything SGR can set. Applied to each
/// cell as it is written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pen {
    pub fg: Color,
    pub bg: Color,
    pub flags: CellFlags,
    pub underline: u16,
}

impl Default for Pen {
    fn default() -> Self {
        Pen {
            fg: Color::DEFAULT,
            bg: Color::DEFAULT,
            flags: CellFlags::empty(),
            underline: 0,
        }
    }
}

impl Pen {
    #[inline]
    pub fn reset(&mut self) {
        *self = Pen::default();
    }
}
