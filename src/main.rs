//! Pixsort desktop entry point.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> eframe::Result {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,pixsort=info"),
    )
    .format_timestamp_secs()
    .init();

    let (settings, warning) = pixsort::config::Settings::load();
    if let Some(warning) = &warning {
        log::warn!("{warning}");
    }
    pixsort::ui::run(settings, warning)
}
