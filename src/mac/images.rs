//! Decoding inline images for the renderer: PNG, JPEG, GIF (its first
//! frame) and anything else ImageIO reads, or kitty's raw pixels, drawn at the
//! size they show at, as premultiplied RGBA in sRGB.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_foundation::{CFData, CFRetained, CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    kCGColorSpaceSRGB, CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGImage,
    CGImageAlphaInfo, CGInterpolationQuality,
};

use crate::term::image::{Format, Image};

#[link(name = "ImageIO", kind = "framework")]
extern "C" {
    fn CGImageSourceCreateWithData(data: &CFData, options: *const c_void) -> *mut CFType;
    fn CGImageSourceGetCount(source: *const CFType) -> usize;
    fn CGImageSourceCreateImageAtIndex(source: *const CFType, index: usize, options: *const c_void) -> *mut CGImage;
}

/// The largest side a texture is made with.
pub const MAX_SIDE: u32 = 8192;
const MAX_ANIM_FRAMES: usize = 120;
const MAX_ANIM_RGBA_BYTES: usize = 16 << 20;

/// An image's pixels, `width` by `height`, premultiplied RGBA.
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// One or more decoded frames (`delays_ms` non-empty only for animated images).
pub struct DecodedImage {
    pub frames: Vec<Pixels>,
    pub delays_ms: Vec<u32>,
}

/// The image as a CGImage, at its own size.
fn cg_image(img: &Image) -> Option<CFRetained<CGImage>> {
    match img.format {
        Format::Encoded => {
            let data = CFData::from_bytes(&img.data);
            let source = NonNull::new(unsafe { CGImageSourceCreateWithData(&data, std::ptr::null()) })?;
            let source = unsafe { CFRetained::from_raw(source) };
            let image = NonNull::new(unsafe { CGImageSourceCreateImageAtIndex(&*source, 0, std::ptr::null()) })?;
            Some(unsafe { CFRetained::from_raw(image) })
        }
        Format::Rgb | Format::Rgba => {
            let (w, h) = (img.width as usize, img.height as usize);
            let bpp = if img.format == Format::Rgb { 3 } else { 4 };
            // Premultiplied RGBA, which is what a bitmap context holds.
            let mut px = Vec::with_capacity(w * h * 4);
            for p in img.data.chunks_exact(bpp).take(w * h) {
                let a = if bpp == 4 { p[3] } else { 255 };
                let m = |c: u8| ((c as u32 * a as u32 + 127) / 255) as u8;
                px.extend_from_slice(&[m(p[0]), m(p[1]), m(p[2]), a]);
            }
            let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
            let ctx = unsafe {
                CGBitmapContextCreate(px.as_mut_ptr().cast(), w, h, 8, w * 4, Some(&space), CGImageAlphaInfo::PremultipliedLast.0)
            }?;
            // Copies the pixels, so `px` can go.
            CGBitmapContextCreateImage(Some(&ctx))
        }
    }
}

fn rasterize_cg_image(image: &CGImage, width: u32, height: u32) -> Option<Pixels> {
    let (w, h) = (width.clamp(1, MAX_SIDE) as usize, height.clamp(1, MAX_SIDE) as usize);
    let mut data = vec![0u8; w * h * 4];
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    let ctx = unsafe {
        CGBitmapContextCreate(data.as_mut_ptr().cast(), w, h, 8, w * 4, Some(&space), CGImageAlphaInfo::PremultipliedLast.0)
    }?;
    CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
    let rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64));
    CGContext::draw_image(Some(&ctx), rect, Some(image));
    drop(ctx);
    Some(Pixels { width: w as u32, height: h as u32, data })
}

/// The image drawn at `width` by `height` pixels.
pub fn decode(img: &Image, width: u32, height: u32) -> Option<Pixels> {
    let image = cg_image(img)?;
    rasterize_cg_image(&image, width, height)
}

