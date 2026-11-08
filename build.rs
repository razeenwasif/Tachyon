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

/// Coverage of a shape whose signed distance is `d`, antialiased over `aa` units.
fn cover(d: f32, aa: f32) -> f32 {
    (0.5 - d / aa).clamp(0.0, 1.0)
}

/// Approximate signed distance to a superellipse ("squircle") of half-extent
/// `h` and exponent `e`. Not a true Euclidean distance, but its gradient is
/// close to 1 along the edge, which is all the antialiasing needs.
fn sd_squircle(px: f32, py: f32, h: f32, e: f32) -> f32 {
    (px.abs().powf(e) + py.abs().powf(e)).powf(1.0 / e) - h
}

/// The mark: a block cursor on a near-black squircle, with a motion trail
/// fading off to the left and a faint blue glow behind it. Drawn in normalised
/// [-1, 1] space and supersampled, so it stays crisp at every size. At 32 px
/// and below the trail and glow are dropped and the cursor grows, because they
/// turn to mush at that scale.
///
/// Matches `assets/icon/tachyon.svg` (the 1024 pt master, 412 pt half-extent),
/// rescaled to the tile half-extent below.
fn draw_icon(size: u32) -> IconImage {
    // Supersample harder at small sizes where each pixel carries more weight.
    let ss: u32 = if size <= 32 { 4 } else { 3 };
    let n = size as f32;
    let small = size <= 32;

    let bg_top = hex(0x1C1D22);
    let bg_bot = hex(0x08080A);
    let glow = hex(0x2F7CFF);
    let cursor = hex(0xF2F4F8);

    // Geometry, in normalised units where the tile spans [-1, 1].
    let tile_half = 0.94;
    let k = tile_half / 412.0; // master-SVG units -> normalised units

    // Block cursor: centre, half-extents, corner radius.
    let (bcx, bhw, bhh, br) = if small {
        (40.0 * k, 95.0 * k, 165.0 * k, 26.0 * k)
    } else {
        (145.0 * k, 75.0 * k, 128.0 * k, 20.0 * k)
    };
    // Trail runs from here to the cursor's left edge.
    let trail_x0 = -300.0 * k;
    let block_left = bcx - bhw;
    let glow_r = 330.0 * k;

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
                    let d_tile = sd_squircle(x, y, tile_half, 5.0);
                    let a_tile = cover(d_tile, aa);
                    if a_tile <= 0.0 {
                        continue;
                    }

                    let t = (y * 0.5 + 0.5).clamp(0.0, 1.0);
                    let mut col = mix(bg_top, bg_bot, t);

                    // Hairline highlight along the top edge so the tile reads
                    // as a surface against a dark Dock or taskbar.
                    if !small {
                        let rim = cover(-(d_tile + 0.012), aa) * (1.0 - cover(-d_tile, aa));
                        let fade = (1.0 - t / 0.35).clamp(0.0, 1.0);
                        col = mix(col, hex(0xFFFFFF), rim * 0.16 * fade);
                    }

                    if !small {
                        // --- glow behind the cursor ---
                        let gd = ((x - bcx).powi(2) + y * y).sqrt() / glow_r;
                        let g = (1.0 - gd).clamp(0.0, 1.0);
                        // Weights here are in linear light. The SVG's 0.34 glow
                        // and 0.07..0.30 trail opacities blend in sRGB, and over
                        // a near-black tile they land at roughly a third of
                        // that in linear terms, so these are scaled to match.
                        col = mix(col, glow, 0.10 * g * g);

                        // --- motion trail ---
                        let d_trail = sd_round_box(
                            x - (trail_x0 + block_left) * 0.5,
                            y,
                            (block_left - trail_x0) * 0.5,
                            bhh,
                            0.0,
                        );
                        let a_trail = cover(d_trail, aa);
                        if a_trail > 0.0 {
                            let s = ((x - trail_x0) / (block_left - trail_x0)).clamp(0.0, 1.0);
                            let o = if s < 0.7 {
                                0.012 * s / 0.7
                            } else {
                                0.012 + 0.078 * (s - 0.7) / 0.3
                            };
                            col = mix(col, cursor, a_trail * o);
                        }
                    }

                    // --- block cursor ---
                    let d_block = sd_round_box(x - bcx, y, bhw, bhh, br);
                    let a_block = cover(d_block, aa);
                    if a_block > 0.0 {
                        col = mix(col, cursor, a_block);
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
