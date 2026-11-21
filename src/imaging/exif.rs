//! EXIF extraction and capture-date resolution.
//!
//! Pixsort uses EXIF only for metadata and orientation — never for duplicate
//! detection.

use chrono::{Datelike, NaiveDate, NaiveDateTime, TimeZone, Utc};
use exif::{In, Tag, Value};

use crate::model::{DateSource, ExifInfo, GpsCoord};

/// Parses the EXIF block out of any container supported by the `kamadak-exif`
/// crate (JPEG, PNG, WebP, TIFF, HEIC). Missing or corrupt EXIF is normal and
/// is not an error: the returned value simply has `has_exif == false`.
pub fn read_exif(bytes: &[u8]) -> ExifInfo {
    let mut info = ExifInfo::default();
    let mut cursor = std::io::Cursor::new(bytes);
    let exif = match exif::Reader::new().read_from_container(&mut cursor) {
        Ok(exif) => exif,
        Err(_) => return info,
    };
    info.has_exif = true;

    let get_string = |tag: Tag| -> Option<String> {
        exif.get_field(tag, In::PRIMARY)
            .map(|f| clean_ascii(&f.display_value().to_string()))
            .filter(|s| !s.is_empty())
    };

    info.date_time_original = get_string(Tag::DateTimeOriginal);
    info.date_time_digitized = get_string(Tag::DateTimeDigitized);
    info.date_time = get_string(Tag::DateTime);
    info.make = get_string(Tag::Make);
    info.model = get_string(Tag::Model);
    info.lens = get_string(Tag::LensModel);
    info.software = get_string(Tag::Software);

    if let Some(field) = exif.get_field(Tag::Orientation, In::PRIMARY) {
        if let Some(v) = field.value.get_uint(0) {
            info.orientation = v.clamp(1, 8) as u16;
        }
    }
    info.pixel_x = exif
        .get_field(Tag::PixelXDimension, In::PRIMARY)
        .and_then(|f| f.value.get_uint(0));
    info.pixel_y = exif
        .get_field(Tag::PixelYDimension, In::PRIMARY)
        .and_then(|f| f.value.get_uint(0));

    info.gps = read_gps(&exif);
    info
}

fn clean_ascii(raw: &str) -> String {
    raw.trim_matches(|c: char| c == '"' || c == '\0' || c == ' ')
        .replace('\0', "")
        .trim()
        .to_string()
}

fn read_gps(exif: &exif::Exif) -> Option<GpsCoord> {
    let dms = |tag: Tag| -> Option<f64> {
        let field = exif.get_field(tag, In::PRIMARY)?;
        let Value::Rational(ref vals) = field.value else {
            return None;
        };
        if vals.len() < 3 {
            return None;
        }
        let deg = vals[0].to_f64();
        let min = vals[1].to_f64();
        let sec = vals[2].to_f64();
        Some(deg + min / 60.0 + sec / 3600.0)
    };
    let latitude = dms(Tag::GPSLatitude)?;
    let longitude = dms(Tag::GPSLongitude)?;
    let lat_ref = exif
        .get_field(Tag::GPSLatitudeRef, In::PRIMARY)
        .map(|f| clean_ascii(&f.display_value().to_string()));
    let lon_ref = exif
        .get_field(Tag::GPSLongitudeRef, In::PRIMARY)
        .map(|f| clean_ascii(&f.display_value().to_string()));
    let latitude = if lat_ref.as_deref() == Some("S") {
        -latitude
    } else {
        latitude
    };
    let longitude = if lon_ref.as_deref() == Some("W") {
        -longitude
    } else {
        longitude
    };
    if !(-90.0..=90.0).contains(&latitude) || !(-180.0..=180.0).contains(&longitude) {
        return None;
    }
    Some(GpsCoord {
        latitude,
        longitude,
    })
}

/// Result of choosing the best available capture timestamp.
#[derive(Debug, Clone)]
pub struct CaptureDate {
    pub date: NaiveDate,
    pub timestamp_ns: i64,
    pub source: DateSource,
}

/// Chooses the capture date following the documented priority:
/// DateTimeOriginal → DateTimeDigitized → EXIF DateTime → filesystem mtime.
///
/// `use_filesystem` can disable the mtime fallback, in which case the caller
/// ends up without a date and Pixsort uses the `Unknown/` directory.
pub fn resolve_capture_date(
    exif: &ExifInfo,
    mtime_ns: i64,
    use_filesystem: bool,
) -> Option<CaptureDate> {
    for (raw, source) in [
        (exif.date_time_original.as_deref(), DateSource::ExifOriginal),
        (
            exif.date_time_digitized.as_deref(),
            DateSource::ExifDigitized,
        ),
        (exif.date_time.as_deref(), DateSource::ExifDateTime),
    ] {
        if let Some(raw) = raw {
            if let Some(dt) = parse_exif_datetime(raw) {
                return Some(CaptureDate {
                    date: dt.date(),
                    timestamp_ns: dt.and_utc().timestamp_nanos_opt()?,
                    source,
                });
            }
        }
    }

    if use_filesystem {
        let secs = mtime_ns.div_euclid(1_000_000_000);
        let nanos = mtime_ns.rem_euclid(1_000_000_000) as u32;
        if let Some(utc) = chrono::DateTime::<Utc>::from_timestamp(secs, nanos) {
            let local = utc.with_timezone(&chrono::Local);
            return Some(CaptureDate {
                date: local.date_naive(),
                timestamp_ns: mtime_ns,
                source: DateSource::FileModified,
            });
        }
    }

    None
}

