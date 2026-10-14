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


/// On macOS, checks whether the download is a ZIP containing a valid app bundle
/// with the same executable as the currently running launcher.
#[cfg(target_os = "macos")]
fn is_app_bundle_archive(path: &Path, executable_name: &str) -> Result<bool> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return Ok(false),
    };

    let archive = match zip::ZipArchive::new(file) {
        Ok(archive) => archive,
        Err(_) => return Ok(false),
    };

    let mut app_roots = std::collections::BTreeSet::new();

    for name in archive.file_names() {
        let path = Path::new(name);
        let components: Vec<_> = path.components().collect();

        for (index, component) in components.iter().enumerate() {
            if component.as_os_str().to_string_lossy().ends_with(".app") {
                let root = components[..=index]
                    .iter()
                    .map(|c| c.as_os_str())
                    .collect::<PathBuf>();

                app_roots.insert(root);
                break;
            }
        }
    }

    for root in app_roots {
        let info = root.join("Contents/Info.plist");
        let executable = root.join("Contents/MacOS").join(executable_name);

        let mut has_info = false;
        let mut has_executable = false;

        for name in archive.file_names() {
            if Path::new(name) == info {
                has_info = true;
            }

            if Path::new(name) == executable {
                has_executable = true;
            }

            if has_info && has_executable {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

/// Installs an app bundle ZIP after the current launcher exits.
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

    let macos_dir = exe
        .parent()
        .ok_or_else(|| anyhow!("invalid executable path"))?;

    let contents_dir = macos_dir
        .parent()
        .ok_or_else(|| anyhow!("could not find Contents directory"))?;

    let app_path = contents_dir
        .parent()
        .ok_or_else(|| anyhow!("could not find the app bundle"))?;

    if app_path.extension().and_then(|v| v.to_str()) != Some("app") {
        bail!("the launcher is not running from a .app bundle");
    }

    let app_parent = app_path
        .parent()
        .ok_or_else(|| anyhow!("the app bundle has no parent directory"))?;

    let app_name = app_path
        .file_name()
        .ok_or_else(|| anyhow!("invalid app bundle name"))?;

    let pid = std::process::id();

    let staging = app_parent.join(format!(
        ".{}.update-staging-{pid}",
        app_name.to_string_lossy()
    ));

    let backup = app_parent.join(format!(
        ".{}.update-backup-{pid}",
        app_name.to_string_lossy()
    ));

    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }

    if backup.exists() {
        fs::remove_dir_all(&backup)?;
    }

    fs::create_dir_all(&staging)?;

    let extract_result = (|| -> Result<()> {
        let file = fs::File::open(downloaded_zip)?;
        let mut archive = zip::ZipArchive::new(file)
            .context("the download is not a valid ZIP archive")?;

        let executable_name = exe
            .file_name()
            .ok_or_else(|| anyhow!("invalid executable name"))?;

        let mut roots = std::collections::BTreeSet::new();

        for name in archive.file_names() {
            let entry_path = Path::new(name);
            let components: Vec<_> = entry_path.components().collect();

            for (index, component) in components.iter().enumerate() {
                if component.as_os_str().to_string_lossy().ends_with(".app") {
                    roots.insert(
                        components[..=index]
                            .iter()
                            .map(|c| c.as_os_str())
                            .collect::<PathBuf>(),
                    );
                    break;
                }
            }
        }

        let root = roots
            .into_iter()
            .find(|root| {
                let info = root.join("Contents/Info.plist");
                let executable = root.join("Contents/MacOS").join(executable_name);

                let mut info_found = false;
                let mut executable_found = false;

                for name in archive.file_names() {
                    let path = Path::new(name);

                    if path == info {
                        info_found = true;
                    }

                    if path == executable {
                        executable_found = true;
                    }
                }

                info_found && executable_found
            })
            .ok_or_else(|| {
                anyhow!("the ZIP does not contain a valid launcher app bundle")
            })?;

        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;

            let Some(entry_path) = entry.enclosed_name().map(Path::to_path_buf)
            else {
                continue;
            };

            let Ok(relative) = entry_path.strip_prefix(&root) else {
                continue;
            };

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

            let mut target = fs::File::create(&output)?;
            std::io::copy(&mut entry, &mut target)?;

            if let Some(mode) = entry.unix_mode() {
                fs::set_permissions(
                    &output,
                    fs::Permissions::from_mode(mode & 0o777),
                )?;
            }
        }

        let staged_executable = staging
            .join("Contents/MacOS")
            .join(executable_name);

        if !staged_executable.is_file()
            || !staging.join("Contents/Info.plist").is_file()
        {
            bail!("the extracted app bundle is incomplete");
        }

        let mut permissions =
            fs::metadata(&staged_executable)?.permissions();

        permissions.set_mode(permissions.mode() | 0o111);

        fs::set_permissions(&staged_executable, permissions)?;

        Ok(())
    })();

    if let Err(error) = extract_result {
        let _ = fs::remove_dir_all(&staging);
        let _ = fs::remove_file(downloaded_zip);

        return Err(error.context("could not prepare the downloaded app bundle"));
    }

    fn shell_quote(value: &std::ffi::OsStr) -> String {
        let value = value.to_string_lossy();
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    let helper = util::app_data_dir()
        .join(format!("update-app-bundle-{pid}.sh"));

    if let Some(parent) = helper.parent() {
        fs::create_dir_all(parent)?;
    }

    let script = format!(
        "#!/bin/sh\n\
         set -eu\n\
         parent=\"$PPID\"\n\
         while kill -0 \"$parent\" 2>/dev/null; do sleep 0.2; done\n\
         app={}\n\
         staging={}\n\
         backup={}\n\
         if [ -e \"$backup\" ]; then rm -rf \"$backup\"; fi\n\
         if [ -e \"$app\" ]; then mv \"$app\" \"$backup\"; fi\n\
         if ! mv \"$staging\" \"$app\"; then \
             [ ! -e \"$backup\" ] || mv \"$backup\" \"$app\"; exit 1; \
         fi\n\
         rm -rf \"$backup\" || true\n\
         exec /usr/bin/open -n \"$app\" --args \"$@\"\n",
        shell_quote(app_path.as_os_str()),
        shell_quote(staging.as_os_str()),
        shell_quote(backup.as_os_str())
    );

    fs::write(&helper, script)?;
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))?;

    let _ = fs::remove_file(downloaded_zip);

    save_state(url, etag, last_modified, Some(size));

    rep.log("App bundle update prepared; it will be installed after the launcher exits.");

    Ok(Outcome::Updated(helper))
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
	
	
	#[cfg(target_os = "macos")]
	{
		let executable_name = exe
			.file_name()
			.and_then(|n| n.to_str())
			.unwrap_or("modpack-launcher");

		if is_app_bundle_archive(&new_path, executable_name)? {
			return apply_app_bundle(
				exe,
				&new_path,
				url,
				&etag,
				&last_modified,
				size,
				rep,
			);
		}
	}


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
