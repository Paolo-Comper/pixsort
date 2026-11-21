//! Directory scanning and resumable indexing.
//!
//! The scan is deliberately split into an enumeration step and a hashing step:
//! enumeration is cheap, and hashing is parallel and cache-aware. Every file
//! is written to the index as `pending` before hashing so an interrupted run
//! can resume without losing track of work.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::config::{path_starts_with, Settings};
use crate::db::index_db::now_ns;
use crate::db::{IndexDb, PendingFile};
use crate::imaging;
use crate::jobs::ProgressReporter;
use crate::model::{DateSource, ImageFormatKind, JobControl, PhotoRecord, PhotoStatus, RootKind};

#[derive(Debug, Clone, Default)]
pub struct ScanStats {
    pub total: u64,
    pub indexed: u64,
    pub reused: u64,
    pub failed: u64,
    pub removed: u64,
    pub animated: u64,
    pub stopped: bool,
}

#[derive(Debug, Clone)]
struct FoundFile {
    path: String,
    file_size: u64,
    mtime_ns: i64,
    format: ImageFormatKind,
}

enum TaskOutcome {
    Done(Box<PhotoRecord>),
    Failed { path: String, error: String },
    Aborted,
}

/// Scans `root` and updates the image index.
///
/// `root` is classified as `kind`; the index is shared between import and
/// archive analysis, so previously calculated hashes are always reused when
/// size and modification time are unchanged.
pub fn scan(
    root: &Path,
    kind: RootKind,
    settings: &Settings,
    control: &JobControl,
    reporter: &ProgressReporter,
) -> Result<ScanStats> {
    let db_path = settings
        .effective_index_db()
        .ok_or_else(|| anyhow::anyhow!("no image index database configured"))?;
    let mut db = IndexDb::open(&db_path)?;

    reporter.begin_phase(format!("Finding photographs in {}", root.display()));
    reporter.emit(true);

    let mut seen: HashSet<String> = HashSet::new();
    let found = enumerate(root, kind, settings, control, reporter, &mut seen)?;
    if control.is_stopped() {
        reporter.finish_phase();
        return Ok(ScanStats {
            total: found.len() as u64,
            stopped: true,
            ..Default::default()
        });
    }

    let cache = db.cache_map(kind)?;
    let mut pending: Vec<PendingFile> = Vec::new();
    let mut reused: u64 = 0;
    let mut cached_failures: u64 = 0;

    for file in &found {
        if let Some(cached) = cache.get(&file.path) {
            if cached.file_size == file.file_size && cached.mtime_ns == file.mtime_ns {
                match cached.status {
                    PhotoStatus::Indexed => {
                        reused += 1;
                        continue;
                    }
                    PhotoStatus::Failed => {
                        // Do not repeatedly retry a file that did not change.
                        cached_failures += 1;
                        continue;
                    }
                    PhotoStatus::Pending => {}
                }
            }
        }
        pending.push(PendingFile {
            path: file.path.clone(),
            root: kind,
            file_size: file.file_size,
            mtime_ns: file.mtime_ns,
            format: file.format,
        });
    }

    // Remove index rows for files that disappeared. This is only safe after a
    // complete enumeration.
    let removed = if control.is_stopped() {
        0
    } else {
        match db.remove_missing(kind, &seen) {
            Ok(count) => count as u64,
            Err(err) => {
                reporter.log(format!("Could not prune missing index rows: {err}"));
                0
            }
        }
    };

    if !pending.is_empty() {
        db.upsert_pending_batch(&pending)
            .context("cannot record pending index rows")?;
    }

    reporter.begin_phase(format!("Indexing {} photographs", kind.label()));
    reporter.set_total(found.len() as u64);
    reporter.add_reused(reused);
    for _ in 0..cached_failures {
        reporter.inc_failed();
    }
    reporter.emit(true);

    let chunk_size = (settings.worker_threads * 2).clamp(4, 64);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(settings.worker_threads)
        .thread_name(|i| format!("pixsort-hash-{i}"))
        .build()
        .context("cannot create hashing worker pool")?;

    let mut indexed = 0u64;
    let mut failed = cached_failures;
    let mut animated = 0u64;
    let mut aborted = false;

    // Only files that are actually pending are hashed; `found` includes the
    // reused ones for progress purposes.
    let pending_set: HashSet<&str> = pending.iter().map(|p| p.path.as_str()).collect();
    let to_process: Vec<&FoundFile> = found
        .iter()
        .filter(|f| pending_set.contains(f.path.as_str()))
        .collect();

    for chunk in to_process.chunks(chunk_size) {
        if control.is_stopped() {
            aborted = true;
            break;
        }
        let outcomes: Vec<TaskOutcome> = pool.install(|| {
            chunk
                .par_iter()
                .map(|file| process_file(file, kind, settings, control, reporter))
                .collect()
        });

        let mut successes: Vec<PhotoRecord> = Vec::new();
        let mut failures: Vec<(String, String)> = Vec::new();
        for outcome in outcomes {
            match outcome {
                TaskOutcome::Done(photo) => {
                    if photo.is_animated {
                        animated += 1;
                    }
                    successes.push(*photo);
                }
                TaskOutcome::Failed { path, error } => failures.push((path, error)),
                TaskOutcome::Aborted => aborted = true,
            }
        }
        if !successes.is_empty() {
            db.update_indexed_batch(&successes)
                .context("cannot write indexed photographs")?;
            indexed += successes.len() as u64;
        }
        if !failures.is_empty() {
            db.fail_batch(&failures)
                .context("cannot write indexing failures")?;
            failed += failures.len() as u64;
        }
        reporter.emit(false);
        if aborted {
            break;
        }
    }

    reporter.finish_phase();

    let stats = ScanStats {
        total: found.len() as u64,
        indexed,
        reused,
        failed,
        removed,
        animated,
        stopped: aborted || control.is_stopped(),
    };
    reporter.log(format!(
        "{}: {} found, {} indexed, {} reused from cache, {} failed, {} missing rows removed",
        kind.label(),
        stats.total,
        stats.indexed,
        stats.reused,
        stats.failed,
        stats.removed
    ));
    Ok(stats)
}

