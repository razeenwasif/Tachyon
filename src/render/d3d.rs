//! Direct3D 11 device, swap chain and pipeline objects.
//!
//! # Presentation
//!
//! The swap chain uses the flip model (`FLIP_DISCARD`) with a waitable frame
//! latency object, and requests tearing support where the adapter offers it.
//! That combination is what actually determines typing latency:
//!
//!  * Flip model hands our buffer straight to the compositor instead of copying
//!    it into a DWM-owned surface, removing a full frame of latency.
//!  * The waitable object lets the render thread block until the display is
//!    ready for the next frame, so we begin drawing as late as possible and the
//!    pixels we present are as fresh as possible. Blocking *before* drawing
//!    rather than after presenting is the whole trick.
//!  * Tearing, when enabled, skips the compositor's vblank gate entirely.
//!
//! # Why D3D11 rather than 12
//!
//! A terminal frame is three draw calls and a couple of hundred kilobytes of
//! instance data. There is no CPU submission cost worth removing, and D3D11's
//! implicit synchronisation is a real reduction in the amount of code that can
//! be wrong. The wins here are in what we *don't* draw, not in how we submit.

use windows::core::{s, Interface, Result, HRESULT, PCSTR};
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};

/// Per-frame shader constants. Must match `cbuffer Globals` in shaders.hlsl.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Globals {
    pub viewport: [f32; 2],
    pub atlas_inv_size: [f32; 2],
    pub gamma: f32,
    pub contrast: f32,
    pub opacity: f32,
    pub _pad: f32,
}

/// A solid rectangle. Must match `RectInstance`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RectInstance {
    pub rect: [f32; 4],
    pub color: [f32; 4],
}

/// A textured glyph quad. Must match `GlyphInstance`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GlyphInstance {
    pub dst: [f32; 4],
    pub uv: [f32; 2],
    /// `[0]` is non-zero for a colour glyph. Kept as floats so the struct
    /// stays 16-byte aligned for the structured buffer.
    pub flags: [f32; 2],
    pub color: [f32; 4],
}

/// A GPU-visible, CPU-written array of `T`.
struct DynBuffer {
    buffer: Option<ID3D11Buffer>,
    srv: Option<ID3D11ShaderResourceView>,
    capacity: usize,
    stride: u32,
}

impl DynBuffer {
    fn new(stride: u32) -> DynBuffer {
        DynBuffer {
            buffer: None,
            srv: None,
            capacity: 0,
            stride,
        }
    }

    /// Grow to hold at least `count` elements. Growth is geometric so a
    /// resizing window does not reallocate every frame.
    fn reserve(&mut self, device: &ID3D11Device, count: usize) -> Result<()> {
        if count <= self.capacity {
            return Ok(());
        }
        let capacity = count.next_power_of_two().max(1024);

        let desc = D3D11_BUFFER_DESC {
            ByteWidth: (capacity * self.stride as usize) as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
            StructureByteStride: self.stride,
        };

        let mut buffer: Option<ID3D11Buffer> = None;
        unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer))? };
        let buffer = buffer.expect("CreateBuffer succeeded with no buffer");

        let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_UNKNOWN,
            ViewDimension: D3D11_SRV_DIMENSION_BUFFER,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D11_BUFFER_SRV {
                    Anonymous1: D3D11_BUFFER_SRV_0 { FirstElement: 0 },
                    Anonymous2: D3D11_BUFFER_SRV_1 {
                        NumElements: capacity as u32,
                    },
                },
            },
        };
        let mut srv: Option<ID3D11ShaderResourceView> = None;
        unsafe { device.CreateShaderResourceView(&buffer, Some(&srv_desc), Some(&mut srv))? };

        self.buffer = Some(buffer);
        self.srv = srv;
        self.capacity = capacity;
        Ok(())
    }

    /// Overwrite the contents. `WRITE_DISCARD` hands us a fresh allocation so
    /// the GPU is never stalled waiting for the previous frame to finish
    /// reading.
    fn upload<T: Copy>(&self, ctx: &ID3D11DeviceContext, data: &[T]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let Some(buffer) = &self.buffer else {
            return Ok(());
        };
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(buffer, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
            std::ptr::copy_nonoverlapping(
                data.as_ptr() as *const u8,
                mapped.pData as *mut u8,
                std::mem::size_of_val(data),
            );
            ctx.Unmap(buffer, 0);
        }
        Ok(())
    }
}

