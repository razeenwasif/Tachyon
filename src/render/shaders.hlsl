// Tachyon shaders.
//
// Three primitive kinds share one vertex layout convention: nothing is bound as
// a vertex buffer at all. Each draw is `DrawInstanced(4, N)` over a triangle
// strip, and the vertex shader synthesises corner positions from SV_VertexID
// while pulling per-instance data out of a StructuredBuffer. That removes input
// assembler work entirely and lets a whole screen of text go out in three
// draw calls.
//
// The render target is bound through an _SRGB view, so everything below works
// in linear light and the hardware handles encoding on write. That is what
// makes blending gamma-correct without a manual conversion in the shader.

// ---------------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------------

cbuffer Globals : register(b0)
{
    float2 gViewport;      // render target size in pixels
    float2 gAtlasInvSize;  // 1 / atlas dimensions

    float  gGamma;         // coverage shaping exponent
    float  gContrast;      // stem darkening, 0..1
    float  gOpacity;       // window alpha
    float  gPad0;
};

struct RectInstance
{
    float4 rect;   // x, y, w, h in pixels
    float4 color;  // straight sRGB, 0..1
};

struct GlyphInstance
{
    float4 dst;    // x, y, w, h in pixels
    float2 uv;     // atlas top-left in texels
    float2 flags;  // x != 0 -> the glyph carries its own colour
    float4 color;  // foreground, straight sRGB
};

StructuredBuffer<RectInstance>  gRects  : register(t0);
StructuredBuffer<GlyphInstance> gGlyphs : register(t1);
Texture2D<float4>               gAtlas  : register(t2);
SamplerState                    gPoint  : register(s0);

// Corner of a 2-triangle strip: 0=(0,0) 1=(1,0) 2=(0,1) 3=(1,1).
float2 corner(uint vid)
{
    return float2(vid & 1, (vid >> 1) & 1);
}

// Pixel space -> clip space. Y is flipped because our grid grows downwards.
float4 to_clip(float2 px)
{
    float2 ndc = px / gViewport * 2.0 - 1.0;
    return float4(ndc.x, -ndc.y, 0.0, 1.0);
}

// sRGB -> linear. Applied to CPU-supplied colours so all arithmetic below
// happens in linear light.
float3 srgb_to_linear(float3 c)
{
    float3 lo = c / 12.92;
    float3 hi = pow(max(c + 0.055, 1e-5) / 1.055, 2.4);
    return lerp(lo, hi, step(0.04045, c));
}

// ---------------------------------------------------------------------------
// Solid rectangles: backgrounds, underlines, strikethrough, cursor
// ---------------------------------------------------------------------------

struct RectVSOut
{
    float4 pos   : SV_Position;
    float4 color : COLOR0;
};

RectVSOut vs_rect(uint vid : SV_VertexID, uint iid : SV_InstanceID)
{
    RectInstance inst = gRects[iid];
    float2 c = corner(vid);

    RectVSOut o;
    o.pos = to_clip(inst.rect.xy + c * inst.rect.zw);
    o.color = float4(srgb_to_linear(inst.color.rgb), inst.color.a);
    return o;
}

// Output is premultiplied: the blend state is ONE / INV_SRC_ALPHA, and a
// composition swap chain interprets the buffer as premultiplied alpha.
float4 ps_rect(RectVSOut i) : SV_Target
{
    float a = i.color.a * gOpacity;
    return float4(i.color.rgb * a, a);
}

// ---------------------------------------------------------------------------
// Glyphs
// ---------------------------------------------------------------------------

struct GlyphVSOut
{
    float4 pos   : SV_Position;
    float2 uv    : TEXCOORD0;
    float4 color : COLOR0;
    float  isColor : TEXCOORD1;
};

GlyphVSOut vs_glyph(uint vid : SV_VertexID, uint iid : SV_InstanceID)
{
    GlyphInstance inst = gGlyphs[iid];
    float2 c = corner(vid);

    GlyphVSOut o;
    o.pos = to_clip(inst.dst.xy + c * inst.dst.zw);
    // Sample at texel centres so point sampling never straddles a boundary.
    o.uv = (inst.uv + c * inst.dst.zw) * gAtlasInvSize;
    o.color = float4(srgb_to_linear(inst.color.rgb), inst.color.a);
    o.isColor = inst.flags.x;
    return o;
}

// Shape raw coverage. `gGamma` fattens or thins stems; `gContrast` lifts the
// low end so antialiased edges do not disappear against a dark background,
// which is the classic complaint about physically-linear text blending.
float3 shape_coverage(float3 cov)
{
    cov = pow(max(cov, 0.0), 1.0 / gGamma);
    return saturate(cov * (1.0 + gContrast) - gContrast * cov * cov);
}

// --- grayscale path -------------------------------------------------------
// One coverage value per pixel; ordinary source-alpha blending.

float4 ps_glyph_gray(GlyphVSOut i) : SV_Target
{
    float4 texel = gAtlas.SampleLevel(gPoint, i.uv, 0);

    // A colour glyph is already the finished picture: use its own sRGB colour
    // and alpha, and do not apply coverage shaping, which is a text-contrast
    // tool and would posterise an emoji.
    if (i.isColor > 0.5)
    {
        float ca = texel.a * i.color.a;
        return float4(srgb_to_linear(texel.rgb) * ca, ca);
    }

    // DirectWrite hands us RGB coverage even when we want grey; averaging the
    // three channels is a better estimator of true coverage than taking one.
    float cov = shape_coverage((texel.r + texel.g + texel.b).xxx / 3.0).r;

    // Text stays fully opaque even when the background is see-through: a
    // translucent terminal should show the desktop behind the *background*, not
    // behind the letters, or it becomes unreadable. So coverage alone drives
    // alpha here, without gOpacity.
    float a = cov * i.color.a;
    return float4(i.color.rgb * a, a);
}

// --- subpixel path --------------------------------------------------------
// Per-channel coverage needs a per-channel blend factor, which fixed-function
// blending can only express through dual-source output: SV_Target1 becomes the
// blend weight, so the pipeline computes
//     dst = src0 * src1 + dst * (1 - src1)
// independently for R, G and B. This is what gives ClearType-quality edges
// without resolving the background in the shader.

struct DualOut
{
    float4 color : SV_Target0;
    float4 blend : SV_Target1;
};

DualOut ps_glyph_subpixel(GlyphVSOut i)
{
    float4 texel = gAtlas.SampleLevel(gPoint, i.uv, 0);
    DualOut o;

    if (i.isColor > 0.5)
    {
        // Uniform blend weight across the channels reduces the dual-source
        // pipeline to ordinary source-over, which is what a colour glyph wants.
        float ca = texel.a * i.color.a;
        o.color = float4(srgb_to_linear(texel.rgb), 1.0);
        o.blend = float4(ca, ca, ca, ca);
        return o;
    }

    float3 cov = shape_coverage(texel.rgb) * i.color.a;
    o.color = float4(i.color.rgb, 1.0);
    o.blend = float4(cov, max(max(cov.r, cov.g), cov.b));
    return o;
}
