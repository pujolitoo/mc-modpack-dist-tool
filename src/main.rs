// On Windows (release builds), no console window is opened behind the app.
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod compat;
mod config;
mod external;
mod game;
mod msg;
mod pack;
mod selfupdate;
mod sync;
mod util;

use eframe::egui::{self, Color32, RichText};
use msg::{Msg, Reporter};
use pack::{LoadedPack, ModState};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};

const ACCENT: Color32 = Color32::from_rgb(0x3f, 0xb8, 0xa0);
const ACCENT_TEXT: Color32 = Color32::from_rgb(0x0d, 0x1a, 0x17);
const MUTE: Color32 = Color32::from_rgb(0x8d, 0x9a, 0xa5);
const BAD: Color32 = Color32::from_rgb(0xef, 0x7b, 0x6e);
const WARN: Color32 = Color32::from_rgb(0xe0, 0xa4, 0x3a);
const MAX_LOG_LINES: usize = 2000;

fn main() -> eframe::Result<()> {
    compat::ensure_classic_zlib(); // Linux: must run before any thread exists (it may restart the process)
    selfupdate::cleanup_old(); // removes leftovers from a previous update
    let title = config::title();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title.clone())
            .with_inner_size([820.0, 740.0])
            .with_min_inner_size([680.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        &title,
        options,
        Box::new(move |cc| {
            setup_style(&cc.egui_ctx);
            Ok(Box::new(App::new(cc)))
        }),
    )
}

fn setup_style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.selection.bg_fill = ACCENT;
    visuals.hyperlink_color = ACCENT;
    ctx.set_visuals(visuals);
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    ctx.set_style(style);
}

// ───────────────────────── ajustes del usuario ─────────────────────────

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    install_dir: String,
    mode: String,
    offline_name: String,
    ms_account: String,
    /// Custom maximum RAM in MB (empty = modpack-recommended amount).
    ram_mb: String,
    /// Client-side-only mods removed by the player, per modpack (pack ID → mod keys).
    excluded: HashMap<String, Vec<String>>,
}

impl Settings {
    fn path() -> PathBuf {
        util::app_data_dir().join("settings.json")
    }
    fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
    fn save(&self) {
        let _ = std::fs::create_dir_all(util::app_data_dir());
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(), s);
        }
    }
}

// ───────────────────────── estado de la interfaz ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Microsoft,
    Offline,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Busy {
    Idle,
    LoadingPack,
    LoggingIn,
    Playing,
    /// Buscando actualizaciones del modpack.
    Checking,
}

