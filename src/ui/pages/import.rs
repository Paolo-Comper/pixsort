use eframe::egui::{self, RichText, Ui};

use crate::config::ValidationLevel;
use crate::model::ImportSummary;

use super::super::types::Page;
use super::super::widgets::{self, BAD, GOOD, MUTED, WARN};
use super::super::PixsortApp;

pub(crate) fn show(app: &mut PixsortApp, ui: &mut Ui) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            widgets::page_title(
                ui,
                "Import photographs",
                "Add new photographs to the archive, reuse cached hashes, detect duplicates and prepare a plan.",
            );

            if let Some(plan) = &app.plan {
                if plan.is_active() && plan.unfinished_operations() > 0 {
                    egui::Frame::new()
                        .fill(WARN.gamma_multiply(0.12))
                        .stroke(egui::Stroke::new(1.0, WARN))
                        .corner_radius(6)
                        .inner_margin(10)
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(
                                    "A plan with unfinished operations exists. Finish, apply or discard it before starting a new import.",
                                )
                                .color(WARN),
                            );
                            ui.horizontal(|ui| {
                                if ui.button("Review / plan").clicked() {
                                    app.page = Page::Plan;
                                }
                            });
                        });
                    ui.add_space(6.0);
                }
            }

            widgets::section_title(ui, "1 · Locations");
            egui::Frame::group(ui.style())
                .inner_margin(12)
                .show(ui, |ui| {
                    path_line(ui, "Input folder", app.settings.input_dir.as_deref());
                    path_line(ui, "Archive root", app.settings.destination_dir.as_deref());
                    path_line(
                        ui,
                        "Image index",
                        app.settings.effective_index_db().as_deref(),
                    );
                    path_line(
                        ui,
                        "Modification plan",
                        app.settings.effective_plan_db().as_deref(),
                    );
                    path_line(
                        ui,
                        "Trash folder",
                        app.settings.effective_trash_dir().as_deref(),
                    );
                    if ui.button("Change locations in Settings").clicked() {
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
            if !issues.is_empty() {
                ui.add_space(6.0);
                for issue in &issues {
                    ui.label(RichText::new(format!("• {issue}")).color(BAD));
                }
            }

            ui.add_space(8.0);
            widgets::section_title(ui, "2 · Scan and compare");
            ui.label(
                RichText::new(
                    "The archive is indexed first, then the import folder. Unchanged files reuse their \
                     stored hashes. Each new photograph is compared with the archive and with the rest \
                     of the batch; archive photographs are not compared with one another.",
                )
                .color(MUTED),
            );
            let blocking_plan = app
                .plan
                .as_ref()
                .map(|plan| plan.is_active() && plan.unfinished_operations() > 0)
                .unwrap_or(false);
            ui.horizontal(|ui| {
                let can_start = app.job.is_none() && issues.is_empty() && !blocking_plan;
                if ui
                    .add_enabled(can_start, egui::Button::new("Start import"))
                    .on_disabled_hover_text(if blocking_plan {
                        "Finish, apply or discard the existing plan first."
                    } else {
                        "Select valid input and archive folders first."
                    })
                    .clicked()
                {
                    app.start_import_job();
                }
                if let Some(job) = &app.job {
                    ui.label(
                        RichText::new(format!("{} job is running…", job.kind().label()))
                            .color(WARN),
                    );
                }
            });

            if let Some(progress) = &app.progress {
                if app.job.as_ref().map(|j| j.kind()) == Some(crate::jobs::JobKind::Import)
                    || app.last_import.is_some()
                {
                    ui.add_space(8.0);
                    let action = widgets::progress_panel(ui, progress, app.job.as_ref());
                    app.handle_progress_action(action);
                }
            }

            if let Some(summary) = app.last_import.clone() {
                ui.add_space(10.0);
                show_summary(app, ui, &summary);
            }

            ui.add_space(10.0);
            widgets::section_title(ui, "How duplicates are decided");
            ui.label(
                "Exact duplicates share the same SHA-256 of normalised pixels. Perceptual matches use a \
                 63-bit pHash with Hamming distance. A percentage is shown as a heuristic indicator, \
                 never as a guaranteed probability. Matches at or above the probable threshold are \
                 pre-selected for you; ambiguous matches always wait for your decision.",
            );

            if let Some(plan) = &app.plan {
                if plan.counts.applied > 0 && plan.session.status == crate::db::SessionStatus::Applied {
                    ui.add_space(6.0);
                    ui.label(RichText::new("The last plan has been applied.").color(GOOD));
                }
            }
        });
}

fn show_summary(app: &mut PixsortApp, ui: &mut Ui, summary: &ImportSummary) {
    if summary.stopped {
        egui::Frame::new()
            .fill(WARN.gamma_multiply(0.12))
            .corner_radius(6)
            .inner_margin(10)
            .show(ui, |ui| {
                ui.label(
                    RichText::new(
                        "The import was stopped. Completed hashes are saved; press Start import again to resume.",
                    )
                    .color(WARN),
                );
            });
        return;
    }

    widgets::section_title(ui, "Import result");
    ui.horizontal_wrapped(|ui| {
        widgets::stat_card(
            ui,
            "New photographs",
            summary.new_photos.to_string(),
            "no match found",
        );
        widgets::stat_card(
            ui,
            "Exact duplicates",
            summary.exact_duplicates.to_string(),
            "identical normalised pixels",
        );
        widgets::stat_card(
            ui,
            "Probable duplicates",
            summary.probable_duplicates.to_string(),
            "≥ probable threshold",
        );
        widgets::stat_card(
            ui,
            "Ambiguous",
            summary.ambiguous.to_string(),
            "needs manual review",
        );
        widgets::stat_card(
            ui,
            "Failed",
            summary.failed.to_string(),
            "unreadable or unsupported",
        );
    });
    ui.label(format!(
        "{} files in the batch · {} in the archive · {} hashes reused · {} newly indexed",
        summary.input_total, summary.archive_total, summary.reused, summary.indexed_new
    ));
    ui.horizontal_wrapped(|ui| {
        ui.label(format!("Plan: {} insert", summary.plan_inserts));
        ui.label(format!("{} replace", summary.plan_replaces));
        ui.label(format!("{} skip", summary.plan_skips));
        ui.label(format!("{} to trash", summary.plan_trash));
        ui.label(format!("{} review", summary.plan_reviews));
    });
    ui.horizontal(|ui| {
        if summary.plan_reviews > 0 {
            if ui.button("Review matches").clicked() {
                app.page = Page::Review;
            }
        }
        if ui.button("Inspect plan").clicked() {
            app.page = Page::Plan;
        }
    });
    let _ = app;
}

fn path_line(ui: &mut Ui, label: &str, path: Option<&std::path::Path>) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [130.0, 18.0],
            egui::Label::new(RichText::new(label).color(MUTED).size(12.0)),
        );
        match path {
            Some(path) => {
                ui.label(
                    RichText::new(path.display().to_string())
                        .monospace()
                        .size(12.0),
                );
            }
            None => {
                ui.label(RichText::new("not selected").color(WARN));
            }
        }
    });
}
