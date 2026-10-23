// On Windows (release builds), no console window is opened behind the app.
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

#[macro_use]
mod i18n;
mod compat;
mod config;
mod external;
mod game;
mod msg;
mod pack;
mod selfupdate;
mod server;
mod sync;
mod util;

use eframe::egui::{self, Color32, RichText};
use i18n::t;
use msg::{Msg, Reporter};
use pack::{LoadedPack, ModState};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const ACCENT: Color32 = Color32::from_rgb(0x3f, 0xb8, 0xa0);
const ACCENT_TEXT: Color32 = Color32::from_rgb(0x0d, 0x1a, 0x17);
const MUTE: Color32 = Color32::from_rgb(0x8d, 0x9a, 0xa5);
const BAD: Color32 = Color32::from_rgb(0xef, 0x7b, 0x6e);
const WARN: Color32 = Color32::from_rgb(0xe0, 0xa4, 0x3a);
const MAX_LOG_LINES: usize = 2000;
/// How often the server status is refreshed.
const SERVER_REFRESH: Duration = Duration::from_secs(30);
/// Width of the server box (top right).
const SERVER_BOX_W: f32 = 300.0;
/// Size of the server icon in the box (Minecraft icons are 64×64).
const SERVER_ICON: f32 = 56.0;

fn main() -> eframe::Result<()> {
    compat::ensure_classic_zlib(); // Linux: must run before any thread exists (it may restart the process)
    selfupdate::cleanup_old(); // removes leftovers from a previous update
    let title = config::title();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title.clone())
            .with_inner_size([820.0, 820.0])
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

// ───────────────────────── user settings ─────────────────────────

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
    /// Auto-join on PLAY (`None` = the default from launcher.toml).
    auto_join: Option<bool>,
    /// Interface language ("en"/"es"; empty = the default from launcher.toml).
    language: String,
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

// ───────────────────────── UI state ─────────────────────────

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
    /// Checking for modpack updates.
    Checking,
}

/// Last known state of the server.
enum ServerState {
    /// Not queried yet (or the address changed).
    Unknown,
    Online(server::Status),
    Offline(String),
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
    /// Removed by the player (client-side-only mod).
    Excluded,
}

struct Row {
    name: String,
    version: String,
    source: String,
    status: RowStatus,
    key: String,
    /// Client-side-only mod: the player can remove it.
    client_only: bool,
    included: bool,
    /// If another included mod needs it, it cannot be removed (tooltip text).
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

    /// Server from launcher.toml (`None` = not set or not valid).
    server_addr: Option<server::Address>,
    auto_join: bool,
    server_state: ServerState,
    /// A query is in progress.
    server_checking: bool,
    /// ID of the latest query (older replies are discarded).
    server_query_id: u64,
    /// When the last query was started (for periodic refresh).
    server_checked_at: Option<Instant>,
    /// Server icon already uploaded to the GPU, and the image it came from (to avoid re-uploading it).
    server_icon: Option<egui::TextureHandle>,
    server_icon_src: Option<server::Icon>,
}

