#![allow(dead_code)]

use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageEncoder, Rgb, RgbImage};
use pixsort::config::Settings;

/// A deterministic, photo-like test image with gradients and shapes.
pub fn test_image(width: u32, height: u32) -> RgbImage {
    let mut img = RgbImage::new(width, height);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let fx = x as f32 / width.max(1) as f32;
        let fy = y as f32 / height.max(1) as f32;
        let mut r = (60.0 + 150.0 * fx) as u8;
        let mut g = (40.0 + 120.0 * fy) as u8;
        let mut b = (80.0 + 100.0 * (fx + fy) / 2.0) as u8;
        // A few solid shapes so the perceptual hash has structure.
        let cx = width as i32 / 2;
        let cy = height as i32 / 2;
        let dx = x as i32 - cx;
        let dy = y as i32 - cy;
        if dx * dx + dy * dy < (width.min(height) as i32 / 5).pow(2) {
            r = 245;
            g = 230;
            b = 90;
        }
        if (x as i32 - cx).abs() < 8 && y < height / 2 {
            r = 20;
            g = 90;
            b = 220;
        }
        if (y as i32 - (height as i32 * 3 / 4)).abs() < 5 {
            r = 240;
            g = 60;
            b = 60;
        }
        *px = Rgb([r, g, b]);
    }
    img
}

/// A completely unrelated image (deterministic blocky pattern), used when a
/// test needs a photograph that is not perceptually similar to `test_image`.
pub fn unrelated_image(width: u32, height: u32) -> RgbImage {
    let mut img = RgbImage::new(width, height);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let block = ((x / 17) ^ (y / 13)) & 1;
        let stripe = ((x + y * 3) / 7) % 5;
        let r = if block == 0 {
            20 + stripe * 40
        } else {
            230 - stripe * 20
        };
        let g = ((x * 7 + y * 11) % 256) as u8;
        let b = if (x / 9 + y / 9) % 2 == 0 { 240 } else { 30 };
        *px = Rgb([r as u8, g, b]);
    }
    img
}

/// Builds a minimal TIFF/EXIF blob with an optional orientation and
/// DateTimeOriginal value. This is the payload the image crate expects in
/// `set_exif_metadata` (it adds the `Exif\0\0` prefix itself for JPEG).
pub fn exif_tiff(orientation: Option<u16>, date_original: Option<&str>) -> Vec<u8> {
    // Header (8 bytes) + IFD0 + optional ExifIFD + data.
    let ifd0_entries: u16 = if date_original.is_some() { 2 } else { 1 };
    let ifd0_offset: u32 = 8;
    let ifd0_size = 2 + 12 * ifd0_entries as u32 + 4;
    let exif_ifd_offset = ifd0_offset + ifd0_size;

    let date_bytes = date_original.map(|d| {
        let mut v = d.as_bytes().to_vec();
        if !v.ends_with(&[0]) {
            v.push(0);
        }
        v
    });
    let exif_ifd_size = if date_bytes.is_some() { 2 + 12 + 4 } else { 0 };
    let date_offset = exif_ifd_offset + exif_ifd_size;

    let mut out = Vec::new();
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&ifd0_offset.to_le_bytes());

    // IFD0
    out.extend_from_slice(&ifd0_entries.to_le_bytes());
    // Orientation (SHORT, count 1, inline)
    out.extend_from_slice(&0x0112u16.to_le_bytes());
    out.extend_from_slice(&3u16.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    let orientation = orientation.unwrap_or(1);
    out.extend_from_slice(&orientation.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    if date_bytes.is_some() {
        // ExifIFDPointer (LONG, count 1)
        out.extend_from_slice(&0x8769u16.to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&exif_ifd_offset.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // next IFD

    if let Some(date_bytes) = &date_bytes {
        out.extend_from_slice(&1u16.to_le_bytes());
        // DateTimeOriginal (ASCII)
        out.extend_from_slice(&0x9003u16.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&(date_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&date_offset.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // next IFD
        out.extend_from_slice(date_bytes);
    }
    out
}

pub fn encode_png(img: &RgbImage, exif: Option<Vec<u8>>) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut encoder = image::codecs::png::PngEncoder::new(&mut buf);
    if let Some(exif) = exif {
        encoder.set_exif_metadata(exif).unwrap();
    }
    encoder
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
    buf
}

pub fn encode_rgba_png(img: &image::RgbaImage) -> Vec<u8> {
    let dynamic = DynamicImage::ImageRgba8(img.clone());
    let mut buf = Vec::new();
    dynamic
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    buf
}

pub fn encode_jpeg(img: &RgbImage, quality: u8, exif: Option<Vec<u8>>) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    if let Some(exif) = exif {
        encoder.set_exif_metadata(exif).unwrap();
    }
    encoder
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
    buf
}

pub fn encode_bmp(img: &RgbImage) -> Vec<u8> {
    let dynamic = DynamicImage::ImageRgb8(img.clone());
    let mut buf = Vec::new();
    dynamic
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Bmp)
        .unwrap();
    buf
}

pub fn write_file(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

pub fn resize_image(img: &RgbImage, scale: f32) -> RgbImage {
    let w = ((img.width() as f32 * scale).round() as u32).max(1);
    let h = ((img.height() as f32 * scale).round() as u32).max(1);
    image::imageops::resize(img, w, h, image::imageops::FilterType::Lanczos3)
}

/// Creates a standard test project layout and settings.
pub struct TestProject {
    pub dir: tempfile::TempDir,
    pub input: PathBuf,
    pub archive: PathBuf,
    pub settings: Settings,
}

impl TestProject {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input");
        let archive = dir.path().join("archive");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&input).unwrap();
        std::fs::create_dir_all(&archive).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let mut settings = Settings::default();
        settings.input_dir = Some(input.clone());
        settings.destination_dir = Some(archive.clone());
        settings.index_db_path = Some(state.join("index.sqlite"));
        settings.plan_db_path = Some(state.join("plan.sqlite"));
        settings.trash_dir = Some(dir.path().join("trash"));
        settings.worker_threads = 2;
        settings.thumbnail_max_dim = 800;
        TestProject {
            dir,
            input,
            archive,
            settings,
        }
    }

    pub fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }
}

/// Drains a progress channel without blocking; used to keep sender alive.
pub fn drain(rx: &std::sync::mpsc::Receiver<pixsort::jobs::JobEvent>) -> Vec<String> {
    let mut logs = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let pixsort::jobs::JobEvent::Log(message) = event {
            logs.push(message);
        }
    }
    logs
}
