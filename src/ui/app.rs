//! Application shell: state, navigation, background-job polling and dialogs.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, Ui};

use crate::config::Settings;
use crate::db::{IndexDb, IndexStats, PlanDb, SessionStatus};
use crate::dirpicker::{self, PickOutcome, PickRequest, PickTarget};
use crate::imaging::heic;
use crate::jobs::{JobEvent, JobHandle, JobKind};
use crate::model::{AnalysisSummary, ApplyReport, ImportSummary, Progress};
use crate::planner;

use super::browser::{BrowserAction, DirBrowser};
use super::image_cache::ImageCache;
use super::pages;
use super::pages::plan::{PlanAction, PlanUiState};
use super::pages::review::{ReviewAction, ReviewUiState};
use super::types::{Page, PlanView, RunningJob, Toast};
use super::widgets::{self, ACCENT, BAD, GOOD, MUTED, WARN};

pub(crate) struct PixsortApp {
    pub(crate) settings: Settings,
    pub(crate) saved_settings: Settings,
    pub(crate) page: Page,
    pub(crate) job: Option<RunningJob>,
    pub(crate) progress: Option<Progress>,
    pub(crate) last_import: Option<ImportSummary>,
    pub(crate) last_analysis: Option<AnalysisSummary>,
    pub(crate) last_apply: Option<ApplyReport>,
    pub(crate) plan: Option<PlanView>,
    pub(crate) plan_error: Option<String>,
    pub(crate) logs: VecDeque<String>,
    pub(crate) error: Option<String>,
    pub(crate) startup_note: Option<String>,
    pub(crate) image_cache: ImageCache,
    pub(crate) picker: Option<PickRequest>,
    pub(crate) browser: Option<DirBrowser>,
    pub(crate) show_apply_modal: bool,
    pub(crate) apply_ack: bool,
    pub(crate) show_discard_modal: bool,
    pub(crate) discard_ack: bool,
    pub(crate) toast: Option<Toast>,
    pub(crate) review_state: ReviewUiState,
    pub(crate) plan_state: PlanUiState,
    pub(crate) heic_status: String,
    pub(crate) stats_cache: Option<(Instant, IndexStats)>,
    pub(crate) last_progress_line: Option<String>,
    pub(crate) show_logs: bool,
    pub(crate) loading_plan: bool,
    screenshot_deadline: Option<Instant>,
    screenshot_requested: bool,
}

impl PixsortApp {
    pub(crate) fn new(
        cc: &eframe::CreationContext<'_>,
        settings: Settings,
        load_warning: Option<String>,
    ) -> Self {
        super::install_style(&cc.egui_ctx);
        let saved = settings.clone();
        let start_page = match std::env::var("PIXSORT_START_PAGE")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "import" => Page::Import,
            "analysis" | "analyse" => Page::Analysis,
            "review" => Page::Review,
            "plan" => Page::Plan,
            "settings" => Page::Settings,
            _ => Page::Dashboard,
        };
        let mut app = Self {
            settings,
            saved_settings: saved,
            page: start_page,
            job: None,
            progress: None,
            last_import: None,
            last_analysis: None,
            last_apply: None,
            plan: None,
            plan_error: None,
            logs: VecDeque::new(),
            error: None,
            startup_note: load_warning,
            image_cache: ImageCache::new(),
            picker: None,
            browser: None,
            show_apply_modal: false,
            apply_ack: false,
            show_discard_modal: false,
            discard_ack: false,
            toast: None,
            review_state: ReviewUiState::default(),
            plan_state: PlanUiState::default(),
            heic_status: heic::availability_message(),
            stats_cache: None,
            last_progress_line: None,
            show_logs: false,
            loading_plan: false,
            screenshot_deadline: std::env::var("PIXSORT_SCREENSHOT_AFTER_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .map(|ms| Instant::now() + Duration::from_millis(ms)),
            screenshot_requested: false,
        };
        if let Some(note) = app.repair_plan_if_needed() {
            app.startup_note = Some(match app.startup_note.take() {
                Some(existing) => format!("{existing} {note}"),
                None => note,
            });
        }
        app.refresh_plan();
        app
    }

