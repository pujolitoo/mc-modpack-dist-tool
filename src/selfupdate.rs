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
//!
//! macOS app bundles: if the update link points to a zipped `.app` (the URL ends in `.zip`) and the launcher runs from
//! inside a `.app`, the whole bundle is replaced instead of just the executable. The new bundle is extracted next to
//! the old one and a small helper script swaps them once this process has exited (a running app cannot replace itself),
//! then reopens the app. If anything goes wrong the previous version is put back.

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
    /// The executable has been replaced: restart using this path.
    /// (macOS app bundles: the path of the helper script that installs the new bundle and reopens the app.)
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
    let exe = std::env::current_exe().context("No se pudo localizar el ejecutable del launcher")?;
    // A zipped .app as the update asset needs different handling, and can fail early with a helpful message.
    let bundle = is_bundle_update(&exe, &url)?;
    rep.busy("Comprobando actualizaciones del launcher…");

    let remote = fetch_remote(&url)?;
    let state = load_state();
    let own_size = fs::metadata(&exe)?.len();

    let has_baseline = state.url == url;
    let newer = if !has_baseline {
        if bundle {
            // The published file is a zipped bundle, so its size cannot be compared with the executable's.
            // Assume the installed version is the published one and remember it as the reference.
            false
        } else {
            // No previous reference: a different file size is the only reliable indication of a new version.
            remote.size.map_or(false, |s| s != own_size)
        }
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
    apply(&exe, &url, rep, bundle)
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


/// If `exe` lives inside a macOS app bundle (`Foo.app/Contents/MacOS/<exe>`), returns the path of `Foo.app`.
fn app_bundle_root(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    let is_bundle = macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && app.extension()? == "app";
    if is_bundle { Some(app.to_path_buf()) } else { None }
}

/// Is the published update a zipped macOS app bundle that can be installed over the running one?
///
/// Returns an error (before downloading anything) when the update is a bundle but cannot be installed:
/// the launcher is not running from a `.app`, or macOS is running it from a temporary read-only location.
fn is_bundle_update(exe: &Path, url: &str) -> Result<bool> {
    let path_part = url.split(|c: char| c == '?' || c == '#').next().unwrap_or("").to_lowercase();
    if !cfg!(target_os = "macos") || !path_part.ends_with(".zip") {
        return Ok(false);
    }
    let Some(app) = app_bundle_root(exe) else {
        bail!("the update is a macOS app bundle (.zip) but the launcher is not running from a .app");
    };
    // "App Translocation": an app opened straight from Downloads runs from a randomized read-only copy.
    if app.to_string_lossy().contains("/AppTranslocation/") {
        bail!(
            "macOS is running the app from a temporary read-only location (it was opened straight from Downloads). \
             Move it to /Applications (or any normal folder) and open it again so it can update itself"
        );
    }
    Ok(true)
}

/// Installs a zipped app bundle over the running one.
///
/// The files are extracted next to the app, and a helper script swaps the bundles after this process exits
/// (a running app cannot replace itself) and then reopens the app. The extracted bundle has no quarantine flag,
/// so Gatekeeper does not ask again.
#[cfg(target_os = "macos")]
fn apply_app_bundle(
    exe: &Path,
    downloaded_zip: &Path,
    url: &str,
    etag: &Option<String>,
    last_modified: &Option<String>,
    size: u64,
    rep: &Reporter,
) -> Result<Outcome> {
    use std::os::unix::fs::PermissionsExt;

    let app_path =
        app_bundle_root(exe).ok_or_else(|| anyhow!("the launcher is not running from a .app bundle"))?;
    let app_parent = app_path.parent().ok_or_else(|| anyhow!("the app bundle has no parent directory"))?;
    let app_name = app_path
        .file_name()
        .ok_or_else(|| anyhow!("invalid app bundle name"))?
        .to_string_lossy()
        .to_string();
    let executable_name = exe.file_name().ok_or_else(|| anyhow!("invalid executable name"))?.to_os_string();

    let pid = std::process::id();
    let staging = app_parent.join(format!(".{app_name}.update-staging-{pid}"));
    let backup = app_parent.join(format!(".{app_name}.update-backup-{pid}"));
    let _ = fs::remove_dir_all(&staging);
    let _ = fs::remove_dir_all(&backup);
    fs::create_dir_all(&staging).map_err(|e| {
        let _ = fs::remove_file(downloaded_zip);
        anyhow!("cannot write next to the app in {} ({e}); does your user have write permissions there?", app_parent.display())
    })?;

    let extract_result = (|| -> Result<()> {
        let mut archive = zip::ZipArchive::new(fs::File::open(downloaded_zip)?)
            .context("the download is not a valid ZIP archive")?;

        // Find the .app folder inside the ZIP. The `__MACOSX/` metadata folder that `ditto` adds is ignored
        // because it contains no Info.plist or executable.
        let names: Vec<PathBuf> = archive.file_names().map(PathBuf::from).collect();
        let mut roots = std::collections::BTreeSet::new();
        for name in &names {
            let components: Vec<_> = name.components().collect();
            if let Some(i) = components.iter().position(|c| c.as_os_str().to_string_lossy().ends_with(".app")) {
                roots.insert(components[..=i].iter().map(|c| c.as_os_str()).collect::<PathBuf>());
            }
        }
        let root = roots
            .into_iter()
            .find(|root| {
                names.contains(&root.join("Contents/Info.plist"))
                    && names.contains(&root.join("Contents/MacOS").join(&executable_name))
            })
            .ok_or_else(|| anyhow!("the ZIP does not contain a valid launcher app bundle"))?;

        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            let Some(entry_path) = entry.enclosed_name() else { continue };
            let Ok(relative) = entry_path.strip_prefix(&root) else { continue };
            if relative.as_os_str().is_empty() {
                continue;
            }
            let output = staging.join(relative);

            if entry.is_dir() {
                fs::create_dir_all(&output)?;
                continue;
            }
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            let mode = entry.unix_mode().unwrap_or(0o644);
            if mode & 0o170000 == 0o120000 {
                // A symbolic link: the entry's content is the link target.
                let mut target = String::new();
                entry.read_to_string(&mut target)?;
                std::os::unix::fs::symlink(target.trim_end_matches('\0'), &output)?;
                continue;
            }
            let mut out = fs::File::create(&output)?;
            std::io::copy(&mut entry, &mut out)?;
            fs::set_permissions(&output, fs::Permissions::from_mode(mode & 0o777))?;
        }

        let staged = staging.join("Contents/MacOS").join(&executable_name);
        if !staged.is_file() || !staging.join("Contents/Info.plist").is_file() {
            bail!("the extracted app bundle is incomplete");
        }
        let mut permissions = fs::metadata(&staged)?.permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        fs::set_permissions(&staged, permissions)?;
        Ok(())
    })();

    let _ = fs::remove_file(downloaded_zip);
    if let Err(error) = extract_result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error.context("could not prepare the downloaded app bundle"));
    }

    // Same executable as the one already running: nothing to install (also avoids restart loops).
    let staged_exe = staging.join("Contents/MacOS").join(&executable_name);
    if util::hash_file(&staged_exe, "sha256")? == util::hash_file(exe, "sha256")? {
        let _ = fs::remove_dir_all(&staging);
        save_state(url, etag, last_modified, Some(size));
        rep.log("Launcher is already up to date.");
        return Ok(Outcome::UpToDate);
    }

    fn shell_quote(value: &std::ffi::OsStr) -> String {
        format!("'{}'", value.to_string_lossy().replace('\'', "'\\''"))
    }

    let helper = util::app_data_dir().join(format!("update-app-bundle-{pid}.sh"));
    if let Some(parent) = helper.parent() {
        fs::create_dir_all(parent)?;
    }
    let script = format!(
        r#"#!/bin/sh
# Waits for the launcher to exit, swaps the app bundle and reopens it.
parent="$PPID"
while kill -0 "$parent" 2>/dev/null; do sleep 0.2; done
app={app}
staging={staging}
backup={backup}
state={state}
rm -rf "$backup"
if mv "$app" "$backup" && mv "$staging" "$app"; then
    rm -rf "$backup"
else
    # Roll back so the previous version keeps working, and forget the saved update state so it is retried.
    [ -e "$app" ] || mv "$backup" "$app"
    rm -rf "$staging"
    rm -f "$state"
fi
rm -f "$0"
exec /usr/bin/open -n "$app" --args "$@"
"#,
        app = shell_quote(app_path.as_os_str()),
        staging = shell_quote(staging.as_os_str()),
        backup = shell_quote(backup.as_os_str()),
        state = shell_quote(state_path().as_os_str()),
    );
    fs::write(&helper, script)?;
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))?;

    save_state(url, etag, last_modified, Some(size));
    rep.log("App bundle update prepared; it will be installed after the launcher exits.");
    Ok(Outcome::Updated(helper))
}

