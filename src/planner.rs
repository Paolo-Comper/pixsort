//! Turning duplicate groups and decisions into a persisted modification plan.
//!
//! The plan represents the desired *final state*: for every group exactly one
//! keeper remains (or, when the user chooses, both are kept), redundant files
//! are moved to trash, and new photographs are inserted into year folders.
//!
//! Destination names are resolved deterministically and never overwrite an
//! existing file: conflicts get a ` (1)` suffix, visible in the plan.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::compare::{Comparison, PhotoCluster};
use crate::config::Settings;
use crate::db::plan_db::{NewGroup, NewPlanItem, PlanSession, SessionMode};
use crate::db::PlanDb;
use crate::model::{
    file_name_of, GroupDecision, GroupKind, GroupState, OpType, PhotoRecord, RootKind,
};
use crate::recommend;

#[derive(Debug, Clone, Default)]
pub struct ImportClassification {
    pub new_photos: u64,
    pub exact_duplicates: u64,
    pub probable_duplicates: u64,
    pub ambiguous: u64,
}

/// Groups for an import run, including one `Single` group per new photograph.
pub fn create_import_groups(comparison: &Comparison) -> Vec<NewGroup> {
    let mut cluster_of: HashMap<usize, &PhotoCluster> = HashMap::new();
    for cluster in &comparison.clusters {
        for &member in &cluster.members {
            cluster_of.insert(member, cluster);
        }
    }

    let mut groups = Vec::new();
    for cluster in &comparison.clusters {
        let members: Vec<PhotoRecord> = cluster
            .members
            .iter()
            .map(|&i| comparison.photos[i].clone())
            .collect();
        if members.is_empty() {
            continue;
        }
        groups.push(group_for_cluster(cluster, &members));
    }

    for index in comparison.archive_len..comparison.photos.len() {
        if cluster_of.contains_key(&index) {
            continue;
        }
        let photo = comparison.photos[index].clone();
        groups.push(NewGroup {
            kind: GroupKind::Single,
            similarity: 100.0,
            distance: 0,
            state: GroupState::Auto,
            decision: GroupDecision::Keep,
            keeper_path: Some(photo.path.clone()),
            recommended_path: Some(photo.path.clone()),
            explanation: "No matching photograph was found in the archive or in this import \
                          batch. The file will be added to the archive."
                .into(),
            title: format!("New: {}", file_name_of(&photo.path)),
            members: vec![photo],
        });
    }

    groups.sort_by(|a, b| a.members[0].path.cmp(&b.members[0].path));
    groups
}

/// Groups for archive analysis (no singles: photos without duplicates are not
/// planned for any change).
pub fn create_analysis_groups(comparison: &Comparison) -> Vec<NewGroup> {
    let mut groups = Vec::new();
    for cluster in &comparison.clusters {
        let members: Vec<PhotoRecord> = cluster
            .members
            .iter()
            .map(|&i| comparison.photos[i].clone())
            .collect();
        if members.len() < 2 {
            continue;
        }
        groups.push(group_for_cluster(cluster, &members));
    }
    groups
}

fn group_for_cluster(cluster: &PhotoCluster, members: &[PhotoRecord]) -> NewGroup {
    let recommendation = recommend::recommend(members);
    let exact = cluster.kind == GroupKind::Exact;
    let keeper = recommendation.keeper_path.clone();
    let explanation = recommend::summarise(&recommendation, members, exact, cluster.similarity);
    let filename = file_name_of(&keeper);
    let title = if exact {
        format!("Exact duplicate: {filename}")
    } else {
        format!(
            "Similar: {filename} + {} other(s)",
            members.len().saturating_sub(1)
        )
    };
    NewGroup {
        kind: cluster.kind,
        similarity: cluster.similarity,
        distance: cluster.distance,
        state: if exact {
            GroupState::Auto
        } else {
            GroupState::NeedsReview
        },
        decision: GroupDecision::None,
        keeper_path: if exact { Some(keeper.clone()) } else { None },
        recommended_path: Some(keeper),
        explanation,
        title,
        members: members.to_vec(),
    }
}

