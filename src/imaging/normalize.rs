//! Pixel normalisation and the exact pixel hash.
//!
//! Policy (documented in `docs/policies.md`):
//!   * decode every supported photograph to 8-bit RGB,
//!   * apply the EXIF orientation first,
//!   * composite any alpha channel over opaque white,
//!   * treat decoded samples as sRGB (ICC profiles are not applied),
//!   * animate formats contribute their first frame only,
//!   * hash `width`, `height` and the raw RGB bytes with SHA-256.
//!
//! As a result, two files with identical pixels but different metadata,
//! filenames, formats or compression settings produce the same hash.

use fast_image_resize::images::ImageRef;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::{DynamicImage, Rgb, RgbImage, RgbaImage};
use sha2::{Digest, Sha256};

pub const NORMALIZATION_MAGIC: &[u8] = b"PIXSORT-PIXEL-HASH-v1";
pub const NORMALIZATION_CHANNELS: &[u8] = b"RGB8";

/// Applies EXIF orientation to a decoded image.
pub fn apply_orientation(img: DynamicImage, orientation: u16) -> DynamicImage {
    use image::imageops;
    let buffer = match orientation.clamp(1, 8) {
        1 => return img,
        2 => imageops::flip_horizontal(&img),
        3 => imageops::rotate180(&img),
        4 => imageops::flip_vertical(&img),
        5 => imageops::flip_horizontal(&imageops::rotate90(&img)),
        6 => imageops::rotate90(&img),
        7 => imageops::flip_horizontal(&imageops::rotate270(&img)),
        8 => imageops::rotate270(&img),
        _ => unreachable!(),
    };
    DynamicImage::ImageRgba8(buffer)
}

/// Composites an RGBA buffer over opaque white.
pub fn flatten_rgba(rgba: &RgbaImage) -> RgbImage {
    let (w, h) = rgba.dimensions();
    let mut out = RgbImage::new(w, h);
    for (x, y, px) in rgba.enumerate_pixels() {
        let [r, g, b, a] = px.0;
        let a = a as u32;
        let inv = 255 - a;
        // Rounded compositing over white: (c*a + 255*(255-a)) / 255.
        let blend = |c: u8| -> u8 { ((c as u32 * a + 255 * inv + 127) / 255).clamp(0, 255) as u8 };
        out.put_pixel(x, y, Rgb([blend(r), blend(g), blend(b)]));
    }
    out
}

/// Converts any decoded image to the canonical RGB representation.
pub fn flatten_to_rgb(img: DynamicImage) -> RgbImage {
    match img {
        DynamicImage::ImageRgb8(rgb) => rgb,
        DynamicImage::ImageRgba8(rgba) => flatten_rgba(&rgba),
        other => flatten_rgba(&other.to_rgba8()),
    }
}

/// Applies orientation and flattens alpha. This is the representation an
/// exact-pixel hash is computed over.
pub fn normalize(img: DynamicImage, orientation: u16) -> RgbImage {
    flatten_to_rgb(apply_orientation(img, orientation))
}

/// SHA-256 over a length-prefixed canonical representation.
pub fn pixel_hash(rgb: &RgbImage) -> String {
    let (width, height) = rgb.dimensions();
    let mut hasher = Sha256::new();
    hasher.update(NORMALIZATION_MAGIC);
    hasher.update(width.to_le_bytes());
    hasher.update(height.to_le_bytes());
    hasher.update(NORMALIZATION_CHANNELS);
    hasher.update(rgb.as_raw());
    hex(&hasher.finalize())
}

/// Convenience for callers that already have decoded pixels.
pub fn hash_and_phash(rgb: &RgbImage) -> (String, u64) {
    (pixel_hash(rgb), perceptual_hash(rgb))
}

pub fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

// ---------------------------------------------------------------------------
// pHash
// ---------------------------------------------------------------------------

const DCT_SIZE: usize = 32;
const DCT_LOW: usize = 8;

/// 63-bit DCT perceptual hash.
///
/// The RGB image is resized to 32×32, converted to luma, and transformed with
/// an orthonormal 2-D DCT-II. The 63 lowest-frequency coefficients (8×8 block
/// without DC) are compared to their median; bits above the median are set.
pub fn perceptual_hash(rgb: &RgbImage) -> u64 {
    let mut small = [0f64; DCT_SIZE * DCT_SIZE];
    resize_luma(rgb, &mut small);
    dct_phash(&small)
}

/// Exposed for tests and for callers that already have a 32×32 luma plane.
#[allow(clippy::needless_range_loop)]
pub fn dct_phash(luma32: &[f64; DCT_SIZE * DCT_SIZE]) -> u64 {
    // Precompute the cosine table for the 8 low-frequency indices.
    let mut cos_table = [[0f64; DCT_SIZE]; DCT_LOW];
    for u in 0..DCT_LOW {
        for x in 0..DCT_SIZE {
            let angle =
                std::f64::consts::PI * (2.0 * x as f64 + 1.0) * u as f64 / (2.0 * DCT_SIZE as f64);
            cos_table[u][x] = angle.cos();
        }
    }

    let mut coeffs = [0f64; (DCT_LOW * DCT_LOW) - 1];
    let mut idx = 0;
    for v in 0..DCT_LOW {
        // DCT along rows once per v, then reuse for all u.
        let mut row = [0f64; DCT_SIZE];
        for y in 0..DCT_SIZE {
            let mut sum = 0.0;
            for x in 0..DCT_SIZE {
                sum += luma32[y * DCT_SIZE + x] * cos_table[v][x];
            }
            row[y] = sum;
        }
        for u in 0..DCT_LOW {
            if u == 0 && v == 0 {
                continue; // DC carries only average brightness.
            }
            let mut sum = 0.0;
            for y in 0..DCT_SIZE {
                sum += row[y] * cos_table[u][y];
            }
            coeffs[idx] = sum;
            idx += 1;
        }
    }

    let mut sorted = coeffs;
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2];

    let mut hash = 0u64;
    for (bit, coefficient) in coeffs.iter().enumerate() {
        if *coefficient > median {
            hash |= 1u64 << bit;
        }
    }
    hash
}

