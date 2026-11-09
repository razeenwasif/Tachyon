// Tachyon shaders, Metal edition. A port of src/render/shaders.hlsl.
//
// Same scheme: no vertex buffers. Each pass draws a 4-vertex triangle strip per
// instance, the vertex shader makes the corner from vertex_id, and instance
// data comes from a buffer indexed by instance_id. A full screen of text is
// three draw calls.
//
// The drawable is BGRA8Unorm_sRGB, so blending happens in linear light and the
// hardware encodes on write. CPU colours arrive as straight sRGB and are
// linearised here. Output is premultiplied alpha.
//
// There is no subpixel path: macOS draws grayscale-antialiased text, and so
// does Tachyon on the Mac.

#include <metal_stdlib>
using namespace metal;

struct Globals {
    float2 viewport;      // drawable size in pixels
    float2 atlas_inv;     // 1 / atlas size
    float  gamma;         // coverage shaping exponent
    float  contrast;      // stem darkening, 0..1
    float  opacity;       // default background alpha (the clear colour's)
    float  pad;
};

struct RectInstance {
    float4 rect;   // x, y, w, h in pixels
    float4 color;  // straight sRGB
};

struct GlyphInstance {
    float4 dst;    // x, y, w, h in pixels
    float2 uv;     // atlas top-left in texels
    float2 flags;  // x != 0: the glyph carries its own colour
    float4 color;  // straight sRGB
};

static float2 corner(uint vid) {
    return float2(vid & 1, (vid >> 1) & 1);
}

static float4 to_clip(float2 px, float2 viewport) {
    float2 ndc = px / viewport * 2.0 - 1.0;
    return float4(ndc.x, -ndc.y, 0.0, 1.0);
}

static float3 srgb_to_linear(float3 c) {
    float3 lo = c / 12.92;
    float3 hi = pow(max(c + 0.055, 1e-5) / 1.055, 2.4);
    return mix(lo, hi, step(0.04045, c));
}

// ---- solid rectangles: backgrounds, cursor, underlines ----------------------

struct RectOut {
    float4 pos [[position]];
    float4 color;
};

vertex RectOut vs_rect(uint vid [[vertex_id]],
                       uint iid [[instance_id]],
                       const device RectInstance* rects [[buffer(0)]],
                       constant Globals& g [[buffer(1)]]) {
    RectInstance r = rects[iid];
    RectOut o;
    o.pos = to_clip(r.rect.xy + corner(vid) * r.rect.zw, g.viewport);
    o.color = float4(srgb_to_linear(r.color.rgb), r.color.a);
    return o;
}

// Rectangles carry their own alpha. Over glass, only the default background
// (the clear colour) and the tab strip are see-through; cell colours an app
// asked for, the cursor, selection and box drawing stay solid, like the text.
fragment float4 fs_rect(RectOut in [[stage_in]], constant Globals& g [[buffer(0)]]) {
    float a = in.color.a;
    return float4(in.color.rgb * a, a);
}

// ---- glyphs -----------------------------------------------------------------

struct GlyphOut {
    float4 pos [[position]];
    float2 uv;
    float4 color;
    float  is_color [[flat]];
};

vertex GlyphOut vs_glyph(uint vid [[vertex_id]],
                         uint iid [[instance_id]],
                         const device GlyphInstance* glyphs [[buffer(0)]],
                         constant Globals& g [[buffer(1)]]) {
    GlyphInstance gi = glyphs[iid];
    float2 c = corner(vid);
    GlyphOut o;
    o.pos = to_clip(gi.dst.xy + c * gi.dst.zw, g.viewport);
    o.uv = (gi.uv + c * gi.dst.zw) * g.atlas_inv;
    o.color = float4(srgb_to_linear(gi.color.rgb), gi.color.a);
    o.is_color = gi.flags.x;
    return o;
}

// `gamma` fattens or thins stems; `contrast` lifts the low end so antialiased
// edges do not vanish against a dark background when blending in linear light.
static float shape_coverage(float cov, constant Globals& g) {
    cov = pow(max(cov, 0.0), 1.0 / g.gamma);
    return saturate(cov * (1.0 + g.contrast) - g.contrast * cov * cov);
}

fragment float4 fs_glyph(GlyphOut in [[stage_in]],
                         texture2d<float> atlas [[texture(0)]],
                         constant Globals& g [[buffer(0)]]) {
    constexpr sampler s(coord::normalized, filter::nearest, address::clamp_to_edge);
    float4 t = atlas.sample(s, in.uv);

    // A colour glyph is a finished picture: its own colour, no shaping.
    if (in.is_color > 0.5) {
        float ca = t.a * in.color.a;
        return float4(srgb_to_linear(t.rgb) * ca, ca);
    }

    // Text stays fully opaque even over a translucent background, so coverage
    // alone drives alpha here, without g.opacity.
    float a = shape_coverage(t.a, g) * in.color.a;
    return float4(in.color.rgb * a, a);
}