/// Classifies every input photograph. This is shown to the user and is
/// independent of their later decisions.
pub fn classify_import(comparison: &Comparison, settings: &Settings) -> ImportClassification {
    let mut cluster_of: HashMap<usize, &PhotoCluster> = HashMap::new();
    for cluster in &comparison.clusters {
        for &member in &cluster.members {
            cluster_of.insert(member, cluster);
        }
    }
    let probable = settings.probable_distance();
    let mut hash_counts: HashMap<&str, u32> = HashMap::new();
    for photo in &comparison.photos {
        if let Some(hash) = photo.pixel_hash.as_deref() {
            *hash_counts.entry(hash).or_insert(0) += 1;
        }
    }
    let mut out = ImportClassification::default();
    for index in comparison.archive_len..comparison.photos.len() {
        let me = &comparison.photos[index];
        let has_exact_match = me
            .pixel_hash
            .as_deref()
            .map(|hash| hash_counts.get(hash).copied().unwrap_or(0) > 1)
            .unwrap_or(false);
        if has_exact_match {
            out.exact_duplicates += 1;
            continue;
        }
        match cluster_of.get(&index) {
            None => out.new_photos += 1,
            Some(cluster) if cluster.kind == GroupKind::Exact => out.exact_duplicates += 1,
            Some(cluster) => {
                let me = &comparison.photos[index];
                let best = cluster
                    .members
                    .iter()
                    .filter(|&&i| i != index)
                    .filter_map(|&i| {
                        let other = &comparison.photos[i];
                        if me.pixel_hash.is_some() && me.pixel_hash == other.pixel_hash {
                            Some(0)
                        } else {
                            match (me.phash, other.phash) {
                                (Some(a), Some(b)) => Some(crate::model::phash_distance(a, b)),
                                _ => None,
                            }
                        }
                    })
                    .min()
                    .unwrap_or(cluster.distance);
                if best <= probable {
                    out.probable_duplicates += 1;
                } else {
                    out.ambiguous += 1;
                }
            }
        }
    }
    out
}

/// Rebuilds pending plan items from the persisted groups and decisions.
///
/// Refuses to run once execution has started, so applied operations are never
/// silently replaced by a regenerated plan.
pub fn rebuild_plan(
    db: &mut PlanDb,
    session: &PlanSession,
    settings: &Settings,
) -> Result<Vec<crate::model::PlanItem>> {
    let existing = db.load_items(session.id)?;
    if existing.iter().any(|item| {
        matches!(
            item.status,
            crate::model::ItemStatus::Running
                | crate::model::ItemStatus::Applied
                | crate::model::ItemStatus::Failed
        )
    }) {
        bail!(
            "The plan has already been (partially) applied; decisions can no longer be \
             regenerated. Review the remaining items or start a new session."
        );
    }

    if !session.uses_current_grouping() {
        bail!(
            "This plan was created with an older duplicate-grouping algorithm (before \
             complete-linkage groups and size limits). Applying or editing it would be unsafe. \
             Discard this session and run Archive analysis again."
        );
    }
    let mut groups = db.load_groups(session.id)?;
    groups.sort_by_key(|g| g.id);
    let mut members: HashMap<i64, Vec<PhotoRecord>> = HashMap::new();
    for (group_id, photo) in db.load_members_for_session(session.id)? {
        members.entry(group_id).or_default().push(photo);
    }

    let destination = settings
        .destination_dir
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("no archive destination configured"))?;
    if !crate::config::same_path(Path::new(&session.dest_dir), destination) {
        bail!(
            "The configured archive destination does not match this session ({}).              Reconnect the original destination or discard the session.",
            session.dest_dir
        );
    }
    if session.mode == SessionMode::Import {
        match settings.input_dir.as_deref() {
            Some(input) if crate::config::same_path(Path::new(&session.input_dir), input) => {}
            _ => bail!(
                "The configured input folder does not match this session ({}).                  Reconnect the original input folder or discard the session.",
                session.input_dir
            ),
        }
    }
    let mut planner = DestinationPlanner::new(destination)?;
    let trash_root = settings
        .effective_trash_dir()
        .unwrap_or_else(|| destination.join("pixsort-trash"));

    let mut items: Vec<NewPlanItem> = Vec::new();
    for group in &groups {
        let group_members = members.remove(&group.id).unwrap_or_default();
        if group_members.is_empty() {
            continue;
        }
        match group.kind {
            GroupKind::Single => {
                if session.mode == SessionMode::Import {
                    let photo = &group_members[0];
                    if photo.root == RootKind::Input {
                        items.push(insert_item(
                            &mut planner,
                            group.id,
                            photo,
                            None,
                            None,
                            "New photograph — no matching photograph exists. ".to_string(),
                            None,
                        )?);
                    }
                }
            }
            GroupKind::Exact | GroupKind::Perceptual => {
                build_group_items(
                    session.mode,
                    group,
                    &group_members,
                    destination,
                    &trash_root,
                    &mut planner,
                    &mut items,
                )?;
            }
        }
    }

    db.replace_pending_items(session.id, &items)
        .context("cannot write plan items")?;
    db.load_items(session.id)
}

