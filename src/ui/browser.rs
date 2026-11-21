//! In-app directory browser used when no native dialog is available.
//!
//! The user never has to type a path: folders are navigated by clicking.

use std::path::PathBuf;

use eframe::egui::{self, Context, RichText};

use crate::dirpicker::PickTarget;

use super::widgets::{MUTED, WARN};

pub(crate) enum BrowserAction {
    None,
    Selected(PathBuf),
    Cancelled,
}

pub(crate) struct DirBrowser {
    pub target: PickTarget,
    current: PathBuf,
    entries: Vec<PathBuf>,
    error: Option<String>,
}

impl DirBrowser {
    pub(crate) fn new(target: PickTarget, start: Option<PathBuf>) -> Self {
        let mut start = start.unwrap_or_else(crate::dirpicker::default_start_dir);
        if start.is_file() {
            start.pop();
        }
        if !start.is_dir() {
            start = crate::dirpicker::default_start_dir();
        }
        let mut browser = Self {
            target,
            current: start,
            entries: Vec::new(),
            error: None,
        };
        browser.refresh();
        browser
    }

    fn refresh(&mut self) {
        self.entries.clear();
        self.error = None;
        match std::fs::read_dir(&self.current) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        self.entries.push(path);
                    }
                }
                self.entries.sort_by(|a, b| {
                    a.file_name()
                        .map(|n| n.to_string_lossy().to_lowercase())
                        .cmp(&b.file_name().map(|n| n.to_string_lossy().to_lowercase()))
                });
            }
            Err(err) => {
                self.error = Some(format!("Cannot read {}: {err}", self.current.display()));
            }
        }
    }

    fn navigate(&mut self, path: PathBuf) {
        if path.is_dir() {
            self.current = path;
            self.refresh();
        }
    }

    pub(crate) fn show(&mut self, ctx: &Context) -> BrowserAction {
        let mut action = BrowserAction::None;
        let mut open = true;
        let title = format!("Choose {}", self.target.label());
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([640.0, 500.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Home").clicked() {
                        self.navigate(crate::dirpicker::default_start_dir());
                    }
                    if ui
                        .add_enabled(self.current.parent().is_some(), egui::Button::new("Up"))
                        .clicked()
                    {
                        if let Some(parent) = self.current.parent() {
                            self.navigate(parent.to_path_buf());
                        }
                    }
                    ui.label(RichText::new(self.current.display().to_string()).monospace());
                });
                ui.separator();

                if let Some(error) = &self.error {
                    ui.label(RichText::new(error).color(WARN));
                }

                let mut navigate_to: Option<PathBuf> = None;
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.entries.is_empty() {
                            ui.label(RichText::new("No sub-folders here.").color(MUTED));
                        }
                        for entry in &self.entries {
                            let name = entry
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            if ui.selectable_label(false, format!("{name}/")).clicked() {
                                navigate_to = Some(entry.clone());
                            }
                        }
                    });
                if let Some(path) = navigate_to {
                    self.navigate(path);
                }

                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(self.current.is_dir(), egui::Button::new("Use this folder"))
                        .clicked()
                    {
                        action = BrowserAction::Selected(self.current.clone());
                    }
                    if ui.button("Cancel").clicked() {
                        action = BrowserAction::Cancelled;
                    }
                    ui.label(
                        RichText::new(format!("Selected: {}", self.current.display()))
                            .color(MUTED)
                            .size(11.5),
                    );
                });
            });

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            action = BrowserAction::Cancelled;
        }
        if !open {
            action = BrowserAction::Cancelled;
        }
        action
    }
}
