//! Safe filesystem primitives.
//!
//! Invariants:
//!   * no existing file is ever overwritten — destination names are verified
//!     and uniquified,
//!   * copies are written to a temporary file, flushed, re-read and verified
//!     against the source hash, and only then published,
//!   * source files are removed only after a verified copy exists,
//!   * discarded photographs are moved to a trash directory, never deleted.

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::scan::current_file_state;

#[derive(Debug, Clone)]
pub struct CopyResult {
    pub path: PathBuf,
    pub bytes: u64,
    pub conflict_resolved: bool,
    pub source_sha256: String,
}

/// Returns a non-existing path, adding ` (1)`, ` (2)`, … before the extension.
pub fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "photo".into());
    let (stem, ext) = split_name(&name);
    for suffix in 1u32..100_000 {
        let candidate = if ext.is_empty() {
            format!("{stem} ({suffix})")
        } else {
            format!("{stem} ({suffix}).{ext}")
        };
        let candidate = parent.join(candidate);
        if !candidate.exists() {
            return candidate;
        }
    }
    path.to_path_buf()
}

fn split_name(name: &str) -> (String, String) {
    match name.rfind('.') {
        Some(index) if index > 0 && index + 1 < name.len() => {
            (name[..index].to_string(), name[index + 1..].to_string())
        }
        _ => (name.to_string(), String::new()),
    }
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    Ok(())
}

/// Streams a SHA-256 hash of the file bytes.
pub fn sha256_file(path: &Path) -> Result<String> {
    let file =
        File::open(path).with_context(|| format!("cannot open {} for hashing", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(crate::imaging::normalize::hex(&hasher.finalize()))
}

/// Copies a file to a specific directory, choosing a unique name based on
/// `preferred_name`. The returned path is guaranteed not to have existed
/// before this call. The copy is verified before it is published.
pub fn copy_to_destination(
    source: &Path,
    dest_dir: &Path,
    preferred_name: &str,
) -> Result<CopyResult> {
    fs::create_dir_all(dest_dir)
        .with_context(|| format!("cannot create {}", dest_dir.display()))?;
    let preferred = dest_dir.join(preferred_name);
    let planned = unique_path(&preferred);

    let source_state = current_file_state(source)?;
    let temp_path = dest_dir.join(format!(".pixsort-partial-{}.tmp", uuid::Uuid::new_v4()));

    let (source_hash, copied_bytes) = copy_to_temp_verified(source, &temp_path, source_state)?;
    let after_state = current_file_state(source)?;
    if after_state != source_state {
        let _ = fs::remove_file(&temp_path);
        bail!(
            "source file changed while it was being copied: {}",
            source.display()
        );
    }

    let final_path = publish_verified(&temp_path, &planned)?;
    sync_parent(&final_path);
    Ok(CopyResult {
        conflict_resolved: final_path != preferred,
        path: final_path,
        bytes: copied_bytes,
        source_sha256: source_hash,
    })
}

fn copy_to_temp_verified(
    source: &Path,
    temp_path: &Path,
    source_state: (u64, i64),
) -> Result<(String, u64)> {
    let input =
        File::open(source).with_context(|| format!("cannot read source {}", source.display()))?;
    let mut reader = BufReader::new(input);
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_path)
        .with_context(|| format!("cannot create temporary file {}", temp_path.display()))?;
    let mut writer = BufWriter::new(output);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    let result = (|| -> Result<()> {
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            writer.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
            total += read as u64;
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;
        Ok(())
    })();
    if let Err(err) = result {
        drop(writer);
        let _ = fs::remove_file(temp_path);
        return Err(err);
    }
    drop(writer);

    if total != source_state.0 {
        let _ = fs::remove_file(temp_path);
        bail!(
            "source size changed while copying (expected {}, read {total})",
            source_state.0
        );
    }

    let expected = crate::imaging::normalize::hex(&hasher.finalize());
    let actual = sha256_file(temp_path)?;
    if expected != actual {
        let _ = fs::remove_file(temp_path);
        bail!("copy verification failed for temporary file");
    }
    Ok((expected, total))
}

/// Publishes a fully verified temporary file without overwriting anything.
fn publish_verified(temp: &Path, preferred: &Path) -> Result<PathBuf> {
    let mut final_path = unique_path(preferred);
    // Hard-link publishes atomically and fails if the name already exists.
    match fs::hard_link(temp, &final_path) {
        Ok(()) => {
            let _ = fs::remove_file(temp);
            return Ok(final_path);
        }
        Err(_) => {
            // Filesystems without hard links (or a race): retry with a unique
            // name and a create-new copy.
            for _ in 0..8 {
                if let Some(parent) = final_path.parent() {
                    fs::create_dir_all(parent).ok();
                }
                match copy_temp_exclusive(temp, &final_path) {
                    Ok(()) => {
                        let _ = fs::remove_file(temp);
                        return Ok(final_path);
                    }
                    Err(err) if final_path.exists() => {
                        final_path = unique_path(preferred);
                        log::warn!("destination race while publishing: {err}");
                    }
                    Err(err) => {
                        let _ = fs::remove_file(temp);
                        return Err(err.context("cannot publish verified copy"));
                    }
                }
            }
            let _ = fs::remove_file(temp);
            bail!("could not publish the verified copy without overwriting a file")
        }
    }
}

fn copy_temp_exclusive(temp: &Path, final_path: &Path) -> Result<()> {
    let mut reader = BufReader::new(
        File::open(temp).with_context(|| format!("cannot reopen {}", temp.display()))?,
    );
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(final_path)
        .with_context(|| format!("cannot create {}", final_path.display()))?;
    let mut writer = BufWriter::new(output);
    let result = (|| -> Result<()> {
        std::io::copy(&mut reader, &mut writer)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        Ok(())
    })();
    drop(writer);
    if let Err(err) = result {
        let _ = fs::remove_file(final_path);
        return Err(err);
    }
    // The content came from an already verified temp file, but verify again
    // because we cannot rule out storage errors.
    let expected = sha256_file(temp)?;
    let actual = sha256_file(final_path)?;
    if expected != actual {
        let _ = fs::remove_file(final_path);
        bail!("verification failed after publishing");
    }
    Ok(())
}

pub fn sync_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        // Directory fsync is a durability nicety; unsupported on Windows.
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}

