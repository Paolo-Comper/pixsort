use eframe::egui::{self, RichText, Ui};

use crate::config::Settings;
use crate::model::{ApplyReport, ItemStatus, Progress};

use super::super::types::{PlanView, RunningJob};
use super::super::widgets::{self, ACCENT, BAD, GOOD, MUTED, WARN};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ItemFilter {
    All,
    Pending,
    Done,
    Problems,
}

pub(crate) struct PlanUiState {
    pub filter: ItemFilter,
}

impl Default for PlanUiState {
    fn default() -> Self {
        Self {
            filter: ItemFilter::All,
        }
    }
}

pub(crate) enum PlanAction {
    ApplyRequested,
    ReviewRequested,
    Refresh,
    DiscardRequested,
    Progress(widgets::ProgressAction),
}

pub(crate) fn show(
    plan: Option<&PlanView>,
    settings: &Settings,
    last_apply: Option<&ApplyReport>,
    progress: Option<&Progress>,
    job: Option<&RunningJob>,
    state: &mut PlanUiState,
    ui: &mut Ui,
) -> Option<PlanAction> {
    widgets::page_title(
        ui,
        "Planned modifications",
        "This is the desired final archive state. Nothing is touched until Apply is confirmed.",
    );

    let Some(plan) = plan else {
        ui.label(
            RichText::new("No modification plan yet. Start an import or analyse the archive.")
                .color(MUTED),
        );
        return None;
    };

    let mut action = None;
    egui::ScrollArea::vertical()
        .id_salt("plan-page")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            plan_contents(
                plan,
                settings,
                last_apply,
                progress,
                job,
                state,
                ui,
                &mut action,
            );
        });
    action
}

