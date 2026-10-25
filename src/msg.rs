//! Messages sent from worker threads to the UI.

use crate::pack::LoadedPack;
use eframe::egui;
use std::sync::{mpsc::Sender, Arc};

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

/// Sends messages to the UI and wakes it up for repainting.
#[derive(Clone)]
pub struct Reporter {
    tx: Sender<Msg>,
    ctx: egui::Context,
}

impl Reporter {
    pub fn new(tx: Sender<Msg>, ctx: egui::Context) -> Self {
        Self { tx, ctx }
    }

    pub fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
        self.ctx.request_repaint();
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
