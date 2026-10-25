//! Integration with the `portablemc`: Microsoft sign-in, installation (with progress) and launching.
//! Accounts of a custom authentication server are handled in yggdrasil.rs; here they are launched with authlib-injector.

use crate::i18n::t;
use crate::msg::{Msg, Reporter};
use crate::pack::LoadedPack;
use crate::{config, logxml, profile, util, yggdrasil};
use anyhow::{anyhow, bail, Context, Result};
use portablemc::{base, fabric, forge, moj, msa};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;

pub enum AccountSel {
    Offline(String),
    Microsoft(String),
    /// Profile name of an account of the custom authentication server (`[auth] url`).
    Custom(String),
}

pub struct PlayParams {
    pub install: PathBuf,
    pub account: AccountSel,
    pub db_path: PathBuf,
    /// Maximum RAM (MB) chosen by the player; `None` = the amount recommended by the modpack.
    pub ram_mb: Option<u32>,
    /// Server to join automatically when the game starts (host, port).
    pub join: Option<(String, u16)>,
}

enum Auth {
    Offline(String),
    Msa(msa::Account),
    /// Custom server: the account, its profile UUID, the API root and the metadata (base64) for authlib-injector.
    Custom { acc: yggdrasil::Account, uuid: uuid::Uuid, root: String, prefetched: String },
}

// ───────── custom authentication server ─────────

/// Normalized `[auth] url`: identifies the saved accounts of this launcher's server.
fn custom_server() -> Result<&'static str> {
    config::auth_url().ok_or_else(|| anyhow!(t("yggdrasil.not_configured")))
}

/// Saved accounts of the custom server (profile names).
pub fn list_custom_accounts() -> Vec<String> {
    match config::auth_url() {
        Some(url) => yggdrasil::list_accounts(&yggdrasil::db_path(), &yggdrasil::normalize_root(url)),
        None => Vec::new(),
    }
}

/// Signs in to the custom server with username/email and password and saves the session. Returns the profile name.
/// The password is only sent to the server; it is never written to disk or to the log.
pub fn login_custom(login: &str, password: &str, rep: &Reporter) -> Result<String> {
    let url = custom_server()?;
    rep.busy(t("yggdrasil.contacting_server"));
    let meta = yggdrasil::fetch_metadata(url)?;
    if let Some(name) = meta.server_name() {
        rep.log(tf!("yggdrasil.server_is", name = name, root = meta.root));
    }
    rep.busy(t("yggdrasil.signing_in"));
    let server = yggdrasil::normalize_root(url);
    let acc = yggdrasil::authenticate(&meta.root, &server, login, password)?;
    let db = yggdrasil::db_path();
    // One account at a time: sign out (best effort) and forget any other saved one of this server.
    for other in yggdrasil::list_accounts(&db, &server).into_iter().filter(|n| *n != acc.name) {
        if let Some(old) = yggdrasil::load_account(&db, &server, &other) {
            if old.uuid != acc.uuid {
                yggdrasil::invalidate(&meta.root, &old);
            }
        }
        let _ = yggdrasil::remove_account(&db, &server, &other);
    }
    yggdrasil::store_account(&db, &acc)?;
    Ok(acc.name)
}

/// Signs out (on the server, best effort) and forgets the saved session.
pub fn logout_custom(name: &str) -> Result<()> {
    let url = custom_server()?;
    let server = yggdrasil::normalize_root(url);
    let db = yggdrasil::db_path();
    if let Some(acc) = yggdrasil::load_account(&db, &server, name) {
        if let Ok(meta) = yggdrasil::fetch_metadata(url) {
            yggdrasil::invalidate(&meta.root, &acc);
        }
    }
    yggdrasil::remove_account(&db, &server, name)
}

/// Writes the embedded authlib-injector to the launcher's data folder (only if missing or different).
fn authlib_injector_path() -> Result<PathBuf> {
    let bytes = config::authlib_injector().ok_or_else(|| anyhow!(t("yggdrasil.no_authlib_injector")))?;
    let dir = util::app_data_dir().join("authlib-injector");
    let file = dir.join("authlib-injector.jar");
    if std::fs::read(&file).map(|b| b != bytes).unwrap_or(true) {
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join("authlib-injector.jar.part");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &file)?;
    }
    Ok(file)
}

