use eframe::egui::{self, RichText, Ui};

use crate::config::ValidationLevel;

use super::super::types::Page;
use super::super::widgets::{self, BAD, MUTED, WARN};
use super::super::PixsortApp;

pub(crate) fn show(app: &mut PixsortApp, ui: &mut Ui) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            widgets::page_title(
                ui,
                "Archive analysis",
                "Find exact and perceptual duplicates inside the existing archive. Read-only until you apply a plan.",
            );

            if let Some(plan) = &app.plan {
                if plan.is_active() && plan.unfinished_operations() > 0 {
                    ui.label(
                        RichText::new(
                            "A plan with unfinished operations exists. Apply, retry or discard it before a new analysis.",
                        )
                        .color(WARN),
                    );
                }
            }

            widgets::section_title(ui, "Archive and storage");
            egui::Frame::group(ui.style())
                .inner_margin(12)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [130.0, 18.0],
                            egui::Label::new(RichText::new("Archive root").color(MUTED)),
                        );
                        match &app.settings.destination_dir {
                            Some(path) => {
                                ui.label(RichText::new(path.display().to_string()).monospace());
                            }
                            None => {
                                ui.label(RichText::new("not selected").color(WARN));
                            }
                        }
                    });
                    ui.label(
                        RichText::new(format!(
                            "Image index: {}",
                            app.settings
                                .effective_index_db()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| "not configured".into())
                        ))
                        .size(12.0)
                        .color(MUTED),
                    );
                    ui.label(
                        RichText::new(format!(
                            "Modification plan: {}",
                            app.settings
                                .effective_plan_db()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| "not configured".into())
                        ))
                        .size(12.0)
                        .color(MUTED),
                    );
                    if ui.button("Change in Settings").clicked() {
                        app.page = Page::Settings;
                    }
                });

            let issues: Vec<String> = app
                .settings
                .issues()
                .into_iter()
                .filter(|i| i.level == ValidationLevel::Error)
                .map(|i| i.message)
                .collect();
            for issue in &issues {
                ui.label(RichText::new(format!("• {issue}")).color(BAD));
            }

            ui.add_space(8.0);
            widgets::section_title(ui, "Scan the archive");
            ui.label(
                RichText::new(
                    "All photographs are indexed with cached hashes. Exact duplicates are grouped by the \
                     normalised pixel hash. Perceptual candidates come from a BK-tree over stored pHashes: \
                     no image is decoded again during comparison.",
                )
                .color(MUTED),
            );
            let blocking_plan = app
                .plan
                .as_ref()
                .map(|plan| plan.is_active() && plan.unfinished_operations() > 0)
                .unwrap_or(false);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        app.job.is_none() && issues.is_empty() && !blocking_plan,
                        egui::Button::new("Analyse archive"),
                    )
                    .on_disabled_hover_text(
                        "Finish, apply or discard the existing plan first.",
                    )
                    .clicked()
                {
                    app.start_analysis_job();
                }
                if ui.button("Review duplicates").clicked() {
                    app.page = Page::Review;
                }
            });

            if let Some(progress) = &app.progress {
                if app.job.as_ref().map(|j| j.kind()) == Some(crate::jobs::JobKind::Analysis)
                    || app.last_analysis.is_some()
                {
                    ui.add_space(8.0);
                    let action = widgets::progress_panel(ui, progress, app.job.as_ref());
                    app.handle_progress_action(action);
                }
            }

            if let Some(summary) = &app.last_analysis {
                if summary.stopped {
                    ui.label(RichText::new("Analysis stopped; cached hashes are preserved.").color(WARN));
                } else {
                    ui.add_space(8.0);
                    widgets::section_title(ui, "Analysis result");
                    ui.horizontal_wrapped(|ui| {
                        widgets::stat_card(
                            ui,
                            "Indexed photographs",
                            summary.indexed_total.to_string(),
                            "in the archive",
                        );
                        widgets::stat_card(
                            ui,
                            "Exact groups",
                            summary.exact_groups.to_string(),
                            "identical pixels",
                        );
                        widgets::stat_card(
                            ui,
                            "Perceptual groups",
                            summary.perceptual_groups.to_string(),
                            "need review",
                        );
                        widgets::stat_card(
                            ui,
                            "Photographs grouped",
                            summary.photos_in_groups.to_string(),
                            "candidates for cleanup",
                        );
                    });
                    ui.label(format!(
                        "Proposed operations: {} move(s) to trash, {} review item(s). Nothing has been modified.",
                        summary.plan_trash, summary.plan_reviews
                    ));
                    if ui.button("Open plan").clicked() {
                        app.page = Page::Plan;
                    }
                }
            }

            ui.add_space(8.0);
            widgets::section_title(ui, "Scope");
            ui.label(
                RichText::new(
                    "Crop matching, object recognition, semantic similarity and AI models are \
                     intentionally not implemented. The goal is different technical versions of the \
                     same photograph: resized, recompressed or re-encoded.",
                )
                .color(MUTED),
            );
        });
}