    fn repair_plan_if_needed(&mut self) -> Option<String> {
        let path = self.settings.effective_plan_db()?;
        if !path.is_file() || self.settings.destination_dir.is_none() {
            return None;
        }
        let mut db = match PlanDb::open(&path) {
            Ok(db) => db,
            Err(err) => return Some(format!("Could not open the stored plan: {err}")),
        };
        let session = match db.current_session() {
            Ok(Some(session)) => session,
            Ok(None) => return None,
            Err(err) => return Some(format!("Could not read the stored plan: {err}")),
        };
        if session.plan_dirty
            && session.status == SessionStatus::Active
            && self.settings.effective_index_db().is_some()
        {
            if let Err(err) = planner::rebuild_plan(&mut db, &session, &self.settings) {
                return Some(format!(
                    "The plan was interrupted and could not be rebuilt automatically: {err:#}"
                ));
            }
            return Some("Recovered the persisted plan after restart.".into());
        }
        None
    }

    pub(crate) fn refresh_plan(&mut self) {
        self.loading_plan = true;
        match PlanView::load(&self.settings) {
            Ok(plan) => {
                self.plan = plan;
                self.plan_error = None;
                // Keep a valid review selection.
                if let (Some(plan), Some(selected)) = (&self.plan, self.review_state.selected) {
                    if !plan.groups.iter().any(|g| g.id == selected) {
                        self.review_state.selected = None;
                    }
                }
            }
            Err(err) => {
                self.plan_error = Some(format!("{err:#}"));
            }
        }
        self.stats_cache = None;
        self.loading_plan = false;
    }

    pub(crate) fn index_stats(&mut self) -> IndexStats {
        if let Some((at, stats)) = &self.stats_cache {
            if at.elapsed() < Duration::from_secs(2) || self.job.is_some() {
                return stats.clone();
            }
        }
        let Some(path) = self.settings.effective_index_db() else {
            return IndexStats::default();
        };
        if !path.is_file() {
            return IndexStats::default();
        }
        match IndexDb::open_read_only(&path).and_then(|db| db.stats()) {
            Ok(stats) => {
                self.stats_cache = Some((Instant::now(), stats.clone()));
                stats
            }
            Err(err) => {
                log::warn!("cannot read index stats: {err:#}");
                IndexStats::default()
            }
        }
    }

    pub(crate) fn handle_progress_action(&mut self, action: widgets::ProgressAction) {
        match action {
            widgets::ProgressAction::None => {}
            widgets::ProgressAction::Pause => {
                if let Some(job) = &self.job {
                    job.handle.control.pause();
                    self.toast = Some(Toast::info("Processing paused."));
                }
            }
            widgets::ProgressAction::Resume => {
                if let Some(job) = &self.job {
                    job.handle.control.resume();
                    self.toast = Some(Toast::info("Processing resumed."));
                }
            }
            widgets::ProgressAction::Stop => {
                if let Some(job) = &self.job {
                    job.handle.control.stop();
                    self.toast = Some(Toast::info(
                        "Stopping after the current item. Completed work is saved.",
                    ));
                }
            }
        }
    }

    fn spawn_job(
        &mut self,
        kind: JobKind,
        initial_phase: &str,
        job: impl FnOnce(
                Settings,
                std::sync::Arc<crate::model::JobControl>,
                crate::jobs::ProgressReporter,
            ) -> Result<(), String>
            + Send
            + 'static,
    ) {
        if self.job.is_some() {
            self.error = Some("A background job is already running.".into());
            return;
        }
        let settings = self.settings.clone();
        let handle = JobHandle::spawn(kind, move |control, reporter| {
            job(settings, control, reporter)
        });
        self.job = Some(RunningJob::new(handle));
        self.progress = Some(Progress::new(initial_phase));
        self.error = None;
        self.last_progress_line = None;
    }

