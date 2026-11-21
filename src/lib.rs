//! Pixsort — a local, fast photo archive organizer and deduplicator.
//!
//! The crate is split into small modules with a deliberate dependency flow:
//! `imaging` → `db` → `scan`/`compare`/`recommend`/`planner` →
//! `import`/`analysis` → `executor` → `ui`.

pub mod analysis;
pub mod compare;
pub mod config;
pub mod db;
pub mod dirpicker;
pub mod executor;
pub mod fsops;
pub mod imaging;
pub mod import;
pub mod jobs;
pub mod model;
pub mod planner;
pub mod recommend;
pub mod scan;
pub mod ui;
