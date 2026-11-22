use std::collections::HashMap;

use eframe::egui::{self, RichText, Ui};

use crate::config::Settings;
use crate::model::{GroupDecision, GroupKind, GroupState, PhotoRecord};

use super::super::image_cache::ImageCache;
use super::super::types::PlanView;
use super::super::widgets::{self, ACCENT, BAD, GOOD, MUTED, WARN};

#[derive(Default)]
pub(crate) struct ReviewUiState {
    pub selected: Option<i64>,
    pub alternative: HashMap<i64, usize>,
    pub show_decided: bool,
}

pub(crate) enum ReviewAction {
    Decide {
        group_id: i64,
        decision: GroupDecision,
        keeper: Option<String>,
    },
    AcceptProbable,
}

pub(crate) fn show(
    plan: Option<&PlanView>,
    settings: &Settings,
    cache: &mut ImageCache,
    state: &mut ReviewUiState,
    ui: &mut Ui,
) -> Option<ReviewAction> {
    widgets::page_title(
        ui,
        "Manual review",
        "The recommended photograph is always shown on the left. Your decision updates the plan immediately.",
    );

    let Some(plan) = plan else {
        ui.label(
            RichText::new("No plan yet. Run an import or analyse the archive first.").color(MUTED),
        );
        return None;
    };

    if plan.is_legacy_grouping() {
        egui::Frame::new()
            .fill(BAD.gamma_multiply(0.15))
            .stroke(egui::Stroke::new(1.0, BAD))
            .corner_radius(6)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.label(
                    RichText::new(
                        "This session was created with the old unbounded grouping algorithm. \
                         Decisions are disabled because its groups may mix unrelated photographs. \
                         Discard the session from “Planned modifications” and run Archive analysis again.",
                    )
                    .color(BAD),
                );
            });
        return None;
    }

    let groups: Vec<&crate::model::DuplicateGroup> = plan
        .duplicate_groups()
        .filter(|g| state.show_decided || g.needs_decision() || g.state == GroupState::Auto)
        .collect();
    if plan.duplicate_groups().next().is_none() {
        ui.label(RichText::new("This session found no duplicate groups.").color(MUTED));
        return None;
    }

    let mut action = None;
    let unresolved = groups.iter().filter(|g| g.needs_decision()).count();
    let probable = groups
        .iter()
        .filter(|g| {
            g.state == GroupState::NeedsReview
                && g.similarity + 0.01 >= settings.probable_similarity_pct
        })
        .count();

    ui.horizontal_wrapped(|ui| {
        ui.label(format!(
            "{} group(s) total · {} needing a decision · {} probable",
            plan.duplicate_groups().count(),
            unresolved,
            probable
        ));
        if probable > 0 && ui.button("Accept all probable recommendations").clicked() {
            action = Some(ReviewAction::AcceptProbable);
        }
        ui.checkbox(&mut state.show_decided, "Show decided groups");
    });
    ui.add_space(6.0);

    if groups.is_empty() {
        ui.label(
            RichText::new(
                "All duplicate groups have been decided. Review them by ticking “Show decided groups”.",
            )
            .color(GOOD),
        );
        return action;
    }

    if !state
        .selected
        .map(|id| groups.iter().any(|g| g.id == id))
        .unwrap_or(false)
    {
        state.selected = groups
            .iter()
            .find(|g| g.needs_decision())
            .map(|g| g.id)
            .or(Some(groups[0].id));
    }

    let available = ui.available_size();
    ui.horizontal_top(|ui| {
        // Group list.
        ui.allocate_ui_with_layout(
            egui::vec2(310.0, available.y),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("review-groups")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for group in &groups {
                            let selected = state.selected == Some(group.id);
                            let color = if group.needs_decision() {
                                if group.similarity + 0.01 >= settings.probable_similarity_pct {
                                    WARN
                                } else {
                                    BAD
                                }
                            } else {
                                GOOD
                            };
                            egui::Frame::new()
                                .fill(if selected {
                                    ACCENT.gamma_multiply(0.18)
                                } else {
                                    egui::Color32::TRANSPARENT
                                })
                                .corner_radius(6)
                                .inner_margin(7)
                                .show(ui, |ui| {
                                    if ui
                                        .add(
                                            egui::Label::new(
                                                RichText::new(plan.group_title(group))
                                                    .strong()
                                                    .size(13.0),
                                            )
                                            .sense(egui::Sense::click()),
                                        )
                                        .clicked()
                                    {
                                        state.selected = Some(group.id);
                                    }
                                    ui.horizontal_wrapped(|ui| {
                                        widgets::badge(ui, group.state.label(), color);
                                        if group.kind == GroupKind::Exact {
                                            widgets::badge(ui, "Exact", GOOD);
                                        } else {
                                            widgets::badge(
                                                ui,
                                                &format!("{:.1}%", group.similarity),
                                                color,
                                            );
                                        }
                                    });
                                });
                        }
                    });
            },
        );
        ui.separator();

        let group = groups
            .iter()
            .find(|g| Some(g.id) == state.selected)
            .copied()
            .unwrap_or(groups[0]);
        let members = plan.members_of(group.id);
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), available.y),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("review-detail")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        show_group(
                            ui,
                            plan,
                            group,
                            members,
                            settings,
                            cache,
                            state,
                            &mut action,
                        );
                    });
            },
        );
    });

    action
}

