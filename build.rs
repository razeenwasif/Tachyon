//! Build script for Tachyon.
//!
//! Two jobs:
//!
//!  1. Draw the application icon procedurally (no binary blobs checked in).
//!  2. Emit a Win32 resource file containing that icon plus the side-by-side
//!     manifest, and hand it to the linker.
//!
//! Note that we assemble the COFF `.res` container by hand rather than shelling
//! out to `rc.exe` / `llvm-rc` / `windres`. `link.exe` accepts a `.res` on its
//! command line directly, so this keeps the build working on machines that have
//! the MSVC linker but no Windows SDK resource compiler on PATH -- and it makes
//! cross-building from WSL possible.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Resource type ids (winuser.h)
// ---------------------------------------------------------------------------
const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_MANIFEST: u16 = 24;

/// Explorer picks the *numerically lowest* icon id as the file's display icon,
/// so the group must be id 1.
const GROUP_ICON_ID: u16 = 1;
const MANIFEST_ID: u16 = 1;

/// Sizes baked into the icon. 256 is what Explorer's extra-large view and the
/// Win11 taskbar (at 200% scaling) actually sample.
const ICON_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/tachyon.manifest");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    // Always produce a standalone .ico: the installer points shortcuts at it,
    // and it is handy for docs.
    let images: Vec<IconImage> = ICON_SIZES.iter().map(|&s| draw_icon(s)).collect();
    let ico_path = out_dir.join("tachyon.ico");
    fs::write(&ico_path, build_ico_file(&images)).expect("write .ico");

    // Mirror it into assets/ so it is available without digging through target/.
    let assets_ico = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/tachyon.ico");
    if let Some(parent) = assets_ico.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&assets_ico, build_ico_file(&images));
    println!("cargo:rustc-env=TACHYON_ICO_PATH={}", ico_path.display());

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/tachyon.manifest"))
        .expect("assets/tachyon.manifest is missing");

    let res_path = out_dir.join("tachyon.res");
    fs::write(&res_path, build_res_file(&images, &manifest)).expect("write .res");

    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_env == "msvc" {
        // link.exe consumes .res files natively.
        println!("cargo:rustc-link-arg-bins={}", res_path.display());
    } else {
        // The GNU toolchain wants a COFF object; convert with windres if we can,
        // otherwise carry on without an embedded icon rather than failing.
        let obj = out_dir.join("tachyon_res.o");
        let ok = std::process::Command::new("x86_64-w64-mingw32-windres")
            .args(["-I", "res", "-O", "coff"])
            .arg(&res_path)
            .arg(&obj)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            println!("cargo:rustc-link-arg-bins={}", obj.display());
        } else {
            println!("cargo:warning=windres not found; building without embedded icon/manifest");
        }
    }
}

// ===========================================================================
// Icon artwork
// ===========================================================================

struct IconImage {
    size: u32,
    /// Bottom-up BGRA, straight (un-premultiplied) alpha, `size * size` pixels.
    bgra: Vec<u8>,
}

/// Linear-space RGB colour, 0..1.
#[derive(Clone, Copy)]
struct Rgb(f32, f32, f32);

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

fn hex(v: u32) -> Rgb {
    let r = ((v >> 16) & 0xFF) as f32 / 255.0;
    let g = ((v >> 8) & 0xFF) as f32 / 255.0;
    let b = (v & 0xFF) as f32 / 255.0;
    Rgb(srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b))
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    Rgb(
        a.0 + (b.0 - a.0) * t,
        a.1 + (b.1 - a.1) * t,
        a.2 + (b.2 - a.2) * t,
    )
}