pub struct Gpu {
    pub device: ID3D11Device,
    pub ctx: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain1,
    /// Present with tearing when the adapter supports it and vsync is off.
    allow_tearing: bool,
    /// Signalled when the display is ready for another frame.
    pub frame_latency_waitable: Option<windows::Win32::Foundation::HANDLE>,

    /// Held for the lifetime of the window when compositing. Dropping these
    /// tears down the visual tree and the window goes black, so they live here
    /// even though nothing reads them after setup.
    _dcomp: Option<(IDCompositionDevice, IDCompositionTarget, IDCompositionVisual)>,
    /// True when the swap chain has a meaningful alpha channel.
    translucent: bool,

    rtv: Option<ID3D11RenderTargetView>,
    width: u32,
    height: u32,

    vs_rect: ID3D11VertexShader,
    ps_rect: ID3D11PixelShader,
    vs_glyph: ID3D11VertexShader,
    ps_glyph_gray: ID3D11PixelShader,
    ps_glyph_subpixel: ID3D11PixelShader,

    blend_alpha: ID3D11BlendState,
    blend_dual: ID3D11BlendState,
    sampler: ID3D11SamplerState,
    raster: ID3D11RasterizerState,

    globals_buffer: ID3D11Buffer,
    rects: DynBuffer,
    /// Decorations and the cursor outline live in their own buffer rather than
    /// sharing one with the backgrounds. See the note in `render`.
    overlay: DynBuffer,
    glyphs: DynBuffer,
}

const SHADER_SRC: &str = include_str!("shaders.hlsl");

impl Gpu {
    /// `translucent` selects the composition path: the swap chain gains a
    /// premultiplied alpha channel and is presented through DirectComposition
    /// rather than bound to the window directly. That is what lets the DWM
    /// backdrop show through the parts of our frame we leave transparent.
    pub fn new(hwnd: HWND, vsync: bool, translucent: bool) -> Result<Gpu> {
        unsafe {
            let mut flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
            if cfg!(debug_assertions) && std::env::var_os("TACHYON_D3D_DEBUG").is_some() {
                flags |= D3D11_CREATE_DEVICE_DEBUG;
            }

            let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
            let mut device: Option<ID3D11Device> = None;
            let mut ctx: Option<ID3D11DeviceContext> = None;

            let mut hr = D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                flags,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            );
            if hr.is_err() {
                // WARP keeps us running on machines with no usable GPU driver,
                // in a VM, or over some remote desktop stacks.
                hr = D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_WARP,
                    HMODULE::default(),
                    flags,
                    Some(&levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut ctx),
                );
            }
            hr?;

            let device = device.expect("device");
            let ctx = ctx.expect("context");

            // Walk up to the factory that created our adapter.
            let dxgi_device: IDXGIDevice = device.cast()?;
            let adapter = dxgi_device.GetAdapter()?;
            let factory: IDXGIFactory2 = adapter.GetParent()?;

            // Tearing is an adapter capability, not a swap chain one.
            let allow_tearing = match factory.cast::<IDXGIFactory5>() {
                Ok(f5) => {
                    let mut supported: u32 = 0;
                    f5.CheckFeatureSupport(
                        DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                        &mut supported as *mut _ as *mut _,
                        std::mem::size_of::<u32>() as u32,
                    )
                    .is_ok()
                        && supported != 0
                }
                Err(_) => false,
            };

            let mut flags_sc = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32;
            if allow_tearing {
                flags_sc |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32;
            }

