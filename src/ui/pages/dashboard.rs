use eframe::egui::{self, RichText, Ui};

use crate::config::ValidationLevel;
use crate::model::format_duration;

use super::super::types::Page;
use super::super::widgets::{self, ACCENT, BAD, GOOD, MUTED, WARN};
use super::super::PixsortApp;

pub(crate) fn show(app: &mut PixsortApp, ui: &mut Ui) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            widgets::page_title(
                ui,
                "Dashboard",
                "A local, resumable photo archive organizer. Nothing is uploaded and no AI models are used.",
            );

            if let Some(error) = &app.error {
                egui::Frame::new()
                    .fill(BAD.gamma_multiply(0.15))
                    .stroke(egui::Stroke::new(1.0, BAD))
                    .corner_radius(6)
                    .inner_margin(10)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Problem").color(BAD).strong());
                            ui.label(RichText::new(error).color(BAD));
                        });
                    });
                ui.add_space(6.0);
            }
            if let Some(note) = &app.startup_note {
                ui.label(RichText::new(note).color(WARN));
                ui.add_space(4.0);
            }

            let stats = app.index_stats();
            let (review_groups, pending_ops, session_label) = match &app.plan {
                Some(plan) => (
                    plan.unresolved_reviews(),
                    plan.counts.pending + plan.counts.blocked,
                    format!(
                        "{} · {}",
                        plan.session.mode.label(),
                        plan.session.status.label()
                    ),
                ),
                None => (0, 0, "No active session".to_string()),
            };

            ui.horizontal_wrapped(|ui| {
                widgets::stat_card(
                    ui,
                    "Archive photographs",
                    widgets::human_count(stats.archive_total),
                    &format!("{} indexed", widgets::human_count(stats.indexed)),
                );
                widgets::stat_card(
                    ui,
                    "Import batch",
                    widgets::human_count(stats.input_total),
                    "currently indexed",
                );
                widgets::stat_card(
                    ui,
                    "Groups to review",
                    review_groups.to_string(),
                    "perceptual matches need a decision",
                );
                widgets::stat_card(
                    ui,
                    "Pending operations",
                    pending_ops.to_string(),
                    "in the current plan",
                );
            });

            ui.add_space(8.0);
            widgets::section_title(ui, "Current session");
            egui::Frame::group(ui.style())
                .inner_margin(12)
                .show(ui, |ui| {
                    ui.label(RichText::new(session_label).strong());
                    match &app.plan {
                        Some(plan) => {
                            ui.label(format!(
                                "Input: {}",
                                app.settings
                                    .input_dir
                                    .as_ref()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_else(|| "not configured".into())
                            ));
                            ui.label(format!("Archive: {}", plan.session.dest_dir));
                            ui.horizontal_wrapped(|ui| {
                                ui.label(format!(
                                    "{} insert(s), {} replace(s), {} skip(s), {} move(s) to trash, {} review item(s)",
                                    plan.counts.inserts,
                                    plan.counts.replaces,
                                    plan.counts.skips,
                                    plan.counts.moves_to_trash,
                                    plan.counts.reviews
                                ));
                            });
                            if plan.counts.failed > 0 || plan.counts.blocked > 0 {
                                ui.label(
                                    RichText::new(format!(
                                        "{} failed, {} blocked",
                                        plan.counts.failed, plan.counts.blocked
                                    ))
                                    .color(BAD),
                                );
                            }
                        }
                        None => {
                            ui.label(
                                RichText::new(
                                    "No plan yet. Start an import or analyse an existing archive.",
                                )
                                .color(MUTED),
                            );
                        }
                    }
                });

            ui.add_space(8.0);
            widgets::section_title(ui, "Quick actions");
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(app.job.is_none(), egui::Button::new("Import photographs"))
                    .clicked()
                {
                    app.page = Page::Import;
                }
                if ui
                    .add_enabled(app.job.is_none(), egui::Button::new("Analyse archive"))
                    .clicked()
                {
                    app.page = Page::Analysis;
                }
                if ui
                    .add_enabled(
                        app.plan
                            .as_ref()
                            .map(|p| p.duplicate_groups().next().is_some())
                            .unwrap_or(false),
                        egui::Button::new("Review duplicates"),
                    )
                    .clicked()
                {
                    app.page = Page::Review;
                }
                if ui.button("Settings").clicked() {
                    app.page = Page::Settings;
                }
            });

            if let Some(progress) = app.last_progress_summary() {
                ui.label(RichText::new(format!("Last activity: {progress}")).color(MUTED));
            }

            ui.add_space(8.0);
            widgets::section_title(ui, "Safety model");
            ui.label(
                "• Scanning, hashing and comparison are read-only.\n\
                 • The archive is modified only after pressing Apply and confirming the dialog.\n\
                 • Discarded versions are moved to a configurable trash folder — never deleted.\n\
                 • Every copy is verified before it is published; existing files are never overwritten.\n\
                 • The plan lives in a separate SQLite database and survives restarts.",
            );

            let issues: Vec<_> = app
                .settings
                .issues()
                .into_iter()
                .filter(|i| i.level != ValidationLevel::Debug)
                .collect();
            if !issues.is_empty() {
                ui.add_space(8.0);
                widgets::section_title(ui, "Configuration checks");
                for issue in issues {
                    let color = if issue.level == ValidationLevel::Error {
                        BAD
                    } else {
                        WARN
                    };
                    ui.label(RichText::new(format!("• {}", issue.message)).color(color));
                }
            }

            if let Some(plan) = &app.plan {
                if plan.session.status == crate::db::SessionStatus::Applied {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(format!(
                            "Session applied in {}.",
                            format_duration(
                                app.last_apply
                                    .as_ref()
                                    .map(|r| r.elapsed_secs)
                                    .unwrap_or(0.0)
                            )
                        ))
                        .color(GOOD),
                    );
                }
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("HEIC support: {}", app.heic_status))
                    .color(if app.heic_status.contains("available") {
                        GOOD
                    } else {
                        MUTED
                    })
                    .size(11.5),
            );
            let _ = ACCENT;
        });
}
