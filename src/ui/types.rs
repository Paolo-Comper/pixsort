//! UI-only state types.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;

use crate::config::Settings;
use crate::db::{PlanCounts, PlanDb, PlanSession, SessionStatus};
use crate::jobs::{JobHandle, JobKind};
use crate::model::{DuplicateGroup, GroupKind, PhotoRecord, PlanItem};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Dashboard,
    Import,
    Analysis,
    Review,
    Plan,
    Settings,
}

impl Page {
    pub(crate) const ALL: [Page; 6] = [
        Page::Dashboard,
        Page::Import,
        Page::Analysis,
        Page::Review,
        Page::Plan,
        Page::Settings,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Page::Dashboard => "Dashboard",
            Page::Import => "Import",
            Page::Analysis => "Archive analysis",
            Page::Review => "Manual review",
            Page::Plan => "Planned modifications",
            Page::Settings => "Settings",
        }
    }

    pub(crate) fn short_label(self) -> &'static str {
        match self {
            Page::Dashboard => "Overview",
            Page::Import => "Import",
            Page::Analysis => "Analyse archive",
            Page::Review => "Review duplicates",
            Page::Plan => "Plan & apply",
            Page::Settings => "Settings",
        }
    }
}

pub(crate) struct PlanView {
    pub session: PlanSession,
    pub groups: Vec<DuplicateGroup>,
    pub members: HashMap<i64, Vec<PhotoRecord>>,
    pub items: Vec<PlanItem>,
    pub counts: PlanCounts,
}

impl PlanView {
    pub(crate) fn load(settings: &Settings) -> Result<Option<Self>> {
        let Some(path) = settings.effective_plan_db() else {
            return Ok(None);
        };
        if !path.is_file() {
            return Ok(None);
        }
        let db = PlanDb::open_read_only(&path)?;
        let Some(session) = db.current_session()? else {
            return Ok(None);
        };
        let groups = db.load_groups(session.id)?;
        let mut members: HashMap<i64, Vec<PhotoRecord>> = HashMap::new();
        for (group_id, photo) in db.load_members_for_session(session.id)? {
            members.entry(group_id).or_default().push(photo);
        }
        let items = db.load_items(session.id)?;
        let counts = db.counts(session.id)?;
        Ok(Some(Self {
            session,
            groups,
            members,
            items,
            counts,
        }))
    }

    pub(crate) fn members_of(&self, group_id: i64) -> &[PhotoRecord] {
        self.members
            .get(&group_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub(crate) fn duplicate_groups(&self) -> impl Iterator<Item = &DuplicateGroup> {
        self.groups.iter().filter(|g| g.kind != GroupKind::Single)
    }

    /// Operations that are not finished: pending, running, blocked or failed
    /// (failed operations are retried by the next Apply).
    pub(crate) fn unfinished_operations(&self) -> u64 {
        self.counts.pending + self.counts.running + self.counts.blocked + self.counts.failed
    }

    pub(crate) fn unresolved_reviews(&self) -> u64 {
        self.duplicate_groups()
            .filter(|g| g.needs_decision())
            .count() as u64
    }

    pub(crate) fn review_items(&self) -> u64 {
        self.items
            .iter()
            .filter(|i| {
                i.op == crate::model::OpType::Review
                    && !matches!(
                        i.status,
                        crate::model::ItemStatus::Applied | crate::model::ItemStatus::Noop
                    )
            })
            .count() as u64
    }

    pub(crate) fn is_active(&self) -> bool {
        matches!(
            self.session.status,
            SessionStatus::Active | SessionStatus::Applying
        )
    }

    pub(crate) fn keeper_path<'a>(&self, group: &'a DuplicateGroup) -> Option<&'a str> {
        group
            .keeper_path
            .as_deref()
            .or(group.recommended_path.as_deref())
    }

    pub(crate) fn group_title(&self, group: &DuplicateGroup) -> String {
        let members = self.members_of(group.id);
        let keeper = self
            .keeper_path(group)
            .and_then(|path| members.iter().find(|m| m.path == path))
            .or_else(|| members.first());
        let name = keeper
            .map(|m| m.filename().to_string())
            .unwrap_or_else(|| "group".into());
        match group.kind {
            GroupKind::Exact => format!("Exact duplicate · {name}"),
            GroupKind::Perceptual => format!(
                "{:.0}% similar · {name} ({} files)",
                group.similarity,
                members.len()
            ),
            GroupKind::Single => format!("New · {name}"),
        }
    }
}

pub(crate) struct RunningJob {
    pub handle: JobHandle,
}

impl RunningJob {
    pub(crate) fn new(handle: JobHandle) -> Self {
        Self { handle }
    }

    pub(crate) fn kind(&self) -> JobKind {
        self.handle.kind
    }
}

pub(crate) struct Toast {
    pub message: String,
    pub error: bool,
    pub created: Instant,
}

impl Toast {
    pub(crate) fn info(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            error: false,
            created: Instant::now(),
        }
    }
    pub(crate) fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            error: true,
            created: Instant::now(),
        }
    }
}
