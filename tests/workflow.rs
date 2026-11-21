mod common;

use std::path::{Path, PathBuf};

use common::*;
use pixsort::analysis;
use pixsort::db::{IndexDb, PlanDb, SessionStatus};
use pixsort::executor;
use pixsort::fsops;
use pixsort::import;
use pixsort::jobs::{JobEvent, ProgressReporter};
use pixsort::model::{GroupDecision, GroupKind, ItemStatus, JobControl, OpType, RootKind};
use pixsort::{planner, scan};

fn run_import(project: &TestProject) -> pixsort::model::ImportSummary {
    let control = JobControl::new();
    let (reporter, rx) = ProgressReporter::channel();
    let summary = import::run_import(&project.settings, &control, &reporter).unwrap();
    drop(rx);
    summary
}

fn run_analysis(project: &TestProject) -> pixsort::model::AnalysisSummary {
    let control = JobControl::new();
    let (reporter, rx) = ProgressReporter::channel();
    let summary = analysis::run_analysis(&project.settings, &control, &reporter).unwrap();
    drop(rx);
    summary
}

fn apply(project: &TestProject) -> pixsort::model::ApplyReport {
    let control = JobControl::new();
    let (reporter, rx) = ProgressReporter::channel();
    let report = executor::apply_plan(&project.settings, true, &control, &reporter).unwrap();
    drop(rx);
    report
}

fn all_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !root.exists() {
        return out;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    out
}

fn find_named(root: &Path, name: &str) -> Option<PathBuf> {
    all_files(root)
        .into_iter()
        .find(|p| p.file_name().map(|n| n == name).unwrap_or(false))
}

#[test]
fn import_skips_exact_duplicate_and_inserts_new_photo() {
    let project = TestProject::new();
    let base = test_image(320, 240);
    let archive_photo = project.archive.join("2020").join("IMG_0001.png");
    write_file(&archive_photo, &encode_png(&base, None));
    // Same pixels, different filename, re-encoded PNG.
    write_file(
        &project.input.join("copy-of-archive.png"),
        &encode_png(&base, None),
    );
    // A genuinely new photograph.
    write_file(
        &project.input.join("new-photo.png"),
        &encode_png(&unrelated_image(400, 300), None),
    );

    let summary = run_import(&project);
    assert_eq!(summary.input_total, 2);
    assert_eq!(summary.exact_duplicates, 1);
    assert_eq!(summary.new_photos, 1);
    assert_eq!(summary.plan_skips, 1);
    assert_eq!(summary.plan_inserts, 1);
    assert_eq!(summary.plan_reviews, 0);

    let report = apply(&project);
    assert_eq!(report.failed, 0);
    assert_eq!(report.blocked, 0);
    assert_eq!(report.inserted, 1);
    assert_eq!(report.skipped, 1);
    assert!(archive_photo.exists(), "the archive original must not move");
    let inserted = find_named(&project.archive, "new-photo.png").unwrap();
    assert!(inserted.exists());
    // Import never deletes or moves the input files.
    assert!(project.input.join("copy-of-archive.png").exists());
    assert!(project.input.join("new-photo.png").exists());

    let plan = PlanDb::open(&project.settings.plan_db_path.clone().unwrap()).unwrap();
    let session = plan.current_session().unwrap().unwrap();
    assert_eq!(session.status, SessionStatus::Applied);
    let counts = plan.counts(session.id).unwrap();
    assert_eq!(counts.pending, 0);
    assert_eq!(counts.applied + counts.skips, 2);
}