#[allow(clippy::too_many_arguments)]
fn plan_contents(
    plan: &PlanView,
    settings: &Settings,
    last_apply: Option<&ApplyReport>,
    progress: Option<&Progress>,
    job: Option<&RunningJob>,
    state: &mut PlanUiState,
    ui: &mut Ui,
    action: &mut Option<PlanAction>,
) {
    let legacy_grouping = plan.is_legacy_grouping();
    let unresolved_groups = plan.unresolved_reviews();
    let review_items = plan.review_items();
    let has_work = plan.counts.pending + plan.counts.blocked + plan.counts.failed > 0;
    let can_apply = job.is_none()
        && plan.is_active()
        && !legacy_grouping
        && unresolved_groups == 0
        && review_items == 0
        && has_work;

    if legacy_grouping {
        egui::Frame::new()
            .fill(BAD.gamma_multiply(0.15))
            .stroke(egui::Stroke::new(1.0, BAD))
            .corner_radius(6)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.label(
                    RichText::new(
                        "This session was created with the old unbounded grouping algorithm. \
                         Its groups may contain unrelated photographs, so applying it is disabled. \
                         Discard this session below and run Archive analysis again.",
                    )
                    .color(BAD),
                );
            });
        ui.add_space(6.0);
    }

    ui.horizontal_wrapped(|ui| {
        widgets::stat_card(
            ui,
            "Insert",
            plan.counts.inserts.to_string(),
            "new archive files",
        );
        widgets::stat_card(
            ui,
            "Replace",
            plan.counts.replaces.to_string(),
            "old version → trash",
        );
        widgets::stat_card(
            ui,
            "Skip",
            plan.counts.skips.to_string(),
            "redundant imports",
        );
        widgets::stat_card(
            ui,
            "Move to trash",
            plan.counts.moves_to_trash.to_string(),
            "never deleted",
        );
        widgets::stat_card(
            ui,
            "Review",
            plan.counts.reviews.to_string(),
            "manual decisions",
        );
    });

    ui.add_space(4.0);
    egui::Frame::group(ui.style())
        .inner_margin(10)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!(
                        "{} · {} · {}",
                        plan.session.mode.label(),
                        plan.session.status.label(),
                        plan.session.dest_dir
                    ))
                    .strong(),
                );
                if plan.counts.applied > 0 {
                    widgets::badge(ui, &format!("{} applied", plan.counts.applied), GOOD);
                }
                if plan.counts.failed > 0 {
                    widgets::badge(ui, &format!("{} failed", plan.counts.failed), BAD);
                }
                if plan.counts.blocked > 0 {
                    widgets::badge(ui, &format!("{} blocked", plan.counts.blocked), WARN);
                }
            });
            if let Some(trash) = settings.effective_trash_dir() {
                ui.label(
                    RichText::new(format!("Trash directory: {}", trash.display()))
                        .color(MUTED)
                        .size(12.0),
                );
            }
        });

    if unresolved_groups > 0 {
        ui.add_space(6.0);
        egui::Frame::new()
            .fill(WARN.gamma_multiply(0.12))
            .stroke(egui::Stroke::new(1.0, WARN))
            .corner_radius(6)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.label(
                    RichText::new(format!(
                        "{unresolved_groups} duplicate group(s) still need a manual decision. The plan cannot be applied yet."
                    ))
                    .color(WARN),
                );
                if ui.button("Review now").clicked() {
                    *action = Some(PlanAction::ReviewRequested);
                }
            });
    }

    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        let apply_label = if plan.counts.failed > 0 {
            "Retry failed / apply remaining…"
        } else {
            "Apply plan…"
        };
        if ui
            .add_enabled(can_apply, egui::Button::new(apply_label))
            .on_disabled_hover_text(if legacy_grouping {
                "This session was created by an older grouping algorithm; discard and rerun analysis."
            } else if unresolved_groups > 0 || review_items > 0 {
                "Resolve all review groups first."
            } else if !has_work {
                "There is nothing left to apply."
            } else {
                "A background job is running."
            })
            .clicked()
        {
            *action = Some(PlanAction::ApplyRequested);
        }
        if ui.button("Refresh").clicked() {
            *action = Some(PlanAction::Refresh);
        }
        if ui.button("Review matches").clicked() {
            *action = Some(PlanAction::ReviewRequested);
        }
        if ui
            .add_enabled(
                plan.is_active() && job.is_none(),
                egui::Button::new("Discard session…"),
            )
            .clicked()
        {
            *action = Some(PlanAction::DiscardRequested);
        }
        if let Some(job) = job {
            ui.label(RichText::new(format!("{} running…", job.kind().label())).color(WARN));
        }
    });

    if let Some(progress) = progress {
        ui.add_space(8.0);
        let progress_action = widgets::progress_panel(ui, progress, job);
        if !matches!(progress_action, widgets::ProgressAction::None) {
            *action = Some(PlanAction::Progress(progress_action));
        }
    }

    if let Some(report) = last_apply {
        ui.add_space(8.0);
        show_report(ui, report);
    }

    ui.add_space(8.0);
    widgets::section_title(ui, "Final state by group");
    egui::CollapsingHeader::new(format!(
        "{} duplicate group(s) — click to inspect keepers",
        plan.duplicate_groups().count()
    ))
    .default_open(false)
    .show(ui, |ui| {
        egui::ScrollArea::vertical()
            .max_height(240.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for group in plan.duplicate_groups() {
                    let members = plan.members_of(group.id);
                    let keeper = plan
                        .keeper_path(group)
                        .and_then(|path| members.iter().find(|m| m.path == path));
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "{} · {}",
                                group.kind.label(),
                                group.state.label()
                            ))
                            .color(MUTED)
                            .size(12.0),
                        );
                        if group.needs_decision() {
                            widgets::badge(ui, "unresolved", WARN);
                        }
                        if group.needs_decision() {
                            if let Some(keeper) = keeper {
                                ui.label(
                                    RichText::new(format!(
                                        "{} files · recommended {} · not decided yet",
                                        members.len(),
                                        keeper.filename()
                                    ))
                                    .size(12.0),
                                );
                            }
                        } else if let Some(keeper) = keeper {
                            ui.label(
                                RichText::new(format!(
                                    "keep {} · {} other file(s) will not remain",
                                    keeper.filename(),
                                    members.len().saturating_sub(1)
                                ))
                                .size(12.0),
                            );
                        } else if group.state == crate::model::GroupState::Ignored {
                            ui.label(
                                RichText::new("keep every version (group ignored)")
                                    .size(12.0)
                                    .color(MUTED),
                            );
                        }
                    });
                }
            });
    });

    ui.add_space(8.0);
    widgets::section_title(ui, "Operations");
    ui.horizontal_wrapped(|ui| {
        for (filter, label) in [
            (ItemFilter::All, "All"),
            (ItemFilter::Pending, "Pending"),
            (ItemFilter::Done, "Done"),
            (ItemFilter::Problems, "Problems"),
        ] {
            if ui.selectable_label(state.filter == filter, label).clicked() {
                state.filter = filter;
            }
        }
        ui.separator();
        ui.label(
            RichText::new(format!(
                "{} pending · {} applied · {} failed · {} blocked",
                plan.counts.pending, plan.counts.applied, plan.counts.failed, plan.counts.blocked
            ))
            .color(MUTED)
            .size(12.0),
        );
    });

    let items: Vec<_> = plan
        .items
        .iter()
        .filter(|item| match state.filter {
            ItemFilter::All => true,
            ItemFilter::Pending => matches!(
                item.status,
                ItemStatus::Pending | ItemStatus::Running | ItemStatus::Blocked
            ),
            ItemFilter::Done => {
                matches!(item.status, ItemStatus::Applied | ItemStatus::Noop)
            }
            ItemFilter::Problems => {
                matches!(item.status, ItemStatus::Failed | ItemStatus::Blocked)
            }
        })
        .collect();

    if items.is_empty() {
        ui.label(RichText::new("No operations in this filter.").color(MUTED));
    }
    for item in items {
        let color = match item.status {
            ItemStatus::Failed | ItemStatus::Blocked => BAD,
            ItemStatus::Applied => GOOD,
            ItemStatus::Noop => MUTED,
            _ => ACCENT,
        };
        egui::Frame::new()
            .fill(color.gamma_multiply(0.06))
            .corner_radius(6)
            .inner_margin(8)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    widgets::badge(ui, item.op.as_str(), color);
                    ui.label(RichText::new(item.short_label()).strong());
                    ui.label(
                        RichText::new(format!(
                            "{} {}",
                            item.source_path
                                .as_deref()
                                .map(crate::model::file_name_of)
                                .unwrap_or("—"),
                            item.dest_path
                                .as_deref()
                                .map(|d| format!("→ {}", d))
                                .unwrap_or_default()
                        ))
                        .size(12.0)
                        .color(MUTED),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        widgets::badge(ui, item.status.label(), color);
                    });
                });
                ui.label(RichText::new(&item.reason).size(12.0).color(MUTED));
                egui::CollapsingHeader::new("Details")
                    .default_open(false)
                    .show(ui, |ui| {
                        if let Some(path) = &item.source_path {
                            ui.label(
                                RichText::new(format!("Source: {path}"))
                                    .monospace()
                                    .size(11.5),
                            );
                        }
                        if let Some(path) = &item.dest_path {
                            ui.label(
                                RichText::new(format!("Destination: {path}"))
                                    .monospace()
                                    .size(11.5),
                            );
                        }
                        if let Some(path) = &item.existing_path {
                            ui.label(
                                RichText::new(format!("Existing: {path}"))
                                    .monospace()
                                    .size(11.5),
                            );
                        }
                        if let Some(path) = &item.preferred_path {
                            ui.label(
                                RichText::new(format!("Preferred: {path}"))
                                    .monospace()
                                    .size(11.5),
                            );
                        }
                        if let Some(detail) = &item.detail {
                            ui.label(RichText::new(detail).size(11.5));
                        }
                        if let Some(result) = &item.result_path {
                            ui.label(
                                RichText::new(format!("Result: {result}"))
                                    .monospace()
                                    .size(11.5),
                            );
                        }
                        if let Some(error) = &item.error {
                            ui.label(RichText::new(error).color(BAD).size(11.5));
                        }
                    });
            });
    }
}