enum Progress {
    None,
    Fraction(f32, String),
    Spinner(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowStatus {
    Installed,
    Pending,
    Disabled,
    ServerOnly,
    /// Quitado por el jugador (mod solo cliente).
    Excluded,
}

struct Row {
    name: String,
    version: String,
    source: String,
    status: RowStatus,
    key: String,
    /// Mod solo cliente: el jugador puede quitarlo.
    client_only: bool,
    included: bool,
    /// Si lo necesita otro mod incluido, no se puede quitar (texto para el tooltip).
    lock_note: Option<String>,
}

enum Action {
    Play,
    PlayAnyway,
    CancelRam,
    Login,
    Reload,
    CheckUpdate,
}

struct App {
    settings: Settings,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    db_path: PathBuf,

    pack: Option<Arc<LoadedPack>>,
    pack_error: Option<String>,
    rows: Vec<Row>,

    install_dir: String,
    mode: Mode,
    offline_name: String,
    /// Custom maximum RAM (field text, in MB).
    ram_text: String,
    /// Total physical system RAM in MB (read once).
    sys_ram_mb: Option<u64>,
    accounts: Vec<String>,
    selected_account: String,
    device_code: Option<(String, String)>,

    busy: Busy,
    status: String,
    status_error: bool,
    progress: Progress,
    log: Vec<String>,
    open_log: Option<bool>,
    /// Pending low-RAM warning: (system RAM in MB, minimum required RAM in MB).
    ram_prompt: Option<(u64, u64)>,
}

fn valid_offline_name(n: &str) -> bool {
    (3..=16).contains(&n.chars().count()) && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn source_label(s: &str) -> String {
    match s {
        "modrinth" => "Modrinth".into(),
        "curseforge" => "CurseForge".into(),
        "local" => "JAR local".into(),
        other => other.to_string(),
    }
}

fn compute_rows(pack: &LoadedPack, install: &str, excluded: &HashSet<String>) -> Vec<Row> {
    let install = install.trim();
    let root = if install.is_empty() { None } else { Some(PathBuf::from(install)) };
    let mods_rel = pack.manifest.mods_dir();
    let eff = pack::effective_excluded(&pack.manifest, excluded);
    let needed = pack::required_by(&pack.manifest, &eff);
    pack.manifest
        .mods
        .iter()
        .map(|m| {
            let key = m.key();
            let client_only = m.state() == ModState::Active && m.is_client_only();
            let removed = client_only && eff.contains(&key);
            let status = if removed {
                RowStatus::Excluded
            } else {
                match m.state() {
                    ModState::Disabled => RowStatus::Disabled,
                    ModState::ServerOnly => RowStatus::ServerOnly,
                    ModState::Active => {
                        let ok = match (&root, m.rel_path(&mods_rel)) {
                            (Some(r), Ok(rel)) => match util::safe_join(r, &rel) {
                                Ok(p) => {
                                    p.is_file()
                                        && m.file.size.map_or(true, |s| {
                                            std::fs::metadata(&p).map(|md| md.len() == s).unwrap_or(false)
                                        })
                                }
                                Err(_) => false,
                            },
                            _ => false,
                        };
                        if ok { RowStatus::Installed } else { RowStatus::Pending }
                    }
                }
            };
            let lock_note = if client_only && !removed {
                needed.get(&key).map(|v| format!("Cannot remove: required by {}", v.join(", ")))
            } else {
                None
            };
            Row {
                name: m.name.clone(),
                version: m.version.number.clone().unwrap_or_default(),
                source: source_label(&m.source),
                status,
                key,
                client_only,
                included: !removed,
                lock_note,
            }
        })
        .collect()
}

/// Total physical system RAM in MB (None if it could not be read).
fn system_ram_mb() -> Option<u64> {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let total = sys.total_memory(); // bytes
    if total == 0 { None } else { Some(total / 1_048_576) }
}

/// Minimum RAM for playing: the value set in launcher.toml, or the modpack recommendation.
fn required_ram_mb(pack: &LoadedPack) -> Option<u64> {
    config::min_ram_override_mb().or(pack.manifest.runtime.recommended_ram_mb.map(u64::from))
}

/// Validates the RAM field: empty = automatic; otherwise, an integer between 1024 MB and system RAM.
fn parse_ram(text: &str, system_mb: Option<u64>) -> Result<Option<u32>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let mb: u32 = t
        .parse()
        .map_err(|_| "RAM must be a whole number of MB (for example, 4096).".to_string())?;
    if mb < 1024 {
        return Err("Minimum RAM is 1024 MB.".to_string());
    }
    if let Some(sys) = system_mb {
        if mb as u64 > sys {
            return Err(format!("Cannot exceed your system RAM ({}).", gb(sys)));
        }
    }
    Ok(Some(mb))
}

fn gb(mb: u64) -> String {
    format!("{:.1} GB", mb as f64 / 1024.0)
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = mpsc::channel();
        let settings = Settings::load();
        let db_path = game::accounts_db_path();
        let accounts = game::list_accounts(&db_path);
        let mode = if settings.mode == "microsoft" { Mode::Microsoft } else { Mode::Offline };
        let selected_account = if accounts.contains(&settings.ms_account) {
            settings.ms_account.clone()
        } else {
            accounts.first().cloned().unwrap_or_default()
        };
        let mut app = App {
            install_dir: settings.install_dir.clone(),
            offline_name: settings.offline_name.clone(),
            ram_text: settings.ram_mb.clone(),
            sys_ram_mb: system_ram_mb(),
            settings,
            tx,
            rx,
            db_path,
            pack: None,
            pack_error: None,
            rows: Vec::new(),
            mode,
            accounts,
            selected_account,
            device_code: None,
            busy: Busy::Idle,
            status: "Listo".to_string(),
            status_error: false,
            progress: Progress::None,
            log: Vec::new(),
            open_log: None,
            ram_prompt: None,
        };
        if std::env::var_os(compat::PRELOADED_MARKER).is_some() {
            app.push_log("Linux: zlib-ng detected; running with a bundled classic zlib (needed to install Forge/NeoForge).".to_string());
        }
        app.load_pack(&cc.egui_ctx);
        app
    }

    fn reporter(&self, ctx: &egui::Context) -> Reporter {
        Reporter::new(self.tx.clone(), ctx.clone())
    }

    fn push_log(&mut self, line: String) {
        self.log.push(line);
        if self.log.len() > MAX_LOG_LINES {
            let extra = self.log.len() - MAX_LOG_LINES;
            self.log.drain(..extra);
        }
    }

    fn fail(&mut self, text: String) {
        for l in text.lines() {
            self.push_log(format!("ERROR: {l}"));
        }
        self.status = text.lines().next().unwrap_or("Error").to_string();
        self.status_error = true;
        self.progress = Progress::None;
        self.busy = Busy::Idle;
        self.open_log = Some(true);
    }

    fn refresh_rows(&mut self) {
        let excluded = self.excluded_set();
        self.rows = match &self.pack {
            Some(p) => compute_rows(p, &self.install_dir, &excluded),
            None => Vec::new(),
        };
    }

    /// Keys of mods the player removed from the current modpack.
    fn excluded_set(&self) -> HashSet<String> {
        match &self.pack {
            Some(p) => self
                .settings
                .excluded
                .get(&p.id())
                .map(|v| v.iter().cloned().collect())
                .unwrap_or_default(),
            None => HashSet::new(),
        }
    }

    /// Excludes or re-includes a client-side-only mod and remembers the choice.
    fn toggle_mod(&mut self, key: String) {
        let Some(id) = self.pack.as_ref().map(|p| p.id()) else { return };
        let list = self.settings.excluded.entry(id).or_default();
        if let Some(pos) = list.iter().position(|k| *k == key) {
            list.remove(pos);
        } else {
            list.push(key);
        }
        self.settings.save();
        self.refresh_rows();
    }

    fn save_settings(&mut self) {
        self.settings.install_dir = self.install_dir.clone();
        self.settings.mode = if self.mode == Mode::Microsoft { "microsoft" } else { "offline" }.to_string();
        self.settings.offline_name = self.offline_name.clone();
        self.settings.ms_account = self.selected_account.clone();
        self.settings.ram_mb = self.ram_text.trim().to_string();
        self.settings.save();
    }

    // ───────── trabajos en segundo plano ─────────

    fn load_pack(&mut self, ctx: &egui::Context) {
        self.busy = Busy::LoadingPack;
        self.pack_error = None;
        self.status = "Loading modpack…".to_string();
        self.status_error = false;
        let rep = self.reporter(ctx);
        std::thread::spawn(move || {
            // Update the launcher first: if a new version is available, replace it and restart before loading anything.
            if let Some(exe) = selfupdate::run_or_log(&rep) {
                rep.send(Msg::SelfUpdated(exe));
                return;
            }
            let r = pack::load(&rep).map(Arc::new).map_err(|e| format!("{e:#}"));
            rep.send(Msg::PackLoaded(r));
        });
    }

    fn start_update_check(&mut self, ctx: &egui::Context) {
        self.busy = Busy::Checking;
        self.status = "Buscando actualizaciones…".to_string();
        self.status_error = false;
        self.progress = Progress::Spinner("Checking modpack…".to_string());
        let rep = self.reporter(ctx);
        std::thread::spawn(move || {
            if let Some(exe) = selfupdate::run_or_log(&rep) {
                rep.send(Msg::SelfUpdated(exe));
                return;
            }
            let r = pack::load(&rep).map(Arc::new).map_err(|e| format!("{e:#}"));
            rep.send(Msg::UpdateChecked(r));
        });
    }

    fn start_login(&mut self, ctx: &egui::Context) {
        self.busy = Busy::LoggingIn;
        self.device_code = None;
        self.status = "Requesting Microsoft code…".to_string();
        self.status_error = false;
        self.progress = Progress::Spinner("Esperando a Microsoft…".to_string());
        let rep = self.reporter(ctx);
        let app_id = config::app_id();
        let db_path = self.db_path.clone();
        std::thread::spawn(move || {
            let r = game::login(&app_id, &db_path, &rep).map_err(|e| format!("{e:#}"));
            rep.send(Msg::LoginDone(r));
        });
    }

    fn start_play(&mut self, ctx: &egui::Context, skip_ram_check: bool) {
        let Some(pack) = self.pack.clone() else { return };
        let install = self.install_dir.trim().to_string();
        if install.is_empty() {
            return self.fail("Choose an installation folder.".to_string());
        }
        let account = match self.mode {
            Mode::Offline => {
                if !valid_offline_name(self.offline_name.trim()) {
                    return self.fail("Offline name must be 3–16 characters: letters, numbers, or underscores.".to_string());
                }
                game::AccountSel::Offline(self.offline_name.trim().to_string())
            }
            Mode::Microsoft => {
                if self.selected_account.is_empty() {
                    return self.fail("Sign in with Microsoft before playing.".to_string());
                }
                game::AccountSel::Microsoft(self.selected_account.clone())
            }
        };
        let ram_mb = match parse_ram(&self.ram_text, self.sys_ram_mb) {
            Ok(v) => v,
            Err(e) => return self.fail(e),
        };
        // Aviso de Insufficient RAM: el jugador puede ignorarlo y continuar.
        if !skip_ram_check {
            if let (Some(have), Some(need)) = (system_ram_mb(), required_ram_mb(&pack)) {
                if have < need {
                    self.ram_prompt = Some((have, need));
                    return;
                }
            }
        }
        self.save_settings();
        self.busy = Busy::Playing;
        self.status_error = false;
        self.status = "Preparando…".to_string();
        self.progress = Progress::Spinner("Preparando…".to_string());
        let rep = self.reporter(ctx);
        let excluded = self.excluded_set();
        let params = game::PlayParams { install: PathBuf::from(&install), account, db_path: self.db_path.clone(), ram_mb };
        std::thread::spawn(move || {
            let r = (|| -> anyhow::Result<Option<i32>> {
                sync::sync_pack(&pack, &params.install, &excluded, &rep)?;
                rep.send(Msg::Refresh);
                game::play(&pack, &params, &rep)
            })()
            .map_err(|e| format!("{e:#}"));
            rep.send(Msg::PlayDone(r));
        });
    }

    fn drain(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::PackLoaded(Ok(p)) => {
                    self.pack = Some(p);
                    self.pack_error = None;
                    self.busy = Busy::Idle;
                    self.progress = Progress::None;
                    self.status = "Ready to play".to_string();
                    if self.install_dir.trim().is_empty() {
                        if let Some(p) = &self.pack {
                            self.install_dir = util::app_data_dir()
                                .join("instances")
                                .join(p.slug())
                                .display()
                                .to_string();
                        }
                    }
                    self.refresh_rows();
                }
                Msg::PackLoaded(Err(e)) => {
                    self.pack = None;
                    self.pack_error = Some(e.clone());
                    self.fail(e);
                    self.refresh_rows();
                }
                Msg::Status { text, error } => {
                    self.status = text;
                    self.status_error = error;
                }
                Msg::Progress { fraction, text } => self.progress = Progress::Fraction(fraction.clamp(0.0, 1.0), text),
                Msg::Busy(text) => self.progress = Progress::Spinner(text),
                Msg::Log(l) => self.push_log(l),
                Msg::DeviceCode { uri, code } => {
                    self.device_code = Some((uri, code));
                    self.status = "Authorize access in your browser using the displayed code.".to_string();
                }
                Msg::LoginDone(Ok(name)) => {
                    self.accounts = game::list_accounts(&self.db_path);
                    self.selected_account = name.clone();
                    self.device_code = None;
                    self.busy = Busy::Idle;
                    self.progress = Progress::None;
                    self.status = format!("Signed in as {name}.");
                    self.status_error = false;
                    self.save_settings();
                }
                Msg::LoginDone(Err(e)) => {
                    self.device_code = None;
                    self.fail(e);
                }
                Msg::PlayDone(Ok(code)) => {
                    self.busy = Busy::Idle;
                    self.progress = Progress::None;
                    self.refresh_rows();
                    match code {
                        Some(0) | None => {
                            self.status = "Minecraft has closed. See you next time!".to_string();
                            self.status_error = false;
                        }
                        Some(c) => {
                            self.fail(format!("Minecraft exited with code {c}. Check the log for details."));
                        }
                    }
                }
                Msg::PlayDone(Err(e)) => {
                    self.fail(e);
                    self.refresh_rows();
                }
                Msg::UpdateChecked(Ok(newp)) => {
                    self.busy = Busy::Idle;
                    self.progress = Progress::None;
                    let same = self.pack.as_ref().map(|o| o.fingerprint == newp.fingerprint).unwrap_or(false);
                    if newp.offline {
                        self.status = "Could not contact the modpack server; updates could not be checked.".to_string();
                        self.status_error = true;
                    } else if same {
                        let v = newp.version().to_string();
                        self.status = if v.is_empty() {
                            "You already have the latest version of the modpack.".to_string()
                        } else {
                            format!("You already have the latest version of the modpack ({v}).")
                        };
                        self.status_error = false;
                    } else {
                        let summary = match &self.pack {
                            Some(old) => {
                                let (a, r, u) = pack::diff_mods(&old.manifest, &newp.manifest);
                                let ver = |p: &LoadedPack| {
                                    if p.version().is_empty() { "(no version)".to_string() } else { p.version().to_string() }
                                };
                                format!(
                                    "Update found: {} → {} · {a} new, {r} removed, {u} updated mods. It will be applied when you click PLAY.",
                                    ver(old),
                                    ver(&newp)
                                )
                            }
                            None => "Modpack loaded..".to_string(),
                        };
                        self.push_log(summary.clone());
                        self.status = summary;
                        self.status_error = false;
                        self.pack = Some(newp);
                        self.refresh_rows();
                    }
                }
                Msg::UpdateChecked(Err(e)) => {
                    self.busy = Busy::Idle;
                    self.progress = Progress::None;
                    for l in e.lines() {
                        self.push_log(format!("ERROR: {l}"));
                    }
                    self.status = format!("Could not check for updates: {}", e.lines().next().unwrap_or("error"));
                    self.status_error = true;
                }
                Msg::SelfUpdated(exe) => {
                    self.status = "Launcher updated. Restarting…".to_string();
                    selfupdate::restart(&exe);
                }
                Msg::Refresh => self.refresh_rows(),
            }
        }
    }
}

