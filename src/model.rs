//! Core data types shared by the scanner, the databases and the GUI.
//!
//! Everything in this module is deliberately plain data: no I/O, no database
//! handles. That keeps the comparison and planning code easy to test.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Number of informative bits in a Pixsort perceptual hash.
///
/// The 2-D DCT is evaluated on a 8x8 low-frequency block; the DC coefficient
/// is skipped because it only carries average brightness.
pub const PHASH_BITS: u32 = 63;

/// A group of pixels closer than this is never considered the same photo.
pub const MIN_REVIEW_SIMILARITY: f32 = 0.0;

// ---------------------------------------------------------------------------
// File / format types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RootKind {
    Archive,
    Input,
}

impl RootKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::Input => "input",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "archive" => Some(Self::Archive),
            "input" => Some(Self::Input),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Archive => "Archive",
            Self::Input => "Import",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormatKind {
    Jpeg,
    Png,
    WebP,
    Bmp,
    Tiff,
    Heic,
}

impl ImageFormatKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Png => "png",
            Self::WebP => "webp",
            Self::Bmp => "bmp",
            Self::Tiff => "tiff",
            Self::Heic => "heic",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Jpeg => "JPEG",
            Self::Png => "PNG",
            Self::WebP => "WebP",
            Self::Bmp => "BMP",
            Self::Tiff => "TIFF",
            Self::Heic => "HEIC",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "jpeg" => Self::Jpeg,
            "png" => Self::Png,
            "webp" => Self::WebP,
            "bmp" => Self::Bmp,
            "tiff" => Self::Tiff,
            "heic" => Self::Heic,
            _ => return None,
        })
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" | "jpe" | "jfif" => Self::Jpeg,
            "png" => Self::Png,
            "webp" => Self::WebP,
            "bmp" => Self::Bmp,
            "tif" | "tiff" => Self::Tiff,
            "heic" | "heif" | "hif" => Self::Heic,
            _ => return None,
        })
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(Self::from_extension)
    }

    /// Detects a still-photograph format from the file's magic bytes.
    pub fn from_magic(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            return Some(Self::Jpeg);
        }
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Some(Self::Png);
        }
        if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            return Some(Self::WebP);
        }
        if bytes.starts_with(b"BM") {
            return Some(Self::Bmp);
        }
        if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
            return Some(Self::Tiff);
        }
        if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
            let brand = &bytes[8..12];
            if matches!(
                brand,
                b"heic"
                    | b"heix"
                    | b"hevc"
                    | b"hevx"
                    | b"heim"
                    | b"heis"
                    | b"hevm"
                    | b"hevs"
                    | b"mif1"
                    | b"msf1"
                    | b"avif"
                    | b"avis"
            ) {
                return Some(Self::Heic);
            }
        }
        None
    }

    /// True for the formats Pixsort is willing to import as photographs.
    pub fn is_supported_file(path: &Path) -> bool {
        Self::from_path(path).is_some()
    }
}

