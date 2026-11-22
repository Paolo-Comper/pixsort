//! Duplicate detection: exact pixel hashes plus a BK-tree over perceptual
//! hashes.
//!
//! # Grouping policy
//!
//! Exact pixel matches form classes directly. Perceptual grouping is *not*
//! single-linkage: that would let a chain of weak matches (A≈B, B≈C) merge
//! hundreds of unrelated photos even when A and C are not similar.
//!
//! Instead, exact classes are merged only while the resulting group stays
//! **complete-linkage coherent**: every pair of classes must be within the
//! configured review radius, and the group is hard-capped in size. Candidates
//! are processed strongest-first, so the most similar matches form groups
//! before weaker ones. Remaining weak candidates are left as new photographs
//! or separate groups; they are never transitively absorbed.
//!
//! No image decoding happens here. Candidate search is pure 63-bit hash
//! arithmetic, which is what makes archive analysis of tens of thousands of
//! photographs practical.

use std::collections::HashMap;

use crate::config::Settings;
use crate::model::{phash_distance, similarity_for_distance, GroupKind, PhotoRecord, PHASH_BITS};

/// A set of photographs plus the duplicate clusters found in it.
pub struct Comparison {
    /// Archive photographs followed by input photographs (for import).
    /// For archive analysis this is simply every archive photograph.
    pub photos: Vec<PhotoRecord>,
    pub archive_len: usize,
    pub clusters: Vec<PhotoCluster>,
}

impl Comparison {
    pub fn archive(&self) -> &[PhotoRecord] {
        &self.photos[..self.archive_len]
    }

    pub fn input(&self) -> &[PhotoRecord] {
        &self.photos[self.archive_len..]
    }

    pub fn is_input(&self, index: usize) -> bool {
        index >= self.archive_len
    }
}

#[derive(Debug, Clone)]
pub struct PhotoCluster {
    /// Indices into `Comparison::photos`, sorted by path.
    pub members: Vec<usize>,
    pub kind: GroupKind,
    /// Worst (largest) Hamming distance between distinct exact-pixel classes
    /// in the group. This is a conservative description of group cohesion.
    pub distance: u32,
    pub similarity: f32,
}

impl PhotoCluster {
    pub fn is_exact(&self) -> bool {
        self.kind == GroupKind::Exact
    }
}

/// A set of photographs that share exactly the same normalized pixel hash.
#[derive(Debug)]
struct ExactClass {
    photos: Vec<usize>,
    phash: Option<u64>,
    has_archive: bool,
    has_input: bool,
}

fn build_exact_classes(photos: &[PhotoRecord], archive_len: usize) -> Vec<ExactClass> {
    let mut by_hash: HashMap<&str, usize> = HashMap::new();
    let mut classes: Vec<ExactClass> = Vec::new();
    for (index, photo) in photos.iter().enumerate() {
        let class_index = match photo
            .pixel_hash
            .as_deref()
            .and_then(|hash| by_hash.get(hash).copied())
        {
            Some(class_index) => class_index,
            None => {
                let class_index = classes.len();
                classes.push(ExactClass {
                    photos: Vec::new(),
                    phash: photo.phash,
                    has_archive: false,
                    has_input: false,
                });
                if let Some(hash) = photo.pixel_hash.as_deref() {
                    by_hash.insert(hash, class_index);
                }
                class_index
            }
        };
        let class = &mut classes[class_index];
        if class.phash.is_none() {
            class.phash = photo.phash;
        }
        if index < archive_len {
            class.has_archive = true;
        } else {
            class.has_input = true;
        }
        class.photos.push(index);
    }
    classes
}

