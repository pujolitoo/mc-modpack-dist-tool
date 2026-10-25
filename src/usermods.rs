//! Mods added by the player: their own `.jar` files, installed next to the modpack's mods.
//!
//! They are copied into the modpack's mods folder of the installation. The launcher never deletes them: the
//! synchronization only removes the mods it installed itself (see `sync.rs`). A JAR in that folder counts as
//! "added by the player" when its file name is not one of the modpack's mods.

use crate::i18n::t;
use crate::pack::LoadedPack;
use crate::util;
use anyhow::{anyhow, bail, Result};
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

fn is_jar(name: &str) -> bool {
    name.to_lowercase().ends_with(".jar")
}

/// Mods folder of the installation.
fn mods_dir(pack: &LoadedPack, install: &Path) -> Result<PathBuf> {
    util::safe_join(install, &pack.manifest.mods_dir())
}

/// File names (lowercase) used in the mods folder by the modpack's mods, whatever their state.
fn pack_names(pack: &LoadedPack, install: &Path, dir: &Path) -> HashSet<String> {
    let rel_dir = pack.manifest.mods_dir();
    pack.manifest
        .mods
        .iter()
        .filter_map(|m| m.rel_path(&rel_dir).ok())
        .filter_map(|rel| util::safe_join(install, &rel).ok())
        .filter(|p| p.parent() == Some(dir))
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()))
        .collect()
}

/// Mods added by the player to this installation (file names), sorted.
pub fn list(pack: &LoadedPack, install: &str) -> Vec<String> {
    let install = install.trim();
    if install.is_empty() {
        return Vec::new();
    }
    let install = Path::new(install);
    let Ok(dir) = mods_dir(pack, install) else { return Vec::new() };
    let Ok(entries) = fs::read_dir(&dir) else { return Vec::new() };
    let known = pack_names(pack, install, &dir);
    let mut out: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| is_jar(n) && !known.contains(&n.to_lowercase()))
        .collect();
    out.sort_by_key(|n| n.to_lowercase());
    out
}

/// A JAR is a zip archive: check the signature, so that a renamed file is not installed by mistake.
fn looks_like_jar(path: &Path) -> bool {
    let mut head = [0u8; 4];
    fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && head == *b"PK\x03\x04"
}

/// Copies the chosen files into the mods folder. Returns how many were added and one message per file that was not.
pub fn add(pack: &LoadedPack, install: &str, files: &[PathBuf]) -> (usize, Vec<String>) {
    let install = install.trim();
    if install.is_empty() {
        return (0, vec![t("main.choose_installation_folder").to_string()]);
    }
    let install = Path::new(install);
    let dir = match mods_dir(pack, install).and_then(|d| fs::create_dir_all(&d).map(|_| d).map_err(Into::into)) {
        Ok(d) => d,
        Err(e) => return (0, vec![tf!("usermods.could_not_copy", name = pack.manifest.mods_dir(), error = e)]),
    };
    let known = pack_names(pack, install, &dir);
    let (mut added, mut errors) = (0, Vec::new());
    for src in files {
        let name = src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if !is_jar(&name) {
            errors.push(tf!("usermods.not_a_jar", name = name));
        } else if known.contains(&name.to_lowercase()) {
            errors.push(tf!("usermods.name_used_by_pack", name = name));
        } else if !looks_like_jar(src) {
            errors.push(tf!("usermods.not_a_mod_archive", name = name));
        } else {
            let dest = dir.join(&name);
            let same = fs::canonicalize(src).ok().is_some_and(|a| fs::canonicalize(&dest).ok() == Some(a));
            match if same { Ok(0) } else { fs::copy(src, &dest) } {
                Ok(_) => added += 1,
                Err(e) => errors.push(tf!("usermods.could_not_copy", name = name, error = e)),
            }
        }
    }
    (added, errors)
}

/// Deletes a mod added by the player (never one of the modpack's).
pub fn remove(pack: &LoadedPack, install: &str, name: &str) -> Result<()> {
    let install = Path::new(install.trim());
    let dir = mods_dir(pack, install)?;
    let name = util::safe_file_name(name)?;
    if !is_jar(&name) || pack_names(pack, install, &dir).contains(&name.to_lowercase()) {
        bail!(tf!("usermods.name_used_by_pack", name = name));
    }
    fs::remove_file(dir.join(&name)).map_err(|e| anyhow!(tf!("usermods.could_not_remove", name = name, error = e)))
}