// ---------------------------------------------------------------------------
// EXIF
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GpsCoord {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExifInfo {
    pub has_exif: bool,
    pub date_time_original: Option<String>,
    pub date_time_digitized: Option<String>,
    pub date_time: Option<String>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    pub software: Option<String>,
    pub gps: Option<GpsCoord>,
    pub pixel_x: Option<u32>,
    pub pixel_y: Option<u32>,
    pub orientation: u16,
}

impl Default for ExifInfo {
    fn default() -> Self {
        Self {
            has_exif: false,
            date_time_original: None,
            date_time_digitized: None,
            date_time: None,
            make: None,
            model: None,
            lens: None,
            software: None,
            gps: None,
            pixel_x: None,
            pixel_y: None,
            orientation: 1,
        }
    }
}

impl ExifInfo {
    /// Number of populated, useful metadata fields (excluding orientation).
    pub fn completeness(&self) -> u32 {
        let mut n = 0;
        if self.date_time_original.is_some() {
            n += 1;
        }
        if self.make.is_some() {
            n += 1;
        }
        if self.model.is_some() {
            n += 1;
        }
        if self.lens.is_some() {
            n += 1;
        }
        if self.gps.is_some() {
            n += 1;
        }
        if self.software.is_some() {
            n += 1;
        }
        n
    }

    pub fn camera_label(&self) -> Option<String> {
        match (&self.make, &self.model) {
            (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => {
                Some(model.clone())
            }
            (Some(make), Some(model)) => Some(format!("{make} {model}")),
            (None, Some(model)) => Some(model.clone()),
            (Some(make), None) => Some(make.clone()),
            _ => None,
        }
    }

    /// Normalised EXIF orientation, clamped to 1..=8.
    pub fn normalized_orientation(&self) -> u16 {
        if (1..=8).contains(&self.orientation) {
            self.orientation
        } else {
            1
        }
    }
}

// ---------------------------------------------------------------------------
// Photo records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PhotoStatus {
    Pending,
    Indexed,
    Failed,
}

impl PhotoStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Indexed => "indexed",
            Self::Failed => "failed",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "indexed" => Some(Self::Indexed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DateSource {
    ExifOriginal,
    ExifDigitized,
    ExifDateTime,
    FileModified,
    Unknown,
}

impl DateSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExifOriginal => "exif_original",
            Self::ExifDigitized => "exif_digitized",
            Self::ExifDateTime => "exif_datetime",
            Self::FileModified => "file_modified",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "exif_original" => Self::ExifOriginal,
            "exif_digitized" => Self::ExifDigitized,
            "exif_datetime" => Self::ExifDateTime,
            "file_modified" => Self::FileModified,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ExifOriginal => "EXIF DateTimeOriginal",
            Self::ExifDigitized => "EXIF DateTimeDigitized",
            Self::ExifDateTime => "EXIF DateTime",
            Self::FileModified => "filesystem modification time",
            Self::Unknown => "no reliable date",
        }
    }

    /// Reliability of the source for the year/date of capture.
    pub fn reliability(self) -> u8 {
        match self {
            Self::ExifOriginal => 4,
            Self::ExifDigitized => 3,
            Self::ExifDateTime => 2,
            Self::FileModified => 1,
            Self::Unknown => 0,
        }
    }

    pub fn is_exif(self) -> bool {
        matches!(
            self,
            Self::ExifOriginal | Self::ExifDigitized | Self::ExifDateTime
        )
    }
}

/// A fully indexed photograph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhotoRecord {
    #[serde(default)]
    pub id: i64,
    pub path: String,
    pub root: RootKind,
    pub file_size: u64,
    pub mtime_ns: i64,
    pub format: ImageFormatKind,
    pub width: u32,
    pub height: u32,
    pub pixel_hash: Option<String>,
    pub phash: Option<u64>,
    /// ISO date (YYYY-MM-DD) chosen for destination-year purposes.
    pub taken_on: Option<String>,
    /// Capture timestamp in nanoseconds since the Unix epoch, when known.
    pub taken_at_ns: Option<i64>,
    pub taken_source: DateSource,
    pub orientation: u16,
    pub is_animated: bool,
    pub exif: ExifInfo,
    pub status: PhotoStatus,
    pub error: Option<String>,
    pub indexed_at: i64,
}

impl PhotoRecord {
    pub fn file_path(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }

    pub fn filename(&self) -> &str {
        self.path.rsplit(['/', '\\']).next().unwrap_or(&self.path)
    }

