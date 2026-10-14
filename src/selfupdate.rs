//! Launcher self-update: if the .exe published at `update_url` (e.g. a Dropbox link) changes,
//! the launcher downloads it, replaces itself and restarts.
//!
//! How it detects changes without downloading the file every time:
//!  - Compara la cabecera ETag (Dropbox la mantiene estable mientras el archivo no cambia), o Last-Modified,
//!    or size, against the values saved last time.
//!  - On first run (with no reference), a file is only considered new if its size differs from the current .exe.
//! Protecciones:
//!  - Only installs the file if it appears to be an executable for this system (avoids installing Dropbox's HTML page).
//!  - If the downloaded file is identical to the current one, it is not installed (avoids restart loops caused by unstable ETags).
//!  - Si falla el reemplazo, restaura el ejecutable anterior.

use crate::config;
use crate::msg::Reporter;
use crate::util;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub enum Outcome {
    /// No hay `update_url` configurada.
    Disabled,
    UpToDate,
    /// Ya se reemplazó el ejecutable: hay que reiniciar con esta ruta.
    Updated(PathBuf),
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    url: String,
    etag: Option<String>,
    last_modified: Option<String>,
    size: Option<u64>,
}

struct Remote {
    etag: Option<String>,
    last_modified: Option<String>,
    size: Option<u64>,
}

fn state_path() -> PathBuf {
    util::app_data_dir().join("update-state.json")
}

