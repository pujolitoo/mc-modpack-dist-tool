//! Integration with the `portablemc`: Microsoft sign-in, installation (with progress) and launching.

use crate::i18n::t;
use crate::msg::{Msg, Reporter};
use crate::pack::LoadedPack;
use crate::util;
use anyhow::{anyhow, bail, Context, Result};
use portablemc::{base, fabric, forge, moj, msa};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;

pub enum AccountSel {
    Offline(String),
    Microsoft(String),
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
    msa::Database::new(db_path.to_path_buf())
        .store(account)
        .map_err(|e| anyhow!(tf!("game.could_not_save_account", error = e)))?;
    Ok(name)
}

fn resolve_auth(sel: &AccountSel, db_path: &Path) -> Result<Auth> {
    match sel {
        AccountSel::Offline(name) => Ok(Auth::Offline(name.clone())),
        AccountSel::Microsoft(name) => {
            let db = msa::Database::new(db_path.to_path_buf());
            let mut acc = db
                .load_from_username(name)
                .map_err(|e| anyhow!(tf!("game.accounts_database", error = e)))?
                .ok_or_else(|| anyhow!(tf!("game.no_saved_session_for_click", name = name)))?;
            // As in the CLI: validate the profile and refresh the token if it has expired.
            match acc.request_profile() {
                Ok(()) => {}
                Err(msa::AuthError::OutdatedToken) => {
                    acc.request_refresh()
                        .map_err(|e| anyhow!(tf!("game.could_not_refresh_session", reason = auth_err(&e))))?;
                }
                Err(e) => bail!(tf!("game.could_not_validate_account", reason = auth_err(&e))),
            }
            db.store(acc.clone()).map_err(|e| anyhow!(tf!("game.could_not_save_account", error = e)))?;
            Ok(Auth::Msa(acc))
        }
    }
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
    let auth = resolve_auth(&params.account, &params.db_path)?;
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
    if let Some((host, port)) = &params.join {
        rep.log(tf!("game.auto_join_game_will_connect", host = host, port = port));
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
        std::thread::spawn(move || pump(r, &rep))
    })
    .collect();

    let status = child.wait().context(t("game.error_while_waiting_for_minecraft"))?;
    for h in readers {
        let _ = h.join();
    }
    Ok(status.code())
}

/// Forwards game output to the log (handles non-UTF-8 bytes).
fn pump(reader: Box<dyn Read + Send>, rep: &Reporter) {
    let mut r = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf).trim_end().to_string();
                if !line.is_empty() {
                    rep.log(line);
                }
            }
        }
    }
}