            // A composition swap chain must size itself to the visual, so it
            // wants STRETCH scaling; an hwnd chain uses NONE so the back buffer
            // maps 1:1 to pixels.
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);

            let desc = DXGI_SWAP_CHAIN_DESC1 {
                // Composition chains reject zero dimensions.
                Width: if translucent {
                    (rect.right - rect.left).max(1) as u32
                } else {
                    0
                },
                Height: if translucent {
                    (rect.bottom - rect.top).max(1) as u32
                } else {
                    0
                },
                // The flip model rejects _SRGB swap chain formats, so the chain
                // is UNORM and we attach an _SRGB render target view to it.
                // Blending then happens in linear light regardless.
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: if translucent {
                    DXGI_SCALING_STRETCH
                } else {
                    DXGI_SCALING_NONE
                },
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: if translucent {
                    DXGI_ALPHA_MODE_PREMULTIPLIED
                } else {
                    DXGI_ALPHA_MODE_IGNORE
                },
                Flags: flags_sc,
            };

            let (swap_chain, dcomp) = if translucent {
                let swap_chain = factory.CreateSwapChainForComposition(&device, &desc, None)?;

                // Minimal visual tree: one visual, holding the swap chain,
                // bound to the window's composition target.
                let dcomp_device: IDCompositionDevice =
                    DCompositionCreateDevice(&dxgi_device)?;
                let target = dcomp_device.CreateTargetForHwnd(hwnd, true)?;
                let visual = dcomp_device.CreateVisual()?;
                visual.SetContent(&swap_chain)?;
                target.SetRoot(&visual)?;
                dcomp_device.Commit()?;

                (swap_chain, Some((dcomp_device, target, visual)))
            } else {
                (
                    factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)?,
                    None,
                )
            };

            // Alt+Enter fullscreen is DXGI's, not ours; we handle window state.
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);

            let (waitable, _) = match swap_chain.cast::<IDXGISwapChain2>() {
                Ok(sc2) => {
                    // One frame of latency is the minimum the API allows and
                    // the right choice here: our frames are cheap enough that
                    // we do not need CPU/GPU overlap to hit refresh rate, so we
                    // spend the slack on latency instead.
                    let _ = sc2.SetMaximumFrameLatency(1);
                    (Some(sc2.GetFrameLatencyWaitableObject()), ())
                }
                Err(_) => (None, ()),
            };

            let (vs_rect, vs_rect_bc) = compile_vs(&device, s!("vs_rect"))?;
            let _ = vs_rect_bc;
            let ps_rect = compile_ps(&device, s!("ps_rect"))?;
            let (vs_glyph, _) = compile_vs(&device, s!("vs_glyph"))?;
            let ps_glyph_gray = compile_ps(&device, s!("ps_glyph_gray"))?;
            let ps_glyph_subpixel = compile_ps(&device, s!("ps_glyph_subpixel"))?;

            let blend_alpha = make_blend(&device, false)?;
            let blend_dual = make_blend(&device, true)?;

            let sampler_desc = D3D11_SAMPLER_DESC {
                // Glyphs are blitted 1:1, so point sampling is not a
                // compromise -- it is the exact right answer, and it keeps
                // coverage values untouched.
                Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler: Option<ID3D11SamplerState> = None;
            device.CreateSamplerState(&sampler_desc, Some(&mut sampler))?;

            let raster_desc = D3D11_RASTERIZER_DESC {
                FillMode: D3D11_FILL_SOLID,
                CullMode: D3D11_CULL_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            };
            let mut raster: Option<ID3D11RasterizerState> = None;
            device.CreateRasterizerState(&raster_desc, Some(&mut raster))?;

            let cb_desc = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<Globals>() as u32,
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            let mut globals_buffer: Option<ID3D11Buffer> = None;
            device.CreateBuffer(&cb_desc, None, Some(&mut globals_buffer))?;

            let mut gpu = Gpu {
                device,
                ctx,
                swap_chain,
                allow_tearing: allow_tearing && !vsync,
                frame_latency_waitable: waitable,
                _dcomp: dcomp,
                translucent,
                rtv: None,
                width: 0,
                height: 0,
                vs_rect,
                ps_rect,
                vs_glyph,
                ps_glyph_gray,
                ps_glyph_subpixel,
                blend_alpha,
                blend_dual,
                sampler: sampler.expect("sampler"),
                raster: raster.expect("rasterizer"),
                globals_buffer: globals_buffer.expect("constant buffer"),
                rects: DynBuffer::new(std::mem::size_of::<RectInstance>() as u32),
                overlay: DynBuffer::new(std::mem::size_of::<RectInstance>() as u32),
                glyphs: DynBuffer::new(std::mem::size_of::<GlyphInstance>() as u32),
            };
            gpu.create_rtv()?;
            Ok(gpu)
        }
    }

    fn create_rtv(&mut self) -> Result<()> {
        unsafe {
            let back: ID3D11Texture2D = self.swap_chain.GetBuffer(0)?;
            let desc = D3D11_RENDER_TARGET_VIEW_DESC {
                Format: DXGI_FORMAT_B8G8R8A8_UNORM_SRGB,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
                },
            };
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            self.device
                .CreateRenderTargetView(&back, Some(&desc), Some(&mut rtv))?;
            self.rtv = rtv;

            let mut td = D3D11_TEXTURE2D_DESC::default();
            back.GetDesc(&mut td);
            self.width = td.Width;
            self.height = td.Height;
        }
        Ok(())
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 || (width == self.width && height == self.height) {
            return Ok(());
        }
        unsafe {
            // Every reference to the back buffer must be released first.
            self.rtv = None;
            self.ctx.OMSetRenderTargets(None, None);
            self.ctx.ClearState();

            let desc = self.swap_chain.GetDesc1()?;
            self.swap_chain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(desc.Flags as i32),
            )?;
        }
        self.create_rtv()
    }

    /// Block until the presentation engine wants another frame.
    ///
    /// Doing this *before* building the frame -- rather than presenting and
    /// immediately looping -- is what keeps the pixels fresh: we sleep through
    /// the idle part of the refresh interval and start work as late as we
    /// safely can.
    pub fn wait_for_frame(&self, timeout_ms: u32) {
        if let Some(h) = self.frame_latency_waitable {
            unsafe {
                windows::Win32::System::Threading::WaitForSingleObjectEx(h, timeout_ms, true);
            }
        }
    }

    /// Draw one frame.
    pub fn render(
        &mut self,
        globals: &Globals,
        clear: [f32; 4],
        bg_rects: &[RectInstance],
        glyphs: &[GlyphInstance],
        overlay_rects: &[RectInstance],
        atlas_srv: &ID3D11ShaderResourceView,
        subpixel: bool,
    ) -> Result<()> {
        // Dual-source subpixel coverage assumes an opaque destination. Over a
        // translucent window the final colour behind each glyph is chosen by
        // the compositor, so per-channel blending would fringe against the
        // wrong backdrop.
        let subpixel = subpixel && !self.translucent;
        let Some(rtv) = self.rtv.clone() else {
            return Ok(());
        };

        // Backgrounds and overlays get separate buffers, each drawn from
        // instance zero.
        //
        // The tempting alternative -- one buffer, with the overlay pass using
        // `StartInstanceLocation` to skip past the backgrounds -- is silently
        // wrong. `SV_InstanceID` does not include `StartInstanceLocation`; that
        // offset only applies to per-instance data fetched by the input
        // assembler. Since the shader indexes a `StructuredBuffer` with
        // `SV_InstanceID` directly, the overlay pass would re-read the *first*
        // N background rectangles and paint them over the text.
        self.rects.reserve(&self.device, bg_rects.len().max(1))?;
        self.overlay.reserve(&self.device, overlay_rects.len().max(1))?;
        self.glyphs.reserve(&self.device, glyphs.len().max(1))?;
        self.rects.upload(&self.ctx, bg_rects)?;
        self.overlay.upload(&self.ctx, overlay_rects)?;
        self.glyphs.upload(&self.ctx, glyphs)?;

        unsafe {
            // Constants.
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx.Map(
                &self.globals_buffer,
                0,
                D3D11_MAP_WRITE_DISCARD,
                0,
                Some(&mut mapped),
            )?;
            std::ptr::copy_nonoverlapping(
                globals as *const Globals as *const u8,
                mapped.pData as *mut u8,
                std::mem::size_of::<Globals>(),
            );
            self.ctx.Unmap(&self.globals_buffer, 0);

            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: self.width as f32,
                Height: self.height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            self.ctx.RSSetViewports(Some(&[viewport]));
            self.ctx.RSSetState(&self.raster);
            self.ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            self.ctx.ClearRenderTargetView(&rtv, &clear);

            self.ctx
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            // No input layout and no vertex buffer: the vertex shader builds
            // corners from SV_VertexID.
            self.ctx.IASetInputLayout(None);

            let cb = Some(self.globals_buffer.clone());
            self.ctx.VSSetConstantBuffers(0, Some(&[cb.clone()]));
            self.ctx.PSSetConstantBuffers(0, Some(&[cb]));
            self.ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));

            self.ctx.OMSetBlendState(&self.blend_alpha, None, 0xFFFF_FFFF);

            // --- pass 1: coalesced background rectangles ---
            if !bg_rects.is_empty() {
                self.ctx
                    .VSSetShaderResources(0, Some(&[self.rects.srv.clone()]));
                self.ctx.VSSetShader(&self.vs_rect, None);
                self.ctx.PSSetShader(&self.ps_rect, None);
                self.ctx.DrawInstanced(4, bg_rects.len() as u32, 0, 0);
            }

            // --- pass 2: glyphs ---
            if !glyphs.is_empty() {
                self.ctx.VSSetShaderResources(
                    0,
                    Some(&[self.rects.srv.clone(), self.glyphs.srv.clone()]),
                );
                self.ctx
                    .PSSetShaderResources(2, Some(&[Some(atlas_srv.clone())]));
                self.ctx.VSSetShader(&self.vs_glyph, None);
                if subpixel {
                    self.ctx.OMSetBlendState(&self.blend_dual, None, 0xFFFF_FFFF);
                    self.ctx.PSSetShader(&self.ps_glyph_subpixel, None);
                } else {
                    self.ctx.PSSetShader(&self.ps_glyph_gray, None);
                }
                self.ctx.DrawInstanced(4, glyphs.len() as u32, 0, 0);
            }

            // --- pass 3: decorations and cursor, over the text ---
            if !overlay_rects.is_empty() {
                self.ctx.OMSetBlendState(&self.blend_alpha, None, 0xFFFF_FFFF);
                self.ctx
                    .VSSetShaderResources(0, Some(&[self.overlay.srv.clone()]));
                self.ctx.VSSetShader(&self.vs_rect, None);
                self.ctx.PSSetShader(&self.ps_rect, None);
                self.ctx.DrawInstanced(4, overlay_rects.len() as u32, 0, 0);
            }
        }
        Ok(())
    }

    pub fn present(&self, vsync: bool) -> HRESULT {
        unsafe {
            let (interval, flags) = if vsync || !self.allow_tearing {
                (1u32, DXGI_PRESENT(0))
            } else {
                (0u32, DXGI_PRESENT_ALLOW_TEARING)
            };
            self.swap_chain.Present(interval, flags)
        }
    }
}