/// Group duplicates for an import run.
///
/// Comparisons performed:
///   * every input photograph against the archive,
///   * every input photograph against other input photographs,
///   * exact pixel equality is also resolved inside the archive matches.
///
/// Archive photographs are never compared against one another directly.
pub fn group_import(
    archive: Vec<PhotoRecord>,
    input: Vec<PhotoRecord>,
    settings: &Settings,
) -> Comparison {
    let archive_len = archive.len();
    let mut photos = archive;
    photos.extend(input);
    let classes = build_exact_classes(&photos, archive_len);
    let edges = import_candidate_edges(&classes, settings.review_distance());
    let mut clusters = cluster_classes(&photos, &classes, edges, settings);
    // Import must not report archive-only duplicate groups: existing archive
    // photographs are never compared with one another during a normal import.
    clusters.retain(|cluster| cluster.members.iter().any(|&index| index >= archive_len));
    Comparison {
        photos,
        archive_len,
        clusters,
    }
}

/// Group duplicates inside an existing archive. Every archive photograph is
/// compared with every other via the BK-tree, but no image is decoded again.
pub fn group_archive(photos: Vec<PhotoRecord>, settings: &Settings) -> Comparison {
    let archive_len = photos.len();
    let classes = build_exact_classes(&photos, archive_len);
    let edges = archive_candidate_edges(&classes, settings.review_distance());
    let clusters = cluster_classes(&photos, &classes, edges, settings);
    Comparison {
        photos,
        archive_len,
        clusters,
    }
}

fn import_candidate_edges(classes: &[ExactClass], radius: u32) -> Vec<(u32, usize, usize)> {
    let archive_entries: Vec<(usize, u64)> = classes
        .iter()
        .enumerate()
        .filter(|(_, class)| class.has_archive)
        .filter_map(|(index, class)| class.phash.map(|hash| (index, hash)))
        .collect();
    let input_entries: Vec<(usize, u64)> = classes
        .iter()
        .enumerate()
        .filter(|(_, class)| class.has_input)
        .filter_map(|(index, class)| class.phash.map(|hash| (index, hash)))
        .collect();
    let archive_tree = BkTree::new(archive_entries);
    let input_tree = BkTree::new(input_entries.clone());

    let mut edges: HashMap<(usize, usize), u32> = HashMap::new();
    let mut found = Vec::new();
    for &(input_class, hash) in &input_entries {
        found.clear();
        archive_tree.search(hash, radius, &mut found);
        for &(archive_class, distance) in &found {
            if archive_class != input_class {
                add_edge(&mut edges, input_class, archive_class, distance);
            }
        }
        found.clear();
        input_tree.search(hash, radius, &mut found);
        for &(other_class, distance) in &found {
            if other_class != input_class {
                add_edge(&mut edges, input_class, other_class, distance);
            }
        }
    }
    sorted_edges(edges)
}

fn archive_candidate_edges(classes: &[ExactClass], radius: u32) -> Vec<(u32, usize, usize)> {
    let entries: Vec<(usize, u64)> = classes
        .iter()
        .enumerate()
        .filter_map(|(index, class)| class.phash.map(|hash| (index, hash)))
        .collect();
    let tree = BkTree::new(entries.clone());
    let mut edges: HashMap<(usize, usize), u32> = HashMap::new();
    let mut found = Vec::new();
    for &(class_index, hash) in &entries {
        found.clear();
        tree.search(hash, radius, &mut found);
        for &(other, distance) in &found {
            if other > class_index {
                add_edge(&mut edges, class_index, other, distance);
            }
        }
    }
    sorted_edges(edges)
}

fn add_edge(edges: &mut HashMap<(usize, usize), u32>, a: usize, b: usize, distance: u32) {
    if a == b {
        return;
    }
    let key = (a.min(b), a.max(b));
    let entry = edges.entry(key).or_insert(distance);
    if distance < *entry {
        *entry = distance;
    }
}

fn sorted_edges(edges: HashMap<(usize, usize), u32>) -> Vec<(u32, usize, usize)> {
    let mut edges: Vec<(u32, usize, usize)> = edges
        .into_iter()
        .map(|((a, b), distance)| (distance, a, b))
        .collect();
    edges.sort_unstable();
    edges
}