#[test]
fn perceptual_review_blocks_apply_then_replace_trashes_old_version() {
    let project = TestProject::new();
    let full = test_image(480, 360);
    let small = resize_image(&full, 0.5);
    let old_archive = project.archive.join("2019").join("small.png");
    write_file(&old_archive, &encode_png(&small, None));
    let new_input = project.input.join("full.png");
    write_file(&new_input, &encode_png(&full, None));

    let summary = run_import(&project);
    assert_eq!(summary.review_groups, 1);
    assert_eq!(summary.probable_duplicates + summary.ambiguous, 1);
    assert_eq!(summary.plan_reviews, 1);

    // Applying with an unresolved review must be refused before touching files.
    let control = JobControl::new();
    let (reporter, _rx) = ProgressReporter::channel();
    let err = executor::apply_plan(&project.settings, true, &control, &reporter).unwrap_err();
    assert!(format!("{err:#}").contains("manual decision"));
    assert!(old_archive.exists());

    // Resolve: keep the higher-resolution imported version.
    let mut plan = PlanDb::open(&project.settings.plan_db_path.clone().unwrap()).unwrap();
    let session = plan.current_session().unwrap().unwrap();
    let groups = plan.load_groups(session.id).unwrap();
    let group = groups
        .iter()
        .find(|g| g.kind == GroupKind::Perceptual)
        .unwrap();
    let members = plan.load_members(group.id).unwrap();
    let keeper = members
        .iter()
        .find(|m| m.root == RootKind::Input)
        .unwrap()
        .path
        .clone();
    planner::resolve_group(
        &mut plan,
        &session,
        &project.settings,
        group.id,
        GroupDecision::Keep,
        Some(&keeper),
    )
    .unwrap();
    let counts = plan.counts(session.id).unwrap();
    assert_eq!(counts.replaces, 1);
    assert_eq!(counts.reviews, 0);

    let report = apply(&project);
    assert_eq!(report.failed, 0, "{:?}", report.problems);
    assert_eq!(report.replaced, 1);
    assert!(!old_archive.exists(), "old archive copy must be retired");
    let trashed = find_named(
        &project.settings.effective_trash_dir().unwrap(),
        "small.png",
    )
    .expect("old version must be in trash");
    assert!(trashed.exists());
    let new_copy = find_named(&project.archive, "full.png").unwrap();
    assert!(new_copy.exists());
    let index = IndexDb::open(&project.settings.index_db_path.clone().unwrap()).unwrap();
    assert!(index
        .get_by_path(&new_copy.to_string_lossy())
        .unwrap()
        .is_some());
}

#[test]
fn multiple_duplicates_in_one_batch_produce_one_insert_and_two_skips() {
    let project = TestProject::new();
    let base = test_image(300, 220);
    let bytes = encode_png(&base, None);
    for name in ["dup-a.png", "dup-b.png", "dup-c.png"] {
        write_file(&project.input.join(name), &bytes);
    }
    let summary = run_import(&project);
    assert_eq!(summary.input_total, 3);
    assert_eq!(summary.exact_duplicates, 3);
    assert_eq!(summary.new_photos, 0);
    assert_eq!(summary.plan_inserts, 1);
    assert_eq!(summary.plan_skips, 2);

    let report = apply(&project);
    assert_eq!(report.inserted, 1);
    assert_eq!(report.skipped, 2);
    let archived = all_files(&project.archive);
    assert_eq!(archived.len(), 1, "exactly one version is archived");
    for name in ["dup-a.png", "dup-b.png", "dup-c.png"] {
        assert!(project.input.join(name).exists(), "input must be untouched");
    }
}

#[test]
fn archive_analysis_finds_groups_without_modifying_the_archive() {
    let project = TestProject::new();
    let base = test_image(240, 180);
    let a = project.archive.join("2020").join("a.png");
    let b = project.archive.join("2021").join("b.png");
    write_file(&a, &encode_png(&base, None));
    write_file(&b, &encode_png(&base, None));
    write_file(
        &project.archive.join("2021").join("c.png"),
        &encode_png(&unrelated_image(150, 150), None),
    );

    let summary = run_analysis(&project);
    assert_eq!(summary.exact_groups, 1);
    assert_eq!(summary.photos_in_groups, 2);
    assert_eq!(summary.plan_trash, 1);
    assert!(a.exists() && b.exists(), "analysis never modifies files");

    let plan = PlanDb::open(&project.settings.plan_db_path.clone().unwrap()).unwrap();
    let session = plan.current_session().unwrap().unwrap();
    let items = plan.load_items(session.id).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].op, OpType::MoveToTrash);
    let planned_trash = items[0].dest_path.as_deref().unwrap();
    assert!(planned_trash.contains("trash"), "{planned_trash}");
    assert!(planned_trash.ends_with(".png"), "{planned_trash}");
}

