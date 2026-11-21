//! Platform-specific directory selection behind a tiny abstraction.
//!
//! Linux prefers Zenity, then KDialog. When neither is available the GUI opens
//! its own directory browser, so the user never has to type a filesystem path.
//! macOS uses `osascript`; Windows currently falls back to the in-app browser
//! (documented as the secondary platform).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickTarget {
    Input,
    Destination,
    IndexDbDir,
    PlanDbDir,
    Trash,
}

impl PickTarget {
    pub fn label(self) -> &'static str {
        match self {
            Self::Input => "input directory",
            Self::Destination => "archive destination",
            Self::IndexDbDir => "image-index database folder",
            Self::PlanDbDir => "modification-plan database folder",
            Self::Trash => "trash directory",
        }
    }
}

#[derive(Debug, Clone)]
pub enum PickOutcome {
    Selected(PathBuf),
    Cancelled,
    /// No native dialog is available; the GUI should open its in-app browser.
    Unavailable(String),
}

pub struct PickRequest {
    pub target: PickTarget,
    pub receiver: Receiver<PickOutcome>,
}

impl PickRequest {
    pub fn poll(&self) -> Option<PickOutcome> {
        self.receiver.try_recv().ok()
    }
}

pub fn request_directory(
    target: PickTarget,
    initial: Option<PathBuf>,
    title: impl Into<String>,
) -> PickRequest {
    let title = title.into();
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("pixsort-folder-dialog".into())
        .spawn(move || {
            let outcome = native_dialog(target, initial.as_deref(), &title);
            let _ = tx.send(outcome);
        })
        .expect("cannot spawn folder dialog thread");
    PickRequest {
        target,
        receiver: rx,
    }
}

#[allow(clippy::needless_return)]
fn native_dialog(_target: PickTarget, initial: Option<&Path>, title: &str) -> PickOutcome {
    #[cfg(target_os = "linux")]
    {
        match run_zenity(initial, title) {
            Ok(Some(path)) => return PickOutcome::Selected(path),
            Ok(None) => return PickOutcome::Cancelled,
            Err(ZenityError::Unavailable) => {}
            Err(ZenityError::Failed(message)) => {
                log::warn!("zenity failed: {message}; trying kdialog");
            }
        }
        match run_kdialog(initial, title) {
            Ok(Some(path)) => return PickOutcome::Selected(path),
            Ok(None) => return PickOutcome::Cancelled,
            Err(ZenityError::Unavailable) => {
                return PickOutcome::Unavailable("Neither Zenity nor KDialog is installed.".into())
            }
            Err(ZenityError::Failed(message)) => {
                return PickOutcome::Unavailable(format!(
                    "Zenity and KDialog could not show a dialog: {message}"
                ))
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        match run_osascript(initial, title) {
            Ok(Some(path)) => return PickOutcome::Selected(path),
            Ok(None) => return PickOutcome::Cancelled,
            Err(message) => return PickOutcome::Unavailable(message),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (initial, title);
        PickOutcome::Unavailable(
            "Native folder dialogs are not implemented on this platform; using the built-in \
             browser."
                .into(),
        )
    }
}

#[cfg(target_os = "linux")]
enum ZenityError {
    Unavailable,
    Failed(String),
}

#[cfg(target_os = "linux")]
fn run_zenity(initial: Option<&Path>, title: &str) -> Result<Option<PathBuf>, ZenityError> {
    let mut command = Command::new("zenity");
    command
        .arg("--file-selection")
        .arg("--directory")
        .arg(format!("--title={title}"));
    if let Some(initial) = initial {
        let mut dir = initial.to_path_buf();
        if dir.is_file() {
            dir.pop();
        }
        let mut text = dir.to_string_lossy().into_owned();
        if !text.ends_with(std::path::MAIN_SEPARATOR) {
            text.push(std::path::MAIN_SEPARATOR);
        }
        command.arg(format!("--filename={text}"));
    }
    let output = command.output().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ZenityError::Unavailable
        } else {
            ZenityError::Failed(err.to_string())
        }
    })?;
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if text.is_empty() {
            Ok(None)
        } else {
            Ok(Some(PathBuf::from(text)))
        }
    } else {
        Ok(None)
    }
}

#[cfg(target_os = "linux")]
fn run_kdialog(initial: Option<&Path>, title: &str) -> Result<Option<PathBuf>, ZenityError> {
    let mut command = Command::new("kdialog");
    command.arg("--title").arg(title);
    command.arg("--getexistingdirectory");
    if let Some(initial) = initial {
        let mut dir = initial.to_path_buf();
        if dir.is_file() {
            dir.pop();
        }
        command.arg(dir);
    }
    let output = command.output().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ZenityError::Unavailable
        } else {
            ZenityError::Failed(err.to_string())
        }
    })?;
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if text.is_empty() {
            Ok(None)
        } else {
            Ok(Some(PathBuf::from(text)))
        }
    } else {
        Ok(None)
    }
}

#[cfg(target_os = "macos")]
fn run_osascript(initial: Option<&Path>, title: &str) -> Result<Option<PathBuf>, String> {
    let title = title.replace('"', "\\\"");
    let location = initial
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .map(|p| format!(" default location POSIX file \"{}\"", p.display()))
        .unwrap_or_default();
    let script = format!("POSIX path of (choose folder with prompt \"{title}\"{location})");
    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|err| format!("osascript unavailable: {err}"))?;
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if text.is_empty() {
            Ok(None)
        } else {
            Ok(Some(PathBuf::from(text)))
        }
    } else {
        Ok(None)
    }
}

/// Returns the platform's default starting directory for a picker.
pub fn default_start_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .filter(|p| p.is_dir())
        })
        .unwrap_or_else(|| PathBuf::from("."))
}