    pub fn extension(&self) -> String {
        Path::new(&self.path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or(self.format.as_str())
            .to_string()
    }

    pub fn pixels(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    pub fn megapixels(&self) -> f64 {
        self.pixels() as f64 / 1_000_000.0
    }

    pub fn resolution_label(&self) -> String {
        format!(
            "{} × {} ({:.1} MP)",
            self.width,
            self.height,
            self.megapixels()
        )
    }

    pub fn size_label(&self) -> String {
        human_bytes(self.file_size)
    }

    pub fn year(&self) -> String {
        self.taken_on
            .as_deref()
            .and_then(|d| d.get(0..4))
            .filter(|y| y.len() == 4)
            .map(|y| y.to_string())
            .unwrap_or_else(|| "Unknown".to_string())
    }

    pub fn date_label(&self) -> String {
        match &self.taken_on {
            Some(d) => format!("{d} ({})", self.taken_source.label()),
            None => self.taken_source.label().to_string(),
        }
    }

    pub fn modified_label(&self) -> String {
        chrono::DateTime::from_timestamp(
            self.mtime_ns.div_euclid(1_000_000_000),
            (self.mtime_ns.rem_euclid(1_000_000_000)) as u32,
        )
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "unknown".to_string())
    }

    pub fn short_hash(&self) -> String {
        self.pixel_hash
            .as_deref()
            .map(|h| h.chars().take(12).collect())
            .unwrap_or_else(|| "—".to_string())
    }

    pub fn similarity_to(&self, other: &PhotoRecord) -> Option<f32> {
        if self.pixel_hash.is_some() && self.pixel_hash == other.pixel_hash {
            return Some(100.0);
        }
        phash_similarity(self.phash?, other.phash?)
    }

    /// Human-readable description of the file's capture date origin.
    pub fn date_source_label(&self) -> &'static str {
        self.taken_source.label()
    }
}

pub fn phash_similarity(a: u64, b: u64) -> Option<f32> {
    let distance = (a ^ b).count_ones().min(PHASH_BITS);
    Some(100.0 * (1.0 - distance as f32 / PHASH_BITS as f32))
}

pub fn phash_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones().min(PHASH_BITS)
}

pub fn similarity_for_distance(distance: u32) -> f32 {
    100.0 * (1.0 - distance.min(PHASH_BITS) as f32 / PHASH_BITS as f32)
}

pub fn distance_for_similarity(pct: f32) -> u32 {
    let pct = pct.clamp(0.0, 100.0);
    (((100.0 - pct) / 100.0) * PHASH_BITS as f32).round() as u32
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------------------
// Duplicate groups and decisions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupKind {
    Exact,
    Perceptual,
    /// A single input photograph with no match. Stored so the persisted plan
    /// can be rebuilt without re-running the scan.
    Single,
}

impl GroupKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Perceptual => "perceptual",
            Self::Single => "single",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "exact" => Some(Self::Exact),
            "perceptual" => Some(Self::Perceptual),
            "single" => Some(Self::Single),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Exact => "Exact pixel duplicate",
            Self::Perceptual => "Perceptually similar",
            Self::Single => "New photograph",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupState {
    /// Exact duplicates — the recommendation is applied unless the user changes it.
    Auto,
    /// Perceptual matches — always waits for the user.
    NeedsReview,
    /// The user picked a keeper.
    Resolved,
    /// The user chose "keep both" or "ignore".
    Ignored,
}

impl GroupState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::NeedsReview => "needs_review",
            Self::Resolved => "resolved",
            Self::Ignored => "ignored",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "needs_review" => Self::NeedsReview,
            "resolved" => Self::Resolved,
            "ignored" => Self::Ignored,
            _ => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Recommended automatically",
            Self::NeedsReview => "Needs review",
            Self::Resolved => "Decided",
            Self::Ignored => "Kept both / ignored",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupDecision {
    None,
    Keep,
    KeepBoth,
    Ignore,
}

impl GroupDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Keep => "keep",
            Self::KeepBoth => "keep_both",
            Self::Ignore => "ignore",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "keep" => Self::Keep,
            "keep_both" => Self::KeepBoth,
            "ignore" => Self::Ignore,
            _ => Self::None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "Undecided",
            Self::Keep => "Keep selected photo",
            Self::KeepBoth => "Keep both",
            Self::Ignore => "Ignore this match",
        }
    }
}

/// Persistent, display-ready duplicate group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateGroup {
    #[serde(default)]
    pub id: i64,
    pub session_id: i64,
    pub kind: GroupKind,
    /// Lowest similarity among the links that formed the group (percent).
    pub similarity: f32,
    /// Hamming distance corresponding to `similarity`.
    pub distance: u32,
    pub state: GroupState,
    pub decision: GroupDecision,
    pub keeper_path: Option<String>,
    pub recommended_path: Option<String>,
    pub explanation: String,
    pub title: String,
}

impl DuplicateGroup {
    /// True when the plan must not be applied until the user decides.
    pub fn needs_decision(&self) -> bool {
        match self.state {
            GroupState::NeedsReview => true,
            GroupState::Resolved => self.keeper_path.is_none(),
            GroupState::Auto | GroupState::Ignored => false,
        }
    }