fn resize_luma(rgb: &RgbImage, out: &mut [f64; DCT_SIZE * DCT_SIZE]) {
    let (w, h) = rgb.dimensions();
    let src = ImageRef::new(w, h, rgb.as_raw(), PixelType::U8x3).expect("valid RGB buffer");
    let mut dst =
        fast_image_resize::images::Image::new(DCT_SIZE as u32, DCT_SIZE as u32, PixelType::U8x3);
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
    let mut resizer = Resizer::new();
    let buffer = match resizer.resize(&src, &mut dst, &options) {
        Ok(()) => dst.buffer().to_vec(),
        Err(_) => {
            // Extremely unlikely fallback: nearest-neighbour sampling.
            let mut buf = Vec::with_capacity(DCT_SIZE * DCT_SIZE * 3);
            for y in 0..DCT_SIZE {
                for x in 0..DCT_SIZE {
                    let sx = (x * w as usize / DCT_SIZE).min(w as usize - 1);
                    let sy = (y * h as usize / DCT_SIZE).min(h as usize - 1);
                    let px = rgb.get_pixel(sx as u32, sy as u32);
                    buf.extend_from_slice(&px.0);
                }
            }
            buf
        }
    };
    for i in 0..DCT_SIZE * DCT_SIZE {
        let r = buffer[i * 3] as f64;
        let g = buffer[i * 3 + 1] as f64;
        let b = buffer[i * 3 + 2] as f64;
        out[i] = 0.299 * r + 0.587 * g + 0.114 * b;
    }
}

/// True when `hash` has exactly `PHASH_BITS` informative bits or fewer.
pub fn phash_mask(hash: u64) -> u64 {
    hash & ((1u64 << crate::model::PHASH_BITS) - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn test_image(w: u32, h: u32) -> RgbImage {
        let mut img = RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            let r = ((x * 255 / w.max(1)) as u8).wrapping_add((y % 32) as u8);
            let g = ((y * 255 / h.max(1)) as u8).wrapping_add((x % 32) as u8);
            let b = ((x + y) % 256) as u8;
            *px = Rgb([r, g, b]);
        }
        img
    }

    #[test]
    fn identical_pixels_have_identical_hashes() {
        let img = test_image(64, 48);
        assert_eq!(pixel_hash(&img), pixel_hash(&img));
        assert_eq!(perceptual_hash(&img), perceptual_hash(&img));
    }

    #[test]
    fn alpha_is_composited_over_white() {
        let mut rgba = RgbaImage::new(2, 1);
        rgba.put_pixel(0, 0, Rgba([0, 0, 0, 255]));
        rgba.put_pixel(1, 0, Rgba([0, 0, 0, 0]));
        let rgb = flatten_rgba(&rgba);
        assert_eq!(rgb.get_pixel(0, 0).0, [0, 0, 0]);
        assert_eq!(rgb.get_pixel(1, 0).0, [255, 255, 255]);
    }

    #[test]
    fn orientation_swaps_dimensions() {
        let img = DynamicImage::ImageRgb8(test_image(30, 10));
        let rotated = apply_orientation(img, 6);
        assert_eq!(rotated.width(), 10);
        assert_eq!(rotated.height(), 30);
    }

    #[test]
    fn all_exif_orientation_mappings_are_correct() {
        // 2x3 stored image with distinct red values:
        //   A B
        //   C D
        //   E F
        let mut img = RgbImage::new(2, 3);
        for (i, px) in img.pixels_mut().enumerate() {
            let value = (i as u8 + 1) * 10;
            *px = Rgb([value, value, value]);
        }
        let stored = DynamicImage::ImageRgb8(img);
        let reds = |orientation: u16| -> (u32, u32, Vec<u8>) {
            let out = apply_orientation(stored.clone(), orientation).to_rgb8();
            let values = out.pixels().map(|p| p.0[0]).collect();
            (out.width(), out.height(), values)
        };
        assert_eq!(reds(1), (2, 3, vec![10, 20, 30, 40, 50, 60]));
        assert_eq!(reds(2), (2, 3, vec![20, 10, 40, 30, 60, 50]));
        assert_eq!(reds(3), (2, 3, vec![60, 50, 40, 30, 20, 10]));
        assert_eq!(reds(4), (2, 3, vec![50, 60, 30, 40, 10, 20]));
        assert_eq!(reds(5), (3, 2, vec![10, 30, 50, 20, 40, 60]));
        assert_eq!(reds(6), (3, 2, vec![50, 30, 10, 60, 40, 20]));
        assert_eq!(reds(7), (3, 2, vec![60, 40, 20, 50, 30, 10]));
        assert_eq!(reds(8), (3, 2, vec![20, 40, 60, 10, 30, 50]));
    }

    #[test]
    fn different_images_have_different_hashes() {
        let a = test_image(64, 64);
        let mut b = a.clone();
        for px in b.pixels_mut() {
            px.0 = [255 - px.0[0], 255 - px.0[1], 255 - px.0[2]];
        }
        assert_ne!(pixel_hash(&a), pixel_hash(&b));
        assert_ne!(perceptual_hash(&a), perceptual_hash(&b));
    }

    #[test]
    fn hash_hex_is_64_chars() {
        let h = pixel_hash(&test_image(3, 3));
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
