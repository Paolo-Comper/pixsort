mod common;

use std::path::PathBuf;

use common::*;
use pixsort::fsops;

#[test]
fn cross_filesystem_style_move_uses_copy_verify_remove() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source with spaces àè.png");
    let target = dir.path().join("moved àè.png");
    write_file(&source, b"some photo bytes");
    fsops::copy_and_remove_verified(&source, &target).unwrap();
    assert!(!source.exists());
    assert_eq!(std::fs::read(&target).unwrap(), b"some photo bytes");
}

#[test]
fn cross_filesystem_move_refuses_to_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("a");
    let target = dir.path().join("b");
    write_file(&source, b"new");
    write_file(&target, b"existing");
    let err = fsops::copy_and_remove_verified(&source, &target).unwrap_err();
    assert!(format!("{err:#}").contains("refusing to overwrite"));
    assert_eq!(std::fs::read(&target).unwrap(), b"existing");
    assert_eq!(std::fs::read(&source).unwrap(), b"new");
}

#[test]
fn unicode_and_space_paths_survive_copy() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("Città – foto 01 (copia).png");
    write_file(&source, b"unicode bytes");
    let dest = dir.path().join("Archivio foto àé");
    let out = fsops::copy_to_destination(&source, &dest, "Città – foto 01 (copia).png").unwrap();
    assert_eq!(std::fs::read(&out.path).unwrap(), b"unicode bytes");
    assert_eq!(out.path.parent().unwrap(), dest);
}

#[test]
fn target_dir_paths_are_stable() {
    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    assert!(tmp.is_absolute());
}

#[test]
fn bk_tree_scales_to_tens_of_thousands_of_hashes() {
    use std::time::Instant;
    // A worst-case-ish random 63-bit hash set. Real pHashes are more clustered,
    // but this gives a useful upper bound on candidate-search work.
    let n = 30_000usize;
    let mut state = 0x9E3779B97F4A7C15u64;
    let mut hashes = Vec::with_capacity(n);
    for _ in 0..n {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        hashes.push(state & ((1u64 << 63) - 1));
    }
    let tree = pixsort::compare::BkTree::new(hashes.iter().copied().enumerate().collect());
    let start = Instant::now();
    let mut hits = 0usize;
    let mut found = Vec::new();
    for (i, &hash) in hashes.iter().take(2_000).enumerate() {
        found.clear();
        tree.search(hash, 19, &mut found);
        hits += found.iter().filter(|(j, _)| *j != i).count();
    }
    let elapsed = start.elapsed();
    eprintln!("BK-tree: 2000 queries over 30000 hashes in {elapsed:?} ({hits} hits)");
    assert!(
        elapsed.as_secs_f64() < 20.0,
        "candidate search too slow: {elapsed:?}"
    );
}