fn show_report(ui: &mut Ui, report: &ApplyReport) {
    ui.add_space(4.0);
    widgets::section_title(ui, "Last execution");
    ui.horizontal_wrapped(|ui| {
        widgets::stat_card(ui, "Applied", report.applied.to_string(), "operations");
        widgets::stat_card(ui, "Inserted", report.inserted.to_string(), "new copies");
        widgets::stat_card(
            ui,
            "Replaced",
            report.replaced.to_string(),
            "old copy trashed",
        );
        widgets::stat_card(ui, "Skipped", report.skipped.to_string(), "no file change");
        widgets::stat_card(
            ui,
            "Problems",
            (report.failed + report.blocked).to_string(),
            "need attention",
        );
    });
    if report.stopped {
        ui.label(
            RichText::new("Execution stopped. Remaining operations can be resumed.").color(WARN),
        );
    }
    if report.problems.is_empty() {
        ui.label(RichText::new("No problems reported.").color(GOOD));
    } else {
        egui::CollapsingHeader::new(format!("{} problem(s)", report.problems.len()))
            .default_open(!report.problems.is_empty())
            .show(ui, |ui| {
                for problem in &report.problems {
                    ui.label(RichText::new(problem).color(BAD).size(12.0));
                }
            });
    }
    if report.conflicts_resolved > 0 {
        ui.label(
            RichText::new(format!(
                "{} filename conflict(s) were resolved without overwriting anything.",
                report.conflicts_resolved
            ))
            .color(MUTED),
        );
    }
}