fn build_group_items(
    mode: SessionMode,
    group: &crate::model::DuplicateGroup,
    members: &[PhotoRecord],
    archive_root: &Path,
    trash_root: &Path,
    planner: &mut DestinationPlanner,
    items: &mut Vec<NewPlanItem>,
) -> Result<()> {
    let input: Vec<&PhotoRecord> = members
        .iter()
        .filter(|m| m.root == RootKind::Input)
        .collect();
    let archive: Vec<&PhotoRecord> = members
        .iter()
        .filter(|m| m.root == RootKind::Archive)
        .collect();
    let exact = group.kind == GroupKind::Exact;
    let similarity_note = if exact {
        "identical normalised pixels".to_string()
    } else {
        format!("{:.1}% heuristic perceptual similarity", group.similarity)
    };

    let keeper_path = group
        .keeper_path
        .clone()
        .or_else(|| group.recommended_path.clone());
    let keeper = keeper_path
        .as_deref()
        .and_then(|path| members.iter().find(|m| m.path == path));

    let unresolved = group.state == GroupState::NeedsReview
        || (group.state == GroupState::Resolved && keeper.is_none());
    if unresolved {
        let preferred = keeper_path.clone();
        items.push(
            NewPlanItem::new(
                OpType::Review,
                format!(
                    "Perceptual match ({similarity_note}) requires your decision before \
                     anything is imported or moved."
                ),
            )
            .group(group.id)
            .source(preferred.clone().unwrap_or_else(|| members[0].path.clone()))
            .preferred(preferred.unwrap_or_else(|| members[0].path.clone()))
            .detail(group.explanation.clone()),
        );
        return Ok(());
    }

    if group.state == GroupState::Ignored {
        // "Keep both" / "Ignore match": nothing is deleted. For an import all
        // batch photographs are inserted; for archive analysis nothing moves.
        if mode == SessionMode::Import {
            for photo in &input {
                items.push(insert_item(
                    planner,
                    group.id,
                    photo,
                    None,
                    None,
                    format!(
                        "Keep both: this photograph was matched ({similarity_note}) but the \
                         user chose to keep every version. "
                    ),
                    Some(&group.explanation),
                )?);
            }
        }
        return Ok(());
    }

    let keeper = match keeper {
        Some(keeper) => keeper,
        None => {
            items.push(
                NewPlanItem::new(
                    OpType::Review,
                    format!("Duplicate group ({similarity_note}) has no chosen keeper."),
                )
                .group(group.id)
                .source(members[0].path.clone())
                .preferred(group.recommended_path.clone().unwrap_or_default())
                .detail(group.explanation.clone()),
            );
            return Ok(());
        }
    };
    let keeper = keeper.clone();
    let keeper_is_input = keeper.root == RootKind::Input;

    match mode {
        SessionMode::Import => {
            if keeper_is_input {
                // Existing archive versions of this group are superseded by
                // the imported keeper and go to trash.
                let primary = archive.first();
                let detail = Some(group.explanation.clone());
                let planned_trash = primary.map(|p| {
                    crate::fsops::planned_trash_path(Path::new(&p.path), archive_root, trash_root)
                });
                items.push(insert_item(
                    planner,
                    group.id,
                    &keeper,
                    primary.map(|p| Path::new(&p.path)),
                    planned_trash.as_deref(),
                    format!(
                        "Recommended imported version replaces the archive copy \
                         ({similarity_note}); the existing file is moved to trash first. "
                    ),
                    detail.as_deref(),
                )?);
                for (index, photo) in archive.iter().enumerate() {
                    if index == 0 {
                        continue;
                    }
                    items.push(trash_item(
                        group,
                        photo,
                        &keeper,
                        exact,
                        group.similarity,
                        archive_root,
                        trash_root,
                    ));
                }
                for photo in &input {
                    if photo.path == keeper.path {
                        continue;
                    }
                    items.push(skip_item(group, photo, &keeper, exact, group.similarity));
                }
            } else {
                // The archive already holds the recommended copy: skip imports.
                for photo in &input {
                    items.push(skip_item(group, photo, &keeper, exact, group.similarity));
                }
            }
        }
        SessionMode::Analysis => {
            if !keeper_is_input {
                for photo in &archive {
                    if photo.path == keeper.path {
                        continue;
                    }
                    items.push(trash_item(
                        group,
                        photo,
                        &keeper,
                        exact,
                        group.similarity,
                        archive_root,
                        trash_root,
                    ));
                }
            }
        }
    }
    Ok(())
}

