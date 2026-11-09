//! Metal renderer: terminal state in, three instanced draw calls out.
//!
//! Frame construction is a port of `src/render/mod.rs`: backgrounds are
//! run-length coalesced per row, blank cells emit nothing, and glyphs come from
//! a shelf-packed atlas. What changes on Apple silicon:
//!
//! - Instance data is written straight into shared-storage `MTLBuffer`s, which
//!   the CPU and GPU address in the same unified memory. There is no upload.
//! - A ring of three buffer sets lets the CPU build frame N+1 while the GPU
//!   draws frame N; a slot is reused only after its command buffer completes.
//! - Glyphs are rasterised by Core Text with grayscale coverage, so there is no
//!   dual-source subpixel path.

use std::mem::size_of;
use std::rc::Rc;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::CGSize;
use objc2_core_graphics::{kCGColorSpaceSRGB, CGColorSpace};
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBlendFactor, MTLBuffer, MTLClearColor, MTLCommandBuffer, MTLCommandEncoder,
    MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLDrawable, MTLLibrary,
    MTLLoadAction, MTLOrigin, MTLPixelFormat, MTLPrimitiveType, MTLRegion,
    MTLRenderCommandEncoder, MTLRenderPassDescriptor, MTLRenderPipelineDescriptor,
    MTLRenderPipelineState, MTLResourceOptions, MTLSize, MTLStoreAction, MTLTexture,
    MTLTextureDescriptor, MTLTextureUsage,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use rustc_hash::FxHashMap;
use unicode_width::UnicodeWidthChar;

use super::font::{CellMetrics, FontSet, GlyphBitmap};
use super::images;
use super::selection::Range;
use crate::config::Config;
use crate::term::{Cell, CellFlags, CursorShape, Palette, Term, TermMode, CLUSTER_TAG};

const SHADERS: &str = include_str!("shaders.metal");
const ATLAS_SIZE: u32 = 2048;
const FRAMES_IN_FLIGHT: usize = 3;

#[repr(C)]
#[derive(Clone, Copy)]
struct Globals {
    viewport: [f32; 2],
    atlas_inv: [f32; 2],
    gamma: f32,
    contrast: f32,
    opacity: f32,
    _pad: f32,
}

/// One inline image to draw: where, and which part of its texture.
#[repr(C)]
#[derive(Clone, Copy)]
struct ImageInstance {
    dst: [f32; 4],
    uv: [f32; 4],
}

/// An inline image's texture, made at the size it last showed at.
struct ImageTexture {
    /// `None` when it couldn't be decoded: not tried again.
    texture: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
    size: (u32, u32),
    /// The frame it was last drawn in.
    used: usize,
}

/// Texture memory kept for inline images not on screen.
const IMAGE_CACHE_BYTES: usize = 384 << 20;

#[repr(C)]
#[derive(Clone, Copy)]
struct RectInstance {
    rect: [f32; 4],
    color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GlyphInstance {
    dst: [f32; 4],
    uv: [f32; 2],
    flags: [f32; 2],
    color: [f32; 4],
}

type Device = ProtocolObject<dyn MTLDevice>;
type Buffer = ProtocolObject<dyn MTLBuffer>;
type CommandBuffer = ProtocolObject<dyn MTLCommandBuffer>;

#[derive(Clone, Copy)]
struct AtlasEntry {
    u: f32,
    v: f32,
    width: f32,
    height: f32,
    left: f32,
    top: f32,
    color: bool,
}

/// Shelf-packed glyph cache. Rows of glyphs fill left to right; a glyph that
/// does not fit starts a new shelf. When the texture is full it is cleared and
/// refilled on the next frame, which only happens with thousands of distinct
/// glyphs on screen.
struct Atlas {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    shelf_y: u32,
    shelf_h: u32,
    pen_x: u32,
    map: FxHashMap<(u16, u16), Option<AtlasEntry>>,
    overflowed: bool,
}

impl Atlas {
    fn new(device: &Device) -> Result<Atlas, String> {
        let desc = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::RGBA8Unorm,
                ATLAS_SIZE as usize,
                ATLAS_SIZE as usize,
                false,
            )
        };
        desc.setUsage(MTLTextureUsage::ShaderRead);
        let texture = device
            .newTextureWithDescriptor(&desc)
            .ok_or("could not create the glyph atlas")?;
        Ok(Atlas {
            texture,
            shelf_y: 0,
            shelf_h: 0,
            pen_x: 0,
            map: FxHashMap::default(),
            overflowed: false,
        })
    }

    fn reset(&mut self) {
        self.map.clear();
        self.shelf_y = 0;
        self.shelf_h = 0;
        self.pen_x = 0;
    }

    fn get(&mut self, fonts: &FontSet, face: u16, glyph: u16) -> Option<AtlasEntry> {
        if let Some(hit) = self.map.get(&(face, glyph)) {
            return *hit;
        }
        let entry = fonts.rasterize(face, glyph).and_then(|b| self.insert(&b));
        self.map.insert((face, glyph), entry);
        entry
    }

    fn insert(&mut self, bmp: &GlyphBitmap) -> Option<AtlasEntry> {
        let pad = 1;
        let (w, h) = (bmp.width, bmp.height);
        if w + pad * 2 > ATLAS_SIZE || h + pad * 2 > ATLAS_SIZE {
            return None;
        }
        if self.pen_x + w + pad > ATLAS_SIZE {
            self.shelf_y += self.shelf_h + pad;
            self.shelf_h = 0;
            self.pen_x = 0;
        }
        if self.shelf_y + h + pad > ATLAS_SIZE {
            self.overflowed = true;
            return None;
        }

        let (x, y) = (self.pen_x + pad, self.shelf_y + pad);
        let region = MTLRegion {
            origin: MTLOrigin { x: x as _, y: y as _, z: 0 },
            size: MTLSize { width: w as _, height: h as _, depth: 1 },
        };
        unsafe {
            self.texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::new_unchecked(bmp.data.as_ptr() as *mut _),
                (w * 4) as _,
            );
        }
        self.pen_x += w + pad;
        self.shelf_h = self.shelf_h.max(h);

        Some(AtlasEntry {
            u: x as f32,
            v: y as f32,
            width: w as f32,
            height: h as f32,
            left: bmp.left,
            top: bmp.top,
            color: bmp.color,
        })
    }
}

/// One frame's worth of GPU-visible instance buffers.
#[derive(Default)]
struct Slot {
    bg: Option<Retained<Buffer>>,
    glyphs: Option<Retained<Buffer>>,
    overlay: Option<Retained<Buffer>>,
    in_flight: Option<Retained<CommandBuffer>>,
}

/// Metal objects shared by every window: one device, one queue, and the two
/// pipelines, so opening a window does not recompile the shaders.
pub struct Gpu {
    device: Retained<Device>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    rect_pso: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    glyph_pso: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    image_pso: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
}

impl Gpu {
    pub fn new() -> Result<Rc<Gpu>, String> {
        let device = MTLCreateSystemDefaultDevice().ok_or("no Metal device")?;
        let queue = device.newCommandQueue().ok_or("could not create a command queue")?;
        let source = NSString::from_str(SHADERS);
        let library = device
            .newLibraryWithSource_options_error(&source, None)
            .map_err(|e| format!("shader compile failed: {}", e.localizedDescription()))?;
        let rect_pso = pipeline(&device, &*library, "vs_rect", "fs_rect")?;
        let glyph_pso = pipeline(&device, &*library, "vs_glyph", "fs_glyph")?;
        let image_pso = pipeline(&device, &*library, "vs_image", "fs_image")?;
        Ok(Rc::new(Gpu {
            device,
            queue,
            rect_pso,
            glyph_pso,
            image_pso,
        }))
    }
}