    pub fn is_ambiguous(&self) -> bool {
        self.kind == GroupKind::Perceptual && self.needs_decision()
    }
}

// ---------------------------------------------------------------------------
// Plan items
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OpType {
    Insert,
    Replace,
    Skip,
    MoveToTrash,
    Review,
}

impl OpType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Insert => "INSERT",
            Self::Replace => "REPLACE",
            Self::Skip => "SKIP",
            Self::MoveToTrash => "MOVE_TO_TRASH",
            Self::Review => "REVIEW",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "INSERT" => Self::Insert,
            "REPLACE" => Self::Replace,
            "SKIP" => Self::Skip,
            "MOVE_TO_TRASH" => Self::MoveToTrash,
            "REVIEW" => Self::Review,
            _ => return None,
        })
    }

    pub fn is_destructive(self) -> bool {
        matches!(self, Self::Replace | Self::MoveToTrash)
    }

    pub fn modifies_archive(self) -> bool {
        !matches!(self, Self::Skip | Self::Review)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemStatus {
    Pending,
    Running,
    Applied,
    Failed,
    Blocked,
    Noop,
}

impl ItemStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Applied => "applied",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Noop => "noop",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "applied" => Self::Applied,
            "failed" => Self::Failed,
            "blocked" => Self::Blocked,
            "noop" => Self::Noop,
            _ => Self::Pending,
        }
    }

    pub fn is_finished(self) -> bool {
        matches!(
            self,
            Self::Applied | Self::Failed | Self::Blocked | Self::Noop
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Running => "Running",
            Self::Applied => "Done",
            Self::Failed => "Failed",
            Self::Blocked => "Blocked",
            Self::Noop => "No action",
        }
    }
}

/// One operation in the modification plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanItem {
    #[serde(default)]
    pub id: i64,
    pub session_id: i64,
    pub group_id: Option<i64>,
    pub op: OpType,
    pub source_path: Option<String>,
    pub dest_path: Option<String>,
    pub existing_path: Option<String>,
    pub preferred_path: Option<String>,
    pub reason: String,
    /// Free-form execution detail (e.g. trash destination, conflict rename).
    pub detail: Option<String>,
    pub status: ItemStatus,
    /// The actual path written/created, when different from `dest_path`.
    pub result_path: Option<String>,
    pub error: Option<String>,
    /// Machine-readable execution recovery state (e.g. the chosen trash path).
    #[serde(default)]
    pub exec_meta: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl PlanItem {
    pub fn short_label(&self) -> String {
        match self.op {
            OpType::Insert => "Add photograph".into(),
            OpType::Replace => "Replace archive photo".into(),
            OpType::Skip => "Skip redundant import".into(),
            OpType::MoveToTrash => "Move duplicate to trash".into(),
            OpType::Review => "Manual decision needed".into(),
        }
    }

    pub fn title(&self) -> String {
        let path = self
            .source_path
            .as_deref()
            .or(self.existing_path.as_deref())
            .unwrap_or("—");
        format!("{} · {}", self.op.as_str(), file_name_of(path))
    }
}