fn load_state() -> State {
    fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(url: &str, etag: &Option<String>, last_modified: &Option<String>, size: Option<u64>) {
    let st = State { url: url.to_string(), etag: etag.clone(), last_modified: last_modified.clone(), size };
    let _ = fs::create_dir_all(util::app_data_dir());
    if let Ok(s) = serde_json::to_string_pretty(&st) {
        let _ = fs::write(state_path(), s);
    }
}

/// Convierte un enlace compartido de Dropbox en uno de descarga directa (dl=1). Otros servidores no se tocan.
pub fn direct_url(link: &str) -> String {
    let link = link.trim();
    let is_dropbox = link
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .map(|host| host.to_lowercase().ends_with("dropbox.com"))
        .unwrap_or(false);
    if !is_dropbox {
        return link.to_string();
    }
    if link.contains("dl=0") {
        return link.replacen("dl=0", "dl=1", 1);
    }
    if link.contains("dl=1") || link.contains("raw=1") {
        return link.to_string();
    }
    if link.contains('?') {
        format!("{link}&dl=1")
    } else {
        format!("{link}?dl=1")
    }
}

fn header(resp: &ureq::Response, name: &str) -> Option<String> {
    resp.header(name).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Checks the server for the published version without downloading it.
fn fetch_remote(url: &str) -> Result<Remote> {
    let agent = util::agent_quick();
    match agent.head(url).call() {
        Ok(resp) => Ok(Remote {
            etag: header(&resp, "ETag"),
            last_modified: header(&resp, "Last-Modified"),
            size: header(&resp, "Content-Length").and_then(|v| v.parse().ok()),
        }),
        Err(ureq::Error::Status(_, _)) => {
            // The server responded but does not support HEAD: request only the first byte.
            // (Si no hay red no se reintenta, para no retrasar el arranque.)
            let resp = agent
                .get(url)
                .set("Range", "bytes=0-0")
                .call()
                .map_err(|e| anyhow!("could not contact the update server ({e})"))?;
            let size = if resp.status() == 206 {
                header(&resp, "Content-Range").and_then(|v| v.rsplit('/').next().and_then(|n| n.trim().parse().ok()))
            } else {
                header(&resp, "Content-Length").and_then(|v| v.parse().ok())
            };
            Ok(Remote { etag: header(&resp, "ETag"), last_modified: header(&resp, "Last-Modified"), size })
        }
        Err(e) => Err(anyhow!("could not contact the update server ({e})")),
    }
}

/// Does this look like an executable for this operating system? (checks the signature of the first bytes)
fn looks_like_executable(path: &Path) -> Result<bool> {
    let mut f = fs::File::open(path)?;
    let mut magic = [0u8; 4];
    if f.read(&mut magic)? < 4 {
        return Ok(false);
    }
    Ok(if cfg!(windows) {
        &magic[..2] == b"MZ"
    } else if cfg!(target_os = "macos") {
        matches!(
            magic,
            [0xCF, 0xFA, 0xED, 0xFE] | [0xFE, 0xED, 0xFA, 0xCF] | [0xCA, 0xFE, 0xBA, 0xBE] | [0xBE, 0xBA, 0xFE, 0xCA]
        )
    } else {
        magic == [0x7f, b'E', b'L', b'F']
    })
}

/// Checks for a new launcher version and installs it if available.
pub fn run(rep: &Reporter) -> Result<Outcome> {
    let Some(link) = config::update_url() else { return Ok(Outcome::Disabled) };
    let url = direct_url(link);
    rep.busy("Comprobando actualizaciones del launcher…");

    let remote = fetch_remote(&url)?;
    let state = load_state();
    let exe = std::env::current_exe().context("No se pudo localizar el ejecutable del launcher")?;
    let own_size = fs::metadata(&exe)?.len();

    let has_baseline = state.url == url;
    let newer = if !has_baseline {
        // No previous reference: a different file size is the only reliable indication of a new version.
        remote.size.map_or(false, |s| s != own_size)
    } else if let (Some(a), Some(b)) = (&remote.etag, &state.etag) {
        a != b
    } else if let (Some(a), Some(b)) = (&remote.last_modified, &state.last_modified) {
        a != b
    } else if let (Some(a), Some(b)) = (remote.size, state.size) {
        a != b
    } else {
        false
    };

    if !newer {
        if !has_baseline {
            save_state(&url, &remote.etag, &remote.last_modified, remote.size);
        }
        return Ok(Outcome::UpToDate);
    }
    rep.log("A new launcher version is available: downloading…");
    apply(&exe, &url, rep)
}

/// Como `run`, pero un fallo (sin red, permisos…) solo se anota en el registro. Devuelve la ruta si hay que reiniciar.
pub fn run_or_log(rep: &Reporter) -> Option<PathBuf> {
    match run(rep) {
        Ok(Outcome::Updated(path)) => Some(path),
        Ok(_) => None,
        Err(e) => {
            rep.log(format!("No se pudo actualizar el launcher: {e:#}"));
            None
        }
    }
}

fn apply(exe: &PathBuf, url: &str, rep: &Reporter) -> Result<Outcome> {
    let dir = exe.parent().ok_or_else(|| anyhow!("invalid launcher path"))?;
    let fname = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| anyhow!("invalid launcher path"))?;
    let new_path = dir.join(format!("{fname}.new"));
    let old_path = dir.join(format!("{fname}.old"));
    let cleanup = || {
        let _ = fs::remove_file(&new_path);
    };

    let resp = util::agent()
        .get(url)
        .call()
        .map_err(|e| anyhow!("could not download update ({e})"))?;
    let etag = header(&resp, "ETag");
    let last_modified = header(&resp, "Last-Modified");
    let total = header(&resp, "Content-Length").and_then(|v| v.parse::<u64>().ok());

    let rep2 = rep.clone();
    let res = util::copy_stream(resp.into_reader(), &new_path, total, move |done, total| match total {
        Some(t) if t > 0 => rep2.progress(
            done as f32 / t as f32,
            format!("Descargando el launcher… {:.1}/{:.1} MB", util::mb(done as f64), util::mb(t as f64)),
        ),
        _ => rep2.busy(format!("Descargando el launcher… {:.1} MB", util::mb(done as f64))),
    });
    if let Err(e) = res {
        cleanup();
        return Err(e.context("launcher download failed"));
    }
    let size = fs::metadata(&new_path)?.len();

    if !looks_like_executable(&new_path)? {
        cleanup();
        bail!(
            "the downloaded file is not a valid executable (does the link point to Dropbox's preview page? \
             Use a link with dl=1)"
        );
    }

    // If it is identical to the running version, there is nothing to install (avoids restart loops).
    if util::hash_file(&new_path, "sha256")? == util::hash_file(exe, "sha256")? {
        cleanup();
        save_state(url, &etag, &last_modified, Some(size));
        rep.log("Launcher is already up to date.");
        return Ok(Outcome::UpToDate);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(exe)?.permissions().mode();
        fs::set_permissions(&new_path, fs::Permissions::from_mode(mode | 0o111))?;
    }

    // A running executable cannot be overwritten, but it can be renamed (Windows, Linux and macOS).
    let _ = fs::remove_file(&old_path);
    fs::rename(exe, &old_path).map_err(|e| {
        cleanup();
        anyhow!("could not replace launcher ({e}). Does the folder have write permissions?")
    })?;
    if let Err(e) = fs::rename(&new_path, exe) {
        let _ = fs::rename(&old_path, exe); // roll back
        cleanup();
        bail!("could not install the new version ({e}); keeping the previous version");
    }
    save_state(url, &etag, &last_modified, Some(size));
    rep.log("Launcher actualizado.");
    Ok(Outcome::Updated(exe.clone()))
}

/// Borra restos de actualizaciones anteriores (el .exe viejo y descargas incompletas). Se llama al arrancar.
pub fn cleanup_old() {
    let Ok(exe) = std::env::current_exe() else { return };
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name()) else { return };
    let name = name.to_string_lossy().to_string();

    let _ = fs::remove_file(dir.join(format!("{name}.new")));

    let old = dir.join(format!("{name}.old"));
    if !old.exists() {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..120 {
            if !old.exists() || fs::remove_file(&old).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    });
}

/// Launches the newly installed executable and closes the current one.
pub fn restart(exe: &Path) -> ! {
    let _ = std::process::Command::new(exe).args(std::env::args_os().skip(1)).spawn();
    std::process::exit(0)
}
