//! Application settings: the four independently configurable paths plus
//! comparison thresholds and processing options.
//!
//! Settings live in a small JSON file in the platform configuration
//! directory. They never contain the indexed photo data itself.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::distance_for_similarity;

pub const SETTINGS_VERSION: u32 = 1;
const SETTINGS_FILE: &str = "settings.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema_version: u32,

    /// Directory containing the photographs to import.
    pub input_dir: Option<PathBuf>,
    /// Root of the existing year-based photo archive.
    pub destination_dir: Option<PathBuf>,
    /// SQLite file holding the operational image index (Database A).
    pub index_db_path: Option<PathBuf>,
    /// SQLite file holding the modification plan (Database B).
    pub plan_db_path: Option<PathBuf>,
    /// Where discarded archive versions are moved. Never permanently deleted.
    /// Defaults to a sibling of the archive root, outside the archive.
    pub trash_dir: Option<PathBuf>,

    pub worker_threads: usize,

    /// Similarity (percent) at or above which a match is "probable duplicate".
    pub probable_similarity_pct: f32,
    /// Similarity (percent) at or above which a match is worth reviewing.
    pub review_similarity_pct: f32,

    /// Use the filesystem modification time as a year fallback when no EXIF
    /// timestamp exists. When disabled such photos go to `Unknown/`.
    pub use_filesystem_year: bool,

    /// Longest edge used for preview thumbnails in the GUI.
    pub thumbnail_max_dim: u32,
}

impl Default for Settings {
    fn default() -> Self {
        let base = config_dir();
        let sessions = base.join("sessions");
        Self {
            schema_version: SETTINGS_VERSION,
            input_dir: None,
            destination_dir: None,
            index_db_path: Some(sessions.join("index.sqlite")),
            plan_db_path: Some(sessions.join("plan.sqlite")),
            trash_dir: None,
            worker_threads: default_workers(),
            probable_similarity_pct: 95.0,
            review_similarity_pct: 70.0,
            use_filesystem_year: true,
            thumbnail_max_dim: 1200,
        }
    }
}

pub fn default_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 64)
}

/// Platform configuration directory, without creating it.
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PIXSORT_CONFIG_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join("Pixsort");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("Pixsort");
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("pixsort");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".config").join("pixsort");
    }
    PathBuf::from(".pixsort")
}

pub fn settings_path() -> PathBuf {
    config_dir().join(SETTINGS_FILE)
}

