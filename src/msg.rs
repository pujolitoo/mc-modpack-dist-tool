//! Messages sent from worker threads to the UI.

use crate::pack::LoadedPack;
use eframe::egui;
use std::io::Write;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{mpsc::Sender, Arc, Mutex};

pub enum Msg {
    PackLoaded(Result<Arc<LoadedPack>, String>),
    /// Result of clicking «Check for updates».
    UpdateChecked(Result<Arc<LoadedPack>, String>),
    Status { text: String, error: bool },
    /// Determinate progress (0.0 ..= 1.0) with a text.
    Progress { fraction: f32, text: String },
    /// Operation without measurable progress.
    Busy(String),
    Log(String),
    DeviceCode { uri: String, code: String },
    LoginDone(Result<String, String>),
    /// Sign-in to the custom authentication server finished (profile name).
    CustomLoginDone(Result<String, String>),
    /// Sign-out of a custom account finished (profile name).
    CustomLogoutDone(Result<String, String>),
    /// Sign-out of the Microsoft account finished (username).
    MsLogoutDone(Result<String, String>),
    PlayDone(Result<Option<i32>, String>),
    Refresh,
    /// Result of a server status query (`id` discards replies that are no longer current).
    ServerStatus { id: u64, result: Result<crate::server::Status, String> },
    /// A skin upload or a name change finished.
    ProfileChanged(Result<ProfileChange, String>),
    /// Skin head of an account (`key` from skin::Who::key); `None` = no skin or not available.
    SkinHead { key: String, head: Option<crate::server::Icon> },
    /// The launcher replaced itself; it must restart using this executable.
    SelfUpdated(std::path::PathBuf),
}

/// Result of changing the skin or the name of an account.
pub struct ProfileChange {
    /// Status text to show.
    pub message: String,
    /// Head to download again (skin::Who::key of the account before the change).
    pub head_key: String,
    /// Name change: (old name, new name).
    pub renamed: Option<(String, String)>,
}

/// Sends messages to the UI and wakes it up for repainting, or (without a window, `--sync-server`) prints them.
#[derive(Clone)]
pub struct Reporter {
    target: Target,
}

#[derive(Clone)]
enum Target {
    Ui { tx: Sender<Msg>, ctx: egui::Context },
    /// Prints to the console and appends to a log file (if any). Progress is printed every 10%.
    Console { log: Option<Arc<Mutex<std::fs::File>>>, last_tenth: Arc<AtomicI32> },
}

impl Reporter {
    pub fn new(tx: Sender<Msg>, ctx: egui::Context) -> Self {
        Self { target: Target::Ui { tx, ctx } }
    }

    /// Reporter for the command line: prints messages and also writes them to `log` (if any).
    pub fn console(log: Option<std::fs::File>) -> Self {
        Self { target: Target::Console { log: log.map(|f| Arc::new(Mutex::new(f))), last_tenth: Arc::new(AtomicI32::new(-1)) } }
    }

    pub fn send(&self, msg: Msg) {
        match &self.target {
            Target::Ui { tx, ctx } => {
                let _ = tx.send(msg);
                ctx.request_repaint();
            }
            Target::Console { log, last_tenth } => {
                let line = match msg {
                    Msg::Log(text) | Msg::Busy(text) => text,
                    Msg::Status { text, error } => {
                        if error {
                            format!("ERROR: {text}")
                        } else {
                            text
                        }
                    }
                    Msg::Progress { fraction, text } => {
                        let tenth = (fraction.clamp(0.0, 1.0) * 10.0).floor() as i32;
                        if last_tenth.swap(tenth, Ordering::SeqCst) == tenth {
                            return;
                        }
                        format!("[{:>3}%] {text}", (fraction.clamp(0.0, 1.0) * 100.0).round() as i32)
                    }
                    _ => return,
                };
                println!("{line}");
                if let Some(f) = log {
                    if let Ok(mut f) = f.lock() {
                        let _ = writeln!(f, "{line}");
                    }
                }
            }
        }
    }

    pub fn log(&self, text: impl Into<String>) {
        self.send(Msg::Log(text.into()));
    }

    pub fn status(&self, text: impl Into<String>) {
        self.send(Msg::Status { text: text.into(), error: false });
    }

    pub fn progress(&self, fraction: f32, text: impl Into<String>) {
        self.send(Msg::Progress { fraction, text: text.into() });
    }

    pub fn busy(&self, text: impl Into<String>) {
        self.send(Msg::Busy(text.into()));
    }
}
