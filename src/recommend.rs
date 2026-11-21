//! Explainable recommendation scoring.
//!
//! Pixsort never claims to know which photograph is aesthetically better. The
//! score only encodes simple, visible archive-keeping preferences:
//!
//!   1. higher pixel resolution (not a guarantee of better quality),
//!   2. older, more reliable capture metadata,
//!   3. more complete metadata,
//!   4. a date embedded in the filename,
//!   5. larger file as a secondary indicator,
//!   6. keeping the existing archive copy as a deterministic tie-break.
//!
//! Every point awarded is shown to the user.

use crate::imaging::date_in_filename;
use crate::model::{DateSource, PhotoRecord, PhotoScore, ScoreFactor};

pub const MAX_RESOLUTION_POINTS: f32 = 40.0;
pub const MAX_DATE_POINTS: f32 = 15.0;
pub const MAX_AGE_POINTS: f32 = 10.0;
pub const MAX_METADATA_POINTS: f32 = 10.0;
pub const MAX_FILENAME_POINTS: f32 = 5.0;
pub const MAX_SIZE_POINTS: f32 = 5.0;

#[derive(Debug, Clone)]
pub struct Recommendation {
    /// Sorted best-first.
    pub scores: Vec<PhotoScore>,
    pub keeper_path: String,
}

impl Recommendation {
    pub fn top(&self) -> Option<&PhotoScore> {
        self.scores.first()
    }

    pub fn explanation(&self) -> String {
        match self.top() {
            Some(top) => {
                let mut out = format!("Recommended: {}\n", top.path);
                for factor in &top.factors {
                    out.push_str(&format!("  {:+.1} — {}\n", factor.points, factor.text));
                }
                out
            }
            None => "No recommendation available.".into(),
        }
    }

    pub fn score_for(&self, path: &str) -> Option<&PhotoScore> {
        self.scores.iter().find(|s| s.path == path)
    }
}

