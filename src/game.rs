//! Integration with the `portablemc`: Microsoft sign-in, installation (with progress) and launching.

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
    /// Maximum RAM (MB) elegida por el jugador; `None` = la recomendada por el modpack.
    pub ram_mb: Option<u32>,
}

enum Auth {
    Offline(String),
    Msa(msa::Account),
}

/// Archivo de cuentas de Microsoft (mismo formato que usa el CLI de PortableMC).
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
        msa::AuthError::Declined => "You declined authorization.".to_string(),
        msa::AuthError::TimedOut => "The code expired before sign-in could be completed.".to_string(),
        msa::AuthError::DoesNotOwnGame => "Esta cuenta de Microsoft no tiene Minecraft: Java Edition.".to_string(),
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
        .map_err(|e| anyhow!("Could not save account: {e}"))?;
    Ok(name)
}

fn resolve_auth(sel: &AccountSel, db_path: &Path) -> Result<Auth> {
    match sel {
        AccountSel::Offline(name) => Ok(Auth::Offline(name.clone())),
        AccountSel::Microsoft(name) => {
            let db = msa::Database::new(db_path.to_path_buf());
            let mut acc = db
                .load_from_username(name)
                .map_err(|e| anyhow!("Base de datos de cuentas: {e}"))?
                .ok_or_else(|| anyhow!("No saved session for «{name}». Pulsa «Sign in with Microsoft»."))?;
            // As in the CLI: validate the profile and refresh the token if it has expired.
            match acc.request_profile() {
                Ok(()) => {}
                Err(msa::AuthError::OutdatedToken) => {
                    acc.request_refresh()
                        .map_err(|e| anyhow!("Could not refresh session: {}", auth_err(&e)))?;
                }
                Err(e) => bail!("Could not validate account: {}", auth_err(&e)),
            }
            db.store(acc.clone()).map_err(|e| anyhow!("Could not save account: {e}"))?;
            Ok(Auth::Msa(acc))
        }
    }
}

/// Applies common settings to any installer (folders and account).
fn configure(inst: &mut moj::Installer, install: &Path, auth: &Auth) {
    inst.base_mut().set_main_dir(install.to_path_buf());
    inst.base_mut().set_mc_dir(install.to_path_buf());
    inst.base_mut().set_bin_dir(install.join("bin"));
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
            E::LoadHierarchy { .. } => self.rep.busy("Loading Minecraft version…"),
            E::LoadClient { .. } => self.rep.busy("Verifying client…"),
            E::LoadLibraries { .. } => self.rep.busy("Verifying libraries…"),
            E::LoadAssets { .. } => self.rep.busy("Verifying game assets…"),
            E::LoadJvm { .. } => self.rep.busy("Buscando Java…"),
            E::DownloadResources { .. } => self.rep.busy("Preparando descargas…"),
            E::DownloadProgress { count, total_count, size, total_size } => {
                let (size, total_size) = (*size as f64, *total_size as f64);
                let fraction = if total_size > 0.0 { (size / total_size) as f32 } else { 0.0 };
                self.rep.progress(
                    fraction,
                    format!(
                        "Descargando Minecraft · {}/{} archivos · {:.1}/{:.1} MB",
                        count,
                        total_count,
                        util::mb(size),
                        util::mb(total_size)
                    ),
                );
            }
            E::DownloadedResources { .. } => self.rep.log("Asset download complete."),
            _ => {}
        }
    }
}

impl moj::Handler for ProgressHandler {
    fn on_event(&mut self, event: moj::Event) {
        if let moj::Event::Base(ev) = &event {
            self.base(ev);
        }
    }
}

impl fabric::Handler for ProgressHandler {
    fn on_event(&mut self, event: fabric::Event) {
        match &event {
            fabric::Event::Mojang(moj::Event::Base(ev)) => self.base(ev),
            fabric::Event::FetchVersion { .. } => self.rep.busy("Fetching loader (Fabric/Quilt)…"),
            _ => {}
        }
    }
}

impl forge::Handler for ProgressHandler {
    fn on_event(&mut self, event: forge::Event) {
        match &event {
            forge::Event::Mojang(moj::Event::Base(ev)) => self.base(ev),
            forge::Event::Mojang(_) => {}
            // Installing the loader itself (downloading the installer and running processors).
            _ => self.rep.busy("Installing loader (the first run may take a few minutes)…"),
        }
    }
}

/// Installs and launches the game and loader specified by the modpack. Returns the exit code.
pub fn play(pack: &LoadedPack, params: &PlayParams, rep: &Reporter) -> Result<Option<i32>> {
    let m = &pack.manifest;
    let mc = m.minecraft.version.trim().to_string();
    if mc.is_empty() {
        bail!("The modpack does not specify a Minecraft version.");
    }
    let kind = m.loader.kind.trim().to_lowercase();
    let lv = m.loader.version.trim().to_string();

    rep.busy("Checking account…");
    let auth = resolve_auth(&params.account, &params.db_path)?;
    let handler = ProgressHandler { rep: rep.clone() };
    rep.busy("Preparando Minecraft…");

    let mut game = match kind.as_str() {
        "" | "vanilla" | "none" => {
            let mut inst = moj::Installer::new(mc.clone());
            configure(&mut inst, &params.install, &auth);
            inst.install(handler).map_err(|e| anyhow!("Minecraft installation: {e}"))?
        }
        "fabric" | "quilt" => {
            let loader = if kind == "quilt" { fabric::Loader::Quilt } else { fabric::Loader::Fabric };
            let mut inst = if lv.is_empty() {
                fabric::Installer::new(loader, mc.clone(), fabric::LoaderVersion::Stable)
            } else {
                fabric::Installer::new(loader, mc.clone(), lv.clone())
            };
            configure(inst.mojang_mut(), &params.install, &auth);
            inst.install(handler).map_err(|e| anyhow!("{kind} installation: {e}"))?
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
            configure(inst.mojang_mut(), &params.install, &auth);
            inst.install(handler).map_err(|e| anyhow!("{kind} installation: {e}"))?
        }
        other => bail!("Loader not supported by the launcher: «{other}»"),
    };

    // Maximum RAM: la del jugador y, si no la puso, la recomendada por el modpack (si la hay).
    if let Some(ram) = params.ram_mb.or(m.runtime.recommended_ram_mb) {
        game.jvm_args.push(format!("-Xmx{ram}M"));
        rep.log(format!("Maximum allocated RAM: {ram} MB"));
    }

    let mut cmd = game.command();
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    rep.busy("Minecraft is running…");
    rep.status("Minecraft is running");
    let mut child = cmd.spawn().context("Could not start Java")?;
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

    let status = child.wait().context("Error while waiting for Minecraft")?;
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
