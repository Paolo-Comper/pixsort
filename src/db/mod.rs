//! SQLite persistence: the operational image index (Database A) and the
//! persistent modification plan (Database B).

pub mod index_db;
pub mod plan_db;

pub use index_db::{CachedFile, IndexDb, IndexStats, PendingFile};
pub use plan_db::{
    NewGroup, NewPlanItem, PlanCounts, PlanDb, PlanSession, SessionMode, SessionStatus,
};