fn insert_item(
    planner: &mut DestinationPlanner,
    group_id: i64,
    photo: &PhotoRecord,
    replace_existing: Option<&Path>,
    replaced_trash_path: Option<&Path>,
    reason_prefix: String,
    detail: Option<&str>,
) -> Result<NewPlanItem> {
    let destination = planner.destination_for(photo, replace_existing)?;
    let conflict_note = if destination.conflict_resolved {
        format!(
            "Name conflict resolved: the file will be written as {}.",
            destination.path.display()
        )
    } else {
        String::new()
    };
    let mut full_detail = detail.unwrap_or("").to_string();
    if !conflict_note.is_empty() {
        if !full_detail.is_empty() {
            full_detail.push('\n');
        }
        full_detail.push_str(&conflict_note);
    }
    let (op, reason) = if let Some(existing) = replace_existing {
        (
            OpType::Replace,
            format!(
                "{}Destination year {} ({}). Existing archive file {} will be moved to trash \
                 before the new copy is verified and written.",
                reason_prefix,
                photo.year(),
                photo.taken_source.label(),
                existing.display()
            ),
        )
    } else {
        (
            OpType::Insert,
            format!(
                "{}Destination year {} ({}).",
                reason_prefix,
                photo.year(),
                photo.taken_source.label()
            ),
        )
    };
    let mut item = NewPlanItem::new(op, reason)
        .group(group_id)
        .source(photo.path.clone())
        .dest(destination.path.to_string_lossy().to_string())
        .preferred(photo.path.clone())
        .detail(full_detail);
    if let Some(existing) = replace_existing {
        item = item.existing(existing.to_string_lossy().to_string());
    }
    if let Some(trash_path) = replaced_trash_path {
        let note = format!(
            "The previous archive file will be moved to {}.",
            trash_path.display()
        );
        item.detail = Some(match item.detail {
            Some(detail) if !detail.is_empty() => format!("{detail}\n{note}"),
            _ => note,
        });
    }
    Ok(item)
}

