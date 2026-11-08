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

pub struct Renderer {
    device: Retained<Device>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    layer: Retained<CAMetalLayer>,
    rect_pso: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    glyph_pso: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    atlas: Atlas,
    slots: [Slot; FRAMES_IN_FLIGHT],
    frame: usize,

    pub fonts: FontSet,
    palette: Palette,
    cfg: Config,
    scale: f32,
    size: (f32, f32),

    bg: Vec<RectInstance>,
    glyphs: Vec<GlyphInstance>,
    overlay: Vec<RectInstance>,
}

impl Renderer {
    pub fn new(layer: Retained<CAMetalLayer>, cfg: &Config, scale: f32) -> Result<Renderer, String> {
        let device = MTLCreateSystemDefaultDevice().ok_or("no Metal device")?;
        let queue = device.newCommandQueue().ok_or("could not create a command queue")?;

        layer.setDevice(Some(&device));
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm_sRGB);
        layer.setFramebufferOnly(true);
        // Two drawables: the lowest latency that still lets the GPU work while
        // the display scans out the previous frame.
        layer.setMaximumDrawableCount(2);
        layer.setDisplaySyncEnabled(cfg.render.vsync);
        layer.setOpaque(cfg.window.opacity >= 1.0);
        if let Some(space) = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })) {
            layer.setColorspace(Some(&space));
        }
        layer.setContentsScale(scale as f64);

        let source = NSString::from_str(SHADERS);
        let library = device
            .newLibraryWithSource_options_error(&source, None)
            .map_err(|e| format!("shader compile failed: {}", e.localizedDescription()))?;
        let rect_pso = pipeline(&device, &*library, "vs_rect", "fs_rect")?;
        let glyph_pso = pipeline(&device, &*library, "vs_glyph", "fs_glyph")?;

        let atlas = Atlas::new(&device)?;
        let fonts = FontSet::new(&cfg.font, scale);

        Ok(Renderer {
            device,
            queue,
            layer,
            rect_pso,
            glyph_pso,
            atlas,
            slots: Default::default(),
            frame: 0,
            fonts,
            palette: cfg.palette(),
            cfg: cfg.clone(),
            scale,
            size: (1.0, 1.0),
            bg: Vec::new(),
            glyphs: Vec::new(),
            overlay: Vec::new(),
        })
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

    pub fn padding(&self) -> (f32, f32) {
        (
            (self.cfg.window.padding_x * self.scale).round(),
            (self.cfg.window.padding_y * self.scale).round(),
        )
    }

    /// Grid dimensions that fit a drawable of the given pixel size.
    pub fn grid_size_for(&self, width_px: f32, height_px: f32) -> (usize, usize) {
        let m = self.fonts.metrics;
        let (pad_x, pad_y) = self.padding();
        let w = (width_px - pad_x * 2.0).max(m.width);
        let h = (height_px - pad_y * 2.0).max(m.height);
        (
            (w / m.width).floor().max(1.0) as usize,
            (h / m.height).floor().max(1.0) as usize,
        )
    }

    fn resolve_colors(&self, cell: &Cell, term: &Term) -> ([u8; 3], [u8; 3]) {
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
        (fg, bg)
    }

    /// Build and present one frame.
    pub fn draw(&mut self, term: &Term, focused: bool) {
        if self.atlas.overflowed {
            self.atlas.reset();
            self.atlas.overflowed = false;
        }
        self.build(term, focused);

        let Some(drawable) = self.layer.nextDrawable() else {
            crate::log!("render: no drawable");
            return;
        };


        let slot_idx = self.frame % FRAMES_IN_FLIGHT;
        self.frame = self.frame.wrapping_add(1);
        if let Some(cb) = self.slots[slot_idx].in_flight.take() {
            cb.waitUntilCompleted();
        }

        let device = &self.device;
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

        let Some(cb) = self.queue.commandBuffer() else {
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

        draw(&self.rect_pso, bg, self.bg.len());
        unsafe { enc.setFragmentTexture_atIndex(Some(&self.atlas.texture), 0) };
        draw(&self.glyph_pso, glyphs, self.glyphs.len());
        draw(&self.rect_pso, overlay, self.overlay.len());
        enc.endEncoding();

        let drawable: &ProtocolObject<dyn MTLDrawable> = ProtocolObject::from_ref(&*drawable);
        cb.presentDrawable(drawable);
        cb.commit();
        slot.in_flight = Some(cb);
    }

    fn build(&mut self, term: &Term, focused: bool) {
        let m = self.fonts.metrics;
        let theme = self.cfg.theme.clone();
        let (pad_x, pad_y) = self.padding();

        self.bg.clear();
        self.glyphs.clear();
        self.overlay.clear();

        let rows = term.rows();
        let cols = term.cols();
        let cursor = term.cursor_state();
        let at_bottom = term.grid().display_offset() == 0;

        for y in 0..rows {
            let row_y = pad_y + y as f32 * m.height;
            let cells = term.grid().row(y).cells();

            // Backgrounds, run-length coalesced; the clear colour is free.
            let mut run_start = 0usize;
            let mut run_color: Option<[u8; 3]> = None;
            for x in 0..=cols {
                let effective = if x < cols {
                    let (_, bg) = self.resolve_colors(&cells[x], term);
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
            if cursor.visible && at_bottom && cursor.y == y {
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

            // Glyphs and decorations.
            let mut x = 0usize;
            while x < cols {
                let cell = cells[x];
                let span = if cell.flags.contains(CellFlags::WIDE) { 2 } else { 1 };
                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    x += 1;
                    continue;
                }
                let (mut fg, _) = self.resolve_colors(&cell, term);
                if focused
                    && cursor.visible
                    && at_bottom
                    && cursor.y == y
                    && cursor.x == x
                    && cursor.shape == CursorShape::Block
                {
                    fg = theme.cursor_text;
                }

                let cell_x = pad_x + x as f32 * m.width;
                if !cell.is_empty() && !cell.flags.contains(CellFlags::HIDDEN) {
                    self.push_glyphs(&cell, term, cell_x, row_y, fg);
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
