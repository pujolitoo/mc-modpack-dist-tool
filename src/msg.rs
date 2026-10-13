//! Messages sent from worker threads to the UI.

use crate::pack::LoadedPack;
use eframe::egui;
use std::sync::{mpsc::Sender, Arc};

pub enum Msg {
    PackLoaded(Result<Arc<LoadedPack>, String>),
    /// Result of clicking «Buscar actualizaciones».
    UpdateChecked(Result<Arc<LoadedPack>, String>),
    Status { text: String, error: bool },
    /// Progreso determinado (0.0 ..= 1.0) con un texto.
    Progress { fraction: f32, text: String },
    /// Operation without measurable progress.
    Busy(String),
    Log(String),
    DeviceCode { uri: String, code: String },
    LoginDone(Result<String, String>),
    PlayDone(Result<Option<i32>, String>),
    Refresh,
    /// The launcher replaced itself; it must restart using this executable.
    SelfUpdated(std::path::PathBuf),
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
