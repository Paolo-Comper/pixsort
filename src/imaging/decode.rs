//! Decoding and one-shot analysis of a photograph.
//!
//! Animated formats policy: the first frame is decoded and treated as a still
//! photograph. The file is flagged as animated so the UI can say so. No
//! animation management is provided. GIF is not a supported photograph format.

use std::path::Path;

use anyhow::{bail, Context, Result};
use image::{DynamicImage, RgbaImage};

use crate::imaging::{exif, heic, normalize};
use crate::model::{ExifInfo, ImageFormatKind};

/// Everything Pixsort needs to know about a decoded photograph.
#[derive(Debug, Clone)]
pub struct PhotoAnalysis {
    pub format: ImageFormatKind,
    pub width: u32,
    pub height: u32,
    pub pixel_hash: String,
    pub phash: u64,
    pub exif: ExifInfo,
    pub orientation: u16,
    pub is_animated: bool,
}

#[derive(Debug)]
pub struct Decoded {
    pub image: DynamicImage,
    pub format: ImageFormatKind,
    pub exif: ExifInfo,
    pub is_animated: bool,
}

pub fn analyze_file(path: &Path) -> Result<PhotoAnalysis> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    analyze_bytes(&bytes, Some(path))
}

pub fn analyze_bytes(bytes: &[u8], path_hint: Option<&Path>) -> Result<PhotoAnalysis> {
    let decoded = decode_bytes(bytes, path_hint)?;
    let orientation = decoded.exif.normalized_orientation();
    let rgb = normalize::normalize(decoded.image, orientation);
    if rgb.width() == 0 || rgb.height() == 0 {
        bail!("image has zero dimensions");
    }
    let (pixel_hash, phash) = normalize::hash_and_phash(&rgb);
    Ok(PhotoAnalysis {
        format: decoded.format,
        width: rgb.width(),
        height: rgb.height(),
        pixel_hash,
        phash,
        exif: decoded.exif,
        orientation,
        is_animated: decoded.is_animated,
    })
}

pub fn decode_file(path: &Path) -> Result<Decoded> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    decode_bytes(&bytes, Some(path))
}

pub fn decode_bytes(bytes: &[u8], path_hint: Option<&Path>) -> Result<Decoded> {
    if bytes.is_empty() {
        bail!("file is empty");
    }
    let format = ImageFormatKind::from_magic(bytes)
        .or_else(|| path_hint.and_then(ImageFormatKind::from_path))
        .ok_or_else(|| anyhow::anyhow!("unsupported or unrecognised image format"))?;

    let is_animated = detect_animation(bytes, format);
    let exif = exif::read_exif(bytes);

    let image = match format {
        ImageFormatKind::Heic => heic::decode(bytes)?,
        other => image::load_from_memory_with_format(bytes, to_image_format(other))
            .with_context(|| format!("cannot decode {} image", other.label()))?,
    };

    if image.width() == 0 || image.height() == 0 {
        bail!("image has zero dimensions");
    }

    Ok(Decoded {
        image,
        format,
        exif,
        is_animated,
    })
}

fn to_image_format(format: ImageFormatKind) -> image::ImageFormat {
    match format {
        ImageFormatKind::Jpeg => image::ImageFormat::Jpeg,
        ImageFormatKind::Png => image::ImageFormat::Png,
        ImageFormatKind::WebP => image::ImageFormat::WebP,
        ImageFormatKind::Bmp => image::ImageFormat::Bmp,
        ImageFormatKind::Tiff => image::ImageFormat::Tiff,
        // Not reached: HEIC is decoded through libheif.
        ImageFormatKind::Heic => image::ImageFormat::Png,
    }
}

/// Loads a preview for the GUI, applying EXIF orientation and downscaling.
pub fn load_preview(path: &Path, max_dim: u32) -> Result<RgbaImage> {
    let decoded = decode_file(path)?;
    let oriented =
        normalize::apply_orientation(decoded.image, decoded.exif.normalized_orientation());
    let rgba = oriented.to_rgba8();
    Ok(downscale_for_preview(rgba, max_dim.max(64)))
}

pub fn downscale_for_preview(img: RgbaImage, max_dim: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let longest = w.max(h);
    if longest <= max_dim {
        return img;
    }
    let scale = max_dim as f64 / longest as f64;
    let nw = ((w as f64 * scale).round() as u32).max(1);
    let nh = ((h as f64 * scale).round() as u32).max(1);
    image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle)
}

/// Detects animated PNG and animated WebP. The first frame is still what we
/// analyse; this flag is informational.
pub fn detect_animation(bytes: &[u8], format: ImageFormatKind) -> bool {
    match format {
        ImageFormatKind::Png => png_is_animated(bytes),
        ImageFormatKind::WebP => webp_is_animated(bytes),
        _ => false,
    }
}

fn png_is_animated(bytes: &[u8]) -> bool {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return false;
    }
    let mut offset = 8usize;
    while offset + 8 <= bytes.len() {
        let length = u32::from_be_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]) as usize;
        let kind = &bytes[offset + 4..offset + 8];
        if kind == b"acTL" {
            return true;
        }
        if kind == b"IDAT" || kind == b"IEND" {
            return false;
        }
        let next = match offset.checked_add(12).and_then(|v| v.checked_add(length)) {
            Some(next) => next,
            None => return false,
        };
        if next > bytes.len() {
            return false;
        }
        offset = next;
    }
    false
}

fn webp_is_animated(bytes: &[u8]) -> bool {
    if bytes.len() < 21 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return false;
    }
    if &bytes[12..16] != b"VP8X" {
        return false;
    }
    // VP8X chunk: 4-byte size, then flags; animation flag is 0x02.
    bytes[20] & 0x02 != 0
}

/// Returns a short human-readable reason for likely decode failures.
pub fn decode_hint(format: ImageFormatKind) -> &'static str {
    match format {
        ImageFormatKind::Heic => {
            "HEIC/HEIF decoding uses libheif; install it or set PIXSORT_LIBHEIF."
        }
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_animation_flags() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&4u32.to_be_bytes());
        png.extend_from_slice(b"acTL");
        png.extend_from_slice(&[0, 0, 0, 1]);
        png.extend_from_slice(&[0; 4]);
        assert!(detect_animation(&png, ImageFormatKind::Png));

        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&0u32.to_le_bytes());
        webp.extend_from_slice(b"WEBPVP8X");
        webp.extend_from_slice(&10u32.to_le_bytes());
        webp.push(0x02);
        webp.extend_from_slice(&[0; 9]);
        assert!(detect_animation(&webp, ImageFormatKind::WebP));
    }
}