fn valid_offline_name(n: &str) -> bool {
    (3..=16).contains(&n.chars().count()) && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn source_label(s: &str) -> String {
    match s {
        "modrinth" => "Modrinth".into(),
        "curseforge" => "CurseForge".into(),
        "local" => t("main.local_jar").into(),
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
                needed.get(&key).map(|v| tf!("main.cannot_remove_required_by", mods = v.join(", ")))
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
    let input = text.trim();
    if input.is_empty() {
        return Ok(None);
    }
    let mb: u32 = input
        .parse()
        .map_err(|_| t("main.ram_must_be_whole_number").to_string())?;
    if mb < 1024 {
        return Err(t("main.minimum_ram_1024_mb").to_string());
    }
    if let Some(sys) = system_mb {
        if mb as u64 > sys {
            return Err(tf!("main.cannot_exceed_system_ram", ram = gb(sys)));
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
        let server_addr = server::parse_address(config::server_address());
        let auto_join = settings.auto_join.unwrap_or_else(config::server_auto_join);
        // The player's language if it still exists in this build; otherwise the default from launcher.toml.
        if !i18n::set(&settings.language) {
            i18n::set(config::default_language());
        }
        let mut app = App {
            server_addr,
            auto_join,
            server_state: ServerState::Unknown,
            server_checking: false,
            server_query_id: 0,
            server_checked_at: None,
            server_icon: None,
            server_icon_src: None,
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
            status: t("main.ready").to_string(),
            status_error: false,
            progress: Progress::None,
            log: Vec::new(),
            open_log: None,
            ram_prompt: None,
        };
        if std::env::var_os(compat::PRELOADED_MARKER).is_some() {
            app.push_log(t("main.linux_zlib_ng_detected_running").to_string());
        }
        app.load_pack(&cc.egui_ctx);
        app.start_server_query(&cc.egui_ctx);
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
        self.settings.auto_join = if self.auto_join == config::server_auto_join() { None } else { Some(self.auto_join) };
        let code = i18n::current().code;
        self.settings.language = if code == config::default_language() { String::new() } else { code.to_string() };
        self.settings.save();
    }

    // ───────── background jobs ─────────

    fn load_pack(&mut self, ctx: &egui::Context) {
        self.busy = Busy::LoadingPack;
        self.pack_error = None;
        self.status = t("main.loading_modpack").to_string();
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
        self.status = t("main.checking_for_updates").to_string();
        self.status_error = false;
        self.progress = Progress::Spinner(t("main.checking_modpack").to_string());
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

    /// Switches the interface language and remembers the choice.
    fn set_language(&mut self, code: &str) {
        i18n::set(code);
        self.save_settings();
        self.refresh_rows(); // the mod list has texts built in advance (source, notes)
        // The current status message was written in the previous language: replace the generic ones.
        if self.busy == Busy::Idle && !self.status_error {
            self.status = if self.pack.is_some() { t("main.ready_play") } else { t("main.ready") }.to_string();
        }
    }

    /// Queries the server status in the background (does nothing if there is no server).
    fn start_server_query(&mut self, ctx: &egui::Context) {
        let Some(addr) = self.server_addr.clone() else { return };
        self.server_query_id += 1;
        self.server_checking = true;
        self.server_checked_at = Some(Instant::now());
        let id = self.server_query_id;
        let rep = self.reporter(ctx);
        std::thread::spawn(move || {
            let result = server::query(&addr).map_err(|e| format!("{e:#}"));
            rep.send(Msg::ServerStatus { id, result });
        });
    }

    /// Server to join automatically on PLAY (if auto-join is enabled).
    fn join_target(&self) -> Option<(String, u16)> {
        if !self.auto_join {
            return None;
        }
        self.server_addr.as_ref().map(|a| (a.host.clone(), a.join_port()))
    }

    /// Server box (top right): icon, status, players and the auto-join checkbox.
    /// Returns (refresh requested, settings changed).
    fn server_box(&mut self, ui: &mut egui::Ui, idle: bool) -> (bool, bool) {
        let (mut check, mut save) = (false, false);
        egui::Frame::group(ui.style()).fill(ui.visuals().extreme_bg_color).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 4.0;
            let offline = matches!(self.server_state, ServerState::Offline(_));

            // ── Icon + name, address and status ──
            ui.horizontal_top(|ui| {
                let size = egui::vec2(SERVER_ICON, SERVER_ICON);
                match &self.server_icon {
                    Some(tex) => {
                        // Greyed out while the server is offline.
                        let tint = if offline { Color32::from_gray(110) } else { Color32::WHITE };
                        ui.add(egui::Image::from_texture(tex).fit_to_exact_size(size).corner_radius(6.0).tint(tint));
                    }
                    None => {
                        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                        ui.painter().rect_filled(rect, 6.0, ui.visuals().widgets.inactive.bg_fill);
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "?",
                            egui::FontId::proportional(24.0),
                            MUTE,
                        );
                    }
                }
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(t("main.server")).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let can = self.server_addr.is_some() && !self.server_checking;
                            if ui.add_enabled(can, egui::Button::new(t("main.refresh")).small()).clicked() {
                                check = true;
                            }
                            if self.server_checking {
                                ui.spinner();
                            }
                        });
                    });
                    ui.label(RichText::new(config::server_address()).monospace().small().color(MUTE));
                    if self.server_addr.is_none() {
                        ui.colored_label(BAD, t("main.invalid_address_launcher_toml"));
                        return;
                    }
                    ui.horizontal_wrapped(|ui| match &self.server_state {
                        ServerState::Unknown => {
                            ui.label(RichText::new(t("main.checking")).color(MUTE));
                        }
                        ServerState::Online(st) => {
                            ui.label(RichText::new(t("main.online")).strong().color(ACCENT));
                            ui.label(RichText::new("·").color(MUTE));
                            let players = ui.label(
                                RichText::new(format!(
                                    "{} / {} {}",
                                    st.online,
                                    st.max,
                                    if st.online == 1 { t("main.player") } else { t("main.players") }
                                ))
                                .strong(),
                            );
                            if !st.sample.is_empty() {
                                let mut list = st.sample.join("\n");
                                let extra = st.online - st.sample.len() as i64;
                                if extra > 0 {
                                    list.push_str(&tf!("main.and_more", extra = extra));
                                }
                                players.on_hover_text(list);
                            }
                        }
                        ServerState::Offline(_) => {
                            ui.label(RichText::new("●").color(BAD));
                            ui.label(RichText::new(t("main.offline")).strong().color(BAD));
                        }
                    });
                });
            });
            if self.server_addr.is_none() {
                return;
            }

            // ── Details ──
            match &self.server_state {
                ServerState::Online(st) => {
                    ui.horizontal_wrapped(|ui| {
                        if let Some(ms) = st.latency_ms {
                            let color = if ms < 100 { ACCENT } else if ms < 250 { WARN } else { BAD };
                            ui.label(RichText::new(format!("{ms} ms")).small().color(color));
                        }
                        if !st.version.is_empty() {
                            ui.label(RichText::new(st.version.as_str()).small().color(MUTE));
                        }
                    });
                    if !st.motd.is_empty() {
                        ui.label(RichText::new(st.motd.as_str()).small().italics().color(MUTE));
                    }
                }
                ServerState::Offline(e) => {
                    ui.label(RichText::new(e.as_str()).small().color(MUTE));
                }
                ServerState::Unknown => {}
            }
            ui.add_space(2.0);
            let cb = ui
                .add_enabled(idle, egui::Checkbox::new(&mut self.auto_join, t("main.join_automatically_when_clicking_play")))
                .on_hover_text(t("main.when_click_play_minecraft_will"));
            if cb.changed() {
                save = true;
            }
        });
        (check, save)
    }

    fn start_login(&mut self, ctx: &egui::Context) {
        self.busy = Busy::LoggingIn;
        self.device_code = None;
        self.status = t("main.requesting_microsoft_code").to_string();
        self.status_error = false;
        self.progress = Progress::Spinner(t("main.waiting_for_microsoft").to_string());
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
            return self.fail(t("main.choose_installation_folder").to_string());
        }
        let account = match self.mode {
            Mode::Offline => {
                if !valid_offline_name(self.offline_name.trim()) {
                    return self.fail(t("main.offline_name_must_be_3").to_string());
                }
                game::AccountSel::Offline(self.offline_name.trim().to_string())
            }
            Mode::Microsoft => {
                if self.selected_account.is_empty() {
                    return self.fail(t("main.sign_with_microsoft_before_playing").to_string());
                }
                game::AccountSel::Microsoft(self.selected_account.clone())
            }
        };
        let ram_mb = match parse_ram(&self.ram_text, self.sys_ram_mb) {
            Ok(v) => v,
            Err(e) => return self.fail(e),
        };
        let join = self.join_target();
        // Insufficient-RAM warning: the player can ignore it and continue.
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
        self.status = t("main.preparing").to_string();
        self.progress = Progress::Spinner(t("main.preparing").to_string());
        let rep = self.reporter(ctx);
        let excluded = self.excluded_set();
        let params = game::PlayParams { install: PathBuf::from(&install), account, db_path: self.db_path.clone(), ram_mb, join };
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

    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::PackLoaded(Ok(p)) => {
                    self.pack = Some(p);
                    self.pack_error = None;
                    self.busy = Busy::Idle;
                    self.progress = Progress::None;
                    self.status = t("main.ready_play").to_string();
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
                    self.status = t("main.authorize_access_browser_using_displayed").to_string();
                }
                Msg::LoginDone(Ok(name)) => {
                    self.accounts = game::list_accounts(&self.db_path);
                    self.selected_account = name.clone();
                    self.device_code = None;
                    self.busy = Busy::Idle;
                    self.progress = Progress::None;
                    self.status = tf!("main.signed_as", name = name);
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
                            self.status = t("main.minecraft_has_closed_see_next").to_string();
                            self.status_error = false;
                        }
                        Some(c) => {
                            self.fail(tf!("main.minecraft_exited_with_code_check", code = c));
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
                        self.status = t("main.could_not_contact_modpack_server").to_string();
                        self.status_error = true;
                    } else if same {
                        let v = newp.version().to_string();
                        self.status = if v.is_empty() {
                            t("main.already_have_latest_version_modpack").to_string()
                        } else {
                            tf!("main.already_have_latest_version_modpack_2", version = v)
                        };
                        self.status_error = false;
                    } else {
                        let summary = match &self.pack {
                            Some(old) => {
                                let (a, r, u) = pack::diff_mods(&old.manifest, &newp.manifest);
                                let ver = |p: &LoadedPack| {
                                    if p.version().is_empty() { t("main.no_version").to_string() } else { p.version().to_string() }
                                };
                                tf!("main.update_found_new_removed_updated", old = ver(old), new = ver(&newp), added = a, removed = r, updated = u)
                            }
                            None => t("main.modpack_loaded").to_string(),
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
                    self.status = tf!("main.could_not_check_for_updates", error = e.lines().next().unwrap_or("error"));
                    self.status_error = true;
                }
                Msg::SelfUpdated(exe) => {
                    self.status = t("main.launcher_updated_restarting").to_string();
                    selfupdate::restart(&exe);
                }
                Msg::Refresh => self.refresh_rows(),
                Msg::ServerStatus { id, result } => {
                    if id == self.server_query_id {
                        self.server_checking = false;
                        self.server_state = match result {
                            Ok(mut st) => {
                                // Online without an icon → remove it; with a new icon → upload it.
                                // (While offline, the last known icon is kept, greyed out.)
                                match st.icon.take() {
                                    None => {
                                        self.server_icon = None;
                                        self.server_icon_src = None;
                                    }
                                    Some(icon) if self.server_icon_src.as_ref() != Some(&icon) => {
                                        let image = egui::ColorImage::from_rgba_unmultiplied([icon.width, icon.height], &icon.rgba);
                                        self.server_icon = Some(ctx.load_texture("server-icon", image, egui::TextureOptions::LINEAR));
                                        self.server_icon_src = Some(icon);
                                    }
                                    Some(_) => {}
                                }
                                ServerState::Online(st)
                            }
                            Err(e) => ServerState::Offline(e),
                        };
                    }
                }
            }
        }
    }
}