/// Prepares a custom-server session for playing: metadata (or its cached copy) and a valid token.
fn resolve_custom(name: &str, rep: &Reporter) -> Result<Auth> {
    let url = custom_server()?;
    let server = yggdrasil::normalize_root(url);
    let db = yggdrasil::db_path();
    let saved = yggdrasil::load_account(&db, &server, name)
        .ok_or_else(|| anyhow!(tf!("yggdrasil.no_saved_session", name = name, server = config::auth_name())))?;
    let (meta, cached) = yggdrasil::metadata_or_cached(url)?;
    if cached {
        rep.log(t("yggdrasil.using_cached_metadata"));
    }
    let acc = match yggdrasil::ensure_valid(&meta.root, &saved)? {
        yggdrasil::Session::Valid(acc) => {
            if acc != saved {
                yggdrasil::store_account(&db, &acc)?;
            }
            acc
        }
        yggdrasil::Session::Unverified(reason) => {
            rep.log(tf!("yggdrasil.session_unverified", reason = reason));
            saved
        }
    };
    let uuid = yggdrasil::parse_uuid(&acc.uuid)?;
    Ok(Auth::Custom { acc, uuid, root: meta.root, prefetched: yggdrasil::base64(meta.json.as_bytes()) })
}

/// Microsoft accounts file (same format used by the PortableMC CLI).
pub fn accounts_db_path() -> PathBuf {
    util::app_data_dir().join("portablemc_msa.json")
}

pub fn list_accounts(db_path: &Path) -> Vec<String> {
    msa::Database::new(db_path.to_path_buf())
        .load_iter()
        .map(|it| it.map(|a| a.username().to_string()).collect())
        .unwrap_or_default()
}

fn auth_err(e: &msa::AuthError) -> String {
    match e {
        msa::AuthError::Declined => t("game.declined_authorization").to_string(),
        msa::AuthError::TimedOut => t("game.code_expired_before_sign_could").to_string(),
        msa::AuthError::DoesNotOwnGame => t("game.this_microsoft_account_does_not").to_string(),
        other => other.to_string(),
    }
}

/// UUID (without dashes) of a saved Microsoft account, to show its skin.
pub fn ms_account_uuid(db_path: &Path, name: &str) -> Option<String> {
    let acc = msa::Database::new(db_path.to_path_buf()).load_from_username(name).ok()??;
    Some(acc.uuid().as_simple().to_string())
}

/// Sign-in using the device-code flow: the user enters a code in the browser.
pub fn login(app_id: &str, db_path: &Path, rep: &Reporter) -> Result<String> {
    let auth = msa::Auth::new(app_id);
    let flow = auth.request_device_code().map_err(|e| anyhow!(auth_err(&e)))?;
    rep.send(Msg::DeviceCode {
        uri: flow.verification_uri().to_string(),
        code: flow.user_code().to_string(),
    });
    rep.log(flow.message().to_string());
    let _ = webbrowser::open(flow.verification_uri());

    let account = flow.wait().map_err(|e| anyhow!(auth_err(&e)))?;
    let name = account.username().to_string();
    let uuid = account.uuid();
    let db = msa::Database::new(db_path.to_path_buf());
    db.store(account).map_err(|e| anyhow!(tf!("game.could_not_save_account", error = e)))?;
    // One Microsoft account at a time: forget any other saved one.
    if let Ok(iter) = db.load_iter() {
        let others: Vec<_> = iter.map(|a| a.uuid()).filter(|u| *u != uuid).collect();
        for other in others {
            let _ = db.remove_from_uuid(other);
        }
    }
    Ok(name)
}

/// Forgets the saved Microsoft account.
pub fn logout_ms(db_path: &Path, name: &str) -> Result<()> {
    msa::Database::new(db_path.to_path_buf())
        .remove_from_username(name)
        .map_err(|e| anyhow!(tf!("game.could_not_save_account", error = e)))?;
    Ok(())
}

fn resolve_auth(sel: &AccountSel, db_path: &Path, rep: &Reporter) -> Result<Auth> {
    match sel {
        AccountSel::Offline(name) => Ok(Auth::Offline(name.clone())),
        AccountSel::Custom(name) => resolve_custom(name, rep),
        AccountSel::Microsoft(name) => Ok(Auth::Msa(valid_ms_account(db_path, name)?)),
    }
}

/// Loads a saved Microsoft account with a valid token (refreshing it if it expired, as the PortableMC CLI does).
fn valid_ms_account(db_path: &Path, name: &str) -> Result<msa::Account> {
    let db = msa::Database::new(db_path.to_path_buf());
    let mut acc = db
        .load_from_username(name)
        .map_err(|e| anyhow!(tf!("game.accounts_database", error = e)))?
        .ok_or_else(|| anyhow!(tf!("game.no_saved_session_for_click", name = name)))?;
    match acc.request_profile() {
        Ok(()) => {}
        Err(msa::AuthError::OutdatedToken) => {
            acc.request_refresh()
                .map_err(|e| anyhow!(tf!("game.could_not_refresh_session", reason = auth_err(&e))))?;
        }
        Err(e) => bail!(tf!("game.could_not_validate_account", reason = auth_err(&e))),
    }
    db.store(acc.clone()).map_err(|e| anyhow!(tf!("game.could_not_save_account", error = e)))?;
    Ok(acc)
}