#[test]
fn interrupted_move_to_trash_is_reconciled() {
    let project = TestProject::new();
    let base = test_image(220, 160);
    let a = project.archive.join("2020").join("a.png");
    let b = project.archive.join("2020").join("b.png");
    write_file(&a, &encode_png(&base, None));
    write_file(&b, &encode_png(&base, None));
    run_analysis(&project);

    let plan = PlanDb::open(&project.settings.plan_db_path.clone().unwrap()).unwrap();
    let session = plan.current_session().unwrap().unwrap();
    let item = plan
        .load_items(session.id)
        .unwrap()
        .into_iter()
        .find(|i| i.op == OpType::MoveToTrash)
        .unwrap();
    let source = PathBuf::from(item.source_path.clone().unwrap());
    let trash_root = project.settings.effective_trash_dir().unwrap();
    let target = fsops::trash_target(&source, &project.archive, &trash_root).unwrap();
    fsops::move_file_verified(&source, &target).unwrap(); // simulate crash after move
    plan.start_item(
        item.id,
        Some("simulated interrupted move"),
        Some(&serde_json::json!({ "trash": target.to_string_lossy() }).to_string()),
    )
    .unwrap();

    let report = apply(&project);
    assert_eq!(report.failed, 0, "{:?}", report.problems);
    assert_eq!(report.blocked, 0, "{:?}", report.problems);
    assert!(target.exists());
    assert!(!source.exists());
    let item = plan.get_item(item.id).unwrap().unwrap();
    assert_eq!(item.status, ItemStatus::Applied);
}

#[test]
fn late_destination_conflict_is_resolved_without_overwriting() {
    let project = TestProject::new();
    write_file(
        &project.input.join("photo.png"),
        &encode_png(&test_image(200, 150), None),
    );
    run_import(&project);

    let plan = PlanDb::open(&project.settings.plan_db_path.clone().unwrap()).unwrap();
    let session = plan.current_session().unwrap().unwrap();
    let insert = plan
        .load_items(session.id)
        .unwrap()
        .into_iter()
        .find(|i| i.op == OpType::Insert)
        .unwrap();
    let planned_dest = PathBuf::from(insert.dest_path.clone().unwrap());
    std::fs::create_dir_all(planned_dest.parent().unwrap()).unwrap();
    std::fs::write(&planned_dest, b"another file appeared later").unwrap();

    let report = apply(&project);
    assert_eq!(report.failed, 0, "{:?}", report.problems);
    assert_eq!(report.conflicts_resolved, 1);
    assert_eq!(
        std::fs::read(&planned_dest).unwrap(),
        b"another file appeared later",
        "the late file must never be overwritten"
    );
    let inserted = find_named(&project.archive, "photo (1).png")
        .or_else(|| find_named(&project.archive, "photo.png"))
        .unwrap();
    assert_ne!(inserted, planned_dest);
    assert!(std::fs::read(&inserted).unwrap().starts_with(b"\x89PNG"));
}

#[test]
fn missing_source_blocks_application() {
    let project = TestProject::new();
    let input = project.input.join("photo.png");
    write_file(&input, &encode_png(&test_image(120, 90), None));
    run_import(&project);
    std::fs::remove_file(&input).unwrap();

    let report = apply(&project);
    assert_eq!(report.failed, 0);
    assert_eq!(report.blocked, 1);
    assert!(all_files(&project.archive).is_empty());
}

#[test]
fn changed_source_blocks_application() {
    let project = TestProject::new();
    let input = project.input.join("photo.png");
    write_file(&input, &encode_png(&test_image(120, 90), None));
    run_import(&project);
    // Same path, different content and size after indexing.
    std::fs::write(&input, b"changed after indexing").unwrap();

    let report = apply(&project);
    assert_eq!(report.blocked, 1);
    assert!(all_files(&project.archive).is_empty());
}

#[test]
fn scan_reuses_cached_hashes_when_files_are_unchanged() {
    let project = TestProject::new();
    write_file(
        &project.input.join("a.png"),
        &encode_png(&test_image(80, 60), None),
    );
    let control = JobControl::new();
    let (reporter, _rx) = ProgressReporter::channel();
    let first = scan::scan(
        &project.input,
        RootKind::Input,
        &project.settings,
        &control,
        &reporter,
    )
    .unwrap();
    assert_eq!(first.indexed, 1);
    assert_eq!(first.reused, 0);

    let control = JobControl::new();
    let (reporter, _rx) = ProgressReporter::channel();
    let second = scan::scan(
        &project.input,
        RootKind::Input,
        &project.settings,
        &control,
        &reporter,
    )
    .unwrap();
    assert_eq!(second.indexed, 0);
    assert_eq!(second.reused, 1);
}