/// One window's renderer: its layer, glyph atlas, fonts and frame buffers.
pub struct Renderer {
    gpu: Rc<Gpu>,
    layer: Retained<CAMetalLayer>,
    atlas: Atlas,
    slots: [Slot; FRAMES_IN_FLIGHT],
    frame: usize,

    pub fonts: FontSet,
    palette: Palette,
    cfg: Config,
    scale: f32,
    size: (f32, f32),
    /// The window shows a tab strip along its bottom edge.
    strip: bool,
    /// When the strip is a glass capsule drawn by AppKit, the height of the
    /// band kept free for it, in pixels; the renderer then draws no strip.
    strip_band: Option<f32>,
    /// Pixels a title bar takes at the top.
    top_inset: f32,

    bg: Vec<RectInstance>,
    glyphs: Vec<GlyphInstance>,
    overlay: Vec<RectInstance>,
    /// Inline images this frame, by image id, drawn in order.
    images: Vec<(u64, ImageInstance)>,
    textures: FxHashMap<u64, ImageTexture>,
}

impl Renderer {
    pub fn new(gpu: Rc<Gpu>, layer: Retained<CAMetalLayer>, cfg: &Config, scale: f32) -> Result<Renderer, String> {
        layer.setDevice(Some(&gpu.device));
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm_sRGB);
        layer.setFramebufferOnly(true);
        // Two drawables: the lowest latency that still lets the GPU work while
        // the display scans out the previous frame.
        layer.setMaximumDrawableCount(2);
        layer.setDisplaySyncEnabled(cfg.render.vsync && !cfg.render.low_latency);
        layer.setOpaque(cfg.window.opacity >= 1.0);
        if let Some(space) = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })) {
            layer.setColorspace(Some(&space));
        }
        layer.setContentsScale(scale as f64);

        let atlas = Atlas::new(&gpu.device)?;
        let fonts = FontSet::new(&cfg.font, scale);

        Ok(Renderer {
            gpu,
            layer,
            atlas,
            slots: Default::default(),
            frame: 0,
            fonts,
            palette: cfg.palette(),
            cfg: cfg.clone(),
            scale,
            size: (1.0, 1.0),
            strip: false,
            top_inset: 0.0,
            strip_band: None,
            bg: Vec::new(),
            glyphs: Vec::new(),
            overlay: Vec::new(),
            images: Vec::new(),
            textures: FxHashMap::default(),
        })
    }

    /// Wait for the display refresh to present, or not: off in a window for
    /// low latency, where the compositor syncs anyway; `vsync` in full screen.
    pub fn set_full_screen(&mut self, full: bool) {
        let sync = self.cfg.render.vsync && (full || !self.cfg.render.low_latency);
        self.layer.setDisplaySyncEnabled(sync);
    }

    pub fn metrics(&self) -> CellMetrics {
        self.fonts.metrics
    }

    /// Drawable size in pixels.
    pub fn resize(&mut self, width_px: f32, height_px: f32) {
        self.size = (width_px.max(1.0), height_px.max(1.0));
        self.layer
            .setDrawableSize(CGSize::new(self.size.0 as f64, self.size.1 as f64));
    }

    /// The window moved to a display with a different backing scale.
    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() < f32::EPSILON {
            return;
        }
        self.scale = scale;
        self.layer.setContentsScale(scale as f64);
        self.fonts = FontSet::new(&self.cfg.font, scale);
        self.atlas.reset();
    }

    /// Pixels at the top of the view taken by a title bar; the grid starts
    /// below them.
    pub fn set_top_inset(&mut self, px: f32) {
        self.top_inset = px.max(0.0);
    }

    pub fn padding(&self) -> (f32, f32) {
        (
            (self.cfg.window.padding_x * self.scale).round(),
            (self.cfg.window.padding_y * self.scale).round(),
        )
    }

    /// Grid dimensions that fit a drawable of the given pixel size, above the
    /// tab strip when there is one.
    pub fn grid_size_for(&self, width_px: f32, height_px: f32) -> (usize, usize) {
        let m = self.fonts.metrics;
        let (pad_x, pad_y) = self.padding();
        let w = (width_px - pad_x * 2.0).max(m.width);
        let h = (height_px - pad_y * 2.0 - self.top_inset - self.strip_height()).max(m.height);
        (
            (w / m.width).floor().max(1.0) as usize,
            (h / m.height).floor().max(1.0) as usize,
        )
    }

    /// Show or hide the tab strip. The caller lays the grid out again.
    pub fn set_strip(&mut self, on: bool) {
        self.strip = on;
    }

    /// Keep `band` pixels free for a strip drawn over the window instead of
    /// drawing one in cells; `None` draws it in cells.
    pub fn set_strip_band(&mut self, band: Option<f32>) {
        self.strip_band = band;
    }

    /// Height of the tab strip in pixels: a row of text with room around it,
    /// enough to aim a click at.
    fn strip_height(&self) -> f32 {
        if !self.strip {
            0.0
        } else if let Some(band) = self.strip_band {
            band
        } else {
            (self.fonts.metrics.height * 1.6).round()
        }
    }

    /// Where tab `i` of `n` starts, in grid columns. Tabs share the width
    /// equally and start on cell boundaries, so labels line up with the text.
    fn strip_col(cols: usize, n: usize, i: usize) -> usize {
        i * cols / n.max(1)
    }

    /// Left and right edges of tab `i` of `n`, in pixels. The outer tabs run
    /// to the window's edges.
    fn strip_edges(&self, cols: usize, n: usize, i: usize) -> (f32, f32) {
        let (pad_x, _) = self.padding();
        let w = self.fonts.metrics.width;
        let x = |i: usize| match i {
            0 => 0.0,
            i if i >= n => self.size.0,
            i => pad_x + Self::strip_col(cols, n, i) as f32 * w,
        };
        (x(i), x(i + 1))
    }

    /// The tab at drawable pixel (`x`, `y`), counted from the top left, if the
    /// point is in the strip.
    pub fn strip_index(&self, x: f32, y: f32, n: usize) -> Option<usize> {
        if !self.strip || self.strip_band.is_some() || n == 0 || y < self.size.1 - self.strip_height() {
            return None;
        }
        let cols = self.grid_size_for(self.size.0, self.size.1).0;
        (0..n).find(|&i| x < self.strip_edges(cols, n, i).1).or(Some(n - 1))
    }

    /// Change the font size in points, keeping the backing scale.
    pub fn set_font_size(&mut self, points: f32) {
        self.cfg.font.size = points;
        self.fonts = FontSet::new(&self.cfg.font, self.scale);
        self.atlas.reset();
    }

    pub fn font_size(&self) -> f32 {
        self.cfg.font.size
    }

    /// Switch colours, for Light and Dark Mode. The caller redraws.
    pub fn set_theme(&mut self, theme: &crate::config::ThemeConfig) {
        self.palette = theme.palette();
        self.cfg.theme = theme.clone();
    }

    fn resolve_colors(&self, cell: &Cell, term: &Term, selected: bool) -> ([u8; 3], [u8; 3]) {
        let theme = &self.cfg.theme;
        let mut fg_color = cell.fg;
        if cell.flags.contains(CellFlags::BOLD) && theme.bold_is_bright {
            fg_color = fg_color.brighten();
        }
        let mut fg = fg_color.resolve(&self.palette, theme.foreground);
        let mut bg = cell.bg.resolve(&self.palette, theme.background);

        let reverse =
            cell.flags.contains(CellFlags::REVERSE) ^ term.mode.contains(TermMode::REVERSE_VIDEO);
        if reverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.flags.contains(CellFlags::DIM) {
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

    /// Where a pane whose top-left cell is (`col`, `row`) of the window's
    /// grid starts, in drawable pixels.
    pub fn pane_origin(&self, col: usize, row: usize) -> (f32, f32) {
        let m = self.fonts.metrics;
        let (pad_x, pad_y) = self.padding();
        (pad_x + col as f32 * m.width, pad_y + self.top_inset + row as f32 * m.height)
    }

    /// Build and present one frame: each pane at its place in the grid, the
    /// dividers between them (pixel rectangles), and the tab strip. With
    /// `cursor_on_layer`, a focused cursor is left out of the frame because a
    /// Core Animation layer draws it.
    /// `presented`, if given, is called with the time (seconds since boot)
    /// the frame reached the display.
    pub fn draw_frame(
        &mut self,
        panes: &[PaneFrame],
        strip: Option<&Strip>,
        dividers: &[[f32; 4]],
        presented: Option<Box<dyn Fn(f64)>>,
    ) {
        if self.atlas.overflowed {
            self.atlas.reset();
            self.atlas.overflowed = false;
        }
        self.bg.clear();
        self.glyphs.clear();
        self.overlay.clear();
        self.images.clear();
        for p in panes {
            let origin = self.pane_origin(p.col, p.row);
            self.build(p.term, origin, p.focused, p.selection, p.cursor_on_layer);
            if p.term.has_images() {
                self.build_images(p.term, origin);
            }
            // Receipt gutter bars sit in the padding left of the text, or for
            // a pane right of a divider, in the divider's cell beside it.
            let (pad_x, _) = self.padding();
            let gutter_x = if p.col == 0 {
                (pad_x * 0.35).round()
            } else {
                (origin.0 - self.fonts.metrics.width * 0.3).round()
            };
            self.build_decor(p.term, origin, gutter_x, p.decor);
            if p.dim {
                // A pane without focus, in a window with focus: its text
                // recedes a little, so the one keys go to stands out.
                let m = self.fonts.metrics;
                let mut veil = srgb(self.cfg.theme.background);
                veil[3] = 0.3;
                let rect = [origin.0, origin.1, p.term.cols() as f32 * m.width, p.term.rows() as f32 * m.height];
                self.overlay.push(RectInstance { rect, color: veil });
            }
        }
        if !dividers.is_empty() {
            let theme = &self.cfg.theme;
            let line: [u8; 3] = std::array::from_fn(|i| {
                (theme.background[i] as f32 + (theme.foreground[i] as f32 - theme.background[i] as f32) * 0.22).round() as u8
            });
            for &rect in dividers {
                self.overlay.push(RectInstance { rect, color: srgb(line) });
            }
        }
        if let Some(strip) = strip.filter(|_| self.strip_band.is_none()) {
            let cols = self.grid_size_for(self.size.0, self.size.1).0;
            self.build_strip(strip, cols);
        }

        let Some(drawable) = self.layer.nextDrawable() else {
            crate::log!("render: no drawable");
            return;
        };


        let slot_idx = self.frame % FRAMES_IN_FLIGHT;
        self.frame = self.frame.wrapping_add(1);
        if let Some(cb) = self.slots[slot_idx].in_flight.take() {
            cb.waitUntilCompleted();
        }

        let device = &self.gpu.device;
        let slot = &mut self.slots[slot_idx];
        let bg = upload(device, &mut slot.bg, &self.bg);
        let glyphs = upload(device, &mut slot.glyphs, &self.glyphs);
        let overlay = upload(device, &mut slot.overlay, &self.overlay);

        let theme = &self.cfg.theme;
        let opacity = self.cfg.window.opacity;
        let lin = |v: u8| -> f64 {
            let c = v as f64 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        let a = opacity as f64;
        let clear = MTLClearColor {
            red: lin(theme.background[0]) * a,
            green: lin(theme.background[1]) * a,
            blue: lin(theme.background[2]) * a,
            alpha: a,
        };

        let pass = MTLRenderPassDescriptor::new();
        let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setTexture(Some(&drawable.texture()));
        attachment.setLoadAction(MTLLoadAction::Clear);
        attachment.setStoreAction(MTLStoreAction::Store);
        attachment.setClearColor(clear);

        let Some(cb) = self.gpu.queue.commandBuffer() else {
            return;
        };
        let Some(enc) = cb.renderCommandEncoderWithDescriptor(&pass) else {
            return;
        };

        let globals = Globals {
            viewport: [self.size.0, self.size.1],
            atlas_inv: [1.0 / ATLAS_SIZE as f32; 2],
            gamma: self.cfg.font.gamma,
            contrast: self.cfg.font.contrast,
            opacity,
            _pad: 0.0,
        };
        let gptr = NonNull::from(&globals).cast();
        unsafe {
            enc.setVertexBytes_length_atIndex(gptr, size_of::<Globals>(), 1);
            enc.setFragmentBytes_length_atIndex(gptr, size_of::<Globals>(), 0);
        }

        let draw = |pso: &ProtocolObject<dyn MTLRenderPipelineState>, buf: &Option<Retained<Buffer>>, n: usize| {
            if n == 0 {
                return;
            }
            if let Some(buf) = buf {
                enc.setRenderPipelineState(pso);
                unsafe {
                    enc.setVertexBuffer_offset_atIndex(Some(buf), 0, 0);
                    enc.drawPrimitives_vertexStart_vertexCount_instanceCount(
                        MTLPrimitiveType::TriangleStrip,
                        0,
                        4,
                        n,
                    );
                }
            }
        };

        draw(&self.gpu.rect_pso, bg, self.bg.len());
        // Images sit on the cell backgrounds, under any text.
        if !self.images.is_empty() {
            enc.setRenderPipelineState(&self.gpu.image_pso);
            for (id, inst) in &self.images {
                let Some(tex) = self.textures.get(id).and_then(|t| t.texture.as_ref()) else { continue };
                unsafe {
                    enc.setVertexBytes_length_atIndex(NonNull::from(inst).cast(), size_of::<ImageInstance>(), 0);
                    enc.setFragmentTexture_atIndex(Some(tex), 0);
                    enc.drawPrimitives_vertexStart_vertexCount_instanceCount(MTLPrimitiveType::TriangleStrip, 0, 4, 1);
                }
            }
        }
        unsafe { enc.setFragmentTexture_atIndex(Some(&self.atlas.texture), 0) };
        draw(&self.gpu.glyph_pso, glyphs, self.glyphs.len());
        draw(&self.gpu.rect_pso, overlay, self.overlay.len());
        enc.endEncoding();

        let drawable: &ProtocolObject<dyn MTLDrawable> = ProtocolObject::from_ref(&*drawable);
        if let Some(f) = presented {
            let block = block2::RcBlock::new(move |d: NonNull<ProtocolObject<dyn MTLDrawable>>| {
                f(unsafe { d.as_ref() }.presentedTime());
            });
            unsafe { drawable.addPresentedHandler(block2::RcBlock::as_ptr(&block)) };
        }
        cb.presentDrawable(drawable);
        cb.commit();
        slot.in_flight = Some(cb);
    }

    /// The pane's inline images: each fitted to its cells keeping its shape,
    /// clipped to the pane, with a texture made at the size it shows at.
    fn build_images(&mut self, term: &Term, origin: (f32, f32)) {
        let m = self.fonts.metrics;
        let pane = (origin.0, origin.1, origin.0 + term.cols() as f32 * m.width, origin.1 + term.rows() as f32 * m.height);
        for v in term.visible_images() {
            let bx = (v.cols as f32 * m.width, v.rows as f32 * m.height);
            let (w, h) = images::contain((v.image.width, v.image.height), bx);
            let (x0, y0) = (origin.0 + v.col as f32 * m.width, origin.1 + v.row as f32 * m.height);
            // The part inside the pane.
            let (cx0, cy0) = (x0.max(pane.0), y0.max(pane.1));
            let (cx1, cy1) = ((x0 + w).min(pane.2), (y0 + h).min(pane.3));
            if cx1 <= cx0 || cy1 <= cy0 {
                continue;
            }
            let uv = [(cx0 - x0) / w, (cy0 - y0) / h, (cx1 - x0) / w, (cy1 - y0) / h];
            let want = (w.round().max(1.0) as u32, h.round().max(1.0) as u32);
            self.image_texture(&v.image, want);
            self.images.push((v.image.id, ImageInstance { dst: [cx0, cy0, cx1 - cx0, cy1 - cy0], uv }));
        }
    }

    /// Make sure there's a texture for `image` at about `want` pixels:
    /// decoded again when it shows at a quite different size (the font size
    /// changed, or the window moved to another display).
    fn image_texture(&mut self, image: &crate::term::image::Image, want: (u32, u32)) {
        let want = (want.0.min(images::MAX_SIDE), want.1.min(images::MAX_SIDE));
        let frame = self.frame;
        if let Some(t) = self.textures.get_mut(&image.id) {
            t.used = frame;
            let near = |a: u32, b: u32| a.abs_diff(b) <= (b / 8).max(2);
            if t.texture.is_none() || (near(t.size.0, want.0) && near(t.size.1, want.1)) {
                return;
            }
        }
        let texture = images::decode(image, want.0, want.1).and_then(|px| {
            let desc = unsafe {
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    MTLPixelFormat::RGBA8Unorm_sRGB,
                    px.width as usize,
                    px.height as usize,
                    false,
                )
            };
            desc.setUsage(MTLTextureUsage::ShaderRead);
            let tex = self.gpu.device.newTextureWithDescriptor(&desc)?;
            let region = MTLRegion {
                origin: MTLOrigin { x: 0, y: 0, z: 0 },
                size: MTLSize { width: px.width as usize, height: px.height as usize, depth: 1 },
            };
            unsafe {
                tex.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                    region,
                    0,
                    NonNull::new(px.data.as_ptr() as *mut _)?,
                    px.width as usize * 4,
                )
            };
            Some(tex)
        });
        if texture.is_none() {
            crate::log!("images: could not decode image {} ({}x{})", image.id, image.width, image.height);
        }
        self.textures.insert(image.id, ImageTexture { texture, size: want, used: frame });
        // Over budget: the least recently drawn go, never one in this frame.
        let bytes = |t: &ImageTexture| t.size.0 as usize * t.size.1 as usize * 4;
        let mut total: usize = self.textures.values().map(bytes).sum();
        while total > IMAGE_CACHE_BYTES {
            let Some((&id, t)) = self.textures.iter().filter(|(_, t)| t.used != frame).min_by_key(|(_, t)| t.used) else { break };
            total -= bytes(t);
            self.textures.remove(&id);
        }
    }

    fn build(&mut self, term: &Term, origin: (f32, f32), focused: bool, selection: Option<Range>, cursor_on_layer: bool) {
        let m = self.fonts.metrics;
        let theme = self.cfg.theme.clone();
        let (pad_x, pad_y) = origin;

        let rows = term.rows();
        let cols = term.cols();
        let cursor = term.cursor_state();
        let at_bottom = term.grid().display_offset() == 0;
        let first_visible = term.grid().history_len().saturating_sub(term.grid().display_offset());
        let selected = |line: usize, x: usize| selection.is_some_and(|r| r.contains(line, x));
        // The focused cursor lives on its own layer when it blinks.
        let draw_cursor = cursor.visible && at_bottom && !(focused && cursor_on_layer);

        for y in 0..rows {
            let row_y = pad_y + y as f32 * m.height;
            let line = first_visible + y;
            let cells = term.grid().row(y).cells();

            // Backgrounds, run-length coalesced; the clear colour is free.
            let mut run_start = 0usize;
            let mut run_color: Option<[u8; 3]> = None;
            for x in 0..=cols {
                let effective = if x < cols {
                    let (_, bg) = self.resolve_colors(&cells[x], term, selected(line, x));
                    (bg != theme.background).then_some(bg)
                } else {
                    None
                };
                if effective != run_color || x == cols {
                    if let Some(c) = run_color {
                        self.bg.push(RectInstance {
                            rect: [
                                pad_x + run_start as f32 * m.width,
                                row_y,
                                (x - run_start) as f32 * m.width,
                                m.height,
                            ],
                            color: srgb(c),
                        });
                    }
                    run_color = effective;
                    run_start = x;
                }
            }

            // Cursor, under the glyph so a block cursor can invert it.
            if draw_cursor && cursor.y == y {
                let cx = pad_x + cursor.x as f32 * m.width;
                let rect = match cursor.shape {
                    CursorShape::Block => [cx, row_y, m.width, m.height],
                    CursorShape::Underline => [
                        cx,
                        row_y + m.height - m.underline_thickness * 2.0,
                        m.width,
                        m.underline_thickness * 2.0,
                    ],
                    CursorShape::Beam => [cx, row_y, (m.width * 0.12).max(self.scale), m.height],
                };
                let c = srgb(theme.cursor);
                if focused {
                    self.bg.push(RectInstance { rect, color: c });
                } else {
                    // Unfocused: a hollow box, visible but clearly not live.
                    let t = self.scale.round().max(1.0);
                    let [x0, y0, w, h] = rect;
                    for r in [[x0, y0, w, t], [x0, y0 + h - t, w, t], [x0, y0, t, h], [x0 + w - t, y0, t, h]] {
                        self.overlay.push(RectInstance { rect: r, color: c });
                    }
                }
            }

            // Glyphs and decorations. Cells before `shaped_until` had their
            // glyphs drawn with an earlier cell's, shaped together.
            let mut shaped_until = 0usize;
            let mut x = 0usize;
            while x < cols {
                let cell = cells[x];
                let span = if cell.flags.contains(CellFlags::WIDE) { 2 } else { 1 };
                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    x += 1;
                    continue;
                }
                let (mut fg, _) = self.resolve_colors(&cell, term, selected(line, x));
                if focused
                    && draw_cursor
                    && cursor.y == y
                    && cursor.x == x
                    && cursor.shape == CursorShape::Block
                {
                    fg = theme.cursor_text;
                }

                let cell_x = pad_x + x as f32 * m.width;
                if x >= shaped_until && !cell.is_empty() && !cell.flags.contains(CellFlags::HIDDEN) {
                    match self.shaped_span(cells, term, x) {
                        Some((end, text, cell_of)) => {
                            let bold = cell.flags.contains(CellFlags::BOLD);
                            let italic = cell.flags.contains(CellFlags::ITALIC);
                            let glyphs = self.fonts.shape(&text, bold, italic);
                            // Each glyph in its own cell's colour.
                            for g in glyphs.iter() {
                                let cx = cell_of.get(g.index as usize).copied().unwrap_or(x);
                                let (mut gfg, _) = self.resolve_colors(&cells[cx], term, selected(line, cx));
                                if focused && draw_cursor && cursor.y == y && cursor.x == cx && cursor.shape == CursorShape::Block {
                                    gfg = theme.cursor_text;
                                }
                                self.push_shaped(g, cell_x, row_y, gfg);
                            }
                            shaped_until = end;
                        }
                        None => self.push_glyphs(&cell, term, cell_x, row_y, fg),
                    }
                }

                let deco_w = m.width * span as f32;
                if cell.flags.intersects(CellFlags::ANY_UNDERLINE) {
                    let color = term
                        .underline_color(cell.underline)
                        .map(|c| c.resolve(&self.palette, fg))
                        .unwrap_or(fg);
                    self.push_underline(cell.flags, cell_x, row_y, deco_w, color, &m);
                }
                if cell.flags.contains(CellFlags::STRIKEOUT) {
                    self.overlay.push(RectInstance {
                        rect: [cell_x, row_y + m.strikeout_y, deco_w, m.strikeout_thickness],
                        color: srgb(fg),
                    });
                }
                if cell.flags.contains(CellFlags::OVERLINE) {
                    self.overlay.push(RectInstance {
                        rect: [cell_x, row_y, deco_w, m.strikeout_thickness],
                        color: srgb(fg),
                    });
                }
                x += span;
            }
        }
    }

    /// Gutter bars at `gutter_x`, left of the text, and receipts at the
    /// right of rows.
    fn build_decor(&mut self, term: &Term, origin: (f32, f32), gutter_x: f32, decor: &Decor) {
        let m = self.fonts.metrics;
        let (pad_x, pad_y) = origin;
        let theme = self.cfg.theme.clone();
        let width = (self.scale * 1.5).round().max(2.0);
        let x = gutter_x;
        // Find matches, under the text: the current one stronger.
        let yellow = self.palette.0[3];
        let mixed = |t: f32| -> [u8; 3] {
            std::array::from_fn(|i| (theme.background[i] as f32 + (yellow[i] as f32 - theme.background[i] as f32) * t).round() as u8)
        };
        for &(row, x0, x1, current) in &decor.highlights {
            if row >= term.rows() || x1 <= x0 {
                continue;
            }
            self.bg.push(RectInstance {
                rect: [pad_x + x0 as f32 * m.width, pad_y + row as f32 * m.height, (x1 - x0) as f32 * m.width, m.height],
                color: srgb(mixed(if current { 0.75 } else { 0.35 })),
            });
        }
        for &(y, outcome) in &decor.gutter {
            let color = match outcome {
                Outcome::Running => self.palette.0[4],
                Outcome::Succeeded => self.palette.0[2],
                Outcome::Failed => self.palette.0[1],
                Outcome::Changed => self.palette.0[3],
            };
            // A changed line's mark is wider, so it stands out from the bar.
            let width = if outcome == Outcome::Changed { width * 2.0 } else { width };
            self.bg.push(RectInstance {
                rect: [x, pad_y + y as f32 * m.height, width, m.height],
                color: srgb(color),
            });
        }

        if !decor.preedit.is_empty() {
            let cursor = term.cursor_state();
            let row_y = pad_y + cursor.y as f32 * m.height;
            let n = decor.preedit.chars().count();
            let x0 = pad_x + cursor.x as f32 * m.width;
            // Cover what is underneath, then draw the text and an underline.
            self.overlay.push(RectInstance {
                rect: [x0, row_y, n as f32 * m.width, m.height],
                color: srgb(theme.background),
            });
            for (i, ch) in decor.preedit.chars().enumerate() {
                let Some((face, glyph)) = self.fonts.glyph_for(ch, false, false) else {
                    continue;
                };
                let Some(e) = self.atlas.get(&self.fonts, face, glyph) else {
                    continue;
                };
                self.glyphs.push(GlyphInstance {
                    dst: [x0 + i as f32 * m.width + e.left, row_y + m.ascent + e.top, e.width, e.height],
                    uv: [e.u, e.v],
                    flags: [if e.color { 1.0 } else { 0.0 }, 0.0],
                    color: srgb(theme.foreground),
                });
            }
            self.overlay.push(RectInstance {
                rect: [x0, row_y + m.underline_y, n as f32 * m.width, m.underline_thickness],
                color: srgb(theme.foreground),
            });
        }

        // Receipts in a muted foreground, never over text: a note is drawn
        // only when the row is empty where it would go.
        let dim = std::array::from_fn::<u8, 3, _>(|i| {
            ((theme.foreground[i] as u16 + theme.background[i] as u16 * 2) / 3) as u8
        });
        if let Some((x, y, text)) = &decor.ghost {
            if *y < term.rows() {
                self.push_text(text, pad_x + *x as f32 * m.width, pad_y + *y as f32 * m.height, dim);
            }
        }
        for (y, text) in &decor.notes {
            if *y >= term.rows() {
                continue;
            }
            let width = text.chars().map(|c| c.width().unwrap_or(1)).sum::<usize>();
            let Some(start) = note_start(term.grid().row(*y).cells(), width) else {
                continue;
            };
            let row_y = pad_y + *y as f32 * m.height;
            self.push_text(text, pad_x + start as f32 * m.width, row_y, dim);
        }
    }

    /// The tab strip along the bottom edge. The tab in front takes the
    /// terminal's own background, so it reads as the same surface as the text
    /// above it; the others sit on a slightly lighter band under a hairline.
    fn build_strip(&mut self, strip: &Strip, cols: usize) {
        let m = self.fonts.metrics;
        let (pad_x, _) = self.padding();
        let theme = self.cfg.theme.clone();
        let mix = |a: [u8; 3], b: [u8; 3], t: f32| -> [u8; 3] {
            std::array::from_fn(|i| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t).round() as u8)
        };
        let band = mix(theme.background, theme.foreground, 0.05);
        let line = mix(theme.background, theme.foreground, 0.14);
        let dim = mix(theme.foreground, theme.background, 0.45);
        let hair = self.scale.round().max(1.0);

        let n = strip.tabs.len();
        let h = self.strip_height();
        let y0 = self.size.1 - h;
        let text_y = y0 + ((h - m.height) / 2.0).round();
        let see_through = |c: [u8; 3], a: f32| {
            let mut v = srgb(c);
            v[3] = a;
            v
        };
        let opacity = self.cfg.window.opacity;

        for (i, (label, unseen)) in strip.tabs.iter().enumerate() {
            let (x0, x1) = self.strip_edges(cols, n, i);
            let active = i == strip.active;
            // The tab in front is left as the window's own background (the
            // clear colour), so over glass it's exactly as see-through as the
            // text area above it. The others get the band, just as see-through.
            if !active {
                self.bg.push(RectInstance { rect: [x0, y0, x1 - x0, h], color: see_through(band, opacity) });
                self.bg.push(RectInstance { rect: [x0, y0, x1 - x0, hair], color: srgb(line) });
                // A short divider between two tabs in the background.
                if i + 1 < n && i + 1 != strip.active {
                    self.bg.push(RectInstance {
                        rect: [x1 - hair, y0 + h * 0.28, hair, h * 0.44],
                        color: srgb(line),
                    });
                }
            }

            // The label, centred in the tab's cells, after a dot when a
            // command finished there unseen.
            let c0 = Self::strip_col(cols, n, i);
            let c1 = Self::strip_col(cols, n, i + 1);
            let mark = if unseen.is_some() { 2 } else { 0 };
            let room = (c1 - c0).saturating_sub(2 + mark);
            if room == 0 {
                continue;
            }
            let text = fit(label, room);
            let width = text.chars().map(|c| c.width().unwrap_or(1)).sum::<usize>() + mark;
            let mut x = pad_x + (c0 + (c1 - c0).saturating_sub(width) / 2) as f32 * m.width;
            if let Some(outcome) = unseen {
                let color = match outcome {
                    Outcome::Failed => self.palette.0[1],
                    _ => self.palette.0[2],
                };
                self.push_text("●", x, text_y, color);
                x += 2.0 * m.width;
            }
            self.push_text(&text, x, text_y, if active { theme.foreground } else { dim });
        }
    }

    /// A line of plain text from pixel (`x`, `row_y`), one cell per column.
    fn push_text(&mut self, text: &str, x: f32, row_y: f32, color: [u8; 3]) {
        let m = self.fonts.metrics;
        let mut pen = x;
        for ch in text.chars() {
            let cells = ch.width().unwrap_or(1).max(1) as f32;
            if ch != ' ' && !is_invisible(ch) {
                if let Some((face, glyph)) = self.fonts.glyph_for(ch, false, false) {
                    if let Some(e) = self.atlas.get(&self.fonts, face, glyph) {
                        self.glyphs.push(GlyphInstance {
                            dst: [pen + e.left, row_y + m.ascent + e.top, e.width, e.height],
                            uv: [e.u, e.v],
                            flags: [if e.color { 1.0 } else { 0.0 }, 0.0],
                            color: srgb(color),
                        });
                    }
                }
            }
            pen += cells * m.width;
        }
    }

    /// Cells from `x` that draw as one piece of shaped text, and that text:
    /// an emoji sequence spread over cells (a ZWJ family, a skin tone, a
    /// flag's two letters), a cluster of several characters, or with a
    /// ligature font, a run of punctuation like `->` or `!==`. `None` for a
    /// cell that draws alone.
    /// Also returns, for each character of the text, the cell it's in.
    fn shaped_span(&self, cells: &[Cell], term: &Term, x: usize) -> Option<(usize, String, Vec<usize>)> {
        // The common case, without allocating: a lone character that no
        // emoji sequence or ligature touches.
        let c = cells[x].ch;
        if c & CLUSTER_TAG == 0 {
            if c < 0x80 {
                let lig = self.fonts.ligatures && LIGATURE_CHARS.contains(&(c as u8));
                let next = cells.get(x + 1).map(|n| n.ch).unwrap_or(0);
                if !lig || next >= 0x80 || !LIGATURE_CHARS.contains(&(next as u8)) {
                    return None;
                }
            } else {
                let after = (x + 1..cells.len()).find(|&j| !cells[j].flags.contains(CellFlags::WIDE_SPACER));
                let head = after.map(|j| cells[j].ch).filter(|h| h & CLUSTER_TAG == 0).unwrap_or(0);
                let modifier = (0x1F3FB..=0x1F3FF).contains(&head);
                let flag = (0x1F1E6..=0x1F1FF).contains(&c) && (0x1F1E6..=0x1F1FF).contains(&head);
                // A cluster next could start with a modifier too.
                let cluster_next = after.is_some_and(|j| cells[j].ch & CLUSTER_TAG != 0);
                if !modifier && !flag && !cluster_next {
                    return None;
                }
            }
        }
        let chars = |c: &Cell| -> Vec<char> {
            if c.ch & CLUSTER_TAG != 0 {
                term.cluster(c.ch).map(<[char]>::to_vec).unwrap_or_default()
            } else {
                char::from_u32(c.ch).into_iter().collect()
            }
        };
        let first = chars(&cells[x]);
        let next_cell = |i: usize| (i..cells.len()).find(|&j| !cells[j].flags.contains(CellFlags::WIDE_SPACER));

        // Ligatures: two or more punctuation cells in one weight and slant.
        let lig = |c: &Cell| c.ch < 0x80 && LIGATURE_CHARS.contains(&(c.ch as u8));
        if self.fonts.ligatures && lig(&cells[x]) {
            let style = cells[x].flags & (CellFlags::BOLD | CellFlags::ITALIC);
            let end = (x..cells.len())
                .find(|&j| !lig(&cells[j]) || cells[j].flags & (CellFlags::BOLD | CellFlags::ITALIC) != style)
                .unwrap_or(cells.len());
            if end - x >= 2 {
                return Some((end, cells[x..end].iter().map(|c| c.ch as u8 as char).collect(), (x..end).collect()));
            }
            return None;
        }

        // Emoji sequences the grid keeps in separate cells, as wide as the
        // shell counts them: joined here only to draw.
        let is_ri = |c: char| ('\u{1F1E6}'..='\u{1F1FF}').contains(&c);
        let mut text: Vec<char> = first.clone();
        let mut cell_of = vec![x; first.len()];
        let mut end = x + 1;
        let mut ris = usize::from(first.len() == 1 && is_ri(first[0]));
        while let Some(j) = next_cell(end) {
            let next = chars(&cells[j]);
            let Some(&head) = next.first() else { break };
            let joins = text.last() == Some(&'\u{200D}')
                || ('\u{1F3FB}'..='\u{1F3FF}').contains(&head)
                || (ris == 1 && next.len() == 1 && is_ri(head));
            if !joins {
                break;
            }
            if is_ri(head) {
                ris += 1;
            }
            cell_of.extend(std::iter::repeat_n(j, next.len()));
            text.extend(next);
            end = j + 1;
        }
        // Any multi-character cluster goes through shaping too, so the font
        // composes or places its marks.
        (end > x + 1 || text.len() > 1).then(|| (end, text.into_iter().collect(), cell_of))
    }

    /// One shaped glyph, at `cell_x` plus its offset.
    fn push_shaped(&mut self, g: &super::font::Shaped, cell_x: f32, row_y: f32, fg: [u8; 3]) {
        let m = self.fonts.metrics;
        let Some(e) = self.atlas.get(&self.fonts, g.face, g.glyph) else {
            return;
        };
        self.glyphs.push(GlyphInstance {
            dst: [cell_x + g.x + e.left, row_y + m.ascent + e.top - g.y, e.width, e.height],
            uv: [e.u, e.v],
            flags: [if e.color { 1.0 } else { 0.0 }, 0.0],
            color: srgb(fg),
        });
    }

    fn push_glyphs(&mut self, cell: &Cell, term: &Term, cell_x: f32, row_y: f32, fg: [u8; 3]) {
        let bold = cell.flags.contains(CellFlags::BOLD);
        let italic = cell.flags.contains(CellFlags::ITALIC);

        let mut buf = [' '; 8];
        let chars: &[char] = if cell.ch & CLUSTER_TAG != 0 {
            match term.cluster(cell.ch) {
                Some(c) => c,
                None => return,
            }
        } else {
            match char::from_u32(cell.ch) {
                Some(c) => {
                    buf[0] = c;
                    &buf[..1]
                }
                None => return,
            }
        };

        let m = self.fonts.metrics;
        let span = if cell.flags.contains(CellFlags::WIDE) { 2.0 } else { 1.0 };

        // Box drawing and block elements are drawn from the cell's geometry so
        // they always meet their neighbours.
        if let [ch] = chars {
            let light = self.scale.round().max(1.0);
            if let Some(pieces) = super::boxdraw::pieces(*ch, cell_x, row_y, m.width, m.height, light) {
                let c = srgb(fg);
                for (rect, alpha) in pieces {
                    self.bg.push(RectInstance {
                        rect,
                        color: [c[0], c[1], c[2], alpha],
                    });
                }
                return;
            }
        }

        for (i, &ch) in chars.iter().enumerate() {
            if ch == ' ' || is_invisible(ch) {
                continue;
            }
            // A skin-tone modifier needs shaping to merge with the emoji before
            // it; drawn on its own it is a coloured square, so show the base
            // emoji alone until shaping lands.
            if ('\u{1F3FB}'..='\u{1F3FF}').contains(&ch) {
                continue;
            }
            let Some((face, glyph)) = self.fonts.glyph_for(ch, bold, italic) else {
                continue;
            };
            let Some(e) = self.atlas.get(&self.fonts, face, glyph) else {
                continue;
            };
            // Combining marks have zero advance and sit to the left of the pen,
            // expecting it to be past the base character already.
            let pen = if i > 0 && e.left < 0.0 && ch.width() == Some(0) {
                cell_x + m.width * span
            } else {
                cell_x
            };
            self.glyphs.push(GlyphInstance {
                dst: [pen + e.left, row_y + m.ascent + e.top, e.width, e.height],
                uv: [e.u, e.v],
                flags: [if e.color { 1.0 } else { 0.0 }, 0.0],
                color: srgb(fg),
            });
        }
    }

    fn push_underline(&mut self, f: CellFlags, x: f32, row_y: f32, w: f32, color: [u8; 3], m: &CellMetrics) {
        let y = row_y + m.underline_y;
        let t = m.underline_thickness;
        let c = srgb(color);
        let mut push = |rect: [f32; 4]| self.overlay.push(RectInstance { rect, color: c });

        if f.contains(CellFlags::DOUBLE_UNDERLINE) {
            push([x, y - t, w, t]);
            push([x, y + t, w, t]);
        } else if f.contains(CellFlags::DOTTED_UNDERLINE) {
            let step = (t * 3.0).max(2.0);
            let mut dx = 0.0;
            while dx < w {
                push([x + dx, y, t.min(w - dx), t]);
                dx += step;
            }
        } else if f.contains(CellFlags::DASHED_UNDERLINE) {
            let dash = (w / 3.0).max(2.0);
            let mut dx = 0.0;
            while dx < w {
                push([x + dx, y, dash.min(w - dx), t]);
                dx += dash * 1.6;
            }
        } else if f.contains(CellFlags::CURLY_UNDERLINE) {
            let seg = (w / 4.0).max(1.0);
            for i in 0..4 {
                let off = if i % 2 == 0 { -t } else { t };
                push([x + i as f32 * seg, y + off, seg, t]);
            }
        } else {
            push([x, y, w, t]);
        }
    }
}

