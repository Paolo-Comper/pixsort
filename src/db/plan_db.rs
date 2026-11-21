//! Database B — the persistent modification plan.
//!
//! The plan is the desired final state of the archive, expressed as
//! group decisions plus the operations derived from them. It survives
//! application restarts, and every decision is recorded.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags};

use crate::db::index_db::now_ns;
use crate::model::{
    DuplicateGroup, GroupDecision, GroupKind, GroupState, ItemStatus, OpType, PhotoRecord, PlanItem,
};

pub const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMode {
    Import,
    Analysis,
}

impl SessionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::Analysis => "analysis",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "analysis" => Self::Analysis,
            _ => Self::Import,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Import => "Import",
            Self::Analysis => "Archive analysis",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Active,
    Applying,
    Applied,
    Superseded,
    Discarded,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Applying => "applying",
            Self::Applied => "applied",
            Self::Superseded => "superseded",
            Self::Discarded => "discarded",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "applying" => Self::Applying,
            "applied" => Self::Applied,
            "superseded" => Self::Superseded,
            "discarded" => Self::Discarded,
            _ => Self::Active,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Applying => "Applying",
            Self::Applied => "Applied",
            Self::Superseded => "Superseded",
            Self::Discarded => "Discarded",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlanSession {
    pub id: i64,
    pub mode: SessionMode,
    pub input_dir: String,
    pub dest_dir: String,
    pub status: SessionStatus,
    pub created_at: i64,
    pub updated_at: i64,
    pub plan_dirty: bool,
}

/// A group ready to be stored. Members are full index records.
#[derive(Debug, Clone)]
pub struct NewGroup {
    pub kind: GroupKind,
    pub similarity: f32,
    pub distance: u32,
    pub state: GroupState,
    pub decision: GroupDecision,
    pub keeper_path: Option<String>,
    pub recommended_path: Option<String>,
    pub explanation: String,
    pub title: String,
    pub members: Vec<PhotoRecord>,
}

/// A plan item ready to be stored.
#[derive(Debug, Clone)]
pub struct NewPlanItem {
    pub group_id: Option<i64>,
    pub op: OpType,
    pub source_path: Option<String>,
    pub dest_path: Option<String>,
    pub existing_path: Option<String>,
    pub preferred_path: Option<String>,
    pub reason: String,
    pub detail: Option<String>,
}

impl NewPlanItem {
    pub fn new(op: OpType, reason: impl Into<String>) -> Self {
        Self {
            group_id: None,
            op,
            source_path: None,
            dest_path: None,
            existing_path: None,
            preferred_path: None,
            reason: reason.into(),
            detail: None,
        }
    }

    pub fn group(mut self, id: i64) -> Self {
        self.group_id = Some(id);
        self
    }
    pub fn source(mut self, path: impl Into<String>) -> Self {
        self.source_path = Some(path.into());
        self
    }
    pub fn dest(mut self, path: impl Into<String>) -> Self {
        self.dest_path = Some(path.into());
        self
    }
    pub fn existing(mut self, path: impl Into<String>) -> Self {
        self.existing_path = Some(path.into());
        self
    }
    pub fn preferred(mut self, path: impl Into<String>) -> Self {
        self.preferred_path = Some(path.into());
        self
    }
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct PlanCounts {
    pub total: u64,
    pub pending: u64,
    pub running: u64,
    pub applied: u64,
    pub failed: u64,
    pub blocked: u64,
    pub inserts: u64,
    pub replaces: u64,
    pub skips: u64,
    pub moves_to_trash: u64,
    pub reviews: u64,
}

impl PlanCounts {
    pub fn unfinished(&self) -> u64 {
        self.pending + self.running + self.blocked
    }