// ───────────────────────── dibujo ─────────────────────────

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain();
        let idle = self.busy == Busy::Idle && self.ram_prompt.is_none();
        let mut action: Option<Action> = None;
        let mut rows_dirty = false;
        let mut toggle: Option<String> = None;

        // ── Header + installation location ──
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(8.0);
            match &self.pack {
                Some(p) => {
                    ui.label(RichText::new(format!("{} {}", p.display_name(), p.version())).size(24.0).strong());
                    ui.label(RichText::new(p.subtitle()).color(MUTE));
                }
                None => {
                    ui.label(RichText::new(config::title()).size(24.0).strong());
                    if let Some(e) = &self.pack_error {
                        ui.colored_label(BAD, e.as_str());
                        if ui.button("Reintentar").clicked() {
                            action = Some(Action::Reload);
                        }
                    } else if self.busy == Busy::LoadingPack {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Loading modpack…");
                        });
                    }
                }
            }
            ui.horizontal(|ui| {
                let has_url = config::modpack_url().is_some();
                let btn = ui.add_enabled(idle && has_url && self.pack.is_some(), egui::Button::new("Buscar actualizaciones"));
                if btn.clicked() {
                    action = Some(Action::CheckUpdate);
                }
                if !has_url {
                    btn.on_disabled_hover_text(
                        "This launcher has the modpack embedded in the executable. To update it, download a new version of the launcher.",
                    );
                }
                let note = if has_url { "Checked against the modpack server" } else { "Modpack embedded in executable" };
                ui.label(RichText::new(note).small().color(MUTE));
            });
            ui.add_space(4.0);
            ui.label(RichText::new("Installation location").strong());
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 110.0).max(80.0);
                let r = ui.add_enabled(idle, egui::TextEdit::singleline(&mut self.install_dir).desired_width(w));
                if r.changed() {
                    rows_dirty = true;
                }
                if ui.add_enabled(idle, egui::Button::new("Examinar…")).clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.install_dir = dir.display().to_string();
                        rows_dirty = true;
                    }
                }
            });
            ui.add_space(6.0);
        });

        // ── Account, progress and PLAY button ──
        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Cuenta").strong());
                ui.add_enabled_ui(idle, |ui| {
                    ui.radio_value(&mut self.mode, Mode::Microsoft, "Microsoft");
                    ui.radio_value(&mut self.mode, Mode::Offline, "Offline");
                });
            });
            ui.add_enabled_ui(idle || self.busy == Busy::LoggingIn, |ui| {
                ui.horizontal(|ui| match self.mode {
                    Mode::Microsoft => {
                        let shown = if self.selected_account.is_empty() {
                            "Not signed in".to_string()
                        } else {
                            self.selected_account.clone()
                        };
                        ui.add_enabled_ui(idle, |ui| {
                            egui::ComboBox::from_id_salt("accounts")
                                .width(220.0)
                                .selected_text(shown)
                                .show_ui(ui, |ui| {
                                    for a in &self.accounts {
                                        ui.selectable_value(&mut self.selected_account, a.clone(), a.as_str());
                                    }
                                });
                        });
                        if ui.add_enabled(idle, egui::Button::new("Sign in with Microsoft")).clicked() {
                            action = Some(Action::Login);
                        }
                    }
                    Mode::Offline => {
                        ui.label("Player name");
                        ui.add_enabled(
                            idle,
                            egui::TextEdit::singleline(&mut self.offline_name).desired_width(220.0).hint_text("Steve"),
                        );
                    }
                });
            });

            // ── Maximum RAM personalizada ──
            let ram_check = parse_ram(&self.ram_text, self.sys_ram_mb);
            let recommended = self.pack.as_ref().and_then(|p| p.manifest.runtime.recommended_ram_mb);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Maximum RAM").strong());
                ui.add_enabled(
                    idle,
                    egui::TextEdit::singleline(&mut self.ram_text)
                        .desired_width(80.0)
                        .hint_text(recommended.map(|r| r.to_string()).unwrap_or_else(|| "Auto".to_string())),
                );
                ui.label("MB");
                if ui.add_enabled(idle && !self.ram_text.is_empty(), egui::Button::new("Restablecer")).clicked() {
                    self.ram_text.clear();
                }
                let rec = recommended.map(|r| format!("{r} MB")).unwrap_or_else(|| "sin definir".to_string());
                let sys = self.sys_ram_mb.map(gb).unwrap_or_else(|| "desconocida".to_string());
                ui.label(RichText::new(format!("Recomendada: {rec} · Tu equipo: {sys}")).small().color(MUTE));
            });
            match &ram_check {
                Err(e) => {
                    ui.colored_label(BAD, e.as_str());
                }
                Ok(Some(mb)) => {
                    if let Some(rec) = recommended {
                        if *mb < rec {
                            ui.colored_label(
                                WARN,
                                format!("This is below the amount recommended by the modpack ({rec} MB); performance may suffer."),
                            );
                        }
                    }
                }
                Ok(None) => {}
            }

            if let Some((uri, code)) = &self.device_code {
                ui.group(|ui| {
                    ui.label("Open this page and enter the code to authorize access:");
                    ui.hyperlink(uri.as_str());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(code.as_str()).monospace().size(22.0).strong());
                        if ui.button("Copy code").clicked() {
                            ui.ctx().copy_text(code.clone());
                        }
                    });
                });
            }

            ui.add_space(4.0);
            if self.status_error {
                ui.colored_label(BAD, self.status.as_str());
            } else {
                ui.colored_label(MUTE, self.status.as_str());
            }
            match &self.progress {
                Progress::Fraction(f, text) => {
                    ui.add(egui::ProgressBar::new(*f).text(text.as_str()));
                }
                Progress::Spinner(text) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(text.as_str());
                    });
                }
                Progress::None => {}
            }

            let account_ok = match self.mode {
                Mode::Offline => valid_offline_name(self.offline_name.trim()),
                Mode::Microsoft => !self.selected_account.is_empty(),
            };
            let can_play = idle && self.pack.is_some() && account_ok && ram_check.is_ok();
            let play = egui::Button::new(RichText::new("JUGAR").size(22.0).strong().color(ACCENT_TEXT))
                .fill(ACCENT)
                .min_size(egui::vec2(ui.available_width(), 50.0));
            if ui.add_enabled(can_play, play).clicked() {
                action = Some(Action::Play);
            }

            egui::CollapsingHeader::new("Registro").default_open(false).open(self.open_log.take()).show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(170.0).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
                    for l in &self.log {
                        ui.monospace(l.as_str());
                    }
                });
            });
            ui.add_space(6.0);
        });

        // ── Lista de mods ──
        egui::CentralPanel::default().show(ctx, |ui| {
            let active = self.rows.iter().filter(|r| matches!(r.status, RowStatus::Installed | RowStatus::Pending)).count();
            let removed = self.rows.iter().filter(|r| r.status == RowStatus::Excluded).count();
            let mut title = format!("Mods ({} activos de {})", active, self.rows.len());
            if removed > 0 {
                title.push_str(&format!(" · {removed} quitados"));
            }
            ui.label(RichText::new(title).strong());
            if self.rows.iter().any(|r| r.client_only) {
                ui.label(
                    RichText::new("You can uncheck any client-side-only mods you don't want to use. Changes apply when you click PLAY.")
                        .small()
                        .color(MUTE),
                );
            }
            ui.add_space(4.0);
            use egui_extras::{Column, TableBuilder};
            TableBuilder::new(ui)
                .striped(true)
                .column(Column::exact(64.0))
                .column(Column::initial(280.0).at_least(140.0).clip(true))
                .column(Column::initial(140.0).at_least(80.0).clip(true))
                .column(Column::initial(90.0).at_least(70.0))
                .column(Column::remainder().at_least(90.0))
                .header(22.0, |mut h| {
                    h.col(|ui| {
                        ui.strong("Incluir");
                    });
                    h.col(|ui| {
                        ui.strong("Mod");
                    });
                    h.col(|ui| {
                        ui.strong("Version");
                    });
                    h.col(|ui| {
                        ui.strong("Origen");
                    });
                    h.col(|ui| {
                        ui.strong("Estado");
                    });
                })
                .body(|body| {
                    body.rows(24.0, self.rows.len(), |mut row| {
                        let r = &self.rows[row.index()];
                        row.col(|ui| {
                            if r.client_only {
                                let mut included = r.included;
                                let locked = r.lock_note.is_some();
                                let resp = ui.add_enabled(idle && !locked, egui::Checkbox::new(&mut included, ""));
                                if locked {
                                    if let Some(note) = &r.lock_note {
                                        resp.on_disabled_hover_text(note.as_str());
                                    }
                                } else if resp.changed() {
                                    toggle = Some(r.key.clone());
                                }
                            } else {
                                ui.label(RichText::new("Obligatorio").small().color(MUTE));
                            }
                        });
                        row.col(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(r.name.as_str());
                                if r.client_only {
                                    ui.label(RichText::new("solo cliente").small().color(MUTE));
                                }
                            });
                        });
                        row.col(|ui| {
                            ui.label(r.version.as_str());
                        });
                        row.col(|ui| {
                            ui.label(r.source.as_str());
                        });
                        row.col(|ui| {
                            let (text, color) = match r.status {
                                RowStatus::Installed => ("Instalado", ACCENT),
                                RowStatus::Pending => ("Pendiente", WARN),
                                RowStatus::Disabled => ("Desactivado", MUTE),
                                RowStatus::ServerOnly => ("Solo servidor", MUTE),
                                RowStatus::Excluded => ("Quitado", MUTE),
                            };
                            ui.colored_label(color, text);
                        });
                    });
                });
        });

        // ── Aviso de Insufficient RAM ──
        if let Some((have, need)) = self.ram_prompt {
            egui::Window::new("Insufficient RAM")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_max_width(420.0);
                    ui.label(
                        RichText::new(format!("Your system has {} of RAM.", gb(have))).strong().color(WARN),
                    );
                    ui.label(format!(
                        "Este modpack necesita al menos {} para funcionar con fluidez. Con menos memoria el juego \
                         puede ir muy lento, congelarse o cerrarse.",
                        gb(need)
                    ));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancelar").clicked() {
                            action = Some(Action::CancelRam);
                        }
                        if ui.button("Play anyway").clicked() {
                            action = Some(Action::PlayAnyway);
                        }
                    });
                });
        }

        if let Some(key) = toggle {
            self.toggle_mod(key);
        }
        if rows_dirty {
            self.refresh_rows();
        }
        match action {
            Some(Action::Play) => self.start_play(ctx, false),
            Some(Action::PlayAnyway) => {
                self.ram_prompt = None;
                self.push_log("Low-RAM warning ignored by the player.".to_string());
                self.start_play(ctx, true);
            }
            Some(Action::CancelRam) => self.ram_prompt = None,
            Some(Action::Login) => self.start_login(ctx),
            Some(Action::Reload) => self.load_pack(ctx),
            Some(Action::CheckUpdate) => self.start_update_check(ctx),
            None => {}
        }
    }
}
