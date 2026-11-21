mod common;

use std::path::PathBuf;

use common::*;
use pixsort::db::{PlanDb, SessionStatus};
use pixsort::jobs::ProgressReporter;
use pixsort::model::{GroupDecision, GroupKind, ItemStatus, JobControl, OpType, RootKind};
use pixsort::{executor, fsops, import, planner};

fn run_import(project: &TestProject) -> pixsort::model::ImportSummary {
    let control = JobControl::new();
    let (reporter, _rx) = ProgressReporter::channel();
    import::run_import(&project.settings, &control, &reporter).unwrap()
}

fn apply(project: &TestProject) -> anyhow::Result<pixsort::model::ApplyReport> {
    let control = JobControl::new();
    let (reporter, _rx) = ProgressReporter::channel();
    executor::apply_plan(&project.settings, true, &control, &reporter)
}

#[test]
fn decisions_and_plan_survive_reopening_the_databases() {
    let project = TestProject::new();
    let full = test_image(480, 320);
    let small = resize_image(&full, 0.5);
    write_file(
        &project.archive.join("2018").join("small.png"),
        &encode_png(&small, None),
    );
    write_file(&project.input.join("full.png"), &encode_png(&full, None));
    run_import(&project);

    let plan_path = project.settings.plan_db_path.clone().unwrap();
    let group_id = {
        let mut db = PlanDb::open(&plan_path).unwrap();
        let session = db.current_session().unwrap().unwrap();
        let groups = db.load_groups(session.id).unwrap();
        let group = groups
            .iter()
            .find(|g| g.kind == GroupKind::Perceptual)
            .unwrap();
        let keeper = db
            .load_members(group.id)
            .unwrap()
            .into_iter()
            .find(|m| m.root == RootKind::Input)
            .unwrap()
            .path;
        planner::resolve_group(
            &mut db,
            &session,
            &project.settings,
            group.id,
            GroupDecision::Keep,
            Some(&keeper),
        )
        .unwrap();
        group.id
    };

    // Re-open exactly as a new application session would.
    let db = PlanDb::open(&plan_path).unwrap();
    let session = db.current_session().unwrap().unwrap();
    let group = db.load_group(group_id).unwrap().unwrap();
    assert_eq!(group.decision, GroupDecision::Keep);
    assert!(group.keeper_path.as_deref().unwrap().contains("full.png"));
    let items = db.load_items(session.id).unwrap();
    assert_eq!(items.iter().filter(|i| i.op == OpType::Replace).count(), 1);

    let report = apply(&project).unwrap();
    assert_eq!(report.replaced, 1);

    let db = PlanDb::open(&plan_path).unwrap();
    let session = db.current_session().unwrap().unwrap();
    assert_eq!(session.status, SessionStatus::Applied);
    assert!(db.load_groups(session.id).unwrap().len() == 1);
}

#[test]
fn interrupted_insert_is_reconciled_without_duplication() {
    let project = TestProject::new();
    write_file(
        &project.input.join("photo.png"),
        &encode_png(&test_image(260, 190), None),
    );
    run_import(&project);

    let plan_path = project.settings.plan_db_path.clone().unwrap();
    let db = PlanDb::open(&plan_path).unwrap();
    let session = db.current_session().unwrap().unwrap();
    let item = db
        .load_items(session.id)
        .unwrap()
        .into_iter()
        .find(|i| i.op == OpType::Insert)
        .unwrap();
    let source = PathBuf::from(item.source_path.clone().unwrap());
    let dest = PathBuf::from(item.dest_path.clone().unwrap());
    let dest_dir = dest.parent().unwrap();
    let name = dest.file_name().unwrap().to_string_lossy().to_string();
    // Simulate a crash after the verified copy was published but before the
    // plan item was marked applied.
    let copy = fsops::copy_to_destination(&source, dest_dir, &name).unwrap();
    db.start_item(
        item.id,
        Some("simulated interrupted copy"),
        Some(&serde_json::json!({ "dest": copy.path.to_string_lossy() }).to_string()),
    )
    .unwrap();

    let report = apply(&project).unwrap();
    assert_eq!(report.failed, 0, "{:?}", report.problems);
    assert_eq!(report.inserted, 1);
    assert_eq!(report.blocked, 0, "{:?}", report.problems);
    assert!(dest.exists(), "the published copy is still in place");
    let item = db.get_item(item.id).unwrap().unwrap();
    assert_eq!(item.status, ItemStatus::Applied);
    assert_eq!(
        all_files(&project.archive).len(),
        1,
        "no second copy created"
    );
}

#[test]
fn changed_destination_is_rejected_before_any_modification() {
    let project = TestProject::new();
    write_file(
        &project.input.join("photo.png"),
        &encode_png(&test_image(120, 90), None),
    );
    run_import(&project);

    let other = project.dir.path().join("other archive");
    std::fs::create_dir_all(&other).unwrap();
    let mut changed = project.settings.clone();
    changed.destination_dir = Some(other.clone());
    let control = JobControl::new();
    let (reporter, _rx) = ProgressReporter::channel();
    let err = executor::apply_plan(&changed, true, &control, &reporter).unwrap_err();
    assert!(format!("{err:#}").contains("does not match this session"));
    assert!(all_files(&other).is_empty());
    assert!(all_files(&project.archive).is_empty());
}

#[test]
fn unicode_paths_work_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("Import àé — foto");
    let archive = dir.path().join("Archivio foto àè");
    let state = dir.path().join("Stato àè");
    for path in [&input, &archive, &state] {
        std::fs::create_dir_all(path).unwrap();
    }
    let mut settings = pixsort::config::Settings::default();
    settings.input_dir = Some(input.clone());
    settings.destination_dir = Some(archive.clone());
    settings.index_db_path = Some(state.join("indice àè.sqlite"));
    settings.plan_db_path = Some(state.join("piano àè.sqlite"));
    settings.trash_dir = Some(dir.path().join("Cestino àè"));
    settings.worker_threads = 2;
    let project = TestProject {
        dir,
        input,
        archive,
        settings,
    };
    write_file(
        &project.input.join("Città 01.png"),
        &encode_png(&test_image(100, 80), None),
    );
    let summary = run_import(&project);
    assert_eq!(summary.new_photos, 1);
    let report = apply(&project).unwrap();
    assert_eq!(report.inserted, 1);
    assert!(find_named(&project.archive, "Città 01.png").is_some());
}

fn all_files(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
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

fn find_named(root: &std::path::Path, name: &str) -> Option<PathBuf> {
    all_files(root)
        .into_iter()
        .find(|p| p.file_name().map(|n| n == name).unwrap_or(false))
}
