use eframe::egui::{self, RichText, Ui};

use crate::config::ValidationLevel;
use crate::dirpicker::PickTarget;

use super::super::widgets::{self, BAD, GOOD, MUTED, WARN};
use super::super::PixsortApp;

pub(crate) fn show(app: &mut PixsortApp, ui: &mut Ui) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            widgets::page_title(
                ui,
                "Settings",
                "All four locations are independent and may live on different disks. Databases must be outside the archive.",
            );

            let dirty = app.settings != app.saved_settings;
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(dirty, egui::Button::new("Save settings"))
                    .clicked()
                {
                    app.save_settings();
                }
                if dirty {
                    ui.label(RichText::new("unsaved changes").color(WARN));
                } else {
                    ui.label(RichText::new("saved").color(MUTED));
                }
            });
            ui.add_space(4.0);

            widgets::section_title(ui, "Locations");
            let mut use_local_trash = false;
            egui::Frame::group(ui.style())
                .inner_margin(12)
                .show(ui, |ui| {
                    let mut pick: Option<PickTarget> = None;
                    location_row(
                        ui,
                        "Input folder",
                        app.settings.input_dir.as_deref(),
                        "Photographs to import. Any folder structure is accepted.",
                        &mut pick,
                        PickTarget::Input,
                    );
                    location_row(
                        ui,
                        "Archive root",
                        app.settings.destination_dir.as_deref(),
                        "Year folders (2007, …, Unknown) are created here.",
                        &mut pick,
                        PickTarget::Destination,
                    );
                    location_row(
                        ui,
                        "Image index DB",
                        app.settings.effective_index_db().as_deref(),
                        "Database A: operational hash cache. Safe to rebuild.",
                        &mut pick,
                        PickTarget::IndexDbDir,
                    );
                    location_row(
                        ui,
                        "Modification plan DB",
                        app.settings.effective_plan_db().as_deref(),
                        "Database B: persisted decisions and operations.",
                        &mut pick,
                        PickTarget::PlanDbDir,
                    );
                    location_row(
                        ui,
                        "Trash folder",
                        app.settings.effective_trash_dir().as_deref(),
                        "Discarded versions are moved here and never deleted. It must be writable; it may be on a different disk.",
                        &mut pick,
                        PickTarget::Trash,
                    );
                    ui.horizontal(|ui| {
                        ui.add_space(150.0);
                        let local = crate::config::local_trash_dir();
                        if ui
                            .add_enabled(
                                app.job.is_none(),
                                egui::Button::new("Use local trash folder"),
                            )
                            .on_hover_text(format!(
                                "Use {} — a user-writable folder on local storage.                                  Files are moved across filesystems with a verified copy.",
                                local.display()
                            ))
                            .clicked()
                        {
                            use_local_trash = true;
                        }
                        ui.label(
                            RichText::new(local.display().to_string())
                                .color(MUTED)
                                .monospace()
                                .size(10.5),
                        );
                    });
                    if let Some(target) = pick {
                        app.start_picker(target);
                    }
                });
            if use_local_trash {
                app.settings.trash_dir = Some(crate::config::local_trash_dir());
                app.save_settings();
            }

            ui.add_space(6.0);
            widgets::section_title(ui, "Processing");
            egui::Frame::group(ui.style())
                .inner_margin(12)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Worker threads");
                        let mut workers = app.settings.worker_threads as u32;
                        if ui
                            .add(egui::Slider::new(&mut workers, 1..=64))
                            .changed()
                        {
                            app.settings.worker_threads = workers as usize;
                        }
                        ui.label(
                            RichText::new("Parallel decoding and hashing. Defaults to all logical CPUs.")
                                .color(MUTED)
                                .size(11.5),
                        );
                    });
                    ui.checkbox(
                        &mut app.settings.use_filesystem_year,
                        "Use filesystem modification time when no EXIF date exists",
                    );
                    ui.label(
                        RichText::new(
                            "When disabled, photographs without a reliable EXIF timestamp go to the Unknown folder.",
                        )
                        .color(MUTED)
                        .size(11.5),
                    );
                });

            ui.add_space(6.0);
            widgets::section_title(ui, "Similarity thresholds");
            egui::Frame::group(ui.style())
                .inner_margin(12)
                .show(ui, |ui| {
                    let mut probable = app.settings.probable_similarity_pct;
                    if ui
                        .add(
                            egui::Slider::new(&mut probable, 80.0..=100.0)
                                .suffix("%")
                                .text("Probable duplicate at or above"),
                        )
                        .changed()
                    {
                        app.settings.probable_similarity_pct = probable;
                    }
                    let mut review = app.settings.review_similarity_pct;
                    if ui
                        .add(
                            egui::Slider::new(&mut review, 50.0..=100.0)
                                .suffix("%")
                                .text("Group / review at or above"),
                        )
                        .changed()
                    {
                        app.settings.review_similarity_pct = review;
                    }
                    let mut max_members = app.settings.max_group_members as u32;
                    if ui
                        .add(
                            egui::Slider::new(&mut max_members, 2..=100)
                                .text("Maximum photographs per perceptual group"),
                        )
                        .changed()
                    {
                        app.settings.max_group_members = max_members as usize;
                    }
                    app.settings.normalize();
                    ui.label(RichText::new(app.settings.threshold_hint()).color(MUTED).size(12.0));
                    ui.label(
                        RichText::new(
                            "Similarity = 1 − Hamming distance / 63, converted from the 63-bit pHash. \
                             These percentages are UI indicators, not statistically calibrated probabilities. \
                             Groups are complete-linkage coherent and size-capped: a weak chain of matches cannot \
                             pull unrelated photographs into one group. The conservative 85% default avoids \
                             transitive over-grouping; lower it only if you want a wider review net. \
                             These settings affect the next scan/analysis; rerun Archive analysis \
                             after changing them to regroup an existing plan.",
                        )
                        .color(MUTED)
                        .size(11.5),
                    );
                });

            ui.add_space(6.0);
            widgets::section_title(ui, "Temporary session lifecycle");
            egui::Frame::group(ui.style())
                .inner_margin(12)
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Validate / reconnect paths").clicked() {
                            app.validate_reconnect();
                        }
                        if ui.button("Check database integrity").clicked() {
                            app.check_databases();
                        }
                        if ui.button("Refresh current session").clicked() {
                            app.refresh_plan();
                        }
                        if ui
                            .add_enabled(
                                app.plan.is_some() && app.job.is_none(),
                                egui::Button::new("Discard current session…"),
                            )
                            .clicked()
                        {
                            app.show_discard_modal = true;
                            app.discard_ack = false;
                        }
                    });
                    ui.label(
                        RichText::new(
                            "Databases are kept when the application closes. Discarding a session removes its \
                             decisions and plan rows, never the database files and never photographs. Pending \
                             operations are never discarded automatically.",
                        )
                        .color(MUTED)
                        .size(11.5),
                    );
                });

            ui.add_space(6.0);
            widgets::section_title(ui, "Configuration checks");
            let issues = app.settings.issues();
            if issues.is_empty() {
                ui.label(RichText::new("No issues detected.").color(GOOD));
            }
            for issue in issues {
                let color = match issue.level {
                    ValidationLevel::Error => BAD,
                    ValidationLevel::Warning => WARN,
                    ValidationLevel::Debug => MUTED,
                };
                let prefix = match issue.level {
                    ValidationLevel::Error => "Error",
                    ValidationLevel::Warning => "Warning",
                    ValidationLevel::Debug => "Note",
                };
                ui.label(RichText::new(format!("{prefix}: {}", issue.message)).color(color));
            }

            ui.add_space(6.0);
            widgets::section_title(ui, "Image support");
            ui.label(format!("HEIC / HEIF: {}", app.heic_status));
            ui.label(
                RichText::new(
                    "JPEG, PNG, WebP, BMP, TIFF and HEIC/HEIF (through libheif when installed). \
                     Animated PNG/WebP files are treated as still photographs: only the first frame is \
                     analysed and the file is flagged. GIF and videos are not supported.",
                )
                .color(MUTED)
                .size(11.5),
            );

            ui.add_space(8.0);
            if let Some(error) = &app.error {
                ui.label(RichText::new(error).color(BAD));
            }
        });
}

fn location_row(
    ui: &mut Ui,
    label: &str,
    path: Option<&std::path::Path>,
    hint: &str,
    pick: &mut Option<PickTarget>,
    target: PickTarget,
) {
    ui.add_space(3.0);
    ui.horizontal(|ui| {
        ui.add_sized(
            [150.0, 18.0],
            egui::Label::new(RichText::new(label).strong().size(13.0)),
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
        if ui.button("Choose…").clicked() {
            *pick = Some(target);
        }
    });
    ui.label(RichText::new(hint).color(MUTED).size(11.0));
    ui.add_space(4.0);
}