impl Settings {
    pub fn load() -> (Self, Option<String>) {
        let path = settings_path();
        match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Self>(&text) {
                Ok(mut settings) => {
                    settings.normalize();
                    (settings, None)
                }
                Err(err) => (
                    Self::default(),
                    Some(format!(
                        "Could not parse {}: {err}. Using defaults.",
                        path.display()
                    )),
                ),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Self::default(), None),
            Err(err) => (
                Self::default(),
                Some(format!(
                    "Could not read {}: {err}. Using defaults.",
                    path.display()
                )),
            ),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let mut settings = self.clone();
        settings.normalize();
        let path = settings_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(&settings)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text)
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", tmp.display()))?;
        // Windows does not replace an existing file with rename.
        if fs::rename(&tmp, &path).is_err() {
            let _ = fs::remove_file(&path);
            fs::rename(&tmp, &path)
                .map_err(|e| anyhow::anyhow!("cannot replace {}: {e}", path.display()))?;
        }
        Ok(())
    }

    /// Enforces invariants after loading or editing.
    pub fn normalize(&mut self) {
        self.schema_version = SETTINGS_VERSION;
        self.worker_threads = self.worker_threads.clamp(1, 64);
        self.probable_similarity_pct = self.probable_similarity_pct.clamp(1.0, 100.0);
        self.review_similarity_pct = self.review_similarity_pct.clamp(0.0, 100.0);
        if self.review_similarity_pct > self.probable_similarity_pct {
            self.review_similarity_pct = self.probable_similarity_pct;
        }
        self.thumbnail_max_dim = self.thumbnail_max_dim.clamp(256, 4096);
    }

    pub fn probable_distance(&self) -> u32 {
        distance_for_similarity(self.probable_similarity_pct)
    }

    pub fn review_distance(&self) -> u32 {
        distance_for_similarity(self.review_similarity_pct)
    }

    /// Human readable conversion shown next to threshold sliders.
    pub fn threshold_hint(&self) -> String {
        format!(
            "pHashes are {bits}-bit. {p:.0}% = Hamming distance ≤ {pd}; {r:.0}% = ≤ {rd}. \
             Similarity = 1 − distance/{bits}, shown as a heuristic indicator.",
            bits = crate::model::PHASH_BITS,
            p = self.probable_similarity_pct,
            pd = self.probable_distance(),
            r = self.review_similarity_pct,
            rd = self.review_distance(),
        )
    }

    /// Returns the trash directory to use, never inside the archive by default.
    pub fn effective_trash_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = &self.trash_dir {
            return Some(dir.clone());
        }
        if let Some(dest) = &self.destination_dir {
            if let Some(parent) = dest.parent() {
                return Some(parent.join("pixsort-trash"));
            }
        }
        Some(config_dir().join("trash"))
    }

    pub fn effective_index_db(&self) -> Option<PathBuf> {
        self.index_db_path
            .clone()
            .or_else(|| Some(config_dir().join("sessions").join("index.sqlite")))
    }

    pub fn effective_plan_db(&self) -> Option<PathBuf> {
        self.plan_db_path
            .clone()
            .or_else(|| Some(config_dir().join("sessions").join("plan.sqlite")))
    }

    pub fn all_paths_present(&self) -> bool {
        self.input_dir.is_some()
            && self.destination_dir.is_some()
            && self.effective_index_db().is_some()
            && self.effective_plan_db().is_some()
    }

    /// Format-independent checks performed before any operation.
    pub fn issues(&self) -> Vec<ValidationIssue> {
        let mut out = Vec::new();

        check_directory("Input directory", self.input_dir.as_deref(), true, &mut out);
        check_directory(
            "Archive destination",
            self.destination_dir.as_deref(),
            true,
            &mut out,
        );

        let index = self.effective_index_db();
        let plan = self.effective_plan_db();
        check_database_path("Image index database", index.as_deref(), &mut out);
        check_database_path("Modification plan database", plan.as_deref(), &mut out);

        if let (Some(a), Some(b)) = (&index, &plan) {
            if same_path(a, b) {
                out.push(ValidationIssue::error(
                    "The image index and modification plan must be different files.",
                ));
            }
        }

        if let (Some(db), Some(dest)) = (&index, &self.destination_dir) {
            if path_starts_with(db, dest) {
                out.push(ValidationIssue::error(format!(
                    "The image index database must not be stored inside the archive ({}).",
                    db.display()
                )));
            }
        }
        if let (Some(db), Some(dest)) = (&plan, &self.destination_dir) {
            if path_starts_with(db, dest) {
                out.push(ValidationIssue::error(format!(
                    "The modification plan database must not be stored inside the archive ({}).",
                    db.display()
                )));
            }
        }

        if let (Some(input), Some(dest)) = (&self.input_dir, &self.destination_dir) {
            if same_path(input, dest) {
                out.push(ValidationIssue::warning(
                    "The input directory is the archive itself. Import will compare the archive \
                     against itself; use Archive analysis instead.",
                ));
            } else if path_starts_with(input, dest) {
                out.push(ValidationIssue::warning(
                    "The input directory is inside the archive. It is not scanned by import \
                     indexing of the archive root.",
                ));
            }
        }

        if let (Some(trash), Some(input)) = (self.effective_trash_dir(), &self.input_dir) {
            if same_path(&trash, input) || path_starts_with(&trash, input) {
                out.push(ValidationIssue::warning(
                    "The trash directory is inside the input folder; discarded files would be \
                     scanned again on the next import.",
                ));
            }
        }
        if let Some(trash) = self.effective_trash_dir() {
            if let Some(dest) = &self.destination_dir {
                if same_path(&trash, dest) {
                    out.push(ValidationIssue::error(
                        "The trash directory must not be the archive root.",
                    ));
                } else if path_starts_with(&trash, dest) {
                    out.push(ValidationIssue::warning(
                        "The trash directory is inside the archive. It will be skipped while \
                         scanning, but keeping trash outside the archive is recommended.",
                    ));
                }
            }
        }

        if self.review_similarity_pct > self.probable_similarity_pct {
            out.push(ValidationIssue::error(
                "The review threshold must not be higher than the probable-duplicate threshold.",
            ));
        }

        out
    }

    fn require_no_errors(&self, ignore_input: bool) -> anyhow::Result<()> {
        let problems: Vec<String> = self
            .issues()
            .into_iter()
            .filter(|issue| issue.level == ValidationLevel::Error)
            .filter(|issue| !(ignore_input && issue.message.starts_with("Input directory")))
            .map(|issue| issue.message)
            .collect();
        if !problems.is_empty() {
            anyhow::bail!("{}", problems.join("\n"));
        }
        Ok(())
    }

    pub fn validate_for_import(&self) -> anyhow::Result<()> {
        self.require_no_errors(false)
    }

    pub fn validate_for_analysis(&self) -> anyhow::Result<()> {
        self.require_no_errors(true)
    }

    pub fn validate_for_apply(&self) -> anyhow::Result<()> {
        self.require_no_errors(true)
    }

    /// Checks that all four locations exist and are accessible before a resume.
    pub fn validate_for_resume(&self) -> anyhow::Result<()> {
        let input = self
            .input_dir
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no input directory selected"))?;
        let dest = self
            .destination_dir
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no archive destination selected"))?;
        let index = self
            .effective_index_db()
            .ok_or_else(|| anyhow::anyhow!("no image index database selected"))?;
        let plan = self
            .effective_plan_db()
            .ok_or_else(|| anyhow::anyhow!("no modification plan database selected"))?;
        if !input.is_dir() {
            anyhow::bail!("input directory is not accessible: {}", input.display());
        }
        if !dest.is_dir() {
            anyhow::bail!("archive destination is not accessible: {}", dest.display());
        }
        for db in [&index, &plan] {
            if db.exists() && !db.is_file() {
                anyhow::bail!("database path is not a file: {}", db.display());
            }
            if let Some(parent) = db.parent() {
                if !parent.exists() {
                    anyhow::bail!("database directory is not accessible: {}", parent.display());
                }
            }
        }
        Ok(())
    }

    pub fn summary(&self) -> String {
        format!(
            "{} worker threads · probable ≥ {:.0}% (d ≤ {}) · review ≥ {:.0}% (d ≤ {})",
            self.worker_threads,
            self.probable_similarity_pct,
            self.probable_distance(),
            self.review_similarity_pct,
            self.review_distance()
        )
    }
}

