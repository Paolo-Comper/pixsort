//! Safe execution of the modification plan.
//!
//! Execution is sequential and idempotent at item granularity:
//!   * the intended trash path is persisted before a destructive move,
//!   * copies are verified and published without overwriting,
//!   * `running` items from an interrupted run are reconciled on restart,
//!   * the archive is only modified after the explicit approval flag.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Settings;
use crate::db::index_db::now_ns;
use crate::db::{IndexDb, PlanDb};
use crate::fsops;
use crate::jobs::ProgressReporter;
use crate::model::{ApplyReport, ItemStatus, JobControl, OpType, PhotoRecord, PlanItem, RootKind};
use crate::scan::current_file_state;

/// The GUI calls this only after showing the explicit approval dialog.
pub fn apply_plan(
    settings: &Settings,
    confirmed: bool,
    control: &JobControl,
    reporter: &ProgressReporter,
) -> Result<ApplyReport> {
    if !confirmed {
        bail!("Refusing to modify the archive without explicit user approval.");
    }
    settings
        .validate_for_apply()
        .context("cannot apply: configuration is not valid")?;

    let plan_path = settings
        .effective_plan_db()
        .ok_or_else(|| anyhow::anyhow!("no modification plan database configured"))?;
    let index_path = settings
        .effective_index_db()
        .ok_or_else(|| anyhow::anyhow!("no image index database configured"))?;
    let dest_root = settings
        .destination_dir
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no archive destination configured"))?;
    let input_root = settings.input_dir.clone();
    let trash_root = settings
        .effective_trash_dir()
        .ok_or_else(|| anyhow::anyhow!("no trash directory configured"))?;

    let plan = PlanDb::open(&plan_path)?;
    let mut index = IndexDb::open(&index_path)?;
    let session = plan
        .current_session()?
        .ok_or_else(|| anyhow::anyhow!("there is no modification plan to apply"))?;
    if !matches!(
        session.status,
        crate::db::SessionStatus::Active | crate::db::SessionStatus::Applying
    ) {
        bail!(
            "the current session is {} and cannot be applied",
            session.status.label()
        );
    }
    if !crate::config::same_path(Path::new(&session.dest_dir), &dest_root) {
        bail!(
            "The configured archive destination does not match this session ({}).              Reconnect the original destination or discard the session.",
            session.dest_dir
        );
    }
    if session.mode == crate::db::SessionMode::Import {
        match input_root.as_deref() {
            Some(input)
                if crate::config::same_path(Path::new(&session.input_dir), input) => {}
            _ => bail!(
                "The configured input folder does not match this session ({}).                  Reconnect the original input folder or discard the session.",
                session.input_dir
            ),
        }
    }
    if !session.uses_current_grouping() {
        bail!(
            "This plan was created with an older duplicate-grouping algorithm (before \
             complete-linkage groups and size limits). It is not safe to apply. Discard this \
             session and run Archive analysis again."
        );
    }
    plan.check_ready_to_apply(session.id)?;
    let pending = plan.load_pending_items(session.id)?;

    // Preflight the write capabilities needed by the pending operations. This
    // runs before the session is marked as applying and before anything could
    // be moved to trash, so an unwritable folder cannot leave the archive
    // partially modified. We check the directories actually touched by each
    // operation rather than assuming the archive root is writable: a move-only
    // plan needs write access to the source folders, not necessarily the root.
    let (archive_dirs, needs_trash) = preflight_targets(&pending);
    if needs_trash {
        preflight_writable_directory(
            &trash_root,
            "trash folder",
            "Choose a writable trash folder in Settings. It may be on a different disk: Pixsort \
             uses a verified copy-and-remove when an atomic move is impossible. The built-in \
             local trash folder is usually a safe choice.",
        )?;
    }
    for dir in archive_dirs {
        preflight_writable_directory(
            &dir,
            "archive folder",
            "On a shared or server disk, add your user to the group that owns these folders or \
             grant a write ACL on just the required folders. Running the whole application as \
             root is not recommended and cannot bypass a read-only mount.",
        )?;
    }
    plan.set_session_status(session.id, crate::db::SessionStatus::Applying)?;

    reporter.begin_phase("Applying modification plan");
    reporter.set_total(pending.len() as u64);
    reporter.log(format!(
        "Applying {} operation(s); trash directory: {}",
        pending.len(),
        trash_root.display()
    ));

    let start = Instant::now();
    let mut report = ApplyReport::default();

    for item in pending {
        if !control.checkpoint() {
            report.stopped = true;
            break;
        }
        reporter.set_current(item.title());
        let outcome = match execute_item(&plan, &mut index, &dest_root, &trash_root, &item) {
            Ok(outcome) => outcome,
            Err(err) => {
                let message = format!("{err:#}");
                let _ = plan.finish_item(
                    item.id,
                    ItemStatus::Failed,
                    None,
                    item.detail.as_deref(),
                    Some(&message),
                );
                report.failed += 1;
                report.problems.push(format!("{}: {message}", item.title()));
                reporter.inc_failed();
                continue;
            }
        };

        let status = match outcome.kind {
            OutcomeKind::Applied => ItemStatus::Applied,
            OutcomeKind::Skipped => ItemStatus::Noop,
            OutcomeKind::Failed => ItemStatus::Failed,
            OutcomeKind::Blocked => ItemStatus::Blocked,
        };
        let detail = outcome.detail.as_deref().or(item.detail.as_deref());
        plan.finish_item(
            item.id,
            status,
            outcome.result_path.as_deref(),
            detail,
            outcome.error.as_deref(),
        )?;

        match outcome.kind {
            OutcomeKind::Applied => {
                report.applied += 1;
                if outcome.inserted {
                    report.inserted += 1;
                }
                if outcome.replaced {
                    report.replaced += 1;
                }
                report.trashed += outcome.trashed;
                if outcome.conflict {
                    report.conflicts_resolved += 1;
                }
                if let Some(warning) = &outcome.error {
                    report.problems.push(format!("{}: {warning}", item.title()));
                }
                reporter.inc_completed();
            }
            OutcomeKind::Skipped => {
                report.skipped += 1;
                reporter.inc_completed();
            }
            OutcomeKind::Failed | OutcomeKind::Blocked => {
                if outcome.kind == OutcomeKind::Failed {
                    report.failed += 1;
                } else {
                    report.blocked += 1;
                }
                report.problems.push(format!(
                    "{}: {}",
                    item.title(),
                    outcome.error.as_deref().unwrap_or("blocked")
                ));
                reporter.inc_failed();
            }
        }
        reporter.emit(false);
    }

    let counts = plan.counts(session.id)?;
    if !report.stopped && counts.unfinished() == 0 && counts.failed == 0 {
        plan.set_session_status(session.id, crate::db::SessionStatus::Applied)?;
    } else {
        plan.set_session_status(session.id, crate::db::SessionStatus::Active)?;
    }

    report.elapsed_secs = start.elapsed().as_secs_f64();
    reporter.finish_phase();
    reporter.log(format!(
        "Apply finished: {} applied, {} skipped, {} failed, {} blocked, {} moved to trash, \
         {} inserted, {} replaced",
        report.applied,
        report.skipped,
        report.failed,
        report.blocked,
        report.trashed,
        report.inserted,
        report.replaced
    ));
    Ok(report)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum OutcomeKind {
    Applied,
    Skipped,
    Failed,
    #[default]
    Blocked,
}

#[derive(Debug, Default)]
struct Outcome {
    kind: OutcomeKind,
    result_path: Option<String>,
    detail: Option<String>,
    error: Option<String>,
    inserted: bool,
    replaced: bool,
    trashed: u64,
    conflict: bool,
}

impl Outcome {
    fn applied() -> Self {
        Self {
            kind: OutcomeKind::Applied,
            ..Default::default()
        }
    }
    fn failed(message: impl Into<String>) -> Self {
        Self {
            kind: OutcomeKind::Failed,
            error: Some(message.into()),
            ..Default::default()
        }
    }
    fn blocked(message: impl Into<String>) -> Self {
        Self {
            kind: OutcomeKind::Blocked,
            error: Some(message.into()),
            ..Default::default()
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ExecMeta {
    trash: Option<String>,
    dest: Option<String>,
}

fn parse_meta(raw: Option<&str>) -> ExecMeta {
    raw.and_then(|s| serde_json::from_str::<ExecMeta>(s).ok())
        .unwrap_or_default()
}

fn meta_json(meta: &ExecMeta) -> Option<String> {
    serde_json::to_string(meta).ok()
}

/// Accepts the item's persisted recovery metadata plus the planned paths.
fn recovery_paths(item: &PlanItem) -> (Option<PathBuf>, Option<PathBuf>) {
    let meta = parse_meta(item.exec_meta.as_deref());
    (
        meta.trash.map(PathBuf::from),
        meta.dest
            .map(PathBuf::from)
            .or_else(|| item.dest_path.clone().map(PathBuf::from)),
    )
}

fn execute_item(
    plan: &PlanDb,
    index: &mut IndexDb,
    dest_root: &Path,
    trash_root: &Path,
    item: &PlanItem,
) -> Result<Outcome> {
    match item.op {
        OpType::Skip => Ok(Outcome {
            kind: OutcomeKind::Skipped,
            detail: Some(
                "No filesystem change: the redundant import stays in the input folder.".into(),
            ),
            ..Default::default()
        }),
        OpType::Review => Ok(Outcome::blocked(
            "This operation needs a manual decision and cannot be executed.",
        )),
        OpType::MoveToTrash => execute_trash(plan, index, dest_root, trash_root, item),
        OpType::Insert => execute_insert(plan, index, dest_root, item),
        OpType::Replace => execute_replace(plan, index, dest_root, trash_root, item),
    }
}

/// Returns the archive directories that must be writable for the pending
/// operations, plus whether the trash folder is needed.
fn preflight_targets(pending: &[PlanItem]) -> (Vec<PathBuf>, bool) {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut needs_trash = false;
    let mut add_parent = |path: Option<&str>| {
        let Some(parent) = path.and_then(|value| Path::new(value).parent()) else {
            return;
        };
        if seen.insert(parent.to_path_buf()) {
            dirs.push(parent.to_path_buf());
        }
    };
    for item in pending {
        match item.op {
            OpType::Insert => add_parent(item.dest_path.as_deref()),
            OpType::Replace => {
                add_parent(item.dest_path.as_deref());
                add_parent(item.existing_path.as_deref());
                needs_trash = true;
            }
            OpType::MoveToTrash => {
                add_parent(
                    item.existing_path
                        .as_deref()
                        .or(item.source_path.as_deref()),
                );
                needs_trash = true;
            }
            OpType::Skip | OpType::Review => {}
        }
    }
    (dirs, needs_trash)
}

fn preflight_writable_directory(path: &Path, label: &str, hint: &str) -> Result<()> {
    if let Err(err) = std::fs::create_dir_all(path) {
        bail!(
            "The {label} is not writable: {}\n{err}\n\nNo archive files were modified. {hint}",
            path.display()
        );
    }
    let probe = path.join(format!(".pixsort-write-test-{}", uuid::Uuid::new_v4()));
    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe);
    match result {
        Ok(file) => {
            drop(file);
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(err) => {
            let _ = std::fs::remove_file(&probe);
            bail!(
                "The {label} is not writable: {}\n{err}\n\nNo archive files were modified. {hint}",
                path.display()
            );
        }
    }
}

fn check_source_record(
    index: &IndexDb,
    path: &Path,
) -> Result<std::result::Result<PhotoRecord, String>> {
    let record = match index.get_by_path(&path.to_string_lossy())? {
        Some(record) => record,
        None => {
            return Ok(Err(format!(
                "{} is not present in the image index; rescan before applying.",
                path.display()
            )))
        }
    };
    let state = match current_file_state(path) {
        Ok(state) => state,
        Err(_) => {
            return Ok(Err(format!(
                "source file is missing or unreadable: {}",
                path.display()
            )))
        }
    };
    if state.0 != record.file_size || state.1 != record.mtime_ns {
        return Ok(Err(format!(
            "{} changed after it was indexed; rescan before applying.",
            path.display()
        )));
    }
    Ok(Ok(record))
}

fn sha256_if_exists(path: &Path) -> Option<String> {
    if path.is_file() {
        fsops::sha256_file(path).ok()
    } else {
        None
    }
}

fn execute_insert(
    plan: &PlanDb,
    index: &mut IndexDb,
    dest_root: &Path,
    item: &PlanItem,
) -> Result<Outcome> {
    let source = match &item.source_path {
        Some(path) => PathBuf::from(path),
        None => return Ok(Outcome::failed("INSERT has no source path")),
    };
    let (_, recorded_dest) = recovery_paths(item);

    // Reconcile an interrupted copy: if the destination was published but the
    // database was not updated yet, finish the bookkeeping.
    if item.status == ItemStatus::Running {
        if let Some(dest) = &recorded_dest {
            if dest.is_file() {
                let record = index.get_by_path(&source.to_string_lossy())?;
                let same_content = match (&record, source.is_file()) {
                    (Some(_), true) => sha256_if_exists(&source) == sha256_if_exists(dest),
                    (Some(record), false) => dest
                        .metadata()
                        .map(|m| m.len() == record.file_size)
                        .unwrap_or(false),
                    _ => false,
                };
                if same_content {
                    let mut outcome = Outcome::applied();
                    if let Some(record) = record {
                        let warning = update_index_after_copy(index, &record, dest, None)?;
                        outcome.error = warning;
                    }
                    outcome.result_path = Some(dest.to_string_lossy().into_owned());
                    outcome.inserted = true;
                    outcome.detail =
                        Some("Resumed: the verified copy was already in place.".into());
                    return Ok(outcome);
                }
            }
        }
    }

    let record = match check_source_record(index, &source)? {
        Ok(record) => record,
        Err(reason) => return Ok(Outcome::blocked(reason)),
    };

    let (dest_dir, preferred_name) = destination_for_item(item, &record, dest_root);
    let meta = ExecMeta {
        trash: None,
        dest: Some(
            dest_dir
                .join(&preferred_name)
                .to_string_lossy()
                .into_owned(),
        ),
    };
    plan.start_item(item.id, item.detail.as_deref(), meta_json(&meta).as_deref())?;

    let copy = fsops::copy_to_destination(&source, &dest_dir, &preferred_name)
        .with_context(|| format!("cannot copy {} into the archive", source.display()))?;
    let actual_meta = ExecMeta {
        trash: None,
        dest: Some(copy.path.to_string_lossy().into_owned()),
    };
    plan.start_item(
        item.id,
        item.detail.as_deref(),
        meta_json(&actual_meta).as_deref(),
    )?;
    let warning = update_index_after_copy(index, &record, &copy.path, None)?;

    let mut detail = String::new();
    if copy.conflict_resolved {
        detail.push_str(&format!(
            "A name conflict was resolved at execution time; written as {}.\n",
            copy.path.display()
        ));
    }
    detail.push_str(&format!(
        "Copied and verified ({}); the input file was left untouched.",
        crate::model::human_bytes(copy.bytes)
    ));
    Ok(Outcome {
        kind: OutcomeKind::Applied,
        result_path: Some(copy.path.to_string_lossy().into_owned()),
        detail: Some(detail),
        error: warning,
        inserted: true,
        conflict: copy.conflict_resolved,
        ..Default::default()
    })
}

fn execute_trash(
    plan: &PlanDb,
    index: &mut IndexDb,
    dest_root: &Path,
    trash_root: &Path,
    item: &PlanItem,
) -> Result<Outcome> {
    let source = match &item.existing_path {
        Some(path) => PathBuf::from(path),
        None => return Ok(Outcome::failed("MOVE_TO_TRASH has no existing path")),
    };
    let (persisted_trash, _) = recovery_paths(item);

    if !source.exists() {
        if let Some(trash) = &persisted_trash {
            if trash.is_file() {
                let _ = index.delete_path(source.to_string_lossy().as_ref());
                return Ok(Outcome {
                    kind: OutcomeKind::Applied,
                    detail: Some(format!(
                        "Resumed: the file had already been moved to {}.",
                        trash.display()
                    )),
                    result_path: Some(trash.to_string_lossy().into_owned()),
                    trashed: 1,
                    ..Default::default()
                });
            }
        }
        return Ok(Outcome::blocked(format!(
            "the archive file to discard is missing: {}",
            source.display()
        )));
    }

    let record = match check_source_record(index, &source)? {
        Ok(record) => record,
        Err(reason) => return Ok(Outcome::blocked(reason)),
    };

    // A previous attempt may have copied the file to trash but crashed before
    // removing the source. Verify and finish that move rather than duplicating.
    if let Some(trash) = &persisted_trash {
        if trash.is_file() {
            let source_hash = sha256_if_exists(&source);
            let trash_hash = sha256_if_exists(trash);
            if source_hash.is_some() && source_hash == trash_hash {
                std::fs::remove_file(&source).with_context(|| {
                    format!(
                        "cannot remove source after verified move: {}",
                        source.display()
                    )
                })?;
                fsops::sync_parent(trash);
                let _ = index.delete_path(source.to_string_lossy().as_ref());
                return Ok(Outcome {
                    kind: OutcomeKind::Applied,
                    detail: Some(format!(
                        "Resumed: verified copy already in trash at {}.",
                        trash.display()
                    )),
                    result_path: Some(trash.to_string_lossy().into_owned()),
                    trashed: 1,
                    ..Default::default()
                });
            }
        }
    }

    let mut target = persisted_trash
        .clone()
        .or_else(|| item.dest_path.clone().map(PathBuf::from))
        .unwrap_or(fsops::trash_target(&source, dest_root, trash_root)?);
    if target.exists() {
        target = fsops::unique_path(&target);
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let meta = ExecMeta {
        trash: Some(target.to_string_lossy().into_owned()),
        dest: None,
    };
    plan.start_item(item.id, item.detail.as_deref(), meta_json(&meta).as_deref())?;
    fsops::move_file_verified(&source, &target)
        .with_context(|| format!("cannot move {} to trash", source.display()))?;
    index.delete_path(&record.path)?;
    Ok(Outcome {
        kind: OutcomeKind::Applied,
        result_path: Some(target.to_string_lossy().into_owned()),
        detail: Some(format!("Moved to {} (never deleted).", target.display())),
        trashed: 1,
        ..Default::default()
    })
}

fn execute_replace(
    plan: &PlanDb,
    index: &mut IndexDb,
    dest_root: &Path,
    trash_root: &Path,
    item: &PlanItem,
) -> Result<Outcome> {
    let source = match &item.source_path {
        Some(path) => PathBuf::from(path),
        None => return Ok(Outcome::failed("REPLACE has no source path")),
    };
    let existing = match &item.existing_path {
        Some(path) => PathBuf::from(path),
        None => return Ok(Outcome::failed("REPLACE has no existing path")),
    };
    let (persisted_trash, recorded_dest) = recovery_paths(item);
    let record = index.get_by_path(&source.to_string_lossy())?;

    // Reconcile an interrupted replace.
    if item.status == ItemStatus::Running {
        if let (Some(record), Some(dest)) = (&record, &recorded_dest) {
            if dest.is_file() {
                let finished_copy = if source.is_file() {
                    sha256_if_exists(&source) == sha256_if_exists(dest)
                } else {
                    dest.metadata()
                        .map(|m| m.len() == record.file_size)
                        .unwrap_or(false)
                };
                if finished_copy && existing != *dest {
                    // The old archive file must still be retired if present.
                    if existing.is_file() && existing != *dest {
                        let target = persisted_trash
                            .clone()
                            .unwrap_or(fsops::trash_target(&existing, dest_root, trash_root)?);
                        fsops::move_file_verified(&existing, &target).with_context(|| {
                            format!("cannot retire the old archive file {}", existing.display())
                        })?;
                    }
                    let warning = update_index_after_copy(index, record, dest, Some(&existing))?;
                    return Ok(Outcome {
                        kind: OutcomeKind::Applied,
                        result_path: Some(dest.to_string_lossy().into_owned()),
                        detail: Some("Resumed: the replacement copy was already verified.".into()),
                        error: warning,
                        replaced: true,
                        trashed: 1,
                        ..Default::default()
                    });
                }
            }
        }
    }

    let source_record = match check_source_record(index, &source)? {
        Ok(record) => record,
        Err(reason) => return Ok(Outcome::blocked(reason)),
    };

    let (dest_dir, preferred_name) = destination_for_item(item, &source_record, dest_root);
    let mut meta = ExecMeta {
        trash: persisted_trash
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        dest: Some(
            dest_dir
                .join(&preferred_name)
                .to_string_lossy()
                .into_owned(),
        ),
    };

    // Retire the old archive file first; the persisted trash path makes this
    // recoverable if anything fails afterwards.
    let existing_state = current_file_state(&existing);
    if let Ok(state) = &existing_state {
        if let Some(existing_record) = index.get_by_path(&existing.to_string_lossy())? {
            if state.0 != existing_record.file_size || state.1 != existing_record.mtime_ns {
                return Ok(Outcome::blocked(format!(
                    "the archive file to replace changed after indexing: {}",
                    existing.display()
                )));
            }
        } else {
            return Ok(Outcome::blocked(format!(
                "the archive file to replace is not in the index: {}",
                existing.display()
            )));
        }
        let mut target = persisted_trash
            .clone()
            .unwrap_or(fsops::trash_target(&existing, dest_root, trash_root)?);
        if target.exists() {
            let existing_hash = sha256_if_exists(&existing);
            let trash_hash = sha256_if_exists(&target);
            if existing_hash.is_some() && existing_hash == trash_hash {
                // A previous cross-filesystem copy completed but the source
                // was not removed yet.
                std::fs::remove_file(&existing).with_context(|| {
                    format!(
                        "cannot remove old archive file after verified copy: {}",
                        existing.display()
                    )
                })?;
                fsops::sync_parent(&target);
            } else {
                // Never overwrite an unrelated file in the trash.
                target = fsops::unique_path(&target);
            }
        }
        if existing.exists() {
            meta.trash = Some(target.to_string_lossy().into_owned());
            plan.start_item(item.id, item.detail.as_deref(), meta_json(&meta).as_deref())?;
            fsops::move_file_verified(&existing, &target).with_context(|| {
                format!(
                    "cannot move the old archive file {} to trash",
                    existing.display()
                )
            })?;
        } else {
            meta.trash = Some(target.to_string_lossy().into_owned());
            plan.start_item(item.id, item.detail.as_deref(), meta_json(&meta).as_deref())?;
        }
    } else {
        // The old file is already gone. Only continue if this item had started
        // earlier and recorded a trash path.
        if persisted_trash.is_none() {
            return Ok(Outcome::blocked(format!(
                "the archive file to replace is missing: {}",
                existing.display()
            )));
        }
        plan.start_item(item.id, item.detail.as_deref(), meta_json(&meta).as_deref())?;
    }

    let copy = fsops::copy_to_destination(&source, &dest_dir, &preferred_name)
        .with_context(|| format!("cannot write the replacement copy for {}", source.display()))?;
    meta.dest = Some(copy.path.to_string_lossy().into_owned());
    plan.start_item(item.id, item.detail.as_deref(), meta_json(&meta).as_deref())?;
    let warning = update_index_after_copy(index, &source_record, &copy.path, Some(&existing))?;
    let mut detail = String::new();
    if copy.conflict_resolved {
        detail.push_str(&format!(
            "A name conflict was resolved at execution time; written as {}.\n",
            copy.path.display()
        ));
    }
    detail.push_str(
        "The new copy was verified. The previous archive file is in the trash directory.",
    );
    Ok(Outcome {
        kind: OutcomeKind::Applied,
        result_path: Some(copy.path.to_string_lossy().into_owned()),
        detail: Some(detail),
        error: warning,
        replaced: true,
        trashed: 1,
        conflict: copy.conflict_resolved,
        ..Default::default()
    })
}

fn destination_for_item(
    item: &PlanItem,
    record: &PhotoRecord,
    dest_root: &Path,
) -> (PathBuf, String) {
    if let Some(dest) = &item.dest_path {
        let dest = PathBuf::from(dest);
        let dir = dest
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| dest_root.join(record.year()));
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| record.filename().to_string());
        (dir, name)
    } else {
        (dest_root.join(record.year()), record.filename().to_string())
    }
}

/// Updates the index after a copy. Returns a warning string when the file
/// operations succeeded but the index could not be updated.
fn update_index_after_copy(
    index: &mut IndexDb,
    source_record: &PhotoRecord,
    destination: &Path,
    replaced_existing: Option<&Path>,
) -> Result<Option<String>> {
    let mut warning = None;
    if let Some(existing) = replaced_existing {
        let existing_str = existing.to_string_lossy().to_string();
        let dest_str = destination.to_string_lossy().to_string();
        let _ = dest_str;
        if existing_str != source_record.path {
            if let Err(err) = index.delete_path(&existing_str) {
                warning = Some(format!("could not remove old index row: {err}"));
            }
        }
    }

    let state = match current_file_state(destination) {
        Ok(state) => state,
        Err(err) => return Ok(Some(format!("could not stat the new archive file: {err}"))),
    };
    let mut record = source_record.clone();
    record.id = 0;
    record.path = destination.to_string_lossy().into_owned();
    record.root = RootKind::Archive;
    record.file_size = state.0;
    record.mtime_ns = state.1;
    record.indexed_at = now_ns();
    if let Err(err) = index.relocate(&source_record.path, &record) {
        warning = Some(format!(
            "file operation succeeded but the image index could not be updated: {err}"
        ));
    }
    Ok(warning)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_roundtrip() {
        let meta = ExecMeta {
            trash: Some("/tmp/trash/a.jpg".into()),
            dest: Some("/archive/2020/a.jpg".into()),
        };
        let raw = meta_json(&meta).unwrap();
        let parsed = parse_meta(Some(&raw));
        assert_eq!(parsed.trash, meta.trash);
        assert_eq!(parsed.dest, meta.dest);
    }
}