fn compile_shader(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    unsafe {
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        let hr = D3DCompile(
            SHADER_SRC.as_ptr() as *const _,
            SHADER_SRC.len(),
            s!("shaders.hlsl"),
            None,
            None,
            entry,
            target,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        );
        if hr.is_err() {
            if let Some(e) = errors {
                let msg = std::slice::from_raw_parts(
                    e.GetBufferPointer() as *const u8,
                    e.GetBufferSize(),
                );
                let text = String::from_utf8_lossy(msg);
                // Shader compilation failing is a build-time bug, not a runtime
                // condition; surface it loudly.
                panic!("shader compilation failed:\n{text}");
            }
            hr?;
        }
        Ok(code.expect("shader blob"))
    }
}

fn compile_vs(device: &ID3D11Device, entry: PCSTR) -> Result<(ID3D11VertexShader, ID3DBlob)> {
    let blob = compile_shader(entry, s!("vs_5_0"))?;
    unsafe {
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize());
        let mut shader: Option<ID3D11VertexShader> = None;
        device.CreateVertexShader(bytes, None, Some(&mut shader))?;
        Ok((shader.expect("vertex shader"), blob))
    }
}

fn compile_ps(device: &ID3D11Device, entry: PCSTR) -> Result<ID3D11PixelShader> {
    let blob = compile_shader(entry, s!("ps_5_0"))?;
    unsafe {
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize());
        let mut shader: Option<ID3D11PixelShader> = None;
        device.CreatePixelShader(bytes, None, Some(&mut shader))?;
        Ok(shader.expect("pixel shader"))
    }
}

/// Alpha blending, or dual-source blending for per-channel subpixel coverage.
fn make_blend(device: &ID3D11Device, dual_source: bool) -> Result<ID3D11BlendState> {
    let rt = if dual_source {
        D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            SrcBlend: D3D11_BLEND_SRC1_COLOR,
            DestBlend: D3D11_BLEND_INV_SRC1_COLOR,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_SRC1_ALPHA,
            DestBlendAlpha: D3D11_BLEND_INV_SRC1_ALPHA,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        }
    } else {
        // Premultiplied source-over. The shaders emit `rgb * a` so that the
        // colour and alpha channels stay consistent, which is what the
        // compositor requires when the swap chain is DXGI_ALPHA_MODE_PREMULTIPLIED.
        // For an opaque window it degenerates to ordinary source-over.
        D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            SrcBlend: D3D11_BLEND_ONE,
            DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_ONE,
            DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        }
    };

    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0] = rt;

    let mut state: Option<ID3D11BlendState> = None;
    unsafe { device.CreateBlendState(&desc, Some(&mut state))? };
    Ok(state.expect("blend state"))
}