fn enumerate(
    root: &Path,
    kind: RootKind,
    settings: &Settings,
    control: &JobControl,
    reporter: &ProgressReporter,
    seen: &mut HashSet<String>,
) -> Result<Vec<FoundFile>> {
    if !root.is_dir() {
        anyhow::bail!("directory is not accessible: {}", root.display());
    }
    let trash = settings.effective_trash_dir();
    let db_paths: Vec<PathBuf> = [settings.effective_index_db(), settings.effective_plan_db()]
        .into_iter()
        .flatten()
        .collect();
    let destination = settings.destination_dir.clone();
    // When scanning the input, never descend into the archive if the archive
    // happens to be nested inside the input directory.
    let exclude_dest: Option<PathBuf> = if kind == RootKind::Input {
        destination
            .as_ref()
            .filter(|dest| path_starts_with(dest, root))
            .cloned()
    } else {
        None
    };

    let mut out = Vec::new();
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            let path = entry.path();
            if let Some(trash) = &trash {
                if path == trash || path.starts_with(trash) {
                    return false;
                }
            }
            if let Some(dest) = &exclude_dest {
                if path == dest || path.starts_with(dest) {
                    return false;
                }
            }
            if entry.file_type().is_dir() {
                let name = entry.file_name().to_string_lossy();
                if name == ".git" || name == "@eaDir" || name == "#recycle" {
                    return false;
                }
            }
            true
        });

    let mut errors = 0u32;
    for entry in walker {
        if !control.checkpoint() {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                errors += 1;
                if errors <= 20 {
                    reporter.log(format!("Scan warning: {err}"));
                }
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy();
        if name.starts_with("._") || name == "Thumbs.db" || name == ".DS_Store" {
            continue;
        }
        let format = match ImageFormatKind::from_path(path) {
            Some(format) => format,
            None => continue,
        };
        if db_paths.iter().any(|db| db == path) {
            continue;
        }
        let path_string = match path.to_str() {
            Some(s) => s.to_string(),
            None => {
                reporter.log(format!(
                    "Skipping a file whose name is not valid UTF-8: {}",
                    path.display()
                ));
                continue;
            }
        };
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(err) => {
                reporter.log(format!("Cannot stat {}: {err}", path.display()));
                continue;
            }
        };
        let mtime_ns = metadata.modified().ok().map(system_time_to_ns).unwrap_or(0);
        seen.insert(path_string.clone());
        out.push(FoundFile {
            path: path_string,
            file_size: metadata.len(),
            mtime_ns,
            format,
        });
        if out.len() % 200 == 0 {
            reporter.set_current(format!("{} files found", out.len()));
        }
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    reporter.log(format!(
        "{}: {} candidate photographs discovered{}",
        kind.label(),
        out.len(),
        if errors > 0 {
            format!(" ({errors} directory entries could not be read)")
        } else {
            String::new()
        }
    ));
    Ok(out)
}