// ───────── skin and name changes ─────────

/// Uploads a new skin for a Microsoft account (Mojang's services API).
pub fn ms_upload_skin(db_path: &Path, name: &str, png: &[u8], model: profile::Model) -> Result<()> {
    profile::check_skin(png, false)?;
    let acc = valid_ms_account(db_path, name)?;
    profile::services_upload_skin(profile::MOJANG_SERVICES, acc.access_token(), png, model)
}

/// Session of a custom-server account ready for an API call: (API root, account with a valid token).
/// Unlike playing, this needs the server to be reachable.
fn custom_session(name: &str) -> Result<(String, yggdrasil::Account)> {
    let url = custom_server()?;
    let db = yggdrasil::db_path();
    let saved = yggdrasil::load_account(&db, &yggdrasil::normalize_root(url), name)
        .ok_or_else(|| anyhow!(tf!("yggdrasil.no_saved_session", name = name, server = config::auth_name())))?;
    let meta = yggdrasil::fetch_metadata(url)?;
    let acc = match yggdrasil::ensure_valid(&meta.root, &saved)? {
        yggdrasil::Session::Valid(acc) => acc,
        yggdrasil::Session::Unverified(reason) => bail!(tf!("yggdrasil.could_not_contact", error = reason)),
    };
    if acc != saved {
        yggdrasil::store_account(&db, &acc)?;
    }
    Ok((meta.root, acc))
}

/// Uploads a new skin for an account of the custom server (texture upload of the Yggdrasil specification).
pub fn custom_upload_skin(name: &str, png: &[u8], model: profile::Model) -> Result<()> {
    profile::check_skin(png, true)?;
    let (root, acc) = custom_session(name)?;
    profile::yggdrasil_upload_skin(&root, &acc.uuid, &acc.access_token, png, model)
}

/// Changes the player name of an account of the custom server. Returns the new name (the saved account is renamed).
pub fn custom_change_name(name: &str, new_name: &str) -> Result<String> {
    let new_name = new_name.trim();
    if !profile::valid_player_name(new_name) {
        bail!(t("profile.invalid_name"));
    }
    let (root, acc) = custom_session(name)?;
    let reported = profile::services_change_name(&format!("{root}minecraftservices/"), &acc.access_token, new_name)?;
    // A new token carries the new name; if refreshing fails, keep the old token with the reported name.
    let updated = yggdrasil::refresh(&root, &acc).unwrap_or_else(|_| yggdrasil::Account { name: reported, ..acc.clone() });
    yggdrasil::store_account(&yggdrasil::db_path(), &updated)?; // replaces the entry with the same UUID
    Ok(updated.name)
}

/// Applies common settings to any installer (folders, account and auto-join).
fn configure(inst: &mut moj::Installer, install: &Path, auth: &Auth, join: Option<&(String, u16)>) {
    inst.base_mut().set_main_dir(install.to_path_buf());
    inst.base_mut().set_mc_dir(install.to_path_buf());
    inst.base_mut().set_bin_dir(install.join("bin"));
    if let Some((host, port)) = join {
        // Quick Play (1.20+); on older versions portablemc falls back to --server/--port.
        inst.set_quick_play(moj::QuickPlay::Multiplayer { host: host.clone(), port: *port });
    }
    match auth {
        Auth::Offline(name) => {
            inst.set_auth_offline_username(name.clone());
        }
        Auth::Msa(acc) => {
            inst.set_auth_msa(acc);
        }
        // UUID and name as an offline session; the real token is put in the arguments after installing (see play).
        Auth::Custom { acc, uuid, .. } => {
            inst.set_auth_offline(*uuid, acc.name.clone());
        }
    }
}

/// Maps portablemc installation events to UI progress.
struct ProgressHandler {
    rep: Reporter,
}

