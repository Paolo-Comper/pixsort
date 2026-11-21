//! Import workflow orchestration: index both sides, compare, classify and
//! build the persisted plan.
//!
//! Comparison is strictly new-versus-archive and new-versus-new. Archive
//! photographs are never compared against one another during import.

use anyhow::{bail, Context, Result};

use crate::compare;
use crate::config::Settings;
use crate::db::{IndexDb, PlanDb, SessionMode};
use crate::jobs::ProgressReporter;
use crate::model::{ImportSummary, JobControl, RootKind};
use crate::{planner, scan};

pub fn run_import(
    settings: &Settings,
    control: &JobControl,
    reporter: &ProgressReporter,
) -> Result<ImportSummary> {
    settings
        .validate_for_import()
        .context("cannot start import: configuration is not valid")?;
    let input_dir = settings
        .input_dir
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no input directory selected"))?;
    let dest_dir = settings
        .destination_dir
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no archive destination selected"))?;

    let plan_path = settings
        .effective_plan_db()
        .ok_or_else(|| anyhow::anyhow!("no modification plan database configured"))?;
    let mut plan_db = PlanDb::open(&plan_path)?;
    if plan_db.has_unfinished_plan()? {
        bail!(
            "A modification plan with unfinished operations already exists. \
             Review and apply it, or explicitly discard that session, before starting a new import."
        );
    }

    reporter.begin_phase("Reading archive");
    reporter.log(format!(
        "Import: input {}, archive {}, image index {}, plan {}",
        input_dir.display(),
        dest_dir.display(),
        settings
            .effective_index_db()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "—".into()),
        plan_path.display()
    ));

    let mut summary = ImportSummary::default();

    let archive_stats = scan::scan(&dest_dir, RootKind::Archive, settings, control, reporter)?;
    summary.archive_total = archive_stats.total;
    summary.reused += archive_stats.reused;
    summary.failed += archive_stats.failed;
    summary.indexed_new += archive_stats.indexed;
    if control.is_stopped() || archive_stats.stopped {
        reporter.finish_phase();
        summary.stopped = true;
        return Ok(summary);
    }

    let input_stats = scan::scan(&input_dir, RootKind::Input, settings, control, reporter)?;
    summary.input_total = input_stats.total;
    summary.reused += input_stats.reused;
    summary.failed += input_stats.failed;
    summary.indexed_new += input_stats.indexed;
    if control.is_stopped() || input_stats.stopped {
        reporter.finish_phase();
        summary.stopped = true;
        return Ok(summary);
    }

    if !control.checkpoint() {
        summary.stopped = true;
        return Ok(summary);
    }

    let index_path = settings
        .effective_index_db()
        .ok_or_else(|| anyhow::anyhow!("no image index database configured"))?;
    let index = IndexDb::open(&index_path)?;
    let archive = index.load_indexed(Some(RootKind::Archive))?;
    let input = index.load_indexed(Some(RootKind::Input))?;
    drop(index);

    reporter.begin_phase("Comparing photographs");
    reporter.set_total(input.len() as u64);
    reporter.log(format!(
        "Comparing {} imported photograph(s) against {} archive photograph(s)",
        input.len(),
        archive.len()
    ));
    let comparison = compare::group_import(archive, input, settings);
    reporter.add_reused(comparison.input().len() as u64);
    reporter.finish_phase();

    if !control.checkpoint() {
        summary.stopped = true;
        return Ok(summary);
    }

    let classification = planner::classify_import(&comparison, settings);
    summary.new_photos = classification.new_photos;
    summary.exact_duplicates = classification.exact_duplicates;
    summary.probable_duplicates = classification.probable_duplicates;
    summary.ambiguous = classification.ambiguous;

    reporter.begin_phase("Building modification plan");
    let groups = planner::create_import_groups(&comparison);
    summary.groups = groups
        .iter()
        .filter(|g| g.kind != crate::model::GroupKind::Single)
        .count() as u64;
    summary.review_groups = groups
        .iter()
        .filter(|g| g.kind == crate::model::GroupKind::Perceptual)
        .count() as u64;

    let session_id = plan_db.create_session(
        SessionMode::Import,
        &input_dir.to_string_lossy(),
        &dest_dir.to_string_lossy(),
        &serde_json::to_string(settings).unwrap_or_else(|_| "{}".into()),
    )?;
    summary.session_id = session_id;
    plan_db
        .save_groups(session_id, &groups)
        .context("cannot persist duplicate groups")?;
    let session = plan_db
        .session(session_id)?
        .ok_or_else(|| anyhow::anyhow!("session disappeared while saving"))?;
    let items = planner::rebuild_plan(&mut plan_db, &session, settings)
        .context("cannot build the modification plan")?;
    let counts = plan_db.counts(session_id)?;
    summary.plan_inserts = counts.inserts;
    summary.plan_replaces = counts.replaces;
    summary.plan_skips = counts.skips;
    summary.plan_trash = counts.moves_to_trash;
    summary.plan_reviews = counts.reviews;

    reporter.finish_phase();
    reporter.log(format!(
        "Plan ready: {} group(s), {} review item(s), {} insert(s), {} replace(s), \
         {} skip(s), {} move(s) to trash",
        summary.groups,
        counts.reviews,
        counts.inserts,
        counts.replaces,
        counts.skips,
        counts.moves_to_trash
    ));
    let _ = items;
    Ok(summary)
}
