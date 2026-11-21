//! Creates a small demo project for screenshots and manual testing.
//!
//! Usage: cargo run --example prepare_demo -- [output-dir]
//!
//! It generates a synthetic archive, an import batch with exact and perceptual
//! duplicates, runs a real import, and writes settings that point at the demo.

use std::path::PathBuf;

use image::{ImageEncoder, Rgb, RgbImage};
use pixsort::config::Settings;
use pixsort::import;
use pixsort::jobs::ProgressReporter;
use pixsort::model::JobControl;

fn demo_image(width: u32, height: u32, seed: u32) -> RgbImage {
    let mut img = RgbImage::new(width, height);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let fx = x as f32 / width as f32;
        let fy = y as f32 / height as f32;
        let mut r = (40.0 + 170.0 * fx + seed as f32 * 3.0) as u8;
        let mut g = (60.0 + 120.0 * fy) as u8;
        let mut b = (90.0 + 90.0 * (fx + fy)) as u8;
        let cx = width as i32 / 2 + seed as i32;
        let cy = height as i32 / 2;
        let dx = x as i32 - cx;
        let dy = y as i32 - cy;
        if dx * dx + dy * dy < (width.min(height) as i32 / 4).pow(2) {
            r = 240;
            g = 210;
            b = 70;
        }
        if (x as i32 - cx).abs() < 6 && y < height / 2 {
            r = 20;
            g = 110;
            b = 230;
        }
        *px = Rgb([r, g, b]);
    }
    img
}

fn write_jpeg(path: &std::path::Path, img: &RgbImage, quality: u8) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut buf = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    encoder
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            image::ExtendedColorType::Rgb8,
        )
        .unwrap();
    std::fs::write(path, buf).unwrap();
}

fn main() -> anyhow::Result<()> {
    let base = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| ".test-config/demo".into()),
    );
    let input = base.join("input");
    let archive = base.join("archive");
    let state = base.join("state");
    let config = base.join("config");
    for dir in [&input, &archive, &state, &config] {
        std::fs::create_dir_all(dir)?;
    }

    let full = demo_image(1200, 800, 0);
    // Two identical archive copies (exact pixel duplicates).
    write_jpeg(&archive.join("2019").join("IMG_0100.jpg"), &full, 92);
    write_jpeg(&archive.join("2021").join("IMG_0100 copy.jpg"), &full, 92);
    // A smaller archive version that will match the import perceptually.
    let small = image::imageops::resize(&full, 700, 466, image::imageops::FilterType::Lanczos3);
    write_jpeg(&archive.join("2019").join("IMG_0200.jpg"), &small, 80);
    // New import batch: a higher-resolution perceptual version and a new photo.
    write_jpeg(&input.join("IMG_0200_original.jpg"), &full, 95);
    write_jpeg(&input.join("new-photo.jpg"), &demo_image(900, 600, 7), 90);

    std::env::set_var("PIXSORT_CONFIG_DIR", &config);
    let mut settings = Settings::default();
    settings.input_dir = Some(input.clone());
    settings.destination_dir = Some(archive.clone());
    settings.index_db_path = Some(state.join("index.sqlite"));
    settings.plan_db_path = Some(state.join("plan.sqlite"));
    settings.trash_dir = Some(base.join("trash"));
    settings.worker_threads = 2;
    settings.save()?;

    let control = JobControl::new();
    let (reporter, _rx) = ProgressReporter::channel();
    let summary = import::run_import(&settings, &control, &reporter)?;
    println!("Demo project at {}", base.display());
    println!("{summary:#?}");
    println!("Start the GUI with:");
    println!("  PIXSORT_CONFIG_DIR={} cargo run", config.display());
    Ok(())
}