impl ProgressHandler {
    fn base(&self, ev: &base::Event) {
        use base::Event as E;
        match ev {
            E::LoadHierarchy { .. } => self.rep.busy(t("game.loading_minecraft_version")),
            E::LoadClient { .. } => self.rep.busy(t("game.verifying_client")),
            E::LoadLibraries { .. } => self.rep.busy(t("game.verifying_libraries")),
            E::LoadAssets { .. } => self.rep.busy(t("game.verifying_game_assets")),
            E::LoadJvm { .. } => self.rep.busy(t("game.looking_for_java")),
            E::DownloadResources { .. } => self.rep.busy(t("game.preparing_downloads")),
            E::DownloadProgress { count, total_count, size, total_size } => {
                let (size, total_size) = (*size as f64, *total_size as f64);
                let fraction = if total_size > 0.0 { (size / total_size) as f32 } else { 0.0 };
                self.rep.progress(
                    fraction,
                    tf!("game.downloading_minecraft_files_mb", count = count, total_count = total_count, done_mb = format!("{:.1}", util::mb(size)), total_mb = format!("{:.1}", util::mb(total_size))),
                );
            }
            E::DownloadedResources { .. } => self.rep.log(t("game.asset_download_complete")),
            _ => {}
        }
    }
}

impl ProgressHandler {
    fn mojang(&self, ev: &moj::Event) {
        match ev {
            moj::Event::Base(ev) => self.base(ev),
            moj::Event::FixedLegacyQuickPlay => {
                self.rep.log(t("game.auto_join_old_minecraft_version"))
            }
            moj::Event::WarnUnsupportedQuickPlay => {
                self.rep.log(t("game.warning_this_minecraft_version_does"))
            }
            _ => {}
        }
    }
}

impl moj::Handler for ProgressHandler {
    fn on_event(&mut self, event: moj::Event) {
        self.mojang(&event);
    }
}

impl fabric::Handler for ProgressHandler {
    fn on_event(&mut self, event: fabric::Event) {
        match &event {
            fabric::Event::Mojang(ev) => self.mojang(ev),
            fabric::Event::FetchVersion { .. } => self.rep.busy(t("game.fetching_loader_fabric_quilt")),
            _ => {}
        }
    }
}

impl forge::Handler for ProgressHandler {
    fn on_event(&mut self, event: forge::Event) {
        match &event {
            forge::Event::Mojang(ev) => self.mojang(ev),
            // Installing the loader itself (downloading the installer and running processors).
            _ => self.rep.busy(t("game.installing_loader_first_run_may")),
        }
    }
}

/// Turns a loader installation error into a user-facing one, adding an explanation for a known cause.
fn install_error(kind: &str, message: &str) -> anyhow::Error {
    let mut text = tf!("game.installation", kind = kind, message = message);
    if message.to_lowercase().contains("processor") {
        text.push_str(t("game.forge_neoforge_installer_checks_exact"));
    }
    anyhow!(text)
}