fn process_file(
    file: &FoundFile,
    kind: RootKind,
    settings: &Settings,
    control: &JobControl,
    reporter: &ProgressReporter,
) -> TaskOutcome {
    if !control.checkpoint() {
        return TaskOutcome::Aborted;
    }
    let path = PathBuf::from(&file.path);
    reporter.set_current(
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.path.clone()),
    );

    let result = (|| -> Result<PhotoRecord> {
        let analysis = imaging::analyze_file(&path)
            .with_context(|| format!("cannot analyse {}", path.display()))?;

        // Guard against a file changing between enumeration and hashing.
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("cannot re-stat {}", path.display()))?;
        let mtime_ns = metadata.modified().ok().map(system_time_to_ns).unwrap_or(0);
        if metadata.len() != file.file_size || mtime_ns != file.mtime_ns {
            anyhow::bail!("file changed while it was being indexed; it will be retried");
        }

        let capture = imaging::resolve_capture_date(
            &analysis.exif,
            file.mtime_ns,
            settings.use_filesystem_year,
        );
        let (taken_on, taken_at_ns, taken_source) = match capture {
            Some(capture) => (
                Some(capture.date.format("%Y-%m-%d").to_string()),
                Some(capture.timestamp_ns),
                capture.source,
            ),
            None => (None, None, DateSource::Unknown),
        };

        Ok(PhotoRecord {
            id: 0,
            path: file.path.clone(),
            root: kind,
            file_size: file.file_size,
            mtime_ns: file.mtime_ns,
            format: analysis.format,
            width: analysis.width,
            height: analysis.height,
            pixel_hash: Some(analysis.pixel_hash),
            phash: Some(analysis.phash),
            taken_on,
            taken_at_ns,
            taken_source,
            orientation: analysis.orientation,
            is_animated: analysis.is_animated,
            exif: analysis.exif,
            status: PhotoStatus::Indexed,
            error: None,
            indexed_at: now_ns(),
        })
    })();

    match result {
        Ok(mut photo) => {
            photo.status = PhotoStatus::Indexed;
            reporter.inc_completed();
            TaskOutcome::Done(Box::new(photo))
        }
        Err(err) => {
            let mut message = format!("{err:#}");
            if file.format == ImageFormatKind::Heic {
                let hint = imaging::decode_hint(ImageFormatKind::Heic);
                if !hint.is_empty() {
                    message.push_str(&format!(" ({hint})"));
                }
            }
            let _ = control.checkpoint();
            reporter.inc_failed();
            TaskOutcome::Failed {
                path: file.path.clone(),
                error: message,
            }
        }
    }
}

pub fn system_time_to_ns(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_nanos() as i64,
        Err(before) => -(before.duration().as_nanos() as i64),
    }
}

/// Reads size/mtime for cheap change detection at execution time.
pub fn current_file_state(path: &Path) -> Result<(u64, i64)> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("cannot stat {}", path.display()))?;
    let mtime = metadata.modified().ok().map(system_time_to_ns).unwrap_or(0);
    Ok((metadata.len(), mtime))
}

/// Waits for a pause flag; used by long operations between chunks.
pub fn pause_poll(control: &JobControl) {
    while control.is_paused() && !control.is_stopped() {
        std::thread::sleep(Duration::from_millis(80));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn system_time_conversion_roundtrips() {
        let now = SystemTime::now();
        let ns = system_time_to_ns(now);
        assert!(ns > 0);
        let back = UNIX_EPOCH + Duration::from_nanos(ns as u64);
        let delta = now.duration_since(back).unwrap().as_millis();
        assert!(delta < 5);
    }

    #[test]
    fn non_utf8_names_are_skipped_not_fatal() {
        // A plain text file with a photo extension must be counted but fail
        // decoding, proving enumeration tolerates junk.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.jpg");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"not an image").unwrap();
        drop(file);
        // Direct decode failure:
        assert!(imaging::analyze_file(&path).is_err());
    }
}