/// Copy `items` into the slot's buffer, growing it to the next power of two
/// when it is too small. Shared storage: the GPU reads exactly these bytes.
fn upload<'a, T: Copy>(
    device: &Device,
    buf: &'a mut Option<Retained<Buffer>>,
    items: &[T],
) -> &'a Option<Retained<Buffer>> {
    if items.is_empty() {
        return buf;
    }
    let bytes = items.len() * size_of::<T>();
    if buf.as_ref().map_or(true, |b| b.length() < bytes) {
        *buf = device.newBufferWithLength_options(
            bytes.next_power_of_two().max(4096),
            MTLResourceOptions::StorageModeShared | MTLResourceOptions::CPUCacheModeWriteCombined,
        );
    }
    if let Some(b) = buf {
        unsafe {
            std::ptr::copy_nonoverlapping(items.as_ptr() as *const u8, b.contents().as_ptr() as *mut u8, bytes);
        }
    }
    buf
}

fn pipeline(
    device: &Device,
    library: &ProtocolObject<dyn MTLLibrary>,
    vs: &str,
    fs: &str,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let vf = library
        .newFunctionWithName(&NSString::from_str(vs))
        .ok_or_else(|| format!("missing shader {vs}"))?;
    let ff = library
        .newFunctionWithName(&NSString::from_str(fs))
        .ok_or_else(|| format!("missing shader {fs}"))?;

    let desc = MTLRenderPipelineDescriptor::new();
    desc.setVertexFunction(Some(&vf));
    desc.setFragmentFunction(Some(&ff));
    let att = unsafe { desc.colorAttachments().objectAtIndexedSubscript(0) };
    att.setPixelFormat(MTLPixelFormat::BGRA8Unorm_sRGB);
    // Premultiplied source-over.
    att.setBlendingEnabled(true);
    att.setSourceRGBBlendFactor(MTLBlendFactor::One);
    att.setSourceAlphaBlendFactor(MTLBlendFactor::One);
    att.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    att.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);

    device
        .newRenderPipelineStateWithDescriptor_error(&desc)
        .map_err(|e| format!("pipeline {vs}/{fs}: {}", e.localizedDescription()))
}