pub fn file_name_of(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

// ---------------------------------------------------------------------------
// Recommendation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreFactor {
    pub points: f32,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhotoScore {
    pub path: String,
    pub total: f32,
    pub factors: Vec<ScoreFactor>,
}

impl PhotoScore {
    pub fn explanation(&self) -> String {
        self.factors
            .iter()
            .map(|f| format!("{:+.1} — {}", f.points, f.text))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ---------------------------------------------------------------------------
// Progress / job control
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Progress {
    pub phase: String,
    pub total: u64,
    pub completed: u64,
    pub failed: u64,
    pub reused: u64,
    pub current: String,
    pub elapsed_secs: f64,
}

impl Progress {
    pub fn new(phase: impl Into<String>) -> Self {
        Self {
            phase: phase.into(),
            total: 0,
            completed: 0,
            failed: 0,
            reused: 0,
            current: String::new(),
            elapsed_secs: 0.0,
        }
    }

    pub fn hashed(&self) -> u64 {
        self.completed.saturating_sub(self.reused)
    }

    pub fn rate_per_sec(&self) -> f64 {
        if self.elapsed_secs <= 0.05 {
            return 0.0;
        }
        self.hashed() as f64 / self.elapsed_secs
    }

    pub fn eta_secs(&self) -> Option<f64> {
        let rate = self.rate_per_sec();
        if rate <= 0.0 {
            return None;
        }
        let remaining = self.total.saturating_sub(self.completed);
        if remaining == 0 {
            return Some(0.0);
        }
        Some(remaining as f64 / rate)
    }
}

pub fn format_duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "—".into();
    }
    let secs = secs.round() as u64;
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Thread-safe pause/stop token shared by the GUI and worker jobs.
#[derive(Debug, Default)]
pub struct JobControl {
    paused: AtomicBool,
    stopped: AtomicBool,
}

impl JobControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Blocks while paused. Returns `false` when the job must abort.
    pub fn checkpoint(&self) -> bool {
        loop {
            if self.is_stopped() {
                return false;
            }
            if !self.is_paused() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(80));
        }
    }
}

// ---------------------------------------------------------------------------
// Outcome summaries
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportSummary {
    #[serde(default)]
    pub stopped: bool,
    pub session_id: i64,
    pub input_total: u64,
    pub archive_total: u64,
    pub indexed_new: u64,
    pub reused: u64,
    pub failed: u64,
    pub new_photos: u64,
    pub exact_duplicates: u64,
    pub probable_duplicates: u64,
    pub ambiguous: u64,
    pub groups: u64,
    pub review_groups: u64,
    pub plan_inserts: u64,
    pub plan_replaces: u64,
    pub plan_skips: u64,
    pub plan_trash: u64,
    pub plan_reviews: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnalysisSummary {
    #[serde(default)]
    pub stopped: bool,
    pub session_id: i64,
    pub indexed_total: u64,
    pub exact_groups: u64,
    pub perceptual_groups: u64,
    pub photos_in_groups: u64,
    pub review_groups: u64,
    pub plan_trash: u64,
    pub plan_reviews: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApplyReport {
    #[serde(default)]
    pub stopped: bool,
    pub applied: u64,
    pub skipped: u64,
    pub failed: u64,
    pub blocked: u64,
    pub inserted: u64,
    pub replaced: u64,
    pub trashed: u64,
    pub conflicts_resolved: u64,
    pub elapsed_secs: f64,
    pub problems: Vec<String>,
}

impl ApplyReport {
    pub fn has_problems(&self) -> bool {
        self.failed > 0 || self.blocked > 0 || !self.problems.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similarity_conversion_is_documented_and_roundtrips() {
        assert_eq!(distance_for_similarity(100.0), 0);
        assert_eq!(distance_for_similarity(95.0), 3);
        assert_eq!(distance_for_similarity(70.0), 19);
        assert!((similarity_for_distance(3) - 95.24).abs() < 0.1);
        assert!((similarity_for_distance(19) - 69.84).abs() < 0.1);
    }

    #[test]
    fn year_comes_from_taken_on() {
        let mut p = PhotoRecord {
            id: 0,
            path: "/a/b/x.jpg".into(),
            root: RootKind::Archive,
            file_size: 1,
            mtime_ns: 0,
            format: ImageFormatKind::Jpeg,
            width: 10,
            height: 10,
            pixel_hash: None,
            phash: None,
            taken_on: Some("2007-05-03".into()),
            taken_at_ns: None,
            taken_source: DateSource::ExifOriginal,
            orientation: 1,
            is_animated: false,
            exif: ExifInfo::default(),
            status: PhotoStatus::Indexed,
            error: None,
            indexed_at: 0,
        };
        assert_eq!(p.year(), "2007");
        p.taken_on = None;
        assert_eq!(p.year(), "Unknown");
        assert_eq!(p.filename(), "x.jpg");
    }

    #[test]
    fn human_bytes_formats() {
        assert_eq!(human_bytes(900), "900 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
    }

    #[test]
    fn job_control_stops() {
        let c = JobControl::new();
        assert!(c.checkpoint());
        c.stop();
        assert!(!c.checkpoint());
    }
}