/// Computes a score for every member and sorts them best-first.
pub fn recommend(members: &[PhotoRecord]) -> Recommendation {
    let mut scores: Vec<PhotoScore> = members.iter().map(score_photo).collect();
    let max_pixels = members.iter().map(|m| m.pixels()).max().unwrap_or(0).max(1);
    let max_size = members
        .iter()
        .map(|m| m.file_size)
        .max()
        .unwrap_or(0)
        .max(1);

    // Age among reliable EXIF capture timestamps: older wins, awarding part of
    // the age points proportionally to how much older it is.
    let exif_times: Vec<i64> = members
        .iter()
        .filter_map(|m| {
            if m.taken_source.is_exif() {
                m.taken_at_ns
            } else {
                None
            }
        })
        .collect();
    let oldest = exif_times.iter().min().copied();
    let newest = exif_times.iter().max().copied();

    for (member, score) in members.iter().zip(scores.iter_mut()) {
        // Factor 1: resolution.
        let ratio = member.pixels() as f32 / max_pixels as f32;
        add_factor(
            score,
            MAX_RESOLUTION_POINTS * ratio,
            format!(
                "Resolution {} — {}",
                member.resolution_label(),
                if member.pixels() == max_pixels {
                    "the highest in this group".to_string()
                } else {
                    format!(
                        "smaller than the largest ({} × {})",
                        members
                            .iter()
                            .max_by_key(|m| m.pixels())
                            .map(|m| m.width)
                            .unwrap_or(0),
                        members
                            .iter()
                            .max_by_key(|m| m.pixels())
                            .map(|m| m.height)
                            .unwrap_or(0)
                    )
                }
            ),
        );

        // Factor 2a: reliability of the capture timestamp.
        let reliability = member.taken_source.reliability() as f32;
        let reliability_points = MAX_DATE_POINTS * (reliability / 4.0);
        add_factor(
            score,
            reliability_points,
            format!(
                "Capture date {} ({})",
                member.taken_on.as_deref().unwrap_or("unknown"),
                member.taken_source.label()
            ),
        );

        // Factor 2b: older original capture metadata.
        let age_points = match (oldest, newest, member.taken_source.is_exif()) {
            (Some(oldest), Some(newest), true) if newest > oldest => {
                let span = (newest - oldest) as f32;
                let position = (newest - member.taken_at_ns.unwrap_or(newest)) as f32;
                MAX_AGE_POINTS * (position / span)
            }
            (Some(_), Some(_), true) => MAX_AGE_POINTS,
            _ => 0.0,
        };
        if age_points > 0.0 {
            add_factor(
                score,
                age_points,
                if member.taken_at_ns == oldest {
                    format!(
                        "Oldest original capture timestamp in the group ({})",
                        member.taken_on.as_deref().unwrap_or("unknown")
                    )
                } else {
                    "Older original capture timestamp than the alternative".to_string()
                },
            );
        }

        // Factor 3: metadata completeness.
        let completeness = member.exif.completeness();
        add_factor(
            score,
            MAX_METADATA_POINTS * (completeness as f32 / 6.0),
            format!(
                "{} of 6 metadata fields present ({})",
                completeness,
                metadata_present(&member.exif)
            ),
        );

        // Factor 4: date embedded in the filename.
        if let Some(date) = date_in_filename(member.filename()) {
            add_factor(
                score,
                MAX_FILENAME_POINTS,
                format!("Filename encodes the date {date}"),
            );
        } else {
            add_factor(score, 0.0, "No date found in the filename".to_string());
        }

        // Factor 5: file size, secondary.
        add_factor(
            score,
            MAX_SIZE_POINTS * (member.file_size as f32 / max_size as f32),
            format!(
                "File size {} — {}",
                member.size_label(),
                if member.file_size == max_size {
                    "largest bytes in this group (secondary indicator)"
                } else {
                    "smaller than the largest file (secondary indicator)"
                }
            ),
        );

        // Factor 6: stability tie-break, only meaningful for near-equal scores.
        if member.root == crate::model::RootKind::Archive {
            add_factor(
                score,
                0.25,
                "Existing archive copy — retention tie-break, not a quality claim".to_string(),
            );
        }
    }

    scores.sort_by(|a, b| {
        b.total
            .partial_cmp(&a.total)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });
    let keeper_path = scores.first().map(|s| s.path.clone()).unwrap_or_default();
    Recommendation {
        scores,
        keeper_path,
    }
}

fn score_photo(photo: &PhotoRecord) -> PhotoScore {
    PhotoScore {
        path: photo.path.clone(),
        total: 0.0,
        factors: Vec::new(),
    }
}

fn add_factor(score: &mut PhotoScore, points: f32, text: String) {
    score.total += points;
    score.factors.push(ScoreFactor { points, text });
}

fn metadata_present(exif: &crate::model::ExifInfo) -> String {
    let mut present = Vec::new();
    if exif.date_time_original.is_some() {
        present.push("date");
    }
    if exif.make.is_some() {
        present.push("make");
    }
    if exif.model.is_some() {
        present.push("model");
    }
    if exif.lens.is_some() {
        present.push("lens");
    }
    if exif.gps.is_some() {
        present.push("GPS");
    }
    if exif.software.is_some() {
        present.push("software");
    }
    if present.is_empty() {
        "none".into()
    } else {
        present.join(", ")
    }
}

/// Formats a similarity indicator for the UI without implying a probability.
pub fn similarity_label(similarity: f32) -> String {
    format!("{similarity:.1}% perceptual similarity (heuristic indicator)")
}