#[test]
fn stopped_scan_can_be_resumed() {
    let project = TestProject::new();
    for i in 0..4 {
        write_file(
            &project.input.join(format!("p{i}.png")),
            &encode_png(&test_image(160 + i * 10, 120), None),
        );
    }
    let control = JobControl::new();
    control.stop();
    let (reporter, _rx) = ProgressReporter::channel();
    let stopped = scan::scan(
        &project.input,
        RootKind::Input,
        &project.settings,
        &control,
        &reporter,
    )
    .unwrap();
    assert!(stopped.stopped);
    assert_eq!(stopped.indexed, 0);

    let control = JobControl::new();
    let (reporter, _rx) = ProgressReporter::channel();
    let resumed = scan::scan(
        &project.input,
        RootKind::Input,
        &project.settings,
        &control,
        &reporter,
    )
    .unwrap();
    assert!(!resumed.stopped);
    assert_eq!(resumed.indexed, 4);
}

#[test]
fn new_photo_goes_into_year_folder_from_filesystem_fallback() {
    let project = TestProject::new();
    write_file(
        &project.input.join("no-exif.png"),
        &encode_png(&test_image(100, 80), None),
    );
    run_import(&project);
    let report = apply(&project);
    assert_eq!(report.inserted, 1);
    let inserted = find_named(&project.archive, "no-exif.png").unwrap();
    let year = inserted
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let now_year = chrono::Local::now().format("%Y").to_string();
    assert_eq!(year, now_year);
}

#[test]
fn job_events_are_emitted() {
    let project = TestProject::new();
    write_file(
        &project.input.join("x.png"),
        &encode_png(&test_image(64, 64), None),
    );
    let control = JobControl::new();
    let (reporter, rx) = ProgressReporter::channel();
    import::run_import(&project.settings, &control, &reporter).unwrap();
    drop(reporter);
    let mut progress = 0;
    for event in rx.try_iter() {
        if matches!(event, JobEvent::Progress(_)) {
            progress += 1;
        }
    }
    assert!(progress > 0);
}

#[test]
fn same_filename_in_different_input_subfolders_is_resolved() {
    let project = TestProject::new();
    write_file(
        &project.input.join("album-a").join("photo.png"),
        &encode_png(&test_image(200, 150), None),
    );
    write_file(
        &project.input.join("album-b").join("photo.png"),
        &encode_png(&unrelated_image(240, 180), None),
    );
    let summary = run_import(&project);
    assert_eq!(summary.plan_inserts, 2);
    let report = apply(&project);
    assert_eq!(report.inserted, 2);
    let names: Vec<String> = all_files(&project.archive)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"photo.png".to_string()));
    assert!(names.iter().any(|n| n == "photo (1).png"));
    assert_eq!(names.len(), 2);
}

#[test]
fn corrupt_image_is_recorded_as_failed_without_aborting_import() {
    let project = TestProject::new();
    write_file(&project.input.join("broken.jpg"), b"this is not a jpeg");
    write_file(
        &project.input.join("good.png"),
        &encode_png(&test_image(200, 150), None),
    );
    let summary = run_import(&project);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.new_photos, 1);
    let index = IndexDb::open(&project.settings.index_db_path.clone().unwrap()).unwrap();
    let broken = index
        .get_by_path(&project.input.join("broken.jpg").to_string_lossy())
        .unwrap()
        .unwrap();
    assert_eq!(broken.status, pixsort::model::PhotoStatus::Failed);
    assert!(broken.error.is_some());

    let report = apply(&project);
    assert_eq!(report.inserted, 1);
    assert!(find_named(&project.archive, "good.png").is_some());
}

#[test]
fn apply_refuses_without_explicit_confirmation() {
    let project = TestProject::new();
    write_file(
        &project.input.join("photo.png"),
        &encode_png(&test_image(120, 90), None),
    );
    run_import(&project);
    let control = JobControl::new();
    let (reporter, _rx) = ProgressReporter::channel();
    let err = executor::apply_plan(&project.settings, false, &control, &reporter).unwrap_err();
    assert!(format!("{err:#}").contains("explicit user approval"));
    assert!(all_files(&project.archive).is_empty());
}

#[test]
fn exif_date_determines_destination_year() {
    let project = TestProject::new();
    let bytes = encode_png(
        &test_image(160, 120),
        Some(exif_tiff(Some(1), Some("2015:06:01 12:00:00"))),
    );
    write_file(&project.input.join("dated.png"), &bytes);
    let summary = run_import(&project);
    assert_eq!(summary.new_photos, 1);
    let report = apply(&project);
    assert_eq!(report.inserted, 1);
    let inserted = find_named(&project.archive, "dated.png").unwrap();
    assert_eq!(inserted.parent().unwrap().file_name().unwrap(), "2015");
}

