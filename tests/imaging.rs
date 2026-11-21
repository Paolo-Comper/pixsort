mod common;

use common::*;
use image::RgbImage;
use pixsort::imaging::{analyze_bytes, analyze_file};
use pixsort::model::{phash_distance, ImageFormatKind};

#[test]
fn exact_pixels_with_different_formats_and_metadata_match() {
    let img = test_image(96, 72);
    let png = encode_png(&img, None);
    let png_with_exif = encode_png(&img, Some(exif_tiff(Some(1), Some("2020:05:01 12:00:00"))));
    let bmp = encode_bmp(&img);

    let a = analyze_bytes(&png, None).unwrap();
    let b = analyze_bytes(&png_with_exif, None).unwrap();
    let c = analyze_bytes(&bmp, None).unwrap();
    assert_eq!(a.pixel_hash, b.pixel_hash);
    assert_eq!(a.pixel_hash, c.pixel_hash);
    assert_eq!(a.width, 96);
    assert_eq!(a.format, ImageFormatKind::Png);
    assert!(b.exif.has_exif);
    assert_eq!(
        b.exif.date_time_original.as_deref(),
        Some("2020-05-01 12:00:00")
    );
}

#[test]
fn alpha_policy_is_consistent_across_encodings() {
    use image::{Rgba, RgbaImage};
    let mut rgba = RgbaImage::new(64, 48);
    for (x, y, px) in rgba.enumerate_pixels_mut() {
        let alpha = ((x * 255) / 63) as u8;
        *px = Rgba([(x % 256) as u8, (y * 3 % 256) as u8, 90, alpha]);
    }
    let flattened = pixsort::imaging::normalize::flatten_rgba(&rgba);
    let rgba_bytes = encode_rgba_png(&rgba);
    let rgb_bytes = encode_png(&flattened, None);
    let a = analyze_bytes(&rgba_bytes, None).unwrap();
    let b = analyze_bytes(&rgb_bytes, None).unwrap();
    assert_eq!(a.pixel_hash, b.pixel_hash);
    assert_eq!((a.width, a.height), (64, 48));
}

#[test]
fn exif_orientation_is_applied_before_hashing() {
    let base = test_image(80, 40);
    // Orientation 6 means "rotate 90° clockwise for display". Store the
    // rotated pixels and let EXIF describe how to display them.
    let rotated = image::imageops::rotate270(&image::DynamicImage::ImageRgb8(base.clone()));
    let rotated: RgbImage = image::DynamicImage::ImageRgba8(rotated).to_rgb8();
    let flat = encode_png(&base, None);
    let with_orientation = encode_png(&rotated, Some(exif_tiff(Some(6), None)));

    let a = analyze_bytes(&flat, None).unwrap();
    let b = analyze_bytes(&with_orientation, None).unwrap();
    assert_eq!((b.width, b.height), (80, 40));
    assert_eq!(a.pixel_hash, b.pixel_hash);
    assert_eq!(a.phash, b.phash);
}

#[test]
fn different_resolutions_match_perceptually_but_not_exactly() {
    let full = test_image(480, 360);
    let small = resize_image(&full, 0.4);
    let a = analyze_bytes(&encode_png(&full, None), None).unwrap();
    let b = analyze_bytes(&encode_png(&small, None), None).unwrap();
    assert_ne!(a.pixel_hash, b.pixel_hash);
    let distance = phash_distance(a.phash, b.phash);
    eprintln!("downscale pHash distance = {distance}");
    assert!(distance <= 6, "distance was {distance}");
}

#[test]
fn recompressed_jpegs_match_perceptually() {
    let img = test_image(640, 480);
    let high = analyze_bytes(&encode_jpeg(&img, 92, None), None).unwrap();
    let low = analyze_bytes(&encode_jpeg(&img, 55, None), None).unwrap();
    assert_ne!(high.pixel_hash, low.pixel_hash);
    let distance = phash_distance(high.phash, low.phash);
    eprintln!("recompression pHash distance = {distance}");
    assert!(distance <= 6, "distance was {distance}");
}

#[test]
fn visibly_different_images_are_far_apart() {
    let a = test_image(300, 200);
    let mut b = a.clone();
    for px in b.pixels_mut() {
        px.0 = [255 - px.0[0], 255 - px.0[1], 255 - px.0[2]];
    }
    let second = resize_image(&b, 1.5);
    let ha = analyze_bytes(&encode_png(&a, None), None).unwrap();
    let hb = analyze_bytes(&encode_png(&second, None), None).unwrap();
    let distance = phash_distance(ha.phash, hb.phash);
    eprintln!("unrelated pHash distance = {distance}");
    assert!(distance > 19, "distance was {distance}");
}

#[test]
fn missing_metadata_is_tolerated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("no metadata.png");
    write_file(&path, &encode_png(&test_image(64, 48), None));
    let analysis = analyze_file(&path).unwrap();
    assert!(!analysis.exif.has_exif);
    assert_eq!(analysis.width, 64);
    assert_eq!(analysis.height, 48);
}

#[test]
fn messaging_style_resize_and_recompress_is_detected() {
    let full = test_image(960, 720);
    let original = analyze_bytes(&encode_png(&full, None), None).unwrap();
    // Simulate a messaging app: downscale and recompress aggressively.
    let small = resize_image(&full, 0.45);
    let whatsapp = analyze_bytes(&encode_jpeg(&small, 45, None), None).unwrap();
    assert_ne!(original.pixel_hash, whatsapp.pixel_hash);
    let distance = phash_distance(original.phash, whatsapp.phash);
    eprintln!("messaging pHash distance = {distance}");
    assert!(
        distance <= 19,
        "expected the perceptual candidate to fall inside the review radius, got {distance}"
    );
}
