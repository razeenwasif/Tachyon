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
    fn CGImageSourceCreateImageAtIndex(source: *const CFType, index: usize, options: *const c_void) -> *mut CGImage;
}

/// The largest side a texture is made with.
pub const MAX_SIDE: u32 = 8192;

/// An image's pixels, `width` by `height`, premultiplied RGBA.
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
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

/// The image drawn at `width` by `height` pixels.
pub fn decode(img: &Image, width: u32, height: u32) -> Option<Pixels> {
    let image = cg_image(img)?;
    let (w, h) = (width.clamp(1, MAX_SIDE) as usize, height.clamp(1, MAX_SIDE) as usize);
    let mut data = vec![0u8; w * h * 4];
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    let ctx = unsafe {
        CGBitmapContextCreate(data.as_mut_ptr().cast(), w, h, 8, w * 4, Some(&space), CGImageAlphaInfo::PremultipliedLast.0)
    }?;
    CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
    let rect = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64));
    CGContext::draw_image(Some(&ctx), rect, Some(&image));
    drop(ctx);
    Some(Pixels { width: w as u32, height: h as u32, data })
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
}