/// Signed distance from `p` to the rounded rectangle centred at the origin with
/// half-extents `b` and corner radius `r`.
fn sd_round_box(px: f32, py: f32, bx: f32, by: f32, r: f32) -> f32 {
    let qx = px.abs() - bx + r;
    let qy = py.abs() - by + r;
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

/// Signed distance from `p` to the capsule (thick line segment) `a`..`b`.
fn sd_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32, r: f32) -> f32 {
    let pax = px - ax;
    let pay = py - ay;
    let bax = bx - ax;
    let bay = by - ay;
    let denom = bax * bax + bay * bay;
    let h = if denom > 0.0 {
        ((pax * bax + pay * bay) / denom).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let dx = pax - bax * h;
    let dy = pay - bay * h;
    (dx * dx + dy * dy).sqrt() - r
}

/// Coverage of a shape whose signed distance is `d`, antialiased over `aa` units.
fn cover(d: f32, aa: f32) -> f32 {
    (0.5 - d / aa).clamp(0.0, 1.0)
}

/// The mark: a dark rounded tile carrying a prompt chevron and a cursor bar,
/// with a speed streak trailing off the chevron. Drawn in normalised [-1, 1]
/// space and supersampled, so it stays crisp at every size.
fn draw_icon(size: u32) -> IconImage {
    // Supersample harder at small sizes where each pixel carries more weight.
    let ss: u32 = if size <= 32 { 4 } else { 3 };
    let n = size as f32;

    let bg_top = hex(0x1C2534);
    let bg_bot = hex(0x090C12);
    let accent_a = hex(0x5EE7FF); // cyan
    let accent_b = hex(0xA97BFF); // violet
    let cursor = hex(0x7DF9C4); // mint

    // Geometry, in normalised units where the tile spans [-1, 1].
    let tile_half = 0.94;
    let tile_radius = 0.42;
    let stroke = 0.135;

    // Chevron ">" vertices.
    let (cx0, cy0) = (-0.50, -0.42);
    let (cx1, cy1) = (-0.03, 0.00);
    let (cx2, cy2) = (-0.50, 0.42);
    // Cursor bar.
    let (bx0, by0) = (0.22, 0.42);
    let (bx1, by1) = (0.62, 0.42);

    let mut rgba_lin = vec![0f32; (size * size * 4) as usize];

    for py in 0..size {
        for px in 0..size {
            let mut acc = [0f32; 4];
            for sy in 0..ss {
                for sx in 0..ss {
                    let fx = px as f32 + (sx as f32 + 0.5) / ss as f32;
                    let fy = py as f32 + (sy as f32 + 0.5) / ss as f32;
                    // Normalised device coords, y down.
                    let x = fx / n * 2.0 - 1.0;
                    let y = fy / n * 2.0 - 1.0;
                    // One pixel expressed in normalised units, for AA width.
                    let aa = 2.0 / n * 1.1;

                    // --- tile ---
                    let d_tile = sd_round_box(x, y, tile_half, tile_half, tile_radius);
                    let a_tile = cover(d_tile, aa);
                    if a_tile <= 0.0 {
                        continue;
                    }

                    let t = (y * 0.5 + 0.5).clamp(0.0, 1.0);
                    let mut col = mix(bg_top, bg_bot, t);

                    // Subtle inner top highlight so the tile reads as a surface.
                    let rim = cover(-(d_tile + 0.055), aa) * (1.0 - cover(-d_tile, aa));
                    col = mix(col, hex(0x39506B), rim * 0.55 * (1.0 - t));

                    // --- speed streak (behind the chevron) ---
                    let d_streak = sd_segment(x, y, -0.86, 0.0, -0.30, 0.0, stroke * 0.42);
                    let a_streak = cover(d_streak, aa) * 0.55;
                    if a_streak > 0.0 {
                        // Fade the streak out towards the left edge.
                        let fade = ((x + 0.86) / 0.56).clamp(0.0, 1.0);
                        col = mix(col, accent_a, a_streak * fade);
                    }

                    // --- chevron ---
                    let d_ch = sd_segment(x, y, cx0, cy0, cx1, cy1, stroke)
                        .min(sd_segment(x, y, cx1, cy1, cx2, cy2, stroke));
                    let a_ch = cover(d_ch, aa);
                    if a_ch > 0.0 {
                        // Gradient along the chevron, cyan at the tip.
                        let g = ((x + 0.55) / 0.60).clamp(0.0, 1.0);
                        col = mix(col, mix(accent_b, accent_a, g), a_ch);
                    }

                    // --- cursor bar ---
                    let d_bar = sd_segment(x, y, bx0, by0, bx1, by1, stroke * 0.80);
                    let a_bar = cover(d_bar, aa);
                    if a_bar > 0.0 {
                        col = mix(col, cursor, a_bar);
                    }

                    acc[0] += col.0 * a_tile;
                    acc[1] += col.1 * a_tile;
                    acc[2] += col.2 * a_tile;
                    acc[3] += a_tile;
                }
            }

            let inv = 1.0 / (ss * ss) as f32;
            let i = ((py * size + px) * 4) as usize;
            rgba_lin[i] = acc[0] * inv;
            rgba_lin[i + 1] = acc[1] * inv;
            rgba_lin[i + 2] = acc[2] * inv;
            rgba_lin[i + 3] = acc[3] * inv;
        }
    }

    // Pack bottom-up BGRA with straight alpha.
    let mut bgra = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        let src_row = size - 1 - y;
        for x in 0..size {
            let s = ((src_row * size + x) * 4) as usize;
            let d = ((y * size + x) * 4) as usize;
            let a = rgba_lin[s + 3];
            let (r, g, b) = if a > 0.0001 {
                // Un-premultiply before encoding.
                (
                    rgba_lin[s] / a,
                    rgba_lin[s + 1] / a,
                    rgba_lin[s + 2] / a,
                )
            } else {
                (0.0, 0.0, 0.0)
            };
            let enc = |v: f32| (linear_to_srgb(v.clamp(0.0, 1.0)) * 255.0 + 0.5) as u8;
            bgra[d] = enc(b);
            bgra[d + 1] = enc(g);
            bgra[d + 2] = enc(r);
            bgra[d + 3] = (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
    }

    IconImage { size, bgra }
}

/// Encode one icon image as an `ICONIMAGE`: a BITMAPINFOHEADER whose height is
/// doubled, the 32bpp colour bits, then a 1bpp AND mask. The mask is fully
/// transparent-zero because the alpha channel does the work, but Windows still
/// requires it to be present and row-padded to 4 bytes.
fn encode_icon_image(img: &IconImage) -> Vec<u8> {
    let size = img.size;
    let mask_stride = ((size + 31) / 32 * 4) as usize;
    let mut out = Vec::with_capacity(40 + img.bgra.len() + mask_stride * size as usize);

    // BITMAPINFOHEADER
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(size as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&((size * 2) as i32).to_le_bytes()); // biHeight (XOR+AND)
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&(img.bgra.len() as u32).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    out.extend_from_slice(&img.bgra);
    out.resize(out.len() + mask_stride * size as usize, 0);
    out
}

/// Standalone `.ico` container (ICONDIR + ICONDIRENTRY[] + images).
fn build_ico_file(images: &[IconImage]) -> Vec<u8> {
    let encoded: Vec<Vec<u8>> = images.iter().map(encode_icon_image).collect();

    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type = icon
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());

    // Images start after the directory.
    let mut offset = 6 + 16 * images.len() as u32;
    for (img, data) in images.iter().zip(&encoded) {
        // 256 is encoded as 0.
        let dim = if img.size >= 256 { 0u8 } else { img.size as u8 };
        out.push(dim); // width
        out.push(dim); // height
        out.push(0); // colour count (0 = truecolour)
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bit count
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for data in &encoded {
        out.extend_from_slice(data);
    }
    out
}

/// The `RT_GROUP_ICON` payload: same directory as a `.ico`, except each entry
/// ends with a 16-bit resource id instead of a 32-bit file offset.
fn build_group_icon(images: &[IconImage], first_id: u16) -> Vec<u8> {
    let encoded: Vec<Vec<u8>> = images.iter().map(encode_icon_image).collect();
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    for (i, (img, data)) in images.iter().zip(&encoded).enumerate() {
        let dim = if img.size >= 256 { 0u8 } else { img.size as u8 };
        out.push(dim);
        out.push(dim);
        out.push(0);
        out.push(0);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(first_id + i as u16).to_le_bytes());
    }
    out
}

// ===========================================================================
// COFF .res container
// ===========================================================================

fn align4(v: usize) -> usize {
    (v + 3) & !3
}

/// Append one resource entry in `.res` wire format.
///
/// Layout, all little-endian:
/// ```text
///   DWORD data_size
///   DWORD header_size          // through `characteristics`, inclusive
///   type  : 0xFFFF, WORD id    (ordinal form)
///   name  : 0xFFFF, WORD id
///   <pad to 4>
///   DWORD data_version
///   WORD  memory_flags
///   WORD  language_id
///   DWORD version
///   DWORD characteristics
///   <data, padded to 4>
/// ```
fn push_res_entry(out: &mut Vec<u8>, ty: u16, name: u16, lang: u16, flags: u16, data: &[u8]) {
    // 8 bytes of sizes + 4 for ordinal type + 4 for ordinal name + 16 trailer.
    let header_size: u32 = 8 + 4 + 4 + 16;

    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&header_size.to_le_bytes());
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&name.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // data_version
    out.extend_from_slice(&flags.to_le_bytes()); // memory_flags
    out.extend_from_slice(&lang.to_le_bytes()); // language_id
    out.extend_from_slice(&0u32.to_le_bytes()); // version
    out.extend_from_slice(&0u32.to_le_bytes()); // characteristics
    out.extend_from_slice(data);
    out.resize(align4(out.len()), 0);
}