#[allow(clippy::too_many_arguments)]
fn show_group(
    ui: &mut Ui,
    plan: &PlanView,
    group: &crate::model::DuplicateGroup,
    members: &[PhotoRecord],
    settings: &Settings,
    cache: &mut ImageCache,
    state: &mut ReviewUiState,
    action: &mut Option<ReviewAction>,
) {
    if members.is_empty() {
        ui.label(RichText::new("This group has no readable members.").color(BAD));
        return;
    }
    let recommended_path = plan
        .keeper_path(group)
        .map(|s| s.to_string())
        .or_else(|| members.first().map(|m| m.path.clone()));
    let left = recommended_path
        .as_deref()
        .and_then(|path| members.iter().find(|m| m.path == path))
        .unwrap_or(&members[0]);
    let alternatives: Vec<&PhotoRecord> = members.iter().filter(|m| m.path != left.path).collect();
    if alternatives.is_empty() {
        ui.label(RichText::new("This group contains only one readable member.").color(WARN));
        return;
    }
    let alt_index = {
        let index = state.alternative.entry(group.id).or_insert(0);
        *index = (*index).min(alternatives.len() - 1);
        *index
    };
    let right = alternatives[alt_index];

    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(plan.group_title(group)).strong().size(16.0));
        widgets::badge(
            ui,
            if group.needs_decision() {
                "Needs your decision"
            } else {
                group.state.label()
            },
            if group.needs_decision() { WARN } else { GOOD },
        );
        if group.kind == GroupKind::Exact {
            widgets::badge(ui, "Exact pixel duplicate", GOOD);
        }
    });
    if members.len() > 8 {
        ui.label(
            RichText::new(format!(
                "This group contains {} files. Check that they really are versions of the same \
                 photograph. A decision applies to the whole group; “Keep both” and “Ignore match” \
                 keep every version.",
                members.len()
            ))
            .color(WARN)
            .size(12.0),
        );
    }
    ui.add_space(4.0);

    // Alternative selector for groups with more than two members.
    if alternatives.len() > 1 {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Compare with:").color(MUTED));
            for (index, member) in alternatives.iter().enumerate() {
                if ui
                    .selectable_label(index == alt_index, member.filename().to_string())
                    .clicked()
                {
                    state.alternative.insert(group.id, index);
                }
            }
        });
    }

    let preview_height = 280.0_f32;
    let column_width = (ui.available_width() - 24.0) / 2.0;
    let left_caption = if group.state == GroupState::Resolved {
        "Kept version"
    } else {
        "Recommended on this side"
    };
    ui.columns(2, |columns| {
        column_preview(
            &mut columns[0],
            cache,
            left,
            left_caption,
            recommended_path.as_deref() == Some(left.path.as_str()),
            preview_height,
            column_width,
            settings.thumbnail_max_dim,
        );
        column_preview(
            &mut columns[1],
            cache,
            right,
            "Alternative",
            false,
            preview_height,
            column_width,
            settings.thumbnail_max_dim,
        );
    });

    let (similarity, explanation) = widgets::similarity_between(left, right);
    ui.add_space(4.0);
    ui.label(
        RichText::new(format!("{:.1}% indicator", similarity))
            .strong()
            .color(if similarity >= settings.probable_similarity_pct {
                WARN
            } else {
                BAD
            }),
    );
    ui.label(RichText::new(explanation).color(MUTED).size(12.0));
    if group.kind == GroupKind::Exact {
        ui.label(
            RichText::new(
                "The pixels are identical; the recommendation is about which physical file to retain.",
            )
            .color(MUTED)
            .size(12.0),
        );
    }

    ui.add_space(4.0);
    ui.add_space(6.0);
    ui.add_space(8.0);
    if group.needs_decision()
        || group.state == GroupState::Resolved
        || group.state == GroupState::Ignored
    {
        ui.horizontal_wrapped(|ui| {
            if ui
                .button(format!("Keep recommended · {}", left.filename()))
                .clicked()
            {
                *action = Some(ReviewAction::Decide {
                    group_id: group.id,
                    decision: GroupDecision::Keep,
                    keeper: Some(left.path.clone()),
                });
            }
            if ui
                .button(format!("Keep alternative · {}", right.filename()))
                .clicked()
            {
                *action = Some(ReviewAction::Decide {
                    group_id: group.id,
                    decision: GroupDecision::Keep,
                    keeper: Some(right.path.clone()),
                });
            }
            if ui.button("Keep both").clicked() {
                *action = Some(ReviewAction::Decide {
                    group_id: group.id,
                    decision: GroupDecision::KeepBoth,
                    keeper: None,
                });
            }
            if ui.button("Ignore match").clicked() {
                *action = Some(ReviewAction::Decide {
                    group_id: group.id,
                    decision: GroupDecision::Ignore,
                    keeper: None,
                });
            }
            if (group.state == GroupState::Resolved || group.state == GroupState::Ignored)
                && ui.button("Undo decision").clicked()
            {
                *action = Some(ReviewAction::Decide {
                    group_id: group.id,
                    decision: GroupDecision::None,
                    keeper: None,
                });
            }
        });
        ui.label(
            RichText::new(
                "Keep both preserves every version (they will all be inserted for an import and left \
                 untouched for archive analysis). Ignore match records that you do not want to treat \
                 these files as duplicates.",
            )
            .color(MUTED)
            .size(11.5),
        );
    } else if group.state == GroupState::Auto {
        ui.label(
            RichText::new(
                "This group is resolved automatically. You can still change the decision:",
            )
            .color(MUTED),
        );
        ui.horizontal_wrapped(|ui| {
            if ui.button("Keep the other version").clicked() {
                *action = Some(ReviewAction::Decide {
                    group_id: group.id,
                    decision: GroupDecision::Keep,
                    keeper: Some(right.path.clone()),
                });
            }
            if ui.button("Keep both").clicked() {
                *action = Some(ReviewAction::Decide {
                    group_id: group.id,
                    decision: GroupDecision::KeepBoth,
                    keeper: None,
                });
            }
            if ui.button("Ignore match").clicked() {
                *action = Some(ReviewAction::Decide {
                    group_id: group.id,
                    decision: GroupDecision::Ignore,
                    keeper: None,
                });
            }
        });
    }

    egui::CollapsingHeader::new("Why this recommendation?")
        .default_open(false)
        .show(ui, |ui| {
            ui.label(RichText::new(&group.explanation).monospace().size(12.0));
        });

    ui.add_space(8.0);
    ui.label(RichText::new("All members of this group").strong());
    for member in members {
        let marker = if member.path == left.path {
            "recommended"
        } else if member.path == right.path {
            "alternative"
        } else {
            ""
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} · {} · {} · {}",
                    member.filename(),
                    member.resolution_label(),
                    member.size_label(),
                    marker
                ))
                .size(12.0),
            );
            ui.label(
                RichText::new(&member.path)
                    .color(MUTED)
                    .monospace()
                    .size(10.5),
            );
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn column_preview(
    ui: &mut Ui,
    cache: &mut ImageCache,
    photo: &PhotoRecord,
    caption: &str,
    recommended: bool,
    height: f32,
    width: f32,
    max_dim: u32,
) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(caption)
                .strong()
                .color(if recommended { ACCENT } else { MUTED }),
        );
    });
    widgets::photo_preview(ui, cache, photo, max_dim, width, height);
    widgets::photo_meta(ui, photo);
}