#[test]
fn paused_scan_does_not_process_until_resumed() {
    use std::sync::Arc;
    use std::time::Duration;

    let project = TestProject::new();
    for i in 0..4 {
        write_file(
            &project.input.join(format!("big{i}.png")),
            &encode_png(&test_image(1000, 750), None),
        );
    }
    let control = Arc::new(JobControl::new());
    control.pause();
    let settings = project.settings.clone();
    let input = project.input.clone();
    let (reporter, _rx) = ProgressReporter::channel();
    let child_control = control.clone();
    let handle = std::thread::spawn(move || {
        scan::scan(
            &input,
            RootKind::Input,
            &settings,
            &child_control,
            &reporter,
        )
    });
    std::thread::sleep(Duration::from_millis(350));
    {
        let db = IndexDb::open(&project.settings.index_db_path.clone().unwrap()).unwrap();
        let stats = db.stats().unwrap();
        assert_eq!(stats.indexed, 0, "no file may be processed while paused");
    }
    control.resume();
    let stats = handle.join().unwrap().unwrap();
    assert!(!stats.stopped);
    assert_eq!(stats.indexed, 4);
}

#[test]
fn keep_both_imports_the_new_version_and_leaves_archive_copy() {
    let project = TestProject::new();
    let full = test_image(480, 360);
    let small = resize_image(&full, 0.5);
    let old = project.archive.join("2019").join("small.png");
    write_file(&old, &encode_png(&small, None));
    write_file(&project.input.join("full.png"), &encode_png(&full, None));
    run_import(&project);

    let mut plan = PlanDb::open(&project.settings.plan_db_path.clone().unwrap()).unwrap();
    let session = plan.current_session().unwrap().unwrap();
    let group = plan
        .load_groups(session.id)
        .unwrap()
        .into_iter()
        .find(|g| g.kind == GroupKind::Perceptual)
        .unwrap();
    planner::resolve_group(
        &mut plan,
        &session,
        &project.settings,
        group.id,
        GroupDecision::KeepBoth,
        None,
    )
    .unwrap();
    let counts = plan.counts(session.id).unwrap();
    assert_eq!(counts.inserts, 1);
    assert_eq!(counts.moves_to_trash, 0);
    assert_eq!(counts.replaces, 0);

    let report = apply(&project);
    assert_eq!(report.inserted, 1);
    assert_eq!(report.trashed, 0);
    assert!(old.exists());
    assert!(find_named(&project.archive, "full.png").is_some());
}

#[test]
fn ignore_match_on_archive_analysis_produces_no_operations() {
    let project = TestProject::new();
    let full = test_image(420, 300);
    let small = resize_image(&full, 0.55);
    let a = project.archive.join("2020").join("full.png");
    let b = project.archive.join("2021").join("small.png");
    write_file(&a, &encode_png(&full, None));
    write_file(&b, &encode_png(&small, None));
    let summary = run_analysis(&project);
    assert!(summary.perceptual_groups + summary.exact_groups >= 1);

    let mut plan = PlanDb::open(&project.settings.plan_db_path.clone().unwrap()).unwrap();
    let session = plan.current_session().unwrap().unwrap();
    let group = plan
        .load_groups(session.id)
        .unwrap()
        .into_iter()
        .find(|g| g.kind == GroupKind::Perceptual)
        .expect("expected a perceptual group");
    planner::resolve_group(
        &mut plan,
        &session,
        &project.settings,
        group.id,
        GroupDecision::Ignore,
        None,
    )
    .unwrap();
    let items = plan.load_items(session.id).unwrap();
    assert!(
        items.is_empty(),
        "ignore must not create operations: {items:?}"
    );
    let report = apply(&project);
    assert_eq!(report.applied, 0);
    assert!(a.exists() && b.exists());
}

#[test]
fn unknown_folder_when_filesystem_fallback_is_disabled_and_no_exif() {
    let mut project = TestProject::new();
    project.settings.use_filesystem_year = false;
    write_file(
        &project.input.join("undated.png"),
        &encode_png(&test_image(100, 80), None),
    );
    let summary = run_import(&project);
    assert_eq!(summary.new_photos, 1);
    let report = apply(&project);
    assert_eq!(report.inserted, 1);
    let inserted = find_named(&project.archive, "undated.png").unwrap();
    assert_eq!(inserted.parent().unwrap().file_name().unwrap(), "Unknown");
}