fn cluster_classes(
    photos: &[PhotoRecord],
    classes: &[ExactClass],
    edges: Vec<(u32, usize, usize)>,
    settings: &Settings,
) -> Vec<PhotoCluster> {
    let class_count = classes.len();
    if class_count == 0 {
        return Vec::new();
    }
    let radius = settings.review_distance();
    let max_members = settings.max_group_members.max(2);

    let mut parent: Vec<usize> = (0..class_count).collect();
    let mut rank: Vec<u8> = vec![0; class_count];
    let mut sizes: Vec<usize> = classes.iter().map(|c| c.photos.len()).collect();
    let mut reps: Vec<Vec<u64>> = classes
        .iter()
        .map(|c| c.phash.into_iter().collect())
        .collect();

    // Strongest candidate edges first. A merge is allowed only when the
    // combined group is complete-linkage coherent at the review radius and
    // within the size limit.
    for (_, a, b) in edges {
        let root_a = dsu_find(&mut parent, a);
        let root_b = dsu_find(&mut parent, b);
        if root_a == root_b {
            continue;
        }
        if sizes[root_a] + sizes[root_b] > max_members {
            continue;
        }
        let coherent = reps[root_a].iter().all(|&hash_a| {
            reps[root_b]
                .iter()
                .all(|&hash_b| phash_distance(hash_a, hash_b) <= radius)
        });
        if !coherent {
            continue;
        }
        let (keep, gone) = if rank[root_a] >= rank[root_b] {
            (root_a, root_b)
        } else {
            (root_b, root_a)
        };
        parent[gone] = keep;
        if rank[root_a] == rank[root_b] {
            rank[keep] = rank[keep].saturating_add(1);
        }
        sizes[keep] += sizes[gone];
        let moved = std::mem::take(&mut reps[gone]);
        reps[keep].extend(moved);
    }

    let mut by_root: HashMap<usize, Vec<usize>> = HashMap::new();
    for class_index in 0..class_count {
        let root = dsu_find(&mut parent, class_index);
        by_root.entry(root).or_default().push(class_index);
    }

    let mut clusters = Vec::new();
    for mut class_ids in by_root.into_values() {
        class_ids.sort_unstable();
        let mut members: Vec<usize> = class_ids
            .iter()
            .flat_map(|&class_index| classes[class_index].photos.iter().copied())
            .collect();
        if members.is_empty() {
            continue;
        }
        let exact = class_ids.len() == 1;
        if exact && members.len() < 2 {
            continue; // a lone photograph is not a group
        }
        let distance = if exact {
            0
        } else {
            let mut worst = 0u32;
            for (position, &left) in class_ids.iter().enumerate() {
                let Some(hash_left) = classes[left].phash else {
                    continue;
                };
                for &right in &class_ids[position + 1..] {
                    if let Some(hash_right) = classes[right].phash {
                        worst = worst.max(phash_distance(hash_left, hash_right));
                    }
                }
            }
            worst.min(PHASH_BITS)
        };
        members.sort_by(|&a, &b| photos[a].path.cmp(&photos[b].path));
        clusters.push(PhotoCluster {
            members,
            kind: if exact {
                GroupKind::Exact
            } else {
                GroupKind::Perceptual
            },
            distance,
            similarity: similarity_for_distance(distance),
        });
    }
    clusters.sort_by(|a, b| photos[a.members[0]].path.cmp(&photos[b.members[0]].path));
    clusters
}

fn dsu_find(parent: &mut [usize], start: usize) -> usize {
    let mut root = start;
    while parent[root] != root {
        root = parent[root];
    }
    let mut current = start;
    while parent[current] != current {
        let next = parent[current];
        parent[current] = root;
        current = next;
    }
    root
}

// ---------------------------------------------------------------------------
// BK-tree
// ---------------------------------------------------------------------------

/// A BK-tree of perceptual hashes. Search returns every stored hash within a
/// Hamming radius using the triangle inequality.
pub struct BkTree {
    nodes: Vec<Node>,
}

struct Node {
    hash: u64,
    photo: usize,
    children: Vec<(u32, usize)>,
}

impl BkTree {
    pub fn new(entries: Vec<(usize, u64)>) -> Self {
        let mut tree = Self { nodes: Vec::new() };
        for (photo, hash) in entries {
            tree.insert(photo, hash);
        }
        tree
    }