fn skip_item(
    group: &crate::model::DuplicateGroup,
    photo: &PhotoRecord,
    keeper: &PhotoRecord,
    exact: bool,
    similarity: f32,
) -> NewPlanItem {
    let reason = if exact {
        format!(
            "Exact pixel duplicate of {} — the redundant import is not copied.",
            keeper.path
        )
    } else {
        format!(
            "Perceptual duplicate of {} ({:.1}% heuristic similarity) — the redundant import \
             is not copied.",
            keeper.path, similarity
        )
    };
    NewPlanItem::new(OpType::Skip, reason)
        .group(group.id)
        .source(photo.path.clone())
        .preferred(keeper.path.clone())
        .detail(group.explanation.clone())
}

fn trash_item(
    group: &crate::model::DuplicateGroup,
    photo: &PhotoRecord,
    keeper: &PhotoRecord,
    exact: bool,
    similarity: f32,
    archive_root: &Path,
    trash_root: &Path,
) -> NewPlanItem {
    let reason = if exact {
        format!(
            "Redundant archive copy of {} (identical normalised pixels). Moved to the trash \
             directory, never deleted.",
            keeper.path
        )
    } else {
        format!(
            "Redundant archive version; keeping {} ({:.1}% heuristic similarity). Moved to \
             the trash directory, never deleted.",
            keeper.path, similarity
        )
    };
    let trash_path =
        crate::fsops::planned_trash_path(Path::new(&photo.path), archive_root, trash_root);
    NewPlanItem::new(OpType::MoveToTrash, reason)
        .group(group.id)
        .source(photo.path.clone())
        .existing(photo.path.clone())
        .dest(trash_path.to_string_lossy().to_string())
        .preferred(keeper.path.clone())
        .detail(group.explanation.clone())
}

/// Applies a review decision and rebuilds the plan.
pub fn resolve_group(
    db: &mut PlanDb,
    session: &PlanSession,
    settings: &Settings,
    group_id: i64,
    decision: GroupDecision,
    keeper_path: Option<&str>,
) -> Result<()> {
    let group = db
        .load_group(group_id)?
        .ok_or_else(|| anyhow::anyhow!("group {group_id} not found"))?;
    if group.session_id != session.id {
        bail!("group does not belong to the current session");
    }
    let members = db.load_members(group_id)?;
    let (state, keeper) = match decision {
        GroupDecision::None => {
            if group.kind == GroupKind::Exact {
                (GroupState::Auto, None)
            } else {
                (GroupState::NeedsReview, None)
            }
        }
        GroupDecision::Keep => {
            let keeper = keeper_path.ok_or_else(|| anyhow::anyhow!("no keeper selected"))?;
            if !members.iter().any(|m| m.path == keeper) {
                bail!("the selected keeper is not a member of this group");
            }
            (GroupState::Resolved, Some(keeper))
        }
        GroupDecision::KeepBoth | GroupDecision::Ignore => (GroupState::Ignored, None),
    };
    db.set_decision(group_id, decision, keeper, state)?;
    rebuild_plan(db, session, settings)?;
    Ok(())
}

/// Accepts every recommended keeper at or above the probable threshold.
pub fn accept_probable_recommendations(
    db: &mut PlanDb,
    session: &PlanSession,
    settings: &Settings,
) -> Result<usize> {
    let groups = db.load_groups(session.id)?;
    let mut accepted = 0;
    for group in &groups {
        if group.state != GroupState::NeedsReview {
            continue;
        }
        if group.similarity + 0.001 < settings.probable_similarity_pct {
            continue;
        }
        if let Some(recommended) = &group.recommended_path {
            db.set_decision(
                group.id,
                GroupDecision::Keep,
                Some(recommended),
                GroupState::Resolved,
            )?;
            accepted += 1;
        }
    }
    if accepted > 0 {
        rebuild_plan(db, session, settings)?;
    }
    Ok(accepted)
}