#[inline]
fn srgb(c: [u8; 3]) -> [f32; 4] {
    [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0, 1.0]
}

/// Format characters that take up no space and must not be drawn: variation
/// selectors, zero-width joiners and the like.
fn is_invisible(ch: char) -> bool {
    matches!(ch, '\u{200B}'..='\u{200F}' | '\u{2060}'..='\u{2064}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}')
        || ('\u{E0100}'..='\u{E01EF}').contains(&ch)
}

/// Cell size for a font configuration at a given scale, for sizing a window
/// before its renderer exists.
pub fn grid_metrics(cfg: &crate::config::FontConfig, scale: f32) -> CellMetrics {
    FontSet::new(cfg, scale).metrics
}

/// How a command ended, for its gutter bar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    Running,
    Succeeded,
    Failed,
    /// Rerun diff: this line differs from the command's last run.
    Changed,
}

/// `text` cut to at most `cols` cells, ending in an ellipsis when cut.
fn fit(text: &str, cols: usize) -> String {
    let width = |c: char| c.width().unwrap_or(1);
    if text.chars().map(width).sum::<usize>() <= cols {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        if used + width(c) + 1 > cols {
            break;
        }
        used += width(c);
        out.push(c);
    }
    out.push('…');
    out
}

/// The tab strip: each tab's label and whether a command finished there
/// unseen, and which tab is in front.
pub struct Strip {
    pub tabs: Vec<(String, Option<Outcome>)>,
    pub active: usize,
}