    pub fn actionable(&self) -> u64 {
        self.inserts + self.replaces + self.skips + self.moves_to_trash + self.reviews
    }
}

pub struct PlanDb {
    conn: Connection,
}

impl PlanDb {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open modification plan {}", path.display()))?;
        configure(&conn)?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_read_only(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("cannot open modification plan {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(10))?;
        Ok(Self { conn })
    }

    pub fn integrity_check(&self) -> Result<()> {
        let result: String = self
            .conn
            .query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if result != "ok" {
            bail!("modification plan failed SQLite quick_check: {result}");
        }
        Ok(())
    }

    fn migrate(&mut self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                mode TEXT NOT NULL,
                input_dir TEXT NOT NULL,
                dest_dir TEXT NOT NULL,
                status TEXT NOT NULL,
                plan_dirty INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                settings_json TEXT NOT NULL DEFAULT '{}'
            );
            CREATE TABLE IF NOT EXISTS groups (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                kind TEXT NOT NULL,
                similarity REAL NOT NULL,
                distance INTEGER NOT NULL,
                state TEXT NOT NULL,
                decision TEXT NOT NULL,
                keeper_path TEXT,
                recommended_path TEXT,
                explanation TEXT NOT NULL,
                title TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS group_members (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
                path TEXT NOT NULL,
                photo_json TEXT NOT NULL,
                is_keeper INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS plan_items (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                group_id INTEGER,
                op TEXT NOT NULL,
                source_path TEXT,
                dest_path TEXT,
                existing_path TEXT,
                preferred_path TEXT,
                reason TEXT NOT NULL,
                detail TEXT,
                status TEXT NOT NULL,
                result_path TEXT,
                error TEXT,
                exec_meta TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                executed_at INTEGER
            );
            CREATE TABLE IF NOT EXISTS reviews (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                group_id INTEGER NOT NULL,
                decision TEXT NOT NULL,
                keeper_path TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_groups_session ON groups(session_id);
            CREATE INDEX IF NOT EXISTS idx_members_group ON group_members(group_id);
            CREATE INDEX IF NOT EXISTS idx_items_session ON plan_items(session_id);
            CREATE INDEX IF NOT EXISTS idx_items_status ON plan_items(status);
            "#,
        )?;
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    // -- sessions ----------------------------------------------------------

    pub fn latest_session(&self) -> Result<Option<PlanSession>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, mode, input_dir, dest_dir, status, created_at, updated_at, plan_dirty
             FROM sessions ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            Ok(Some(map_session(row)?))
        } else {
            Ok(None)
        }
    }

    /// The session the app should show: the newest active/applying one,
    /// otherwise the newest of any status (so reports remain visible).
    pub fn current_session(&self) -> Result<Option<PlanSession>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, mode, input_dir, dest_dir, status, created_at, updated_at, plan_dirty
             FROM sessions
             ORDER BY CASE status WHEN 'active' THEN 0 WHEN 'applying' THEN 1 ELSE 2 END,
                      id DESC
             LIMIT 1",
        )?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            Ok(Some(map_session(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn session(&self, id: i64) -> Result<Option<PlanSession>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, mode, input_dir, dest_dir, status, created_at, updated_at, plan_dirty
             FROM sessions WHERE id=?1",
        )?;
        let mut rows = stmt.query([id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(map_session(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn create_session(
        &mut self,
        mode: SessionMode,
        input_dir: &str,
        dest_dir: &str,
        settings_json: &str,
    ) -> Result<i64> {
        let tx = self.conn.transaction()?;
        let now = now_ns();
        tx.execute(
            "UPDATE sessions SET status='superseded', updated_at=?1 WHERE status IN ('active','applying')",
            [now],
        )?;
        tx.execute(
            "INSERT INTO sessions
             (mode, input_dir, dest_dir, status, plan_dirty, created_at, updated_at, settings_json)
             VALUES (?1, ?2, ?3, 'active', 1, ?4, ?4, ?5)",
            params![mode.as_str(), input_dir, dest_dir, now, settings_json],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    pub fn set_session_status(&self, session_id: i64, status: SessionStatus) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET status=?1, updated_at=?2 WHERE id=?3",
            params![status.as_str(), now_ns(), session_id],
        )?;
        Ok(())
    }

    pub fn set_plan_dirty(&self, session_id: i64, dirty: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET plan_dirty=?1, updated_at=?2 WHERE id=?3",
            params![dirty as i64, now_ns(), session_id],
        )?;
        Ok(())
    }

    pub fn delete_session(&mut self, session_id: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        // Explicit deletion of a session cascades to decisions and plan rows.
        tx.execute("DELETE FROM sessions WHERE id=?1", [session_id])?;
        tx.commit()?;
        Ok(())
    }

    /// True when any active session still has unfinished items.
    pub fn has_unfinished_plan(&self) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM plan_items i
             JOIN sessions s ON s.id = i.session_id
             WHERE s.status IN ('active','applying')
               AND i.status IN ('pending','running','blocked','failed')",
            [],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    // -- groups ------------------------------------------------------------

    /// Replaces all groups and members of a session.
    pub fn save_groups(
        &mut self,
        session_id: i64,
        groups: &[NewGroup],
    ) -> Result<Vec<DuplicateGroup>> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM groups WHERE session_id=?1", [session_id])?;
        let now = now_ns();
        let mut out = Vec::with_capacity(groups.len());
        for draft in groups {
            tx.execute(
                "INSERT INTO groups
                 (session_id, kind, similarity, distance, state, decision, keeper_path,
                  recommended_path, explanation, title, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
                params![
                    session_id,
                    draft.kind.as_str(),
                    draft.similarity as f64,
                    draft.distance as i64,
                    draft.state.as_str(),
                    draft.decision.as_str(),
                    draft.keeper_path,
                    draft.recommended_path,
                    draft.explanation,
                    draft.title,
                    now
                ],
            )?;
            let group_id = tx.last_insert_rowid();
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO group_members (group_id, path, photo_json, is_keeper)
                     VALUES (?1, ?2, ?3, ?4)",
                )?;
                for member in &draft.members {
                    let is_keeper = draft.keeper_path.as_deref() == Some(member.path.as_str());
                    stmt.execute(params![
                        group_id,
                        member.path,
                        serde_json::to_string(member)?,
                        is_keeper as i64
                    ])?;
                }
            }
            out.push(DuplicateGroup {
                id: group_id,
                session_id,
                kind: draft.kind,
                similarity: draft.similarity,
                distance: draft.distance,
                state: draft.state,
                decision: draft.decision,
                keeper_path: draft.keeper_path.clone(),
                recommended_path: draft.recommended_path.clone(),
                explanation: draft.explanation.clone(),
                title: draft.title.clone(),
            });
        }
        tx.execute(
            "UPDATE sessions SET plan_dirty=1, updated_at=?1 WHERE id=?2",
            params![now, session_id],
        )?;
        tx.commit()?;
        Ok(out)
    }

    pub fn load_groups(&self, session_id: i64) -> Result<Vec<DuplicateGroup>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, kind, similarity, distance, state, decision, keeper_path,
                    recommended_path, explanation, title
             FROM groups WHERE session_id=?1
             ORDER BY CASE state WHEN 'needs_review' THEN 0 ELSE 1 END, id",
        )?;
        let rows = stmt.query_map([session_id], |row| {
            Ok(DuplicateGroup {
                id: row.get(0)?,
                session_id: row.get(1)?,
                kind: GroupKind::from_str(&row.get::<_, String>(2)?).unwrap_or(GroupKind::Exact),
                similarity: row.get::<_, f64>(3)? as f32,
                distance: row.get::<_, i64>(4)? as u32,
                state: GroupState::from_str(&row.get::<_, String>(5)?),
                decision: GroupDecision::from_str(&row.get::<_, String>(6)?),
                keeper_path: row.get(7)?,
                recommended_path: row.get(8)?,
                explanation: row.get(9)?,
                title: row.get(10)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn load_group(&self, group_id: i64) -> Result<Option<DuplicateGroup>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, kind, similarity, distance, state, decision, keeper_path,
                    recommended_path, explanation, title
             FROM groups WHERE id=?1",
        )?;
        let mut rows = stmt.query([group_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(DuplicateGroup {
                id: row.get(0)?,
                session_id: row.get(1)?,
                kind: GroupKind::from_str(&row.get::<_, String>(2)?).unwrap_or(GroupKind::Exact),
                similarity: row.get::<_, f64>(3)? as f32,
                distance: row.get::<_, i64>(4)? as u32,
                state: GroupState::from_str(&row.get::<_, String>(5)?),
                decision: GroupDecision::from_str(&row.get::<_, String>(6)?),
                keeper_path: row.get(7)?,
                recommended_path: row.get(8)?,
                explanation: row.get(9)?,
                title: row.get(10)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn load_members(&self, group_id: i64) -> Result<Vec<PhotoRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT photo_json FROM group_members WHERE group_id=?1 ORDER BY id")?;
        let rows = stmt.query_map([group_id], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let json = row?;
            let photo: PhotoRecord = serde_json::from_str(&json).unwrap_or_else(|_| {
                // A corrupt member row must not take down the whole UI.
                PhotoRecord {
                    id: 0,
                    path: "<corrupt group member>".into(),
                    root: crate::model::RootKind::Archive,
                    file_size: 0,
                    mtime_ns: 0,
                    format: crate::model::ImageFormatKind::Jpeg,
                    width: 0,
                    height: 0,
                    pixel_hash: None,
                    phash: None,
                    taken_on: None,
                    taken_at_ns: None,
                    taken_source: crate::model::DateSource::Unknown,
                    orientation: 1,
                    is_animated: false,
                    exif: Default::default(),
                    status: crate::model::PhotoStatus::Failed,
                    error: Some("corrupt group member record".into()),
                    indexed_at: 0,
                }
            });
            out.push(photo);
        }
        Ok(out)
    }

    pub fn load_members_for_session(&self, session_id: i64) -> Result<Vec<(i64, PhotoRecord)>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.group_id, m.photo_json FROM group_members m
             JOIN groups g ON g.id = m.group_id
             WHERE g.session_id=?1 ORDER BY m.group_id, m.id",
        )?;
        let rows = stmt.query_map([session_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (group_id, json) = row?;
            if let Ok(photo) = serde_json::from_str::<PhotoRecord>(&json) {
                out.push((group_id, photo));
            }
        }
        Ok(out)
    }

    pub fn set_decision(
        &mut self,
        group_id: i64,
        decision: GroupDecision,
        keeper_path: Option<&str>,
        state: GroupState,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        let session_id: i64 = tx.query_row(
            "SELECT session_id FROM groups WHERE id=?1",
            [group_id],
            |r| r.get(0),
        )?;
        tx.execute(
            "UPDATE groups SET decision=?1, keeper_path=?2, state=?3, updated_at=?4 WHERE id=?5",
            params![
                decision.as_str(),
                keeper_path,
                state.as_str(),
                now_ns(),
                group_id
            ],
        )?;
        tx.execute(
            "UPDATE group_members SET is_keeper = CASE WHEN path=?1 THEN 1 ELSE 0 END
             WHERE group_id=?2",
            params![keeper_path, group_id],
        )?;
        tx.execute(
            "INSERT INTO reviews (session_id, group_id, decision, keeper_path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session_id,
                group_id,
                decision.as_str(),
                keeper_path,
                now_ns()
            ],
        )?;
        tx.execute(
            "UPDATE sessions SET plan_dirty=1, updated_at=?1 WHERE id=?2",
            params![now_ns(), session_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    // -- plan items --------------------------------------------------------

    pub fn replace_pending_items(&mut self, session_id: i64, items: &[NewPlanItem]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM plan_items WHERE session_id=?1 AND status IN ('pending','blocked')",
            [session_id],
        )?;
        let now = now_ns();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO plan_items
                 (session_id, group_id, op, source_path, dest_path, existing_path,
                  preferred_path, reason, detail, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', ?10, ?10)",
            )?;
            for item in items {
                stmt.execute(params![
                    session_id,
                    item.group_id,
                    item.op.as_str(),
                    item.source_path,
                    item.dest_path,
                    item.existing_path,
                    item.preferred_path,
                    item.reason,
                    item.detail,
                    now
                ])?;
            }
        }
        tx.execute(
            "UPDATE sessions SET plan_dirty=0, updated_at=?1 WHERE id=?2",
            params![now, session_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_items(&self, session_id: i64) -> Result<Vec<PlanItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, group_id, op, source_path, dest_path, existing_path,
                    preferred_path, reason, detail, status, result_path, error, created_at,
                    updated_at, exec_meta
             FROM plan_items WHERE session_id=?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([session_id], map_item)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn load_pending_items(&self, session_id: i64) -> Result<Vec<PlanItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, group_id, op, source_path, dest_path, existing_path,
                    preferred_path, reason, detail, status, result_path, error, created_at,
                    updated_at, exec_meta
             FROM plan_items WHERE session_id=?1 AND status IN ('pending','running','blocked','failed')
             ORDER BY CASE op WHEN 'MOVE_TO_TRASH' THEN 0 WHEN 'REPLACE' THEN 1
                              WHEN 'INSERT' THEN 2 ELSE 3 END, id",
        )?;
        let rows = stmt.query_map([session_id], map_item)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn get_item(&self, id: i64) -> Result<Option<PlanItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, group_id, op, source_path, dest_path, existing_path,
                    preferred_path, reason, detail, status, result_path, error, created_at,
                    updated_at, exec_meta
             FROM plan_items WHERE id=?1",
        )?;
        let mut rows = stmt.query([id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(map_item(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn start_item(&self, id: i64, detail: Option<&str>, exec_meta: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE plan_items SET status='running', detail=?1, exec_meta=?2, updated_at=?3,
             error=NULL, result_path=NULL
             WHERE id=?4",
            params![detail, exec_meta, now_ns(), id],
        )?;
        Ok(())
    }

    pub fn finish_item(
        &self,
        id: i64,
        status: ItemStatus,
        result_path: Option<&str>,
        detail: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE plan_items SET status=?1, result_path=?2, detail=?3, error=?4,
             updated_at=?5, executed_at=?5 WHERE id=?6",
            params![status.as_str(), result_path, detail, error, now_ns(), id],
        )?;
        Ok(())
    }

    pub fn counts(&self, session_id: i64) -> Result<PlanCounts> {
        let mut counts = PlanCounts::default();
        {
            let mut stmt = self.conn.prepare(
                "SELECT op, status, COUNT(*) FROM plan_items WHERE session_id=?1
                 GROUP BY op, status",
            )?;
            let rows = stmt.query_map([session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? as u64,
                ))
            })?;
            for row in rows {
                let (op, status, count) = row?;
                counts.total += count;
                match op.as_str() {
                    "INSERT" => counts.inserts += count,
                    "REPLACE" => counts.replaces += count,
                    "SKIP" => counts.skips += count,
                    "MOVE_TO_TRASH" => counts.moves_to_trash += count,
                    "REVIEW" => counts.reviews += count,
                    _ => {}
                }
                match status.as_str() {
                    "pending" => counts.pending += count,
                    "running" => counts.running += count,
                    "applied" => counts.applied += count,
                    "failed" => counts.failed += count,
                    "blocked" => counts.blocked += count,
                    _ => {}
                }
            }
        }
        Ok(counts)
    }

    pub fn pending_review_count(&self, session_id: i64) -> Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM plan_items WHERE session_id=?1 AND op='REVIEW'
             AND status IN ('pending','running','blocked')",
            [session_id],
            |r| r.get(0),
        )?;
        Ok(count as u64)
    }

    pub fn unresolved_group_count(&self, session_id: i64) -> Result<u64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM groups WHERE session_id=?1 AND state='needs_review'",
            [session_id],
            |r| r.get(0),
        )?;
        Ok(count as u64)
    }

    /// Approves no filesystem change; simply reports whether execution can start.
    pub fn check_ready_to_apply(&self, session_id: i64) -> Result<()> {
        let unresolved = self.unresolved_group_count(session_id)?;
        if unresolved > 0 {
            bail!(
                "{unresolved} duplicate group(s) still need a manual decision. \
                 Resolve them before applying the plan."
            );
        }
        let reviews = self.pending_review_count(session_id)?;
        if reviews > 0 {
            bail!("{reviews} REVIEW operation(s) remain. Resolve them before applying.");
        }
        Ok(())
    }

    pub fn recent_sessions(&self, limit: usize) -> Result<Vec<PlanSession>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, mode, input_dir, dest_dir, status, created_at, updated_at, plan_dirty
             FROM sessions ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], map_session)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // FULL is deliberate: losing a committed row here can lose track of a
    // filesystem operation that already happened.
    conn.pragma_update(None, "synchronous", "FULL")?;
    let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    Ok(())
}

fn map_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<PlanSession> {
    Ok(PlanSession {
        id: row.get(0)?,
        mode: SessionMode::from_str(&row.get::<_, String>(1)?),
        input_dir: row.get(2)?,
        dest_dir: row.get(3)?,
        status: SessionStatus::from_str(&row.get::<_, String>(4)?),
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
        plan_dirty: row.get::<_, i64>(7)? != 0,
    })
}

fn map_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<PlanItem> {
    Ok(PlanItem {
        id: row.get(0)?,
        session_id: row.get(1)?,
        group_id: row.get(2)?,
        op: OpType::from_str(&row.get::<_, String>(3)?).unwrap_or(OpType::Review),
        source_path: row.get(4)?,
        dest_path: row.get(5)?,
        existing_path: row.get(6)?,
        preferred_path: row.get(7)?,
        reason: row.get(8)?,
        detail: row.get(9)?,
        status: ItemStatus::from_str(&row.get::<_, String>(10)?),
        result_path: row.get(11)?,
        error: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
        exec_meta: row.get(15)?,
    })
}

/// Convenience for the rare callers that want to check whether a plan file
/// exists before opening it.
pub fn plan_exists(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DateSource, ExifInfo, ImageFormatKind, PhotoStatus, RootKind};

    fn member(path: &str) -> PhotoRecord {
        PhotoRecord {
            id: 0,
            path: path.into(),
            root: RootKind::Archive,
            file_size: 10,
            mtime_ns: 0,
            format: ImageFormatKind::Jpeg,
            width: 100,
            height: 100,
            pixel_hash: Some("aaaa".into()),
            phash: Some(1),
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

    #[test]
    fn sessions_groups_items_persist() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = PlanDb::open(&dir.path().join("plan.sqlite")).unwrap();
        let id = db
            .create_session(SessionMode::Import, "/in", "/archive", "{}")
            .unwrap();
        let groups = db
            .save_groups(
                id,
                &[NewGroup {
                    kind: GroupKind::Exact,
                    similarity: 100.0,
                    distance: 0,
                    state: GroupState::Auto,
                    decision: GroupDecision::None,
                    keeper_path: Some("/archive/a.jpg".into()),
                    recommended_path: Some("/archive/a.jpg".into()),
                    explanation: "validated".into(),
                    title: "Group".into(),
                    members: vec![member("/archive/a.jpg"), member("/in/b.jpg")],
                }],
            )
            .unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(db.load_members(groups[0].id).unwrap().len(), 2);
        db.replace_pending_items(
            id,
            &[NewPlanItem::new(OpType::Skip, "duplicate")
                .source("/in/b.jpg")
                .preferred("/archive/a.jpg")
                .group(groups[0].id)],
        )
        .unwrap();
        let counts = db.counts(id).unwrap();
        assert_eq!(counts.skips, 1);
        assert!(!db.session(id).unwrap().unwrap().plan_dirty);
        db.set_decision(
            groups[0].id,
            GroupDecision::Keep,
            Some("/in/b.jpg"),
            GroupState::Resolved,
        )
        .unwrap();
        assert!(db.session(id).unwrap().unwrap().plan_dirty);
        db.integrity_check().unwrap();
    }

    #[test]
    fn corrupt_plan_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan.sqlite");
        std::fs::write(&path, b"not a database at all").unwrap();
        assert!(PlanDb::open(&path).is_err());
        let read_only = PlanDb::open_read_only(&path).unwrap();
        assert!(read_only.integrity_check().is_err());
        assert!(read_only.current_session().is_err());
    }

    #[test]
    fn latest_session_is_newest() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = PlanDb::open(&dir.path().join("plan.sqlite")).unwrap();
        let a = db
            .create_session(SessionMode::Import, "/in", "/archive", "{}")
            .unwrap();
        let b = db
            .create_session(SessionMode::Analysis, "/archive", "/archive", "{}")
            .unwrap();
        assert_eq!(db.current_session().unwrap().unwrap().id, b);
        assert_eq!(
            db.session(a).unwrap().unwrap().status,
            SessionStatus::Superseded
        );
        assert!(db.latest_session().unwrap().is_some());
    }
}