// ---------------------------------------------------------------------------
// Destination planning
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PlannedDestination {
    pub path: PathBuf,
    pub conflict_resolved: bool,
}

/// Chooses `<destination>/<year>/<filename>`, preserving names when possible
/// and appending ` (1)`, ` (2)` when needed. Reads existing names once so the
/// plan can be rebuilt deterministically.
pub struct DestinationPlanner {
    root: PathBuf,
    existing: HashMap<String, HashSet<String>>,
    reserved: HashSet<String>,
}

impl DestinationPlanner {
    pub fn new(root: &Path) -> Result<Self> {
        if !root.is_dir() {
            bail!("archive destination is not accessible: {}", root.display());
        }
        let mut existing: HashMap<String, HashSet<String>> = HashMap::new();
        let entries = std::fs::read_dir(root)
            .with_context(|| format!("cannot read archive root {}", root.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let year = entry.file_name().to_string_lossy().to_lowercase();
            let mut names = HashSet::new();
            if let Ok(children) = std::fs::read_dir(&path) {
                for child in children.flatten() {
                    names.insert(child.file_name().to_string_lossy().to_lowercase());
                }
            }
            existing.insert(year, names);
        }
        Ok(Self {
            root: root.to_path_buf(),
            existing,
            reserved: HashSet::new(),
        })
    }

    pub fn destination_for(
        &mut self,
        photo: &PhotoRecord,
        replace_existing: Option<&Path>,
    ) -> Result<PlannedDestination> {
        let year = photo.year();
        let year_dir = self.root.join(&year);
        let base = sanitize_filename(photo.filename());

        // In a REPLACE keeping the same file name and year, reuse the path: the
        // executor moves the old file away before writing, never overwriting.
        if let Some(existing) = replace_existing {
            if existing.parent() == Some(year_dir.as_path())
                && existing
                    .file_name()
                    .map(|n| n.to_string_lossy().eq_ignore_ascii_case(&base))
                    .unwrap_or(false)
            {
                let key = reserve_key(&year, &base);
                if !self.reserved.contains(&key) {
                    self.reserved.insert(key);
                    return Ok(PlannedDestination {
                        path: existing.to_path_buf(),
                        conflict_resolved: false,
                    });
                }
            }
        }

        let (stem, extension) = split_name(&base);
        for suffix in 0u32..100_000 {
            let candidate = if suffix == 0 {
                base.clone()
            } else {
                suffix_name(&stem, &extension, suffix)
            };
            if self.is_taken(&year, &candidate) {
                continue;
            }
            let key = reserve_key(&year, &candidate);
            self.reserved.insert(key);
            return Ok(PlannedDestination {
                path: year_dir.join(&candidate),
                conflict_resolved: suffix > 0,
            });
        }
        bail!(
            "could not find an unused destination name for {}",
            photo.path
        )
    }

    fn is_taken(&self, year: &str, name: &str) -> bool {
        let key = reserve_key(year, name);
        if self.reserved.contains(&key) {
            return true;
        }
        self.existing
            .get(&year.to_lowercase())
            .map(|names| names.contains(&name.to_lowercase()))
            .unwrap_or(false)
    }
}

fn reserve_key(year: &str, name: &str) -> String {
    format!("{}\0{}", year.to_lowercase(), name.to_lowercase())
}

/// Replaces characters that are invalid on common filesystems and trims names
/// that Windows cannot represent. Unicode is preserved.
pub fn sanitize_filename(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    out = out.trim().trim_matches('.').to_string();
    if out.is_empty() {
        out = "photo".into();
    }

    let (stem, ext) = split_name(&out);
    let stem = if is_windows_reserved(&stem) {
        format!("_{stem}")
    } else {
        stem
    };
    let mut full = if ext.is_empty() {
        stem
    } else {
        format!("{stem}.{ext}")
    };

    // Filenames are limited to 255 bytes on common filesystems. Truncate the
    // stem on a character boundary while keeping the extension.
    const LIMIT: usize = 200;
    if full.len() > LIMIT {
        let (stem, ext) = split_name(&full);
        let ext_budget = ext.len().min(LIMIT / 2) + usize::from(!ext.is_empty());
        let stem_budget = LIMIT.saturating_sub(ext_budget);
        let mut truncated = String::new();
        for ch in stem.chars() {
            if truncated.len() + ch.len_utf8() > stem_budget {
                break;
            }
            truncated.push(ch);
        }
        full = if ext.is_empty() {
            truncated
        } else {
            format!("{truncated}.{ext}")
        };
    }
    full
}

fn is_windows_reserved(stem: &str) -> bool {
    let upper = stem.trim_end_matches(' ').to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn split_name(name: &str) -> (String, String) {
    match name.rfind('.') {
        Some(index) if index > 0 && index + 1 < name.len() => {
            (name[..index].to_string(), name[index + 1..].to_string())
        }
        _ => (name.to_string(), String::new()),
    }
}

fn suffix_name(stem: &str, ext: &str, suffix: u32) -> String {
    if ext.is_empty() {
        format!("{stem} ({suffix})")
    } else {
        format!("{stem} ({suffix}).{ext}")
    }
}

/// Formats the operation summary used by the preview screen.
pub fn describe_plan_item(item: &crate::model::PlanItem) -> String {
    match item.op {
        OpType::Insert => format!(
            "Copy {} → {}",
            item.source_path.as_deref().unwrap_or("?"),
            item.dest_path.as_deref().unwrap_or("?")
        ),
        OpType::Replace => format!(
            "Replace {} with {} (old copy → trash)",
            item.existing_path.as_deref().unwrap_or("?"),
            item.source_path.as_deref().unwrap_or("?")
        ),
        OpType::Skip => format!(
            "Leave {} in the input folder",
            item.source_path.as_deref().unwrap_or("?")
        ),
        OpType::MoveToTrash => format!(
            "Move {} to trash",
            item.source_path.as_deref().unwrap_or("?")
        ),
        OpType::Review => "Wait for a manual decision".to_string(),
    }
}

/// Keeps an unused import from being reported as an error.
pub fn supported_destination_root(settings: &Settings) -> Option<&Path> {
    settings.destination_dir.as_deref()
}

pub fn year_source_label(photo: &PhotoRecord) -> String {
    format!(
        "{} ({}; file format {})",
        photo.year(),
        photo.taken_source.label(),
        photo.format.label()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_unicode_and_removes_bad_chars() {
        assert_eq!(sanitize_filename("Città 2020:01.jpg"), "Città 2020_01.jpg");
        assert_eq!(sanitize_filename("..."), "photo");
        assert_eq!(sanitize_filename("CON.jpg"), "_CON.jpg");
    }

    #[test]
    fn names_are_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("2020")).unwrap();
        std::fs::write(dir.path().join("2020").join("a.jpg"), b"x").unwrap();
        let mut planner = DestinationPlanner::new(dir.path()).unwrap();
        let photo = PhotoRecord {
            id: 0,
            path: "/in/a.jpg".into(),
            root: RootKind::Input,
            file_size: 1,
            mtime_ns: 0,
            format: crate::model::ImageFormatKind::Jpeg,
            width: 10,
            height: 10,
            pixel_hash: None,
            phash: None,
            taken_on: Some("2020-01-01".into()),
            taken_at_ns: None,
            taken_source: crate::model::DateSource::ExifOriginal,
            orientation: 1,
            is_animated: false,
            exif: Default::default(),
            status: crate::model::PhotoStatus::Indexed,
            error: None,
            indexed_at: 0,
        };
        let first = planner.destination_for(&photo, None).unwrap();
        assert_eq!(first.path, dir.path().join("2020/a (1).jpg"));
        assert!(first.conflict_resolved);
        let second = planner.destination_for(&photo, None).unwrap();
        assert_eq!(second.path, dir.path().join("2020/a (2).jpg"));
    }
}
