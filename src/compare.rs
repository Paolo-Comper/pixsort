//! Duplicate detection: exact pixel hashes plus a BK-tree over perceptual
//! hashes.
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
    /// Lowest Hamming distance between *different* pixel hashes in the group.
    pub distance: u32,
    pub similarity: f32,
}

impl PhotoCluster {
    pub fn is_exact(&self) -> bool {
        self.kind == GroupKind::Exact
    }
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
    let mut uf = UnionFind::new(photos.len());
    let mut perceptual_edges: HashMap<(usize, usize), u32> = HashMap::new();
    let review_distance = settings.review_distance();

    // --- exact pixel duplicates ------------------------------------------
    let mut by_hash: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, photo) in photos.iter().enumerate() {
        if let Some(hash) = photo.pixel_hash.as_deref() {
            by_hash.entry(hash).or_default().push(i);
        }
    }
    for members in by_hash.values() {
        if members.len() < 2 {
            continue;
        }
        let touches_input = members.iter().any(|&i| i >= archive_len);
        if touches_input {
            for pair in members.windows(2) {
                uf.union(pair[0], pair[1]);
            }
        }
    }

    // --- perceptual duplicates (only input as the query side) ------------
    if review_distance > 0 {
        let archive_hashes: Vec<(usize, u64)> = (0..archive_len)
            .filter_map(|i| photos[i].phash.map(|h| (i, h)))
            .collect();
        let input_hashes: Vec<(usize, u64)> = (archive_len..photos.len())
            .filter_map(|i| photos[i].phash.map(|h| (i, h)))
            .collect();
        let archive_tree = BkTree::new(archive_hashes);
        let input_tree = BkTree::new(input_hashes.clone());
        let mut found = Vec::new();
        for &(i, hash) in &input_hashes {
            found.clear();
            archive_tree.search(hash, review_distance, &mut found);
            for &(j, distance) in &found {
                if photos[i].pixel_hash == photos[j].pixel_hash && photos[i].pixel_hash.is_some() {
                    continue; // already an exact match
                }
                uf.union(i, j);
                record_edge(&mut perceptual_edges, i, j, distance);
            }
            found.clear();
            input_tree.search(hash, review_distance, &mut found);
            for &(j, distance) in &found {
                if i == j {
                    continue;
                }
                if photos[i].pixel_hash == photos[j].pixel_hash && photos[i].pixel_hash.is_some() {
                    continue;
                }
                uf.union(i, j);
                record_edge(&mut perceptual_edges, i, j, distance);
            }
        }
    }

    let clusters = build_clusters(&photos, &mut uf, &perceptual_edges);
    Comparison {
        photos,
        archive_len,
        clusters,
    }
}

/// Group duplicates inside an existing archive. Every archive photograph is
/// compared with every other via the BK-tree, but no image is decoded again.
pub fn group_archive(photos: Vec<PhotoRecord>, settings: &Settings) -> Comparison {
    let len = photos.len();
    let archive_len = len;
    let mut uf = UnionFind::new(len);
    let mut perceptual_edges: HashMap<(usize, usize), u32> = HashMap::new();
    let review_distance = settings.review_distance();

    // Exact classes: all photos sharing a pixel hash.
    let mut by_hash: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, photo) in photos.iter().enumerate() {
        if let Some(hash) = photo.pixel_hash.as_deref() {
            by_hash.entry(hash).or_default().push(i);
        }
    }
    let mut representatives: Vec<usize> = Vec::new();
    let mut class_of: Vec<Option<usize>> = vec![None; len];
    for members in by_hash.values() {
        for pair in members.windows(2) {
            uf.union(pair[0], pair[1]);
        }
        let rep = members[0];
        for &member in members {
            class_of[member] = Some(rep);
        }
        representatives.push(rep);
    }
    // Photos without any pixel hash (should be none in an indexed archive)
    // still take part, as their own class.
    for i in 0..len {
        if class_of[i].is_none() {
            class_of[i] = Some(i);
            representatives.push(i);
        }
    }

    if review_distance > 0 {
        let rep_hashes: Vec<(usize, u64)> = representatives
            .iter()
            .filter_map(|&i| photos[i].phash.map(|h| (i, h)))
            .collect();
        let tree = BkTree::new(rep_hashes.clone());
        let mut found = Vec::new();
        for &(i, hash) in &rep_hashes {
            found.clear();
            tree.search(hash, review_distance, &mut found);
            for &(j, distance) in &found {
                if i == j {
                    continue;
                }
                // Different exact classes only; equal hashes are already merged.
                let same_pixels = match (
                    photos[i].pixel_hash.as_deref(),
                    photos[j].pixel_hash.as_deref(),
                ) {
                    (Some(a), Some(b)) => a == b,
                    _ => false,
                };
                if same_pixels {
                    continue;
                }
                uf.union(i, j);
                record_edge(&mut perceptual_edges, i, j, distance);
            }
        }
    }

    let clusters = build_clusters(&photos, &mut uf, &perceptual_edges);
    Comparison {
        photos,
        archive_len,
        clusters,
    }
}