/// Where a note `width` cells wide starts on a row, or `None` if it won't
/// fit without covering text. It goes at the right, leaving a cell to the
/// edge; with a right prompt there (text reaching the right edge after a gap
/// of three or more blank cells, like zsh's RPROMPT or fish's
/// `fish_right_prompt`), it goes just left of the prompt instead. Either way
/// two blank cells stay between it and the text on its left.
pub fn note_start(cells: &[Cell], width: usize) -> Option<usize> {
    let cols = cells.len();
    let blank = |c: &Cell| c.ch == 0 || c.ch == ' ' as u32;
    let used = cells.iter().rposition(|c| !blank(c)).map_or(0, |i| i + 1);
    // Room at the right edge.
    if width + 3 <= cols && used + 2 <= cols - width - 1 {
        return Some(cols - width - 1);
    }
    // A right prompt: text ending at the edge, after a gap of 3+ blanks.
    if used + 2 < cols {
        return None;
    }
    let mut x = used;
    let mut gap = 0;
    let mut prompt_start = None;
    while x > 0 {
        x -= 1;
        if blank(&cells[x]) {
            gap += 1;
            if gap >= 3 {
                prompt_start = Some(x + gap);
                break;
            }
        } else {
            gap = 0;
        }
    }
    let prompt_start = prompt_start?;
    let left = cells[..prompt_start].iter().rposition(|c| !blank(c)).map_or(0, |i| i + 1);
    let end = prompt_start.checked_sub(2)?;
    let start = end.checked_sub(width)?;
    (start >= left + 2).then_some(start)
}