/// Moves `source` into the trash directory, preserving its path relative to
/// the archive root when possible. Returns the trash path.
pub fn move_to_trash(source: &Path, archive_root: &Path, trash_root: &Path) -> Result<PathBuf> {
    let target = trash_target(source, archive_root, trash_root)?;
    move_file_verified(source, &target)?;
    Ok(target)
}

/// Computes the trash path a plan would use, without creating anything.
pub fn planned_trash_path(source: &Path, archive_root: &Path, trash_root: &Path) -> PathBuf {
    let relative = source
        .strip_prefix(archive_root)
        .ok()
        .filter(|rel| {
            rel.components()
                .all(|c| !matches!(c, Component::ParentDir | Component::RootDir))
        })
        .map(|rel| rel.to_path_buf())
        .unwrap_or_else(|| {
            PathBuf::from(
                source
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "photo".into()),
            )
        });
    let target = trash_root.join(relative);
    if target.exists() {
        unique_path(&target)
    } else {
        target
    }
}

pub fn trash_target(source: &Path, archive_root: &Path, trash_root: &Path) -> Result<PathBuf> {
    let target = planned_trash_path(source, archive_root, trash_root);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    Ok(target)
}

/// Moves a file without losing it. Same-filesystem renames are atomic; for
/// cross-device moves a verified copy is made first and the source is removed
/// only afterwards.
pub fn move_file_verified(source: &Path, target: &Path) -> Result<()> {
    move_file_impl(source, target, false)
}

/// The cross-filesystem fallback, exposed so it can be tested directly:
/// copy, verify, then remove the source.
pub fn copy_and_remove_verified(source: &Path, target: &Path) -> Result<()> {
    move_file_impl(source, target, true)
}