fn record_edge(edges: &mut HashMap<(usize, usize), u32>, a: usize, b: usize, distance: u32) {
    let key = (a.min(b), a.max(b));
    let entry = edges.entry(key).or_insert(distance);
    if distance < *entry {
        *entry = distance;
    }
}

fn build_clusters(
    photos: &[PhotoRecord],
    uf: &mut UnionFind,
    perceptual_edges: &HashMap<(usize, usize), u32>,
) -> Vec<PhotoCluster> {
    let mut buckets: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..photos.len() {
        if photos[i].phash.is_none() {
            continue;
        }
        let root = uf.find(i);
        buckets.entry(root).or_default().push(i);
    }

    let mut clusters = Vec::new();
    for members in buckets.into_values() {
        if members.len() < 2 {
            continue;
        }
        let first_hash = photos[members[0]].pixel_hash.as_deref();
        let exact = first_hash.is_some()
            && members
                .iter()
                .all(|&i| photos[i].pixel_hash.as_deref() == first_hash);
        let distance = if exact {
            0
        } else {
            let set: std::collections::HashSet<usize> = members.iter().copied().collect();
            let mut min = PHASH_BITS;
            for (&(a, b), &d) in perceptual_edges {
                if set.contains(&a) && set.contains(&b) {
                    min = min.min(d);
                }
            }
            // Fallback: derive from pairwise comparison when an edge was lost
            // (e.g. a cluster rooted through exact links). These groups are
            // tiny in practice.
            if min == PHASH_BITS && members.len() <= 64 {
                for a in 0..members.len() {
                    for b in a + 1..members.len() {
                        if let (Some(ha), Some(hb)) =
                            (photos[members[a]].phash, photos[members[b]].phash)
                        {
                            if photos[members[a]].pixel_hash != photos[members[b]].pixel_hash {
                                min = min.min(phash_distance(ha, hb));
                            }
                        }
                    }
                }
            }
            min
        };
        let mut sorted = members;
        sorted.sort_by(|&a, &b| photos[a].path.cmp(&photos[b].path));
        clusters.push(PhotoCluster {
            members: sorted,
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

// ---------------------------------------------------------------------------
// Union-find
// ---------------------------------------------------------------------------

struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return;
        }
        match self.rank[ra].cmp(&self.rank[rb]) {
            std::cmp::Ordering::Less => self.parent[ra] = rb,
            std::cmp::Ordering::Greater => self.parent[rb] = ra,
            std::cmp::Ordering::Equal => {
                self.parent[rb] = ra;
                self.rank[ra] += 1;
            }
        }
    }
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
        let mut settings = Settings::default();
        settings.review_similarity_pct = review_pct;
        settings.probable_similarity_pct = 95.0;
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
}