// ───────────────────────── drawing ─────────────────────────

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain(ctx);
        let idle = self.busy == Busy::Idle && self.ram_prompt.is_none();
        let mut action: Option<Action> = None;
        let mut rows_dirty = false;
        let mut toggle: Option<String> = None;
        let mut check_server = false;
        let mut new_lang: Option<&'static str> = None;
        let mut save_server = false;

        // ── Server status: periodic refresh ──
        if self.server_addr.is_some() && !self.server_checking {
            let left = self.server_checked_at.map_or(Duration::ZERO, |t| SERVER_REFRESH.saturating_sub(t.elapsed()));
            if left.is_zero() {
                check_server = true;
            } else {
                ctx.request_repaint_after(left);
            }
        }

        // ── Header + installation location ──
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(8.0);
            let has_server = !config::server_address().is_empty();
            // Right column (always present): language selector and, below it, the server box.
            let left_w = (ui.available_width() - SERVER_BOX_W - 12.0).max(240.0);
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(left_w);
                    match &self.pack {
                        Some(p) => {
                            ui.label(RichText::new(format!("{} {}", p.display_name(), p.version())).size(24.0).strong());
                            ui.label(RichText::new(p.subtitle()).color(MUTE));
                        }
                        None => {
                            ui.label(RichText::new(config::title()).size(24.0).strong());
                            if let Some(e) = &self.pack_error {
                                ui.colored_label(BAD, e.as_str());
                                if ui.button(t("main.retry")).clicked() {
                                    action = Some(Action::Reload);
                                }
                            } else if self.busy == Busy::LoadingPack {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(t("main.loading_modpack"));
                                });
                            }
                        }
                    }
                    ui.horizontal(|ui| {
                        let has_url = config::modpack_url().is_some();
                        let btn = ui.add_enabled(idle && has_url && self.pack.is_some(), egui::Button::new(t("main.check_for_updates")));
                        if btn.clicked() {
                            action = Some(Action::CheckUpdate);
                        }
                        if !has_url {
                            btn.on_disabled_hover_text(
                                t("main.this_launcher_has_modpack_embedded"),
                            );
                        }
                        let note = if has_url {
                            t("main.checked_against_modpack_server")
                        } else {
                            t("main.modpack_embedded_executable")
                        };
                        ui.label(RichText::new(note).small().color(MUTE));
                    });
                });
                ui.vertical(|ui| {
                    ui.set_width(SERVER_BOX_W);
                    // ── Language selector (top right) ──
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let cur = i18n::current();
                            egui::ComboBox::from_id_salt("language").width(120.0).selected_text(cur.name.as_str()).show_ui(
                                ui,
                                |ui| {
                                    for l in i18n::languages() {
                                        if ui.selectable_label(cur.code == l.code, l.name.as_str()).clicked() && cur.code != l.code {
                                            new_lang = Some(l.code);
                                        }
                                    }
                                },
                            );
                            ui.label(RichText::new(t("main.language")).small().color(MUTE));
                        });
                    });
                    if has_server {
                        let (c, sv) = self.server_box(ui, idle);
                        check_server |= c;
                        save_server |= sv;
                    }
                });
            });
            ui.add_space(4.0);
            ui.label(RichText::new(t("main.installation_location")).strong());
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 110.0).max(80.0);
                let r = ui.add_enabled(idle, egui::TextEdit::singleline(&mut self.install_dir).desired_width(w));
                if r.changed() {
                    rows_dirty = true;
                }
                if ui.add_enabled(idle, egui::Button::new(t("main.browse"))).clicked() {
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
                ui.label(RichText::new(t("main.account")).strong());
                ui.add_enabled_ui(idle, |ui| {
                    ui.radio_value(&mut self.mode, Mode::Microsoft, "Microsoft");
                    ui.radio_value(&mut self.mode, Mode::Offline, "Offline");
                });
            });
            ui.add_enabled_ui(idle || self.busy == Busy::LoggingIn, |ui| {
                ui.horizontal(|ui| match self.mode {
                    Mode::Microsoft => {
                        let shown = if self.selected_account.is_empty() {
                            t("main.not_signed").to_string()
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
                        if ui.add_enabled(idle, egui::Button::new(t("main.sign_with_microsoft"))).clicked() {
                            action = Some(Action::Login);
                        }
                    }
                    Mode::Offline => {
                        ui.label(t("main.player_name"));
                        ui.add_enabled(
                            idle,
                            egui::TextEdit::singleline(&mut self.offline_name).desired_width(220.0).hint_text("Steve"),
                        );
                    }
                });
            });

            // ── Custom maximum RAM ──
            let ram_check = parse_ram(&self.ram_text, self.sys_ram_mb);
            let recommended = self.pack.as_ref().and_then(|p| p.manifest.runtime.recommended_ram_mb);
            ui.horizontal(|ui| {
                ui.label(RichText::new(t("main.maximum_ram")).strong());
                ui.add_enabled(
                    idle,
                    egui::TextEdit::singleline(&mut self.ram_text)
                        .desired_width(80.0)
                        .hint_text(recommended.map(|r| r.to_string()).unwrap_or_else(|| "Auto".to_string())),
                );
                ui.label("MB");
                if ui.add_enabled(idle && !self.ram_text.is_empty(), egui::Button::new(t("main.reset"))).clicked() {
                    self.ram_text.clear();
                }
                let rec = recommended.map(|r| format!("{r} MB")).unwrap_or_else(|| t("main.not_set").to_string());
                let sys = self.sys_ram_mb.map(gb).unwrap_or_else(|| t("main.unknown").to_string());
                ui.label(RichText::new(tf!("main.recommended_system", rec = rec, sys = sys)).small().color(MUTE));
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
                                tf!("main.this_below_amount_recommended_by", rec = rec),
                            );
                        }
                    }
                }
                Ok(None) => {}
            }

            if let Some((uri, code)) = &self.device_code {
                ui.group(|ui| {
                    ui.label(t("main.open_this_page_and_enter"));
                    ui.hyperlink(uri.as_str());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(code.as_str()).monospace().size(22.0).strong());
                        if ui.button(t("main.copy_code")).clicked() {
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
            let play = egui::Button::new(RichText::new(t("main.play")).size(22.0).strong().color(ACCENT_TEXT))
                .fill(ACCENT)
                .min_size(egui::vec2(ui.available_width(), 50.0));
            if ui.add_enabled(can_play, play).clicked() {
                action = Some(Action::Play);
            }

            egui::CollapsingHeader::new(t("main.log")).id_salt("log").default_open(false).open(self.open_log.take()).show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(170.0).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
                    for l in &self.log {
                        ui.monospace(l.as_str());
                    }
                });
            });
            ui.add_space(6.0);
        });

        // ── Mod list ──
        egui::CentralPanel::default().show(ctx, |ui| {
            let active = self.rows.iter().filter(|r| matches!(r.status, RowStatus::Installed | RowStatus::Pending)).count();
            let removed = self.rows.iter().filter(|r| r.status == RowStatus::Excluded).count();
            let mut title = tf!("main.mods_active", active = active, total = self.rows.len());
            if removed > 0 {
                title.push_str(&tf!("main.removed", removed = removed));
            }
            ui.label(RichText::new(title).strong());
            if self.rows.iter().any(|r| r.client_only) {
                ui.label(
                    RichText::new(t("main.can_uncheck_any_client_side"))
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
                        ui.strong(t("main.include"));
                    });
                    h.col(|ui| {
                        ui.strong("Mod");
                    });
                    h.col(|ui| {
                        ui.strong(t("main.version"));
                    });
                    h.col(|ui| {
                        ui.strong(t("main.source"));
                    });
                    h.col(|ui| {
                        ui.strong(t("main.status"));
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
                                ui.label(RichText::new(t("main.required")).small().color(MUTE));
                            }
                        });
                        row.col(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(r.name.as_str());
                                if r.client_only {
                                    ui.label(RichText::new(t("main.client_only")).small().color(MUTE));
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
                                RowStatus::Installed => (t("main.installed"), ACCENT),
                                RowStatus::Pending => (t("main.pending"), WARN),
                                RowStatus::Disabled => (t("main.disabled"), MUTE),
                                RowStatus::ServerOnly => (t("main.server_only"), MUTE),
                                RowStatus::Excluded => (t("main.removed_2"), MUTE),
                            };
                            ui.colored_label(color, text);
                        });
                    });
                });
        });

        // ── Insufficient-RAM warning ──
        if let Some((have, need)) = self.ram_prompt {
            egui::Window::new(t("main.insufficient_ram")).id(egui::Id::new("ram_prompt"))
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_max_width(420.0);
                    ui.label(
                        RichText::new(tf!("main.system_has_ram", ram = gb(have))).strong().color(WARN),
                    );
                    ui.label(tf!("main.this_modpack_needs_at_least", ram = gb(need)));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button(t("main.cancel")).clicked() {
                            action = Some(Action::CancelRam);
                        }
                        if ui.button(t("main.play_anyway")).clicked() {
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
        if save_server {
            self.save_settings();
        }
        if check_server {
            self.start_server_query(ctx);
        }
        if let Some(l) = new_lang {
            self.set_language(l);
        }
        match action {
            Some(Action::Play) => self.start_play(ctx, false),
            Some(Action::PlayAnyway) => {
                self.ram_prompt = None;
                self.push_log(t("main.low_ram_warning_ignored_by").to_string());
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