    pub(crate) fn start_import_job(&mut self) {
        if self.settings.validate_for_import().is_err() {
            self.error = Some(
                self.settings
                    .validate_for_import()
                    .err()
                    .map(|e| format!("{e:#}"))
                    .unwrap_or_default(),
            );
            self.page = Page::Settings;
            return;
        }
        if let Some(plan) = &self.plan {
            if plan.is_active() && plan.unfinished_operations() > 0 {
                self.error = Some(
                    "A plan with unfinished operations exists. Finish or discard it first.".into(),
                );
                self.page = Page::Plan;
                return;
            }
        }
        self.last_import = None;
        self.page = Page::Import;
        self.spawn_job(
            JobKind::Import,
            "Starting import",
            |settings, control, reporter| match crate::import::run_import(
                &settings, &control, &reporter,
            ) {
                Ok(summary) => {
                    reporter.send(JobEvent::ImportDone(summary));
                    Ok(())
                }
                Err(err) => Err(format!("{err:#}")),
            },
        );
    }

    pub(crate) fn start_analysis_job(&mut self) {
        if let Err(err) = self.settings.validate_for_analysis() {
            self.error = Some(format!("{err:#}"));
            self.page = Page::Settings;
            return;
        }
        if let Some(plan) = &self.plan {
            if plan.is_active() && plan.unfinished_operations() > 0 {
                self.error = Some(
                    "A plan with unfinished operations exists. Finish or discard it first.".into(),
                );
                self.page = Page::Plan;
                return;
            }
        }
        self.last_analysis = None;
        self.page = Page::Analysis;
        self.spawn_job(
            JobKind::Analysis,
            "Starting archive analysis",
            |settings, control, reporter| match crate::analysis::run_analysis(
                &settings, &control, &reporter,
            ) {
                Ok(summary) => {
                    reporter.send(JobEvent::AnalysisDone(summary));
                    Ok(())
                }
                Err(err) => Err(format!("{err:#}")),
            },
        );
    }

    fn start_apply_job(&mut self, confirmed: bool) {
        if !confirmed {
            return;
        }
        self.last_apply = None;
        self.page = Page::Plan;
        self.show_apply_modal = false;
        self.apply_ack = false;
        self.spawn_job(
            JobKind::Apply,
            "Applying plan",
            |settings, control, reporter| match crate::executor::apply_plan(
                &settings, true, &control, &reporter,
            ) {
                Ok(report) => {
                    reporter.send(JobEvent::ApplyDone(report));
                    Ok(())
                }
                Err(err) => Err(format!("{err:#}")),
            },
        );
    }

    pub(crate) fn save_settings(&mut self) {
        self.settings.normalize();
        match self.settings.save() {
            Ok(()) => {
                self.saved_settings = self.settings.clone();
                self.toast = Some(Toast::info(format!(
                    "Settings saved to {}",
                    crate::config::settings_path().display()
                )));
                self.stats_cache = None;
                self.refresh_plan();
            }
            Err(err) => {
                self.error = Some(format!("Could not save settings: {err:#}"));
                self.toast = Some(Toast::error("Could not save settings."));
            }
        }
    }

    pub(crate) fn start_picker(&mut self, target: PickTarget) {
        if self.job.is_some() {
            self.toast = Some(Toast::error(
                "Wait for the running job to finish before changing locations.",
            ));
            return;
        }
        if self.picker.is_some() || self.browser.is_some() {
            return;
        }
        let initial = self.picker_start(target);
        let title = match target {
            PickTarget::Input => "Pixsort — choose the input folder",
            PickTarget::Destination => "Pixsort — choose the archive root",
            PickTarget::IndexDbDir => "Pixsort — choose the image-index database folder",
            PickTarget::PlanDbDir => "Pixsort — choose the plan database folder",
            PickTarget::Trash => "Pixsort — choose the trash folder",
        };
        self.picker = Some(dirpicker::request_directory(target, initial, title));
    }