/// Characters that take part in programming ligatures (`->`, `!=`, `|>`,
/// `/*`): only runs of these are shaped as ligatures.
const LIGATURE_CHARS: &[u8] = b"!#$%&*+-./:;<=>?@\\^_|~";

/// One pane of a frame: its terminal and where it starts in the window's
/// grid, in cells.
pub struct PaneFrame<'a> {
    pub term: &'a Term,
    pub col: usize,
    pub row: usize,
    pub focused: bool,
    pub selection: Option<Range>,
    pub cursor_on_layer: bool,
    pub decor: &'a Decor,
    /// Veil it: a pane without focus beside one with it.
    pub dim: bool,
}

/// Shell-integration decorations for one frame, by viewport row.
#[derive(Default)]
pub struct Decor {
    pub gutter: Vec<(usize, Outcome)>,
    /// Receipt text, drawn right-aligned on the row when it fits.
    pub notes: Vec<(usize, String)>,
    /// Text being composed in an input method (a dead key's accent, or
    /// Japanese before conversion), drawn underlined at the cursor.
    pub preedit: String,
    /// An inline suggestion: its text, drawn dim from (column, row).
    pub ghost: Option<(usize, usize, String)>,
    /// Find matches on screen: (row, first column, end column, current).
    pub highlights: Vec<(usize, usize, usize, bool)>,
}

