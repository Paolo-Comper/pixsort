//! Background job plumbing: progress reporting and job result events.
//!
//! Service functions run on their own thread. They report through a
//! `ProgressReporter`, which throttles messages so the GUI never drowns in
//! events while processing tens of thousands of photographs.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::model::{
    format_duration, AnalysisSummary, ApplyReport, ImportSummary, JobControl, Progress,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Import,
    Analysis,
    Apply,
}

impl JobKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Import => "Import",
            Self::Analysis => "Archive analysis",
            Self::Apply => "Applying plan",
        }
    }
}

#[derive(Debug)]
pub enum JobEvent {
    Progress(Progress),
    Log(String),
    ImportDone(ImportSummary),
    AnalysisDone(AnalysisSummary),
    ApplyDone(ApplyReport),
    Failed(String),
}

/// Receives (control, receiver) for the GUI.
pub struct JobHandle {
    pub kind: JobKind,
    pub control: Arc<JobControl>,
    pub receiver: Receiver<JobEvent>,
}

impl JobHandle {
    pub fn spawn(
        kind: JobKind,
        job: impl FnOnce(Arc<JobControl>, ProgressReporter) -> Result<(), String> + Send + 'static,
    ) -> Self {
        let control = Arc::new(JobControl::new());
        let (tx, rx) = mpsc::channel();
        let reporter = ProgressReporter::new(tx.clone());
        let child_control = control.clone();
        std::thread::Builder::new()
            .name(format!("pixsort-{}-job", kind.label().to_lowercase()))
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    job(child_control, reporter)
                }));
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        let _ = tx.send(JobEvent::Failed(err));
                    }
                    Err(panic) => {
                        let message = panic
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".to_string());
                        let _ = tx.send(JobEvent::Failed(format!(
                            "The background job panicked: {message}"
                        )));
                    }
                }
            })
            .expect("failed to spawn background job");
        Self {
            kind,
            control,
            receiver: rx,
        }
    }
}

/// Wraps a sender and a set of counters for one operation phase.
pub struct ProgressReporter {
    tx: Arc<Mutex<Sender<JobEvent>>>,
    phase: Mutex<String>,
    current: Mutex<String>,
    total: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
    reused: AtomicU64,
    started: Mutex<Instant>,
    last_emit: Mutex<Instant>,
}

impl Clone for ProgressReporter {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            phase: Mutex::new(self.phase.lock().map(|s| s.clone()).unwrap_or_default()),
            current: Mutex::new(self.current.lock().map(|s| s.clone()).unwrap_or_default()),
            total: AtomicU64::new(self.total.load(Ordering::Relaxed)),
            completed: AtomicU64::new(self.completed.load(Ordering::Relaxed)),
            failed: AtomicU64::new(self.failed.load(Ordering::Relaxed)),
            reused: AtomicU64::new(self.reused.load(Ordering::Relaxed)),
            started: Mutex::new(*self.started.lock().unwrap_or_else(|e| e.into_inner())),
            last_emit: Mutex::new(Instant::now()),
        }
    }
}

impl ProgressReporter {
    pub fn new(tx: Sender<JobEvent>) -> Self {
        let now = Instant::now();
        Self {
            tx: Arc::new(Mutex::new(tx)),
            phase: Mutex::new("Starting".into()),
            current: Mutex::new(String::new()),
            total: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            reused: AtomicU64::new(0),
            started: Mutex::new(now),
            last_emit: Mutex::new(now),
        }
    }

    pub fn channel() -> (Self, Receiver<JobEvent>) {
        let (tx, rx) = mpsc::channel();
        (Self::new(tx), rx)
    }

    pub fn begin_phase(&self, phase: impl Into<String>) {
        *self.phase.lock().unwrap_or_else(|e| e.into_inner()) = phase.into();
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = String::new();
        self.total.store(0, Ordering::Relaxed);
        self.completed.store(0, Ordering::Relaxed);
        self.failed.store(0, Ordering::Relaxed);
        self.reused.store(0, Ordering::Relaxed);
        *self.started.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        self.emit(true);
    }

    pub fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
        self.emit(false);
    }

    pub fn set_current(&self, current: impl Into<String>) {
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = current.into();
        self.emit(false);
    }

    pub fn add_reused(&self, count: u64) {
        self.reused.fetch_add(count, Ordering::Relaxed);
        self.completed.fetch_add(count, Ordering::Relaxed);
        self.emit(false);
    }

    pub fn inc_completed(&self) {
        self.completed.fetch_add(1, Ordering::Relaxed);
        self.emit(false);
    }

    pub fn inc_failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        self.completed.fetch_add(1, Ordering::Relaxed);
        self.emit(false);
    }

    pub fn snapshot(&self) -> Progress {
        let started = *self.started.lock().unwrap_or_else(|e| e.into_inner());
        Progress {
            phase: self
                .phase
                .lock()
                .map(|s| s.clone())
                .unwrap_or_else(|e| e.into_inner().clone()),
            total: self.total.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            reused: self.reused.load(Ordering::Relaxed),
            current: self
                .current
                .lock()
                .map(|s| s.clone())
                .unwrap_or_else(|e| e.into_inner().clone()),
            elapsed_secs: started.elapsed().as_secs_f64(),
        }
    }

    pub fn emit(&self, force: bool) {
        let now = Instant::now();
        {
            let mut last = self.last_emit.lock().unwrap_or_else(|e| e.into_inner());
            if !force && now.duration_since(*last) < Duration::from_millis(100) {
                return;
            }
            *last = now;
        }
        let progress = self.snapshot();
        if let Ok(sender) = self.tx.lock() {
            let _ = sender.send(JobEvent::Progress(progress));
        }
    }

    pub fn finish_phase(&self) {
        self.emit(true);
    }

    pub fn log(&self, message: impl Into<String>) {
        if let Ok(sender) = self.tx.lock() {
            let _ = sender.send(JobEvent::Log(message.into()));
        }
    }

    pub fn send(&self, event: JobEvent) {
        if let Ok(sender) = self.tx.lock() {
            let _ = sender.send(event);
        }
    }

    pub fn pretty_progress(progress: &Progress) -> String {
        let total = if progress.total == 0 {
            "?".into()
        } else {
            progress.total.to_string()
        };
        let eta = progress
            .eta_secs()
            .map(format_duration)
            .unwrap_or_else(|| "—".into());
        format!(
            "{}: {}/{} ({} failed, {} reused) · {:.1}/s · ETA {} · {}",
            progress.phase,
            progress.completed,
            total,
            progress.failed,
            progress.reused,
            progress.rate_per_sec(),
            eta,
            progress.current
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reporter_counts_and_snapshots() {
        let (reporter, rx) = ProgressReporter::channel();
        reporter.begin_phase("Testing");
        reporter.set_total(10);
        reporter.add_reused(3);
        reporter.inc_completed();
        reporter.inc_failed();
        let p = reporter.snapshot();
        assert_eq!(p.total, 10);
        assert_eq!(p.completed, 5);
        assert_eq!(p.failed, 1);
        assert_eq!(p.reused, 3);
        assert_eq!(p.hashed(), 2);
        reporter.finish_phase();
        let mut saw_progress = false;
        for event in rx.try_iter() {
            if matches!(event, JobEvent::Progress(_)) {
                saw_progress = true;
            }
        }
        assert!(saw_progress);
    }
}