    fn insert(&mut self, photo: usize, hash: u64) {
        if self.nodes.is_empty() {
            self.nodes.push(Node {
                hash,
                photo,
                children: Vec::new(),
            });
            return;
        }
        let mut current = 0usize;
        loop {
            let distance = phash_distance(self.nodes[current].hash, hash);
            if let Some(&(_, child)) = self.nodes[current]
                .children
                .iter()
                .find(|(d, _)| *d == distance)
            {
                current = child;
                continue;
            }
            let new_index = self.nodes.len();
            self.nodes.push(Node {
                hash,
                photo,
                children: Vec::new(),
            });
            self.nodes[current].children.push((distance, new_index));
            break;
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Appends `(photo_index, distance)` for every hash within `radius`.
    pub fn search(&self, hash: u64, radius: u32, out: &mut Vec<(usize, u32)>) {
        if self.nodes.is_empty() {
            return;
        }
        let mut stack = vec![0usize];
        while let Some(index) = stack.pop() {
            let node = &self.nodes[index];
            let distance = phash_distance(node.hash, hash);
            if distance <= radius {
                out.push((node.photo, distance));
            }
            let low = distance.saturating_sub(radius);
            let high = distance.saturating_add(radius);
            for &(edge, child) in &node.children {
                if edge >= low && edge <= high {
                    stack.push(child);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DateSource, ExifInfo, ImageFormatKind, PhotoStatus, RootKind};

    fn photo(path: &str, hash: &str, phash: u64) -> PhotoRecord {
        PhotoRecord {
            id: 0,
            path: path.into(),
            root: if path.starts_with("/in") {
                RootKind::Input
            } else {
                RootKind::Archive
            },
            file_size: 10,
            mtime_ns: 0,
            format: ImageFormatKind::Jpeg,
            width: 100,
            height: 100,
            pixel_hash: Some(hash.into()),
            phash: Some(phash),
            taken_on: Some("2020-01-01".into()),
            taken_at_ns: None,
            taken_source: DateSource::ExifOriginal,
            orientation: 1,
            is_animated: false,
            exif: ExifInfo::default(),
            status: PhotoStatus::Indexed,
            error: None,
            indexed_at: 0,
        }
    }

    fn settings(review_pct: f32) -> Settings {
        settings_with_cap(review_pct, 20)
    }

    fn settings_with_cap(review_pct: f32, max_group_members: usize) -> Settings {
        let mut settings = Settings::default();
        settings.review_similarity_pct = review_pct;
        settings.probable_similarity_pct = 95.0;
        settings.max_group_members = max_group_members;
        settings
    }

    #[test]
    fn bk_tree_matches_brute_force() {
        let hashes: Vec<u64> = (0..500u64)
            .map(|i| i.wrapping_mul(0x9E3779B97F4A7C15) ^ (i << 17))
            .map(|h| h & ((1u64 << PHASH_BITS) - 1))
            .collect();
        let entries: Vec<(usize, u64)> = hashes.iter().copied().enumerate().collect();
        let tree = BkTree::new(entries);
        let query = hashes[42] ^ 0b1111;
        let radius = 4;
        let mut found = Vec::new();
        tree.search(query, radius, &mut found);
        found.sort();
        let mut expected: Vec<(usize, u32)> = hashes
            .iter()
            .enumerate()
            .map(|(i, h)| (i, phash_distance(*h, query)))
            .filter(|(_, d)| *d <= radius)
            .collect();
        expected.sort();
        assert_eq!(found, expected);
    }

    #[test]
    fn exact_and_perceptual_matches_group() {
        let s = settings(70.0);
        let archive = vec![photo("/archive/a.jpg", "aaaa", 0)];
        let input = vec![
            photo("/in/exact.jpg", "aaaa", 0),
            photo("/in/close.jpg", "bbbb", 0b111),
            photo("/in/new.jpg", "cccc", u64::MAX),
        ];
        let comparison = group_import(archive, input, &s);
        assert_eq!(comparison.clusters.len(), 1);
        assert_eq!(comparison.clusters[0].members.len(), 3);
        assert_eq!(comparison.clusters[0].kind, GroupKind::Perceptual);
        assert_eq!(comparison.clusters[0].distance, 3);
    }

    #[test]
    fn archive_mode_does_not_group_distant_images() {
        let s = settings(70.0);
        let photos = vec![
            photo("/archive/a.jpg", "aaaa", 0),
            photo("/archive/b.jpg", "bbbb", (1u64 << PHASH_BITS) - 1),
        ];
        let comparison = group_archive(photos, &s);
        assert!(comparison.clusters.is_empty());
    }

    #[test]
    fn archive_mode_finds_exact_duplicates() {
        let s = settings(70.0);
        let photos = vec![
            photo("/archive/a.jpg", "aaaa", 123),
            photo("/archive/b.jpg", "aaaa", 123),
            photo("/archive/c.jpg", "cccc", (1u64 << PHASH_BITS) - 1),
        ];
        let comparison = group_archive(photos, &s);
        assert_eq!(comparison.clusters.len(), 1);
        assert_eq!(comparison.clusters[0].kind, GroupKind::Exact);
        assert_eq!(comparison.clusters[0].members.len(), 2);
    }

    #[test]
    fn weak_chains_are_not_transitively_merged() {
        // h0--9--h1--9--h2, but h0--18--h2. At the 85% radius (distance <= 9)
        // h0 and h1 may group, but h2 must not be pulled in through h1.
        let s = settings(85.0);
        let h0 = 0u64;
        let h1 = (1u64 << 9) - 1;
        let h2 = (1u64 << 18) - 1;
        let photos = vec![
            photo("/archive/a.jpg", "aaaa", h0),
            photo("/archive/b.jpg", "bbbb", h1),
            photo("/archive/c.jpg", "cccc", h2),
        ];
        let comparison = group_archive(photos, &s);
        assert_eq!(comparison.clusters.len(), 1);
        assert_eq!(comparison.clusters[0].members.len(), 2);
        assert!(!comparison.clusters[0].members.contains(&2));
    }

    #[test]
    fn group_size_cap_splits_dense_clusters() {
        let s = settings_with_cap(85.0, 3);
        let photos: Vec<PhotoRecord> = (0..6)
            .map(|i| photo(&format!("/archive/{i}.jpg"), &format!("hash{i}"), 0))
            .collect();
        let comparison = group_archive(photos, &s);
        assert_eq!(comparison.clusters.len(), 2);
        for cluster in &comparison.clusters {
            assert!(cluster.members.len() <= 3, "{:?}", cluster.members);
        }
    }

    #[test]
    fn default_review_radius_is_conservative() {
        // Distance 12 is 81%, inside the old 70% review band but below the
        // conservative 85% grouping default.
        let h0 = 0u64;
        let h1 = (1u64 << 12) - 1;
        let make = || {
            vec![
                photo("/archive/a.jpg", "aaaa", h0),
                photo("/archive/b.jpg", "bbbb", h1),
            ]
        };
        let conservative = Settings::default();
        assert!(group_archive(make(), &conservative).clusters.is_empty());
        let loose = settings(70.0);
        assert_eq!(group_archive(make(), &loose).clusters.len(), 1);
    }

    #[test]
    fn import_never_groups_archive_only_photographs() {
        let s = settings(85.0);
        let archive = vec![
            photo("/archive/a.jpg", "aaaa", 0),
            photo("/archive/b.jpg", "bbbb", 0b111),
        ];
        let input = vec![photo("/in/x.jpg", "cccc", (1u64 << PHASH_BITS) - 1)];
        let comparison = group_import(archive, input, &s);
        assert!(comparison.clusters.is_empty());
    }

    #[test]
    fn import_does_not_report_archive_only_exact_duplicates() {
        let s = settings(85.0);
        let archive = vec![
            photo("/archive/a.jpg", "samepixels", 0),
            photo("/archive/b.jpg", "samepixels", 0),
        ];
        let input = vec![photo("/in/x.jpg", "different", (1u64 << PHASH_BITS) - 1)];
        let comparison = group_import(archive, input, &s);
        assert!(comparison.clusters.is_empty());
    }
}