    fn picker_start(&self, target: PickTarget) -> Option<PathBuf> {
        match target {
            PickTarget::Input => self.settings.input_dir.clone(),
            PickTarget::Destination => self.settings.destination_dir.clone(),
            PickTarget::IndexDbDir => self.settings.effective_index_db(),
            PickTarget::PlanDbDir => self.settings.effective_plan_db(),
            PickTarget::Trash => self.settings.effective_trash_dir(),
        }
    }

    fn apply_picked_path(&mut self, target: PickTarget, path: PathBuf) {
        if !path.is_dir() {
            self.toast = Some(Toast::error(format!(
                "{} is not a directory.",
                path.display()
            )));
            return;
        }
        match target {
            PickTarget::Input => self.settings.input_dir = Some(path.clone()),
            PickTarget::Destination => self.settings.destination_dir = Some(path.clone()),
            PickTarget::Trash => self.settings.trash_dir = Some(path.clone()),
            PickTarget::IndexDbDir => {
                let name = self
                    .settings
                    .index_db_path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_os_string())
                    .unwrap_or_else(|| "index.sqlite".into());
                self.settings.index_db_path = Some(path.join(name));
            }
            PickTarget::PlanDbDir => {
                let name = self
                    .settings
                    .plan_db_path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_os_string())
                    .unwrap_or_else(|| "plan.sqlite".into());
                self.settings.plan_db_path = Some(path.join(name));
            }
        }
        self.settings.normalize();
        match self.settings.save() {
            Ok(()) => {
                self.saved_settings = self.settings.clone();
                self.toast = Some(Toast::info(format!("Updated {}.", target.label())));
            }
            Err(err) => {
                self.error = Some(format!("Could not save settings: {err:#}"));
            }
        }
        self.stats_cache = None;
        self.refresh_plan();
    }

    fn poll_picker(&mut self, ctx: &egui::Context) {
        let outcome = self
            .picker
            .as_ref()
            .and_then(|request| request.poll().map(|outcome| (request.target, outcome)));
        if let Some((target, outcome)) = outcome {
            self.picker = None;
            match outcome {
                PickOutcome::Selected(path) => self.apply_picked_path(target, path),
                PickOutcome::Cancelled => {}
                PickOutcome::Unavailable(message) => {
                    self.log(format!(
                        "No native folder dialog available ({message}); using the built-in browser."
                    ));
                    self.browser = Some(DirBrowser::new(target, self.picker_start(target)));
                    ctx.request_repaint();
                }
            }
        }

        if let Some(mut browser) = self.browser.take() {
            let target = browser.target;
            match browser.show(ctx) {
                BrowserAction::Selected(path) => {
                    self.apply_picked_path(target, path);
                }
                BrowserAction::Cancelled => {}
                BrowserAction::None => {
                    self.browser = Some(browser);
                }
            }
        }
    }

    fn poll_job(&mut self, ctx: &egui::Context) {
        let mut events = Vec::new();
        if let Some(job) = &self.job {
            while let Ok(event) = job.handle.receiver.try_recv() {
                events.push(event);
            }
        }
        let mut finished = false;
        for event in events {
            match event {
                JobEvent::Progress(progress) => {
                    self.progress = Some(progress);
                }
                JobEvent::Log(message) => {
                    self.log(message);
                }
                JobEvent::ImportDone(summary) => {
                    self.last_progress_line = self
                        .progress
                        .as_ref()
                        .map(|p| format!("Import · {} files processed", p.completed));
                    self.last_import = Some(summary);
                    self.page = Page::Import;
                    self.toast = Some(
                        if self
                            .last_import
                            .as_ref()
                            .map(|s| s.stopped)
                            .unwrap_or(false)
                        {
                            Toast::info("Import stopped; progress is saved.")
                        } else {
                            Toast::info("Import finished. The plan is ready for review.")
                        },
                    );
                    finished = true;
                }
                JobEvent::AnalysisDone(summary) => {
                    self.last_analysis = Some(summary);
                    self.page = Page::Analysis;
                    self.toast = Some(Toast::info("Archive analysis finished."));
                    finished = true;
                }
                JobEvent::ApplyDone(report) => {
                    self.last_apply = Some(report);
                    self.page = Page::Plan;
                    self.toast = Some(Toast::info("Plan execution finished."));
                    finished = true;
                }
                JobEvent::Failed(message) => {
                    self.error = Some(message.clone());
                    self.log(format!("Job failed: {message}"));
                    self.toast = Some(Toast::error("The background job failed."));
                    finished = true;
                }
            }
        }
        if finished {
            self.job = None;
            self.refresh_plan();
        }
        if self.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }

    pub(crate) fn log(&mut self, message: impl Into<String>) {
        let message = message.into();
        log::info!("{message}");
        self.logs.push_back(message);
        while self.logs.len() > 300 {
            self.logs.pop_front();
        }
    }

    pub(crate) fn last_progress_summary(&self) -> Option<String> {
        if self.job.is_some() {
            return self
                .progress
                .as_ref()
                .map(crate::jobs::ProgressReporter::pretty_progress);
        }
        self.last_progress_line.clone()
    }

    // -- review / plan actions --------------------------------------------

    fn handle_review_action(&mut self, action: ReviewAction) {
        let Some(plan) = &self.plan else { return };
        let session = plan.session.clone();
        let Some(path) = self.settings.effective_plan_db() else {
            return;
        };
        let mut db = match PlanDb::open(&path) {
            Ok(db) => db,
            Err(err) => {
                self.error = Some(format!("Could not open the plan: {err:#}"));
                return;
            }
        };
        match action {
            ReviewAction::Decide {
                group_id,
                decision,
                keeper,
            } => {
                match planner::resolve_group(
                    &mut db,
                    &session,
                    &self.settings,
                    group_id,
                    decision,
                    keeper.as_deref(),
                ) {
                    Ok(()) => {
                        self.toast = Some(Toast::info("Decision saved; the plan was rebuilt."));
                        self.image_cache.clear();
                    }
                    Err(err) => {
                        self.error = Some(format!("{err:#}"));
                    }
                }
            }
            ReviewAction::AcceptProbable => {
                match planner::accept_probable_recommendations(&mut db, &session, &self.settings) {
                    Ok(count) => {
                        self.toast = Some(Toast::info(format!(
                            "Accepted {count} probable duplicate recommendation(s)."
                        )));
                    }
                    Err(err) => {
                        self.error = Some(format!("{err:#}"));
                    }
                }
            }
        }
        self.refresh_plan();
    }

    fn handle_plan_action(&mut self, action: PlanAction) {
        match action {
            PlanAction::ApplyRequested => {
                self.show_apply_modal = true;
                self.apply_ack = false;
            }
            PlanAction::ReviewRequested => {
                self.page = Page::Review;
            }
            PlanAction::Refresh => {
                self.refresh_plan();
                self.toast = Some(Toast::info("Plan reloaded from disk."));
            }
            PlanAction::DiscardRequested => {
                self.show_discard_modal = true;
                self.discard_ack = false;
            }
            PlanAction::Progress(action) => self.handle_progress_action(action),
        }
    }

    pub(crate) fn validate_reconnect(&mut self) {
        match self.settings.validate_for_resume() {
            Ok(()) => {
                self.toast = Some(Toast::info(
                    "All configured locations are accessible; the session can be resumed.",
                ));
            }
            Err(err) => {
                let message = format!("{err:#}");
                self.error = Some(message.clone());
                self.toast = Some(Toast::error(message));
            }
        }
    }

    pub(crate) fn check_databases(&mut self) {
        let mut messages = Vec::new();
        if let Some(path) = self.settings.effective_index_db() {
            if path.is_file() {
                match IndexDb::open_read_only(&path).and_then(|db| db.integrity_check()) {
                    Ok(()) => messages.push(format!("Image index OK ({})", path.display())),
                    Err(err) => messages.push(format!("Image index problem: {err:#}")),
                }
            } else {
                messages.push("Image index does not exist yet.".into());
            }
        }
        if let Some(path) = self.settings.effective_plan_db() {
            if path.is_file() {
                match PlanDb::open_read_only(&path).and_then(|db| db.integrity_check()) {
                    Ok(()) => messages.push(format!("Plan database OK ({})", path.display())),
                    Err(err) => messages.push(format!("Plan database problem: {err:#}")),
                }
            } else {
                messages.push("Plan database does not exist yet.".into());
            }
        }
        for message in &messages {
            self.log(message.clone());
        }
        self.toast = Some(Toast::info(messages.join(" · ")));
    }

    fn discard_current_session(&mut self) {
        let Some(path) = self.settings.effective_plan_db() else {
            return;
        };
        let mut db = match PlanDb::open(&path) {
            Ok(db) => db,
            Err(err) => {
                self.error = Some(format!("Could not open the plan: {err:#}"));
                return;
            }
        };
        let session = match db.current_session() {
            Ok(Some(session)) => session,
            Ok(None) => return,
            Err(err) => {
                self.error = Some(format!("{err:#}"));
                return;
            }
        };
        if session.status == SessionStatus::Applying {
            self.toast = Some(Toast::error(
                "The plan is being applied; stop the job before discarding.",
            ));
            return;
        }
        match db.delete_session(session.id) {
            Ok(()) => {
                self.log(format!(
                    "Discarded session {} ({}). Database files were kept.",
                    session.id,
                    session.mode.label()
                ));
                self.toast = Some(Toast::info(
                    "Session discarded. Database files and photographs were not deleted.",
                ));
                self.show_discard_modal = false;
                self.image_cache.clear();
                self.refresh_plan();
            }
            Err(err) => {
                self.error = Some(format!("Could not discard the session: {err:#}"));
            }
        }
    }

    // -- chrome ------------------------------------------------------------

    fn top_bar(&mut self, ui: &mut Ui) {
        egui::Frame::new()
            .fill(Color32::from_rgb(22, 25, 31))
            .inner_margin(egui::Margin::symmetric(14, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Pixsort").size(24.0).strong().color(ACCENT));
                    ui.label(
                        RichText::new("local photo archive organizer")
                            .color(MUTED)
                            .size(12.5),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(job) = &self.job {
                            ui.spinner();
                            let phase = self
                                .progress
                                .as_ref()
                                .map(|p| p.phase.clone())
                                .unwrap_or_else(|| "working".into());
                            ui.label(
                                RichText::new(format!("{}: {phase}", job.kind().label()))
                                    .color(WARN),
                            );
                            if job.handle.control.is_paused() {
                                widgets::badge(ui, "paused", WARN);
                            }
                        } else {
                            ui.label(RichText::new("idle").color(MUTED));
                        }
                        if let Some(plan) = &self.plan {
                            widgets::badge(ui, plan.session.status.label(), GOOD);
                        }
                        if ui.button("Settings").clicked() {
                            self.page = Page::Settings;
                        }
                    });
                });
            });
    }

    fn nav(&mut self, ui: &mut Ui) {
        ui.add_space(8.0);
        let review_badge = self
            .plan
            .as_ref()
            .map(|p| p.unresolved_reviews())
            .unwrap_or(0);
        for page in Page::ALL {
            let selected = self.page == page;
            let mut label = page.short_label().to_string();
            if page == Page::Review && review_badge > 0 {
                label.push_str(&format!("  ({review_badge})"));
            }
            if page == Page::Plan
                && self
                    .plan
                    .as_ref()
                    .map(|p| p.counts.unfinished())
                    .unwrap_or(0)
                    > 0
            {
                label.push_str("  •");
            }
            let button = egui::Button::new(label)
                .min_size(egui::vec2(ui.available_width(), 32.0))
                .fill(if selected {
                    ACCENT.gamma_multiply(0.35)
                } else {
                    Color32::TRANSPARENT
                });
            if ui.add(button).on_hover_text(page.label()).clicked() {
                self.page = page;
            }
        }
        ui.add_space(12.0);
        ui.separator();
        ui.label(RichText::new("Configuration").color(MUTED).size(11.5));
        let input = self
            .settings
            .input_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "no input folder".into());
        let archive = self
            .settings
            .destination_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "no archive".into());
        ui.add(egui::Label::new(RichText::new(input).size(10.5).color(MUTED)).wrap());
        ui.add(egui::Label::new(RichText::new(archive).size(10.5).color(MUTED)).wrap());
        if let Some(plan) = &self.plan {
            ui.add_space(8.0);
            ui.label(RichText::new("Current session").color(MUTED).size(11.5));
            ui.label(format!(
                "{} · {}",
                plan.session.mode.label(),
                plan.session.status.label()
            ));
            if plan.counts.unfinished() > 0 {
                ui.label(
                    RichText::new(format!(
                        "{} pending · {} failed",
                        plan.counts.pending, plan.counts.failed
                    ))
                    .size(11.0)
                    .color(WARN),
                );
            }
        }
        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.label(
                RichText::new(format!("HEIC: {}", self.heic_status))
                    .size(10.0)
                    .color(MUTED),
            );
            if self.show_logs {
                ui.collapsing("Activity log", |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(180.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for line in self.logs.iter() {
                                ui.label(RichText::new(line).size(10.0).color(MUTED));
                            }
                        });
                });
            }
            ui.checkbox(&mut self.show_logs, "Show activity log");
        });
    }

    fn page_ui(&mut self, ui: &mut Ui) {
        match self.page {
            Page::Dashboard => pages::dashboard::show(self, ui),
            Page::Import => pages::import::show(self, ui),
            Page::Analysis => pages::analysis::show(self, ui),
            Page::Settings => pages::settings::show(self, ui),
            Page::Review => {
                let action = pages::review::show(
                    self.plan.as_ref(),
                    &self.settings,
                    &mut self.image_cache,
                    &mut self.review_state,
                    ui,
                );
                if let Some(action) = action {
                    self.handle_review_action(action);
                }
            }
            Page::Plan => {
                let action = pages::plan::show(
                    self.plan.as_ref(),
                    &self.settings,
                    self.last_apply.as_ref(),
                    self.progress.as_ref(),
                    self.job.as_ref(),
                    &mut self.plan_state,
                    ui,
                );
                if let Some(action) = action {
                    self.handle_plan_action(action);
                }
            }
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if self.show_apply_modal {
            let mut close = false;
            let mut confirm = false;
            egui::Window::new("Apply the modification plan?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_max_width(560.0);
                    if let Some(plan) = &self.plan {
                        ui.label(
                            RichText::new(
                                "This is the first and only step that modifies the archive.",
                            )
                            .strong()
                            .color(WARN),
                        );
                        ui.add_space(4.0);
                        ui.label(format!(
                            "{} file(s) will be copied into the archive, {} replaced, {} moved to trash, {} skipped.",
                            plan.counts.inserts,
                            plan.counts.replaces,
                            plan.counts.moves_to_trash,
                            plan.counts.skips
                        ));
                        ui.label(format!(
                            "Files are moved to: {}",
                            self.settings
                                .effective_trash_dir()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| "not configured".into())
                        ));
                        ui.label(
                            RichText::new(
                                "Existing files are never overwritten. If execution is interrupted, completed operations are detected and the rest can be resumed.",
                            )
                            .color(MUTED),
                        );
                    } else {
                        ui.label("No plan is loaded.");
                    }
                    ui.separator();
                    ui.checkbox(
                        &mut self.apply_ack,
                        "I reviewed the plan and approve modifying the archive.",
                    );
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                self.apply_ack && self.job.is_none() && self.plan.is_some(),
                                egui::Button::new("Apply plan"),
                            )
                            .clicked()
                        {
                            confirm = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            if confirm {
                self.start_apply_job(true);
            } else if close {
                self.show_apply_modal = false;
                self.apply_ack = false;
            }
        }

        if self.show_discard_modal {
            let mut close = false;
            let mut confirm = false;
            egui::Window::new("Discard the current session?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_max_width(520.0);
                    ui.label(
                        "This removes the session's duplicate groups, decisions and plan rows. \
                         Database files and photographs are kept.",
                    );
                    if let Some(plan) = &self.plan {
                        if plan.counts.unfinished() > 0 {
                            ui.label(
                                RichText::new(format!(
                                    "{} unfinished operation(s) will be forgotten. Files already written remain in place.",
                                    plan.counts.unfinished()
                                ))
                                .color(WARN),
                            );
                        }
                    }
                    ui.separator();
                    ui.checkbox(
                        &mut self.discard_ack,
                        "I understand this is irreversible for the plan.",
                    );
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(self.discard_ack && self.job.is_none(), egui::Button::new("Discard session"))
                            .clicked()
                        {
                            confirm = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            if confirm {
                self.discard_current_session();
            } else if close {
                self.show_discard_modal = false;
                self.discard_ack = false;
            }
        }

        if let Some(toast) = &self.toast {
            let age = toast.created.elapsed();
            if age > Duration::from_secs(7) {
                self.toast = None;
            } else {
                let mut dismiss = false;
                let color = if toast.error { BAD } else { GOOD };
                egui::Window::new("message")
                    .title_bar(false)
                    .resizable(false)
                    .anchor(egui::Align2::RIGHT_BOTTOM, [-18.0, -18.0])
                    .show(ctx, |ui| {
                        ui.set_max_width(460.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&toast.message).color(color));
                            if ui.small_button("x").clicked() {
                                dismiss = true;
                            }
                        });
                    });
                if dismiss {
                    self.toast = None;
                }
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }

        if let Some(error) = &self.error {
            if !error.is_empty() && self.page != Page::Dashboard {
                let mut dismiss = false;
                egui::Window::new("Last problem")
                    .collapsible(false)
                    .resizable(true)
                    .default_size([540.0, 180.0])
                    .show(ctx, |ui| {
                        ui.label(RichText::new(error).color(BAD));
                        if ui.button("Dismiss").clicked() {
                            dismiss = true;
                        }
                    });
                if dismiss {
                    self.error = None;
                }
            }
        }
    }

    /// Development aid: capture this window after a delay (used for visual
    /// checks and documentation screenshots).
    fn maybe_take_screenshot(&mut self, ctx: &egui::Context) {
        let Some(deadline) = self.screenshot_deadline else {
            return;
        };
        if self.screenshot_requested || Instant::now() < deadline {
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        self.screenshot_requested = true;
        let path = std::env::var("PIXSORT_SCREENSHOT_PATH")
            .or_else(|_| std::env::var("EFRAME_SCREENSHOT_TO"));
        let Ok(path) = path else {
            return;
        };
        let callback = egui::ScreenshotCallback::new(move |image| {
            let width = image.size[0] as u32;
            let height = image.size[1] as u32;
            let mut rgba = Vec::with_capacity(image.pixels.len() * 4);
            for pixel in &image.pixels {
                rgba.extend_from_slice(&pixel.to_array());
            }
            match image::save_buffer(&path, &rgba, width, height, image::ColorType::Rgba8) {
                Ok(()) => log::info!("Saved screenshot to {path}"),
                Err(err) => log::error!("Could not save screenshot to {path}: {err}"),
            }
            std::process::exit(0);
        });
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(callback));
        ctx.request_repaint();
    }
}

impl eframe::App for PixsortApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_job(&ctx);
        self.poll_picker(&ctx);
        self.image_cache.poll(&ctx);

        egui::Panel::top("pixsort-top")
            .frame(egui::Frame::NONE)
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::left("pixsort-nav")
            .resizable(false)
            .default_size(205.0)
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(25, 28, 34))
                    .inner_margin(egui::Margin::symmetric(8, 6)),
            )
            .show(ui, |ui| self.nav(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(plan_error) = &self.plan_error {
                ui.label(RichText::new(format!("Plan warning: {plan_error}")).color(BAD));
            }
            self.page_ui(ui);
        });

        self.dialogs(&ctx);

        if self.job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        self.maybe_take_screenshot(&ctx);
    }
}

impl Drop for PixsortApp {
    fn drop(&mut self) {
        if self.settings != self.saved_settings {
            let _ = self.settings.save();
        }
    }
}