fn move_file_impl(source: &Path, target: &Path, force_copy: bool) -> Result<()> {
    if !source.exists() {
        bail!("source file is missing: {}", source.display());
    }
    if target.exists() {
        bail!("refusing to overwrite existing file: {}", target.display());
    }
    ensure_parent(target)?;
    let before = current_file_state(source)?;

    if !force_copy {
        match fs::rename(source, target) {
            Ok(()) => {
                sync_parent(target);
                return Ok(());
            }
            Err(rename_error) => {
                log::info!(
                    "rename {} → {} failed ({rename_error}); falling back to verified copy",
                    source.display(),
                    target.display()
                );
            }
        }
    }

    // Cross-filesystem (or otherwise unsupported) fallback.
    if target.exists() {
        bail!("refusing to overwrite existing file: {}", target.display());
    }
    let temp = target.with_file_name(format!(".pixsort-partial-{}.tmp", uuid::Uuid::new_v4()));
    let (source_hash, _) = copy_to_temp_verified(source, &temp, before)?;
    let after = current_file_state(source)?;
    if after != before {
        let _ = fs::remove_file(&temp);
        bail!(
            "source file changed while it was being moved: {}",
            source.display()
        );
    }
    let actual = publish_verified(&temp, target)?;
    if sha256_file(&actual)? != source_hash {
        bail!("verification of the moved copy failed");
    }
    let final_state = current_file_state(source)?;
    if final_state != before {
        bail!(
            "source file changed before it could be removed; both copies were kept: {}",
            source.display()
        );
    }
    fs::remove_file(source).with_context(|| {
        format!(
            "cannot remove source after verified copy: {}",
            source.display()
        )
    })?;
    sync_parent(&actual);
    Ok(())
}

/// Copies a file to a specific target path (no overwrite), verifies it, and
/// returns the hash. Used by tests and recovery tooling.
pub fn copy_file_verified(source: &Path, target: &Path) -> Result<CopyResult> {
    let state = current_file_state(source)?;
    if target.exists() {
        bail!("refusing to overwrite existing file: {}", target.display());
    }
    ensure_parent(target)?;
    let temp = target.with_file_name(format!(".pixsort-partial-{}.tmp", uuid::Uuid::new_v4()));
    let (hash, bytes) = copy_to_temp_verified(source, &temp, state)?;
    let after = current_file_state(source)?;
    if after != state {
        let _ = fs::remove_file(&temp);
        bail!("source file changed while copying");
    }
    let final_path = publish_verified(&temp, target)?;
    sync_parent(&final_path);
    Ok(CopyResult {
        conflict_resolved: final_path != target,
        path: final_path,
        bytes,
        source_sha256: hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_is_verified_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.jpg");
        fs::write(&src, b"hello photo").unwrap();
        let out = copy_to_destination(&src, &dir.path().join("dest"), "src.jpg").unwrap();
        assert_eq!(fs::read(&out.path).unwrap(), b"hello photo");
        assert!(!out.conflict_resolved);

        let second = copy_to_destination(&src, &dir.path().join("dest"), "src.jpg").unwrap();
        assert!(second.conflict_resolved);
        assert_ne!(second.path, out.path);
        assert_eq!(fs::read(&second.path).unwrap(), b"hello photo");
        assert_eq!(fs::read(&src).unwrap(), b"hello photo");
    }

    #[test]
    fn move_to_trash_preserves_structure() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive");
        fs::create_dir_all(archive.join("2020")).unwrap();
        let photo = archive.join("2020").join("a.jpg");
        fs::write(&photo, b"jpeg bytes").unwrap();
        let trash = dir.path().join("trash");
        let target = move_to_trash(&photo, &archive, &trash).unwrap();
        assert!(!photo.exists());
        assert_eq!(target, trash.join("2020").join("a.jpg"));
        assert_eq!(fs::read(&target).unwrap(), b"jpeg bytes");
    }

    #[test]
    fn trash_conflicts_are_suffixed() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive");
        fs::create_dir_all(&archive).unwrap();
        let trash = dir.path().join("trash");
        fs::create_dir_all(&trash).unwrap();
        fs::write(trash.join("a.jpg"), b"older trash").unwrap();
        let photo = archive.join("a.jpg");
        fs::write(&photo, b"new trash").unwrap();
        let target = move_to_trash(&photo, &archive, &trash).unwrap();
        assert_eq!(target, trash.join("a (1).jpg"));
        assert_eq!(fs::read(trash.join("a.jpg")).unwrap(), b"older trash");
    }

    #[test]
    fn unique_path_handles_no_extension() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("photo"), b"x").unwrap();
        assert_eq!(
            unique_path(&dir.path().join("photo")),
            dir.path().join("photo (1)")
        );
    }

    #[test]
    fn sha256_matches_known_value() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a");
        fs::write(&file, b"abc").unwrap();
        assert_eq!(
            sha256_file(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