/// What the cursor layer should show: a rectangle in drawable pixels and, for
/// a block cursor, an image of the cell with its character in the cursor-text
/// colour (RGBA8, premultiplied, rows top to bottom).
#[derive(Clone, PartialEq)]
pub struct CursorOverlay {
    pub rect: [f32; 4],
    pub color: [u8; 3],
    pub image: Option<(u32, u32, Vec<u8>)>,
}

impl Renderer {
    /// The cursor as a layer would draw it, or `None` when it should not be
    /// shown (hidden, or scrolled back into history).
    /// `col` and `row` place the terminal's pane in the window's grid.
    pub fn cursor_overlay(&mut self, term: &Term, col: usize, row: usize) -> Option<CursorOverlay> {
        let cursor = term.cursor_state();
        if !cursor.visible || term.grid().display_offset() != 0 {
            return None;
        }
        let m = self.fonts.metrics;
        let (pad_x, pad_y) = self.pane_origin(col, row);
        let theme = self.cfg.theme.clone();
        let x = pad_x + cursor.x as f32 * m.width;
        let y = pad_y + cursor.y as f32 * m.height;

        let rect = match cursor.shape {
            CursorShape::Block => [x, y, m.width, m.height],
            CursorShape::Underline => [
                x,
                y + m.height - m.underline_thickness * 2.0,
                m.width,
                m.underline_thickness * 2.0,
            ],
            CursorShape::Beam => [x, y, (m.width * 0.12).max(self.scale).round(), m.height],
        };
        if cursor.shape != CursorShape::Block {
            return Some(CursorOverlay { rect, color: theme.cursor, image: None });
        }

        // Block: paint the cell in the cursor colour with its character on top
        // in the cursor-text colour, the same inversion the frame would draw.
        let (w, h) = (m.width.max(1.0) as usize, m.height.max(1.0) as usize);
        let mut img = vec![0u8; w * h * 4];
        for px in img.chunks_exact_mut(4) {
            px.copy_from_slice(&[theme.cursor[0], theme.cursor[1], theme.cursor[2], 255]);
        }
        let cell = term.grid().row(cursor.y).cells().get(cursor.x).copied();
        let ch = cell.and_then(|c| {
            if c.ch & CLUSTER_TAG != 0 {
                term.cluster(c.ch).and_then(|cl| cl.first().copied())
            } else {
                c.scalar()
            }
        });
        if let (Some(cell), Some(ch)) = (cell, ch) {
            let bold = cell.flags.contains(CellFlags::BOLD);
            let italic = cell.flags.contains(CellFlags::ITALIC);
            let glyph = (ch != ' ' && super::boxdraw::pieces(ch, 0.0, 0.0, 1.0, 1.0, 1.0).is_none())
                .then(|| self.fonts.glyph_for(ch, bold, italic))
                .flatten()
                .and_then(|(face, g)| self.fonts.rasterize(face, g));
            if let Some(bmp) = glyph {
                let ox = bmp.left.round() as isize;
                let oy = (m.ascent + bmp.top).round() as isize;
                for gy in 0..bmp.height as isize {
                    for gx in 0..bmp.width as isize {
                        let (dx, dy) = (ox + gx, oy + gy);
                        if dx < 0 || dy < 0 || dx >= w as isize || dy >= h as isize {
                            continue;
                        }
                        let s = ((gy * bmp.width as isize + gx) * 4) as usize;
                        let d = ((dy * w as isize + dx) * 4) as usize;
                        let a = bmp.data[s + 3] as u32;
                        let src = if bmp.color {
                            [bmp.data[s], bmp.data[s + 1], bmp.data[s + 2]]
                        } else {
                            theme.cursor_text
                        };
                        for c in 0..3 {
                            img[d + c] = ((src[c] as u32 * a + img[d + c] as u32 * (255 - a)) / 255) as u8;
                        }
                    }
                }
            }
        }
        Some(CursorOverlay {
            rect,
            color: theme.cursor,
            image: Some((w as u32, h as u32, img)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{fit, note_start};
    use crate::term::Cell;

    fn row(text: &str, cols: usize) -> Vec<Cell> {
        let mut cells = vec![Cell::EMPTY; cols];
        for (i, ch) in text.chars().enumerate() {
            cells[i].ch = ch as u32;
        }
        cells
    }

    #[test]
    fn notes_go_left_of_a_right_prompt() {
        // Room at the right: at the right, a cell from the edge.
        assert_eq!(note_start(&row("~ make", 40), 10), Some(29));
        // fish with conda's right prompt: just left of it, a gap kept.
        let r = row(&format!("{:<33}(base)", "~ make"), 40);
        assert_eq!(note_start(&r, 10), Some(21));
        // No room between the command and the right prompt.
        let r = row(&format!("{:<33}(base)", "~ python train.py --lr 3e-4"), 40);
        assert_eq!(note_start(&r, 10), None);
        // A line full of text: nowhere.
        assert_eq!(note_start(&row(&"x".repeat(40), 40), 5), None);
    }

    #[test]
    fn labels_fit_their_tab() {
        assert_eq!(fit("zsh", 10), "zsh");
        assert_eq!(fit("python train.py --epochs 3", 10), "python tr…");
        assert_eq!(fit("python train.py", 10).chars().count(), 10);
        // Wide characters take two cells each.
        assert_eq!(fit("日本語のファイル", 7), "日本語…");
    }
}
