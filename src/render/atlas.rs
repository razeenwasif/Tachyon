//! Glyph atlas.
//!
//! Every distinct glyph is rasterized once and blitted into one texture; from
//! then on drawing a character is a quad that samples it. This is what makes
//! render cost scale with the number of *distinct* glyphs on screen rather than
//! the number of cells -- a screen of 10,000 characters typically touches fewer
//! than 100 unique glyphs.
//!
//! Packing is a shelf allocator: glyphs are placed left to right on a row whose
//! height is set by the first glyph to land on it. For a terminal that is very
//! close to optimal, because glyphs from a single face at a single size are all
//! nearly the same height. When the atlas fills we bump a generation counter and
//! start over rather than trying to compact -- it happens rarely, and a rebuild
//! costs a few milliseconds once.

use rustc_hash::FxHashMap;
use windows::core::Result;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::font::{FaceKey, FontSet};

pub const ATLAS_SIZE: u32 = 1024;

/// Identifies a rasterized glyph.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct GlyphKey {
    pub face: FaceKey,
    pub glyph: u16,
}

/// Where a glyph lives in the atlas and how to place it in a cell.
#[derive(Clone, Copy, Debug)]
pub struct AtlasEntry {
    /// Top-left in atlas texels.
    pub u: f32,
    pub v: f32,
    pub width: f32,
    pub height: f32,
    /// Offset from the glyph's pen position.
    pub left: f32,
    pub top: f32,
    /// The glyph carries its own colour (an emoji or other COLR glyph) and
    /// must be drawn as-is rather than tinted with the cell foreground.
    pub color: bool,
}

pub struct Atlas {
    texture: ID3D11Texture2D,
    pub srv: ID3D11ShaderResourceView,

    /// Current shelf.
    shelf_y: u32,
    shelf_height: u32,
    pen_x: u32,

    /// `None` marks a glyph with no ink, so we do not re-rasterize spaces.
    map: FxHashMap<GlyphKey, Option<AtlasEntry>>,

    /// Bumped on reset so callers can discard cached lookups.
    pub generation: u32,
}

impl Atlas {
    pub fn new(device: &ID3D11Device) -> Result<Atlas> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: ATLAS_SIZE,
            Height: ATLAS_SIZE,
            MipLevels: 1,
            ArraySize: 1,
            // Coverage is not colour: keep it UNORM so the shader sees the raw
            // values DirectWrite produced and applies its own shaping.
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };

        let mut texture: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
        let texture = texture.expect("CreateTexture2D returned success with no texture");

        let mut srv: Option<ID3D11ShaderResourceView> = None;
        unsafe { device.CreateShaderResourceView(&texture, None, Some(&mut srv))? };
        let srv = srv.expect("CreateShaderResourceView returned success with no view");

        Ok(Atlas {
            texture,
            srv,
            shelf_y: 0,
            shelf_height: 0,
            pen_x: 0,
            map: FxHashMap::default(),
            generation: 0,
        })
    }

    /// Drop every cached glyph. Needed when the font or DPI changes.
    pub fn reset(&mut self) {
        self.map.clear();
        self.shelf_y = 0;
        self.shelf_height = 0;
        self.pen_x = 0;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Fetch a glyph, rasterizing and uploading it on first use.
    pub fn get(
        &mut self,
        ctx: &ID3D11DeviceContext,
        fonts: &mut FontSet,
        key: GlyphKey,
    ) -> Option<AtlasEntry> {
        if let Some(hit) = self.map.get(&key) {
            return *hit;
        }

        let entry = fonts
            .rasterize(key.face, key.glyph)
            .and_then(|bmp| self.insert(ctx, &bmp));

        self.map.insert(key, entry);
        entry
    }

    fn insert(
        &mut self,
        ctx: &ID3D11DeviceContext,
        bmp: &super::font::GlyphBitmap,
    ) -> Option<AtlasEntry> {
        // One texel of padding stops neighbouring glyphs bleeding into each
        // other if sampling ever lands off-centre.
        let pad = 1u32;
        let w = bmp.width;
        let h = bmp.height;
        if w + pad * 2 > ATLAS_SIZE || h + pad * 2 > ATLAS_SIZE {
            return None;
        }

        if self.pen_x + w + pad > ATLAS_SIZE {
            // Start a new shelf.
            self.shelf_y += self.shelf_height + pad;
            self.shelf_height = 0;
            self.pen_x = 0;
        }
        if self.shelf_y + h + pad > ATLAS_SIZE {
            // Out of room: start over. Callers re-request what they need.
            self.reset();
        }

        let x = self.pen_x;
        let y = self.shelf_y;

        let box_ = D3D11_BOX {
            left: x,
            top: y,
            front: 0,
            right: x + w,
            bottom: y + h,
            back: 1,
        };
        unsafe {
            ctx.UpdateSubresource(
                &self.texture,
                0,
                Some(&box_),
                bmp.pixels.as_ptr() as *const _,
                w * 4,
                0,
            );
        }

        self.pen_x = x + w + pad;
        self.shelf_height = self.shelf_height.max(h);

        Some(AtlasEntry {
            u: x as f32,
            v: y as f32,
            width: w as f32,
            height: h as f32,
            left: bmp.left as f32,
            top: bmp.top as f32,
            color: bmp.color,
        })
    }
}
