//! Archive analysis mode: find duplicates across an existing archive.
//!
//! This mode never modifies files and remains separate from the import
//! workflow. Candidate search uses stored hashes only; no photograph is
//! decoded again during comparison.

use anyhow::{bail, Context, Result};

use crate::compare;
use crate::config::Settings;
use crate::db::{IndexDb, PlanDb, SessionMode};
use crate::jobs::ProgressReporter;
use crate::model::{AnalysisSummary, JobControl, RootKind};
use crate::{planner, scan};

pub fn run_analysis(
    settings: &Settings,
    control: &JobControl,
    reporter: &ProgressReporter,
) -> Result<AnalysisSummary> {
    settings
        .validate_for_analysis()
        .context("cannot start archive analysis: configuration is not valid")?;
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
             Review and apply it, or explicitly discard that session, before analysing the archive."
        );
    }

    let mut summary = AnalysisSummary::default();
    reporter.begin_phase("Reading archive");
    reporter.log(format!(
        "Archive analysis: {} (read-only; nothing is modified)",
        dest_dir.display()
    ));

    let stats = scan::scan(&dest_dir, RootKind::Archive, settings, control, reporter)?;
    if control.is_stopped() || stats.stopped {
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
    let photos = index.load_indexed(Some(RootKind::Archive))?;
    summary.indexed_total = photos.len() as u64;
    drop(index);

    reporter.begin_phase("Finding duplicate groups");
    reporter.set_total(photos.len() as u64);
    let comparison = compare::group_archive(photos, settings);
    reporter.add_reused(comparison.photos.len() as u64);
    reporter.finish_phase();

    if !control.checkpoint() {
        summary.stopped = true;
        return Ok(summary);
    }

    let groups = planner::create_analysis_groups(&comparison);
    summary.exact_groups = groups
        .iter()
        .filter(|g| g.kind == crate::model::GroupKind::Exact)
        .count() as u64;
    summary.perceptual_groups = groups
        .iter()
        .filter(|g| g.kind == crate::model::GroupKind::Perceptual)
        .count() as u64;
    summary.photos_in_groups = groups.iter().map(|g| g.members.len() as u64).sum();
    summary.review_groups = summary.perceptual_groups;

    reporter.begin_phase("Building modification plan");
    let session_id = plan_db.create_session(
        SessionMode::Analysis,
        &dest_dir.to_string_lossy(),
        &dest_dir.to_string_lossy(),
        &settings.session_settings_json(),
    )?;
    summary.session_id = session_id;
    plan_db
        .save_groups(session_id, &groups)
        .context("cannot persist duplicate groups")?;
    let session = plan_db
        .session(session_id)?
        .ok_or_else(|| anyhow::anyhow!("session disappeared while saving"))?;
    planner::rebuild_plan(&mut plan_db, &session, settings)
        .context("cannot build the archive analysis plan")?;
    let counts = plan_db.counts(session_id)?;
    summary.plan_trash = counts.moves_to_trash;
    summary.plan_reviews = counts.reviews;

    reporter.finish_phase();
    reporter.log(format!(
        "Analysis complete: {} exact group(s), {} perceptual group(s), {} photograph(s) in groups; \
         {} move(s) to trash proposed, {} review item(s)",
        summary.exact_groups,
        summary.perceptual_groups,
        summary.photos_in_groups,
        counts.moves_to_trash,
        counts.reviews
    ));
    Ok(summary)
}