fn build_res_file(images: &[IconImage], manifest: &[u8]) -> Vec<u8> {
    // MOVEABLE | PURE | DISCARDABLE -- what rc.exe emits for icons.
    const FLAGS_ICON: u16 = 0x1030;
    const FLAGS_MANIFEST: u16 = 0x0030;
    // MAKELANGID(LANG_ENGLISH, SUBLANG_ENGLISH_US)
    const LANG_EN_US: u16 = 0x0409;

    let mut out = Vec::new();

    // Every .res starts with a null entry acting as a format marker.
    out.extend_from_slice(&0u32.to_le_bytes()); // data_size
    out.extend_from_slice(&32u32.to_le_bytes()); // header_size
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    // Individual images. Ids start at 1 and the group takes GROUP_ICON_ID in a
    // different type namespace, so there is no collision.
    let first_icon_id: u16 = 1;
    for (i, img) in images.iter().enumerate() {
        let data = encode_icon_image(img);
        push_res_entry(
            &mut out,
            RT_ICON,
            first_icon_id + i as u16,
            LANG_EN_US,
            FLAGS_ICON,
            &data,
        );
    }

    let group = build_group_icon(images, first_icon_id);
    push_res_entry(
        &mut out,
        RT_GROUP_ICON,
        GROUP_ICON_ID,
        LANG_EN_US,
        FLAGS_ICON,
        &group,
    );

    push_res_entry(
        &mut out,
        RT_MANIFEST,
        MANIFEST_ID,
        LANG_EN_US,
        FLAGS_MANIFEST,
        manifest,
    );

    out
}

// Keep `Write` in scope for the error path below without warning noise.
#[allow(dead_code)]
fn _unused(mut w: impl Write) {
    let _ = w.flush();
}