/// macOS: removes leftovers of an interrupted bundle update (staging/backup folders, the downloaded ZIP, helper scripts).
#[cfg(target_os = "macos")]
fn cleanup_bundle_leftovers(exe: &Path) {
    if let Some(app) = app_bundle_root(exe) {
        if let (Some(parent), Some(name)) = (app.parent(), app.file_name()) {
            let name = name.to_string_lossy();
            let prefixes = [format!(".{name}.update-staging-"), format!(".{name}.update-backup-")];
            if let Ok(entries) = fs::read_dir(parent) {
                for entry in entries.flatten() {
                    let file_name = entry.file_name().to_string_lossy().to_string();
                    if prefixes.iter().any(|p| file_name.starts_with(p.as_str())) {
                        let _ = fs::remove_dir_all(entry.path());
                    }
                }
            }
        }
    }
    let data = util::app_data_dir();
    let _ = fs::remove_file(data.join("launcher-update.zip"));
    if let Ok(entries) = fs::read_dir(&data) {
        for entry in entries.flatten() {
            let file_name = entry.file_name().to_string_lossy().to_string();
            if file_name.starts_with("update-app-bundle-") && file_name.ends_with(".sh") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

fn apply(exe: &PathBuf, url: &str, rep: &Reporter, bundle: bool) -> Result<Outcome> {
    let dir = exe.parent().ok_or_else(|| anyhow!("invalid launcher path"))?;
    let fname = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| anyhow!("invalid launcher path"))?;
    // A bundle ZIP is downloaded outside the app (the app folder is not touched until the swap).
    let new_path = if bundle {
        util::app_data_dir().join("launcher-update.zip")
    } else {
        dir.join(format!("{fname}.new"))
    };
    if let Some(parent) = new_path.parent() {
        fs::create_dir_all(parent)?;
    }
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

    #[cfg(target_os = "macos")]
    {
        if bundle {
            return apply_app_bundle(exe, &new_path, url, &etag, &last_modified, size, rep);
        }
    }
    let _ = bundle; // only used on macOS

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
    #[cfg(target_os = "macos")]
    cleanup_bundle_leftovers(&exe);
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