pub fn parse_exif_datetime(raw: &str) -> Option<NaiveDateTime> {
    let raw = raw.trim();
    for fmt in [
        "%Y:%m:%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y:%m:%d %H:%M",
        "%Y-%m-%dT%H:%M:%S%.f",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(raw, fmt) {
            // Reject obviously invalid placeholder dates.
            if dt.year() >= 1826 {
                return Some(dt);
            }
        }
    }
    None
}

/// Parses a date embedded in a filename, e.g. `IMG_20230715_1234.jpg`,
/// `2019-05-01 12.30.00.jpg` or `Screenshot 2020-01-02.png`.
pub fn date_in_filename(name: &str) -> Option<NaiveDate> {
    let chars: Vec<char> = name.chars().collect();
    // YYYYMMDD (character based so Unicode filenames cannot split a code point)
    for i in 0..chars.len().saturating_sub(7) {
        let window = &chars[i..i + 8];
        if window.iter().all(|c| c.is_ascii_digit()) {
            let text: String = window.iter().collect();
            if let (Ok(y), Ok(m), Ok(d)) = (
                text[0..4].parse::<i32>(),
                text[4..6].parse::<u32>(),
                text[6..8].parse::<u32>(),
            ) {
                if (1826..=2200).contains(&y) {
                    if let Some(date) = NaiveDate::from_ymd_opt(y, m, d) {
                        return Some(date);
                    }
                }
            }
        }
    }
    // YYYY-MM-DD / YYYY_MM_DD
    for i in 0..chars.len().saturating_sub(9) {
        let window = &chars[i..i + 10];
        if (window[4] == '-' || window[4] == '_')
            && window[7] == window[4]
            && window[0..4].iter().all(|c| c.is_ascii_digit())
            && window[5..7].iter().all(|c| c.is_ascii_digit())
            && window[8..10].iter().all(|c| c.is_ascii_digit())
        {
            let year: String = window[0..4].iter().collect();
            let month: String = window[5..7].iter().collect();
            let day: String = window[8..10].iter().collect();
            if let (Ok(y), Ok(m), Ok(d)) = (
                year.parse::<i32>(),
                month.parse::<u32>(),
                day.parse::<u32>(),
            ) {
                if (1826..=2200).contains(&y) {
                    if let Some(date) = NaiveDate::from_ymd_opt(y, m, d) {
                        return Some(date);
                    }
                }
            }
        }
    }
    None
}

/// Local-midnight-based helper for tests and UI display.
pub fn local_date_from_ns(ns: i64) -> Option<NaiveDate> {
    let secs = ns.div_euclid(1_000_000_000);
    let nanos = ns.rem_euclid(1_000_000_000) as u32;
    let utc = Utc.timestamp_opt(secs, nanos).single()?;
    Some(utc.with_timezone(&chrono::Local).date_naive())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exif_dates() {
        let dt = parse_exif_datetime("2007:05:03 18:12:34").unwrap();
        assert_eq!(dt.date().to_string(), "2007-05-03");
        assert!(parse_exif_datetime("0000:00:00 00:00:00").is_none());
        assert!(parse_exif_datetime("not a date").is_none());
    }

    #[test]
    fn parses_dates_in_filenames() {
        assert_eq!(
            date_in_filename("IMG_20230715_121212.jpg")
                .unwrap()
                .to_string(),
            "2023-07-15"
        );
        assert_eq!(
            date_in_filename("Screenshot 2020-01-02 at 11.22.33.png")
                .unwrap()
                .to_string(),
            "2020-01-02"
        );
        assert!(date_in_filename("DSC_0001.JPG").is_none());
        assert_eq!(
            date_in_filename("Città20200102.jpg").unwrap().to_string(),
            "2020-01-02"
        );
        assert_eq!(
            date_in_filename("Été 2020-01-02 àé.png")
                .unwrap()
                .to_string(),
            "2020-01-02"
        );
    }

    #[test]
    fn capture_date_priority_is_respected() {
        let mut exif = ExifInfo::default();
        exif.date_time_original = None;
        exif.date_time_digitized = Some("2012-03-04 05:06:07".into());
        exif.date_time = Some("2013-01-01 00:00:00".into());
        let capture = resolve_capture_date(&exif, 1_600_000_000_000_000_000, true).unwrap();
        assert_eq!(capture.source, DateSource::ExifDigitized);
        assert_eq!(capture.date.to_string(), "2012-03-04");
        exif.date_time_original = Some("2011:01:02 03:04:05".into());
        let capture = resolve_capture_date(&exif, 1_600_000_000_000_000_000, true).unwrap();
        assert_eq!(capture.source, DateSource::ExifOriginal);
        assert_eq!(capture.date.to_string(), "2011-01-02");
    }

    #[test]
    fn mtime_fallback_can_be_disabled() {
        let exif = ExifInfo::default();
        assert!(resolve_capture_date(&exif, 1_600_000_000_000_000_000, false).is_none());
        assert_eq!(
            resolve_capture_date(&exif, 1_600_000_000_000_000_000, true)
                .unwrap()
                .source,
            DateSource::FileModified
        );
    }
}