/// Installs and launches the game and loader specified by the modpack. Returns the exit code.
pub fn play(pack: &LoadedPack, params: &PlayParams, rep: &Reporter) -> Result<Option<i32>> {
    let m = &pack.manifest;
    let mc = m.minecraft.version.trim().to_string();
    if mc.is_empty() {
        bail!(t("game.modpack_does_not_specify_minecraft"));
    }
    let kind = m.loader.kind.trim().to_lowercase();
    let lv = m.loader.version.trim().to_string();

    rep.busy(t("game.checking_account"));
    let auth = resolve_auth(&params.account, &params.db_path, rep)?;
    let handler = ProgressHandler { rep: rep.clone() };
    rep.busy(t("game.preparing_minecraft"));

    let mut game = match kind.as_str() {
        "" | "vanilla" | "none" => {
            let mut inst = moj::Installer::new(mc.clone());
            configure(&mut inst, &params.install, &auth, params.join.as_ref());
            inst.install(handler).map_err(|e| anyhow!(tf!("game.minecraft_installation", error = e)))?
        }
        "fabric" | "quilt" => {
            let loader = if kind == "quilt" { fabric::Loader::Quilt } else { fabric::Loader::Fabric };
            let mut inst = if lv.is_empty() {
                fabric::Installer::new(loader, mc.clone(), fabric::LoaderVersion::Stable)
            } else {
                fabric::Installer::new(loader, mc.clone(), lv.clone())
            };
            configure(inst.mojang_mut(), &params.install, &auth, params.join.as_ref());
            inst.install(handler).map_err(|e| anyhow!(tf!("game.installation_2", kind = kind, error = e)))?
        }
        "forge" | "neoforge" => {
            // Forge uses names like «<mc>-<version>»; NeoForge uses the version as-is (e.g. 21.1.77).
            let (loader, version) = if kind == "neoforge" {
                let v = if lv.is_empty() { forge::Version::Stable(mc.clone()) } else { forge::Version::Name(lv.clone()) };
                (forge::Loader::NeoForge, v)
            } else {
                let v = if lv.is_empty() {
                    forge::Version::Stable(mc.clone())
                } else if lv.starts_with(&format!("{mc}-")) {
                    forge::Version::Name(lv.clone())
                } else {
                    forge::Version::Name(format!("{mc}-{lv}"))
                };
                (forge::Loader::Forge, v)
            };
            let mut inst = forge::Installer::new(loader, version);
            configure(inst.mojang_mut(), &params.install, &auth, params.join.as_ref());
            inst.install(handler).map_err(|e| install_error(&kind, &e.to_string()))?
        }
        other => bail!(tf!("game.loader_not_supported_by_launcher", loader = other)),
    };

    // Maximum RAM: the player's choice or, if not set, the modpack's recommendation (if any).
    if let Some(ram) = params.ram_mb.or(m.runtime.recommended_ram_mb) {
        game.jvm_args.push(format!("-Xmx{ram}M"));
        rep.log(tf!("game.maximum_allocated_ram_mb", ram = ram));
    }
    // Use the system proxy settings (as HMCL does): on networks/PCs with a proxy, direct connections from the game
    // (and from authlib-injector) time out. Java never sends localhost / 127.* through the proxy.
    if !game.jvm_args.iter().any(|a| a.starts_with("-Djava.net.useSystemProxies=") || a.starts_with("-Dhttp.proxy") || a.starts_with("-Dhttps.proxy") || a.starts_with("-DsocksProxy")) {
        game.jvm_args.push("-Djava.net.useSystemProxies=true".to_string());
    }
    if let Some((host, port)) = &params.join {
        rep.log(tf!("game.auto_join_game_will_connect", host = host, port = port));
    }
    // Custom authentication server: real token in the arguments + authlib-injector pointing to the server.
    let mut secret = None;
    if let Auth::Custom { acc, root, prefetched, .. } = &auth {
        yggdrasil::patch_game_args(&mut game.game_args, acc);
        let jar = authlib_injector_path()?;
        game.jvm_args.insert(0, format!("-javaagent:{}={}", jar.display(), root));
        game.jvm_args.insert(1, format!("-Dauthlibinjector.yggdrasil.prefetched={prefetched}"));
        // Tell authlib-injector it runs in a client (as HMCL does): with Forge/NeoForge the main class is a bootstrap
        // launcher, so authlib-injector cannot detect the side by itself.
        game.jvm_args.insert(2, "-Dauthlibinjector.side=client".to_string());
        // Diagnostics: MODPACK_LAUNCHER_AUTH_DEBUG=1 makes authlib-injector log every request it forwards (and where).
        if std::env::var("MODPACK_LAUNCHER_AUTH_DEBUG").map(|v| !v.trim().is_empty() && v.trim() != "0").unwrap_or(false) {
            game.jvm_args.insert(3, "-Dauthlibinjector.debug".to_string());
            rep.log(t("yggdrasil.debug_enabled"));
        }
        rep.log(tf!("yggdrasil.launching_with", name = acc.name, root = root));
        secret = Some(acc.access_token.clone());
    }

    let mut cmd = game.command();
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    rep.busy(t("game.minecraft_running"));
    rep.status(t("game.minecraft_running_2"));
    let mut child = cmd.spawn().context(t("game.could_not_start_java"))?;
    let readers: Vec<_> = [
        child.stdout.take().map(|o| Box::new(o) as Box<dyn Read + Send>),
        child.stderr.take().map(|e| Box::new(e) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|r| {
        let rep = rep.clone();
        let secret = secret.clone();
        std::thread::spawn(move || pump(r, &rep, secret.as_deref()))
    })
    .collect();

    let status = child.wait().context(t("game.error_while_waiting_for_minecraft"))?;
    for h in readers {
        let _ = h.join();
    }
    Ok(status.code())
}

/// Forwards game output to the log (handles non-UTF-8 bytes). The game's log4j XML events are turned into readable
/// lines (see logxml.rs). `secret` (the access token) is masked if it appears.
fn pump(reader: Box<dyn Read + Send>, rep: &Reporter, secret: Option<&str>) {
    let mut r = BufReader::new(reader);
    let mut buf = Vec::new();
    let mut xml = logxml::Parser::default();
    let emit = |text: String| {
        let mut text = text.trim_end().to_string();
        if let Some(s) = secret.filter(|s| s.len() >= 8) {
            text = text.replace(s, "********");
        }
        if !text.trim().is_empty() {
            rep.log(text);
        }
    };
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                if let Some(text) = xml.feed(line.trim_end_matches(['\r', '\n'])) {
                    emit(text);
                }
            }
        }
    }
    if let Some(rest) = xml.finish() {
        emit(rest);
    }
}
