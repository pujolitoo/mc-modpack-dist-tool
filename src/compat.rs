//! Linux: works around zlib-ng breaking the Forge/NeoForge installer.
//!
//! Forge and NeoForge generate JAR files while installing and then verify their exact checksums. Distributions that
//! replace zlib with zlib-ng (Fedora, openSUSE Tumbleweed, CachyOS...) compress differently, so that check fails even
//! though nothing is corrupted. The installer's processors are run by the system Java, which uses the system zlib.
//!
//! Fix: when zlib-ng is detected, the launcher restarts itself once with a classic zlib preloaded (`LD_PRELOAD`).
//! The classic `libz.so.1` is embedded in Linux builds (see build.rs and assets/linux). Every process the launcher
//! starts afterwards, including the Java that runs Forge's processors, inherits it.
//!
//! Overrides: MODPACK_LAUNCHER_ZLIB=force (always do it) | off (never do it).

#[cfg(target_os = "linux")]
use crate::{config, util};
#[cfg(target_os = "linux")]
use std::{
    fs,
    path::{Path, PathBuf},
};

/// Set once the launcher runs with the classic zlib, so it is never restarted twice.
pub const PRELOADED_MARKER: &str = "MODPACK_LAUNCHER_ZLIB_PRELOADED";

/// Restarts the launcher with a classic zlib preloaded when the system uses zlib-ng.
/// It must be the first thing `main` does: it replaces the process, so no thread may exist yet.
#[cfg(target_os = "linux")]
pub fn ensure_classic_zlib() {
    use std::os::unix::process::CommandExt;

    if std::env::var_os(PRELOADED_MARKER).is_some() {
        return; // already running with the classic zlib
    }
    let mode = std::env::var("MODPACK_LAUNCHER_ZLIB").unwrap_or_default().trim().to_lowercase();
    if mode == "off" {
        return;
    }
    if mode != "force" {
        // The user already preloads a libz of their own (for example the manual workaround): leave it alone.
        if std::env::var("LD_PRELOAD").map(|v| v.contains("libz")).unwrap_or(false) {
            return;
        }
        if !system_zlib_is_ng() {
            return;
        }
    }
    let Some(bytes) = config::classic_zlib() else { return }; // this build does not include one
    let Some(lib) = install_classic_zlib(bytes) else { return };
    let lib = lib.to_string_lossy().to_string();
    let preload = match std::env::var("LD_PRELOAD") {
        Ok(existing) if !existing.trim().is_empty() => format!("{lib} {existing}"),
        _ => lib,
    };
    let Ok(exe) = std::env::current_exe() else { return };
    // `exec` replaces this process and only returns if it fails; in that case just carry on without the preload.
    let _ = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("LD_PRELOAD", preload)
        .env(PRELOADED_MARKER, "1")
        .exec();
}

#[cfg(not(target_os = "linux"))]
pub fn ensure_classic_zlib() {}

/// Does the system's `libz.so.1` come from zlib-ng?
#[cfg(target_os = "linux")]
fn system_zlib_is_ng() -> bool {
    const CANDIDATES: &[&str] = &[
        "/usr/lib64/libz.so.1",
        "/lib64/libz.so.1",
        "/usr/lib/libz.so.1",
        "/lib/libz.so.1",
        "/usr/lib/x86_64-linux-gnu/libz.so.1",
        "/lib/x86_64-linux-gnu/libz.so.1",
        "/usr/lib/aarch64-linux-gnu/libz.so.1",
        "/lib/aarch64-linux-gnu/libz.so.1",
    ];
    CANDIDATES
        .iter()
        .filter_map(|path| fs::canonicalize(path).ok())
        .any(|real| is_zlib_ng_file_name(&real) || file_mentions_zlib_ng(&real))
}

/// zlib-ng's zlib-compatible build installs the real file as `libz.so.1.x.y.zlib-ng`.
#[cfg(target_os = "linux")]
fn is_zlib_ng_file_name(path: &Path) -> bool {
    path.file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .map(|name| name.contains("zlib-ng") || name.contains("libz-ng"))
        .unwrap_or(false)
}

/// zlib-ng's library contains its own name in its version string (a library is only ~100 KB, so reading it is cheap).
#[cfg(target_os = "linux")]
fn file_mentions_zlib_ng(path: &Path) -> bool {
    fs::read(path).map(|bytes| bytes.windows(7).any(|window| window == b"zlib-ng")).unwrap_or(false)
}

/// Writes the embedded libz.so.1 to the launcher's data folder (only if missing or different) and returns its path.
/// `LD_PRELOAD` entries are split on spaces and colons, so a path containing either cannot be used.
#[cfg(target_os = "linux")]
fn install_classic_zlib(bytes: &[u8]) -> Option<PathBuf> {
    let dir = util::app_data_dir().join("compat");
    let file = dir.join("libz.so.1");
    if file.to_string_lossy().chars().any(|c| c.is_whitespace() || c == ':') {
        return None;
    }
    if fs::read(&file).map(|existing| existing == bytes).unwrap_or(false) {
        return Some(file);
    }
    fs::create_dir_all(&dir).ok()?;
    let tmp = dir.join("libz.so.1.part");
    fs::write(&tmp, bytes).ok()?;
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).ok()?;
    }
    fs::rename(&tmp, &file).ok()?;
    Some(file)
}