/// Decode all frames of `img` (animated GIFs return multiple frames + per-frame
/// delays in milliseconds; static images return a single frame).
pub fn decode_frames(img: &Image, width: u32, height: u32) -> Option<DecodedImage> {
    if img.format == Format::Encoded {
        let data = CFData::from_bytes(&img.data);
        if let Some(source) = NonNull::new(unsafe { CGImageSourceCreateWithData(&data, std::ptr::null()) }) {
            let source = unsafe { CFRetained::from_raw(source) };
            let total_count = unsafe { CGImageSourceGetCount(&*source) };
            if total_count > 1 {
                let count = total_count.min(MAX_ANIM_FRAMES);
                let mut w = width.clamp(1, MAX_SIDE);
                let mut h = height.clamp(1, MAX_SIDE);
                let est = (w as u64) * (h as u64) * 4 * (count as u64);
                if est > MAX_ANIM_RGBA_BYTES as u64 {
                    let scale = (MAX_ANIM_RGBA_BYTES as f64 / est as f64).sqrt();
                    w = ((w as f64 * scale).floor() as u32).max(1);
                    h = ((h as f64 * scale).floor() as u32).max(1);
                }
                let mut parsed_delays = crate::term::image::gif_frame_delays_ms(&img.data).unwrap_or_default();
                let mut frames = Vec::with_capacity(count);
                let mut delays_ms = Vec::with_capacity(count);
                for idx in 0..count {
                    let Some(raw_img) = NonNull::new(unsafe {
                        CGImageSourceCreateImageAtIndex(&*source, idx, std::ptr::null())
                    }) else {
                        continue;
                    };
                    let cg = unsafe { CFRetained::from_raw(raw_img) };
                    if let Some(px) = rasterize_cg_image(&cg, w, h) {
                        frames.push(px);
                        delays_ms.push(parsed_delays.get(idx).copied().unwrap_or(100));
                    }
                }
                if frames.len() > 1 {
                    parsed_delays.clear();
                    return Some(DecodedImage { frames, delays_ms });
                }
                if let Some(px) = frames.into_iter().next() {
                    return Some(DecodedImage { frames: vec![px], delays_ms: Vec::new() });
                }
            }
        }
    }
    let px = decode(img, width, height)?;
    Some(DecodedImage { frames: vec![px], delays_ms: Vec::new() })
}

/// The size an image of `img` pixels shows at in a box of `bx` pixels: as
/// large as fits, keeping its shape.
pub fn contain(img: (u32, u32), bx: (f32, f32)) -> (f32, f32) {
    let (w, h) = (img.0.max(1) as f32, img.1.max(1) as f32);
    let s = (bx.0 / w).min(bx.1 / h);
    (w * s, h * s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_raw_and_encoded_images() {
        // 2x1 raw RGB: red, then green.
        let raw = Image { id: 1, format: Format::Rgb, width: 2, height: 1, data: vec![255, 0, 0, 0, 255, 0] };
        let p = decode(&raw, 2, 1).unwrap();
        assert_eq!(&p.data[..4], &[255, 0, 0, 255]);
        assert_eq!(&p.data[4..], &[0, 255, 0, 255]);

        // A 1x1 opaque blue PNG.
        let png: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
            0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00,
            0x0C, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x60, 0x60, 0xF8, 0x0F, 0x00, 0x01, 0x03, 0x01, 0x00, 0x08,
            0x89, 0xC2, 0xEC, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let enc = Image { id: 2, format: Format::Encoded, width: 1, height: 1, data: png.to_vec() };
        let p = decode(&enc, 4, 4).expect("ImageIO reads it");
        assert_eq!((p.width, p.height), (4, 4));
        assert_eq!(&p.data[..4], &[0, 0, 255, 255]);

        let junk = Image { id: 3, format: Format::Encoded, width: 1, height: 1, data: b"not an image".to_vec() };
        assert!(decode(&junk, 4, 4).is_none());
    }

    #[test]
    fn images_keep_their_shape() {
        assert_eq!(contain((100, 50), (200.0, 200.0)), (200.0, 100.0));
        assert_eq!(contain((100, 50), (50.0, 200.0)), (50.0, 25.0));
    }

    #[test]
    fn decodes_animated_gif_frames_and_delays() {
        // Minimal 1x1 2-frame animated GIF89a: frame 0 red (60ms), frame 1 blue (120ms).
        let mut gif = Vec::new();
        gif.extend_from_slice(b"GIF89a");
        gif.extend_from_slice(&[1, 0, 1, 0, 0x80, 0, 0]);
        gif.extend_from_slice(&[255, 0, 0, 0, 0, 255]);
        // NETSCAPE2.0 looping application extension
        gif.extend_from_slice(&[0x21, 0xFF, 0x0B]);
        gif.extend_from_slice(b"NETSCAPE2.0");
        gif.extend_from_slice(&[3, 1, 0, 0, 0]);
        // Frame 0: 6cs = 60ms, color index 0 (red)
        gif.extend_from_slice(&[0x21, 0xF9, 0x04, 0x04, 6, 0, 0, 0]);
        gif.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x44, 0x01, 0]);
        // Frame 1: 12cs = 120ms, color index 1 (blue)
        gif.extend_from_slice(&[0x21, 0xF9, 0x04, 0x04, 12, 0, 0, 0]);
        gif.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x4C, 0x01, 0]);
        gif.push(0x3B);

        let img = Image { id: 9, format: Format::Encoded, width: 1, height: 1, data: gif };
        let dec = decode_frames(&img, 1, 1).expect("animated GIF");
        assert_eq!(dec.frames.len(), 2);
        assert_eq!(dec.delays_ms, vec![60, 120]);
        assert_eq!(&dec.frames[0].data[..4], &[255, 0, 0, 255]);
        assert_eq!(&dec.frames[1].data[..4], &[0, 0, 255, 255]);
    }
}