fn check_directory(
    label: &str,
    path: Option<&Path>,
    required: bool,
    out: &mut Vec<ValidationIssue>,
) {
    match path {
        None if required => out.push(ValidationIssue::error(format!("{label}: not selected."))),
        None => {}
        Some(p) => {
            if !p.is_dir() {
                out.push(ValidationIssue::error(format!(
                    "{label} is not an accessible directory: {}",
                    p.display()
                )));
            }
        }
    }
}

fn check_database_path(label: &str, path: Option<&Path>, out: &mut Vec<ValidationIssue>) {
    match path {
        None => out.push(ValidationIssue::error(format!("{label}: not selected."))),
        Some(p) => {
            if p.is_dir() {
                out.push(ValidationIssue::error(format!(
                    "{label} points at a directory, not a file: {}",
                    p.display()
                )));
            }
            if let Some(parent) = p.parent() {
                if !parent.exists() {
                    out.push(ValidationIssue::debug(format!(
                        "{label}: folder will be created on first use: {}",
                        parent.display()
                    )));
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationLevel {
    Error,
    Warning,
    Debug,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    pub level: ValidationLevel,
    pub message: String,
}

impl ValidationIssue {
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            level: ValidationLevel::Error,
            message: message.into(),
        }
    }
    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            level: ValidationLevel::Warning,
            message: message.into(),
        }
    }
    pub fn debug(message: impl Into<String>) -> Self {
        Self {
            level: ValidationLevel::Debug,
            message: message.into(),
        }
    }
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    normalize_for_compare(a) == normalize_for_compare(b)
}

fn normalize_for_compare(p: &Path) -> PathBuf {
    let mut out = p.to_path_buf();
    if let Ok(canon) = fs::canonicalize(p) {
        out = canon;
    }
    out
}

/// Lexical "is `child` inside `parent`" check, with canonicalisation when possible.
pub fn path_starts_with(child: &Path, parent: &Path) -> bool {
    let child = normalize_for_compare(child);
    let parent = normalize_for_compare(parent);
    child != parent && child.starts_with(parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_convert() {
        let s = Settings::default();
        assert_eq!(s.probable_distance(), 3);
        assert_eq!(s.review_distance(), 19);
        assert!(s.threshold_hint().contains("Hamming"));
    }

    #[test]
    fn normalize_clamps_thresholds() {
        let mut s = Settings::default();
        s.review_similarity_pct = 99.0;
        s.probable_similarity_pct = 80.0;
        s.worker_threads = 0;
        s.normalize();
        assert_eq!(s.review_similarity_pct, 80.0);
        assert_eq!(s.worker_threads, 1);
    }

    #[test]
    fn db_inside_archive_is_an_error() {
        let mut s = Settings::default();
        s.destination_dir = Some(PathBuf::from("/photos/archive"));
        s.index_db_path = Some(PathBuf::from("/photos/archive/.pixsort/index.sqlite"));
        let issues = s.issues();
        assert!(issues
            .iter()
            .any(|i| i.message.contains("inside the archive")));
    }
}