/// A concise, human-readable per-group recommendation summary.
pub fn summarise(
    recommendation: &Recommendation,
    members: &[PhotoRecord],
    exact: bool,
    similarity: f32,
) -> String {
    let keeper = recommendation.top();
    let mut out = String::new();
    if exact {
        out.push_str("Identical normalised pixels (exact pixel hash). ");
    } else {
        out.push_str(&format!(
            "Different pixels, but {}.\n",
            similarity_label(similarity)
        ));
    }
    if let Some(top) = keeper {
        out.push_str(&format!("Suggested keeper: {}.\n", top.path));
        let runner_up = recommendation.scores.get(1);
        for factor in &top.factors {
            if factor.points > 0.0 {
                out.push_str(&format!("  • {} ({:+.1})\n", factor.text, factor.points));
            }
        }
        if let Some(runner) = runner_up {
            let delta = top.total - runner.total;
            out.push_str(&format!(
                "  Score {:.1} vs {:.1} for the next candidate (difference {:.1}).",
                top.total, runner.total, delta
            ));
        }
    }
    let _ = members;
    out
}

/// Debug helper used in docs/tests.
pub fn describe_source(source: DateSource) -> &'static str {
    source.label()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ExifInfo, ImageFormatKind, PhotoStatus, RootKind};

    fn photo(
        path: &str,
        w: u32,
        h: u32,
        root: RootKind,
        taken: Option<(&str, DateSource)>,
        size: u64,
    ) -> PhotoRecord {
        let mut exif = ExifInfo::default();
        if let Some((date, source)) = taken {
            if source.is_exif() {
                exif.has_exif = true;
                exif.date_time_original = Some(date.to_string());
            }
        }
        PhotoRecord {
            id: 0,
            path: path.into(),
            root,
            file_size: size,
            mtime_ns: 0,
            format: ImageFormatKind::Jpeg,
            width: w,
            height: h,
            pixel_hash: Some("aaaa".into()),
            phash: Some(0),
            taken_on: taken.map(|(d, _)| d[..10].to_string()),
            taken_at_ns: taken.map(|(d, _)| {
                chrono::NaiveDate::parse_from_str(&d[..10], "%Y-%m-%d")
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
                    .timestamp_nanos_opt()
                    .unwrap()
            }),
            taken_source: taken.map(|(_, s)| s).unwrap_or(DateSource::Unknown),
            orientation: 1,
            is_animated: false,
            exif,
            status: PhotoStatus::Indexed,
            error: None,
            indexed_at: 0,
        }
    }

    #[test]
    fn higher_resolution_wins() {
        let members = vec![
            photo("/in/small.jpg", 800, 600, RootKind::Input, None, 100),
            photo("/in/large.jpg", 4000, 3000, RootKind::Input, None, 100),
        ];
        let rec = recommend(&members);
        assert_eq!(rec.keeper_path, "/in/large.jpg");
        assert!(rec.explanation().contains("Resolution"));
    }

    #[test]
    fn older_exif_date_beats_newer() {
        let members = vec![
            photo(
                "/in/new.jpg",
                4000,
                3000,
                RootKind::Input,
                Some(("2020-01-01 00:00:00", DateSource::ExifOriginal)),
                100,
            ),
            photo(
                "/in/old.jpg",
                4000,
                3000,
                RootKind::Input,
                Some(("2007-05-03 00:00:00", DateSource::ExifOriginal)),
                100,
            ),
        ];
        let rec = recommend(&members);
        assert_eq!(rec.keeper_path, "/in/old.jpg");
        assert!(rec.explanation().contains("Oldest original capture"));
    }

    #[test]
    fn archive_copy_wins_exact_tie() {
        let members = vec![
            photo(
                "/in/same.jpg",
                1000,
                1000,
                RootKind::Input,
                Some(("2010-01-01 00:00:00", DateSource::ExifOriginal)),
                100,
            ),
            photo(
                "/archive/same.jpg",
                1000,
                1000,
                RootKind::Archive,
                Some(("2010-01-01 00:00:00", DateSource::ExifOriginal)),
                100,
            ),
        ];
        let rec = recommend(&members);
        assert_eq!(rec.keeper_path, "/archive/same.jpg");
    }

    #[test]
    fn similarity_label_is_hedged() {
        let label = similarity_label(97.4);
        assert!(label.contains("heuristic"));
    }
}
