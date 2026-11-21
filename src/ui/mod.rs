//! egui/eframe desktop interface.
//!
//! The UI is deliberately thin: it reads the SQLite plan, starts background
//! jobs and never performs filesystem changes itself.

mod app;
mod browser;
mod image_cache;
mod pages;
mod types;
mod widgets;

pub(crate) use app::PixsortApp;

/// Applies the small, functional visual style used by all screens.
pub(crate) fn install_style(ctx: &eframe::egui::Context) {
    use eframe::egui::{FontFamily, FontId, TextStyle};
    ctx.all_styles_mut(|style| {
        style.text_styles.insert(
            TextStyle::Heading,
            FontId::new(22.0, FontFamily::Proportional),
        );
        style
            .text_styles
            .insert(TextStyle::Body, FontId::new(14.5, FontFamily::Proportional));
        style.text_styles.insert(
            TextStyle::Button,
            FontId::new(14.5, FontFamily::Proportional),
        );
        style.spacing.item_spacing = eframe::egui::vec2(8.0, 7.0);
        style.spacing.button_padding = eframe::egui::vec2(10.0, 5.0);
    });
}

use crate::config::Settings;

/// Starts the native desktop application.
pub fn run(settings: Settings, load_warning: Option<String>) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 880.0])
            .with_min_inner_size([980.0, 620.0])
            .with_title("Pixsort"),
        centered: true,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "Pixsort",
        options,
        Box::new(move |cc| Ok(Box::new(PixsortApp::new(cc, settings, load_warning)))),
    )
}
