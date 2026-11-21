//! Small reusable widgets shared by the screens.

use eframe::egui::{self, Color32, RichText, Ui};

use crate::model::{phash_distance, similarity_for_distance, PhotoRecord, Progress};

use super::image_cache::ImageCache;
use super::types::RunningJob;

pub(crate) const ACCENT: Color32 = Color32::from_rgb(90, 160, 235);
pub(crate) const GOOD: Color32 = Color32::from_rgb(100, 190, 120);
pub(crate) const WARN: Color32 = Color32::from_rgb(230, 180, 80);
pub(crate) const BAD: Color32 = Color32::from_rgb(230, 110, 100);
pub(crate) const MUTED: Color32 = Color32::from_rgb(150, 155, 165);

pub(crate) fn page_title(ui: &mut Ui, title: &str, subtitle: &str) {
    ui.add_space(2.0);
    ui.heading(title);
    ui.label(RichText::new(subtitle).color(MUTED));
    ui.add_space(6.0);
}

pub(crate) fn section_title(ui: &mut Ui, title: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(title).strong().size(16.0));
}

pub(crate) fn badge(ui: &mut Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.18))
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(color).size(12.5));
        });
}

pub(crate) fn stat_card(ui: &mut Ui, label: &str, value: impl Into<String>, hint: &str) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_min_width(130.0);
            ui.label(RichText::new(label).color(MUTED).size(12.0));
            ui.label(RichText::new(value.into()).size(22.0).strong());
            if !hint.is_empty() {
                ui.label(RichText::new(hint).color(MUTED).size(11.5));
            }
        });
}

pub(crate) enum ProgressAction {
    None,
    Pause,
    Resume,
    Stop,
}

pub(crate) fn progress_panel(
    ui: &mut Ui,
    progress: &Progress,
    job: Option<&RunningJob>,
) -> ProgressAction {
    let mut action = ProgressAction::None;
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&progress.phase).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(job) = job {
                        if job.handle.control.is_stopped() {
                            ui.label(RichText::new("Stopping…").color(WARN));
                        } else if job.handle.control.is_paused() {
                            if ui.button("Resume").clicked() {
                                action = ProgressAction::Resume;
                            }
                        } else if ui.button("Pause").clicked() {
                            action = ProgressAction::Pause;
                        }
                        if ui.button("Stop").clicked() {
                            action = ProgressAction::Stop;
                        }
                    }
                });
            });

            let fraction = if progress.total == 0 {
                0.0
            } else {
                (progress.completed as f32 / progress.total as f32).clamp(0.0, 1.0)
            };
            let bar = egui::ProgressBar::new(fraction)
                .show_percentage()
                .animate(job.is_some());
            ui.add(bar);

            ui.horizontal_wrapped(|ui| {
                ui.label(format!(
                    "{} / {} processed",
                    progress.completed, progress.total
                ));
                ui.separator();
                ui.label(format!("{} failed", progress.failed));
                ui.separator();
                ui.label(format!("{} reused from cache", progress.reused));
                ui.separator();
                ui.label(format!("{:.1} files/s", progress.rate_per_sec()));
                ui.separator();
                ui.label(format!(
                    "ETA {}",
                    progress
                        .eta_secs()
                        .map(crate::model::format_duration)
                        .unwrap_or_else(|| "—".into())
                ));
            });
            if !progress.current.is_empty() {
                ui.label(
                    RichText::new(format!("Current: {}", progress.current))
                        .color(MUTED)
                        .size(12.0),
                );
            }
        });
    action
}

pub(crate) fn photo_preview(
    ui: &mut Ui,
    cache: &mut ImageCache,
    photo: &PhotoRecord,
    max_dim: u32,
    width: f32,
    height: f32,
) {
    let texture = cache.texture(ui.ctx(), photo, max_dim);
    egui::Frame::new()
        .fill(Color32::from_rgb(24, 26, 30))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::same(4))
        .show(ui, |ui| {
            ui.set_min_size(egui::vec2(width, height));
            ui.vertical_centered(|ui| {
                if let Some(texture) = texture {
                    let size = texture.size_vec2();
                    let scale = (width / size.x).min(height / size.y).clamp(0.01, 1.0);
                    ui.add(
                        egui::Image::new(egui::load::SizedTexture::from_handle(&texture))
                            .fit_to_exact_size(size * scale),
                    );
                } else if let Some(error) = cache.error_for(photo, max_dim) {
                    ui.add_space(height * 0.35);
                    ui.label(RichText::new("Preview unavailable").color(BAD));
                    ui.label(RichText::new(error).color(MUTED).size(11.0));
                } else {
                    ui.add_space(height * 0.4);
                    ui.spinner();
                    ui.label(RichText::new("Loading preview…").color(MUTED));
                }
            });
        });
}

pub(crate) fn photo_meta(ui: &mut Ui, photo: &PhotoRecord) {
    egui::Grid::new(format!("meta-{}", photo.path))
        .num_columns(2)
        .spacing([12.0, 3.0])
        .show(ui, |ui| {
            row(ui, "Resolution", &photo.resolution_label());
            row(ui, "File size", &photo.size_label());
            row(ui, "Format", photo.format.label());
            row(ui, "Capture date", &photo.date_label());
            row(ui, "Modified", &photo.modified_label());
            row(
                ui,
                "Location",
                if photo.root == crate::model::RootKind::Input {
                    "Import batch"
                } else {
                    "Archive"
                },
            );
        });
    ui.add_space(4.0);
    ui.label(RichText::new("Original path").color(MUTED).size(11.5));
    ui.add(egui::Label::new(RichText::new(&photo.path).monospace().size(11.5)).wrap());
}

fn row(ui: &mut Ui, label: &str, value: &str) {
    ui.label(RichText::new(label).color(MUTED).size(12.0));
    ui.label(RichText::new(value).size(12.5));
    ui.end_row();
}

pub(crate) fn similarity_between(a: &PhotoRecord, b: &PhotoRecord) -> (f32, String) {
    if a.pixel_hash.is_some() && a.pixel_hash == b.pixel_hash {
        return (
            100.0,
            "Identical normalised pixels (exact SHA-256 match).".into(),
        );
    }
    match (a.phash, b.phash) {
        (Some(ha), Some(hb)) => {
            let distance = phash_distance(ha, hb);
            (
                similarity_for_distance(distance),
                format!(
                    "Hamming distance {distance} of 63 bits → {:.1}% heuristic similarity. \
                     This percentage is an indicator, not a probability.",
                    similarity_for_distance(distance)
                ),
            )
        }
        _ => (0.0, "No perceptual hash available.".into()),
    }
}

pub(crate) fn human_count(n: u64) -> String {
    if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}
