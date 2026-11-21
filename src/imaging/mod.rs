//! Image decoding, normalisation, hashing and metadata extraction.

pub mod decode;
pub mod exif;
pub mod heic;
pub mod normalize;

pub use decode::{
    analyze_bytes, analyze_file, decode_bytes, decode_file, decode_hint, detect_animation,
    load_preview, PhotoAnalysis,
};
pub use exif::{date_in_filename, read_exif, resolve_capture_date, CaptureDate};
pub use normalize::{hash_and_phash, normalize, perceptual_hash, pixel_hash};
