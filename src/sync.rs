//! Synchronizes the installation with the modpack: downloads/verifies mods, applies overrides and removes leftovers.
//!
//! Two sides: the player's installation (`sync_pack`) and a dedicated server (`sync_server`, used by
//! `--sync-server`), which gets the mods that run on a server and not the client-only ones.

use crate::i18n::t;
use crate::msg::Reporter;
use crate::pack::{effective_excluded, LoadedPack, ModEntry, ModState};
use crate::util;
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Cursor};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use zip::ZipArchive;

const STATE_FILE: &str = ".modpack-forge-state.json";
const WORKERS: usize = 6;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    pack_id: String,
    pack_version: String,
    /// Relative paths of mods installed by the launcher (only these are deleted during updates).
    mods: Vec<String>,
}

/// Synchronizes the player's installation (`excluded`: client-side-only mods the player removed).
pub fn sync_pack(pack: &LoadedPack, install: &Path, excluded: &HashSet<String>, rep: &Reporter) -> Result<()> {
    // Client-side-only mods the player chose to exclude (filtered already: dependencies and required mods are never included).
    let eff = effective_excluded(&pack.manifest, excluded);
    let removed_by_player = |m: &&ModEntry| m.is_client_only() && eff.contains(&m.key());
    let active: Vec<&ModEntry> = pack
        .manifest
        .mods
        .iter()
        .filter(|m| m.state() == ModState::Active && !removed_by_player(m))
        .collect();
    let skipped = pack.manifest.mods.iter().filter(|m| m.state() == ModState::Active && removed_by_player(m)).count();
    if skipped > 0 {
        rep.log(tf!("sync.mods_removed_by_player_skipped", skipped = skipped));
    }
    sync_files(pack, install, &active, pack.override_dirs(), false, rep)
}

/// Synchronizes a dedicated server: mods that can run on a server (not the client-only ones) and, if
/// `apply_config`, the configuration (overrides; the client-only ones are skipped). Returns the number of mods.
pub fn sync_server(pack: &LoadedPack, server_dir: &Path, apply_config: bool, rep: &Reporter) -> Result<usize> {
    let mods_rel = pack.manifest.mods_dir();
    let mut active: Vec<&ModEntry> = Vec::new();
    let mut client_only: Vec<&str> = Vec::new();
    for m in pack.manifest.mods.iter().filter(|m| m.enabled) {
        let rel = m.rel_path(&mods_rel)?;
        if m.side.server.as_deref() == Some("unsupported") || client_only_file(&rel) {
            client_only.push(&m.name);
        } else {
            active.push(m);
        }
    }
    if !client_only.is_empty() {
        rep.log(tf!("sync.client_only_skipped", count = client_only.len(), mods = client_only.join(", ")));
    }
    // Modrinth packs: the server takes server-overrides/ instead of client-overrides/.
    let dirs: Vec<String> = if apply_config {
        pack.override_dirs()
            .iter()
            .map(|d| if d == "client-overrides/" { "server-overrides/".to_string() } else { d.clone() })
            .collect()
    } else {
        Vec::new()
    };
    sync_files(pack, server_dir, &active, &dirs, true, rep)?;
    Ok(active.len())
}

/// Files that only matter to a game client (player options, server list, shaders, resource packs).
fn client_only_file(rel: &str) -> bool {
    let rel = rel.replace('\\', "/").to_lowercase();
    let top = rel.split('/').next().unwrap_or("");
    ["options.txt", "optionsof.txt", "optionsshaders.txt", "servers.dat", "shaderpacks", "resourcepacks"].contains(&top)
}

/// Common part: installs `active`, removes the mods it installed before that are no longer wanted and applies the
/// override folders `override_dirs` (on a server, skipping the client-only files).
fn sync_files(
    pack: &LoadedPack,
    install: &Path,
    active: &[&ModEntry],
    override_dirs: &[String],
    server: bool,
    rep: &Reporter,
) -> Result<()> {
    fs::create_dir_all(install)?;
    let mods_rel = pack.manifest.mods_dir();
    fs::create_dir_all(util::safe_join(install, &mods_rel)?)?;
    let state_path = install.join(STATE_FILE);
    let old: State = fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let mut wanted: Vec<String> = Vec::with_capacity(active.len());
    for m in active {
        wanted.push(m.rel_path(&mods_rel)?);
    }

    // 1) Remove mods installed by the launcher that are no longer in the pack.
    for rel in &old.mods {
        if !wanted.contains(rel) {
            if let Ok(p) = util::safe_join(install, rel) {
                if p.is_file() && fs::remove_file(&p).is_ok() {
                    rep.log(tf!("sync.deleted", path = rel));
                }
            }
        }
    }

    // 2) Download / extract / verify in parallel.
    let total = active.len();
    rep.progress(0.0, tf!("sync.checking_mods", total = total));
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let warnings: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let agent = util::agent();

    std::thread::scope(|scope| {
        for _ in 0..WORKERS.min(total.max(1)) {
            let rep = rep.clone();
            let (next, done, failures, warnings) = (&next, &done, &failures, &warnings);
            let (agent, wanted) = (&agent, &wanted);
            scope.spawn(move || {
                let mut archive = pack.archive().ok().flatten();
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= active.len() {
                        break;
                    }
                    let m = active[i];
                    match ensure_mod(pack, &mut archive, agent, install, &wanted[i], m) {
                        Ok(Some(how)) => rep.log(format!("{how}: {}", m.name)),
                        Ok(None) => {}
                        Err(e) => {
                            let line = format!("{}: {e:#}", m.name);
                            if m.required {
                                failures.lock().unwrap().push(line);
                            } else {
                                warnings.lock().unwrap().push(line);
                            }
                        }
                    }
                    let d = done.fetch_add(1, Ordering::SeqCst) + 1;
                    rep.progress(d as f32 / total as f32, format!("Mods {d}/{total}"));
                }
            });
        }
    });

    for w in warnings.into_inner().unwrap() {
        rep.log(tf!("sync.warning_optional_mod_skipped", details = w));
    }
    let failures = failures.into_inner().unwrap();
    if !failures.is_empty() {
        bail!(tf!("sync.these_required_mods_could_not", mods = failures.join("\n- ")));
    }

    // 3) Overrides: only if the pack changed or the file is missing (preserves your settings between sessions).
    rep.busy(t("sync.applying_configuration"));
    let fresh = old.pack_id != pack.id() || old.pack_version != pack.version();
    let mut applied = 0usize;
    if let Some(mut ar) = pack.archive()? {
        // Multiple possible folders (e.g. Modrinth: overrides/ and client-overrides/); later ones override earlier ones.
        for dir in override_dirs {
            let prefix = format!("{}{}", pack.root(), dir);
            for i in 0..ar.len() {
                let mut f = ar.by_index(i)?;
                if f.is_dir() {
                    continue;
                }
                let name = f.name().to_string();
                let Some(rel) = name.strip_prefix(&prefix) else { continue };
                if rel.is_empty() || (server && client_only_file(rel)) {
                    continue;
                }
                let target = util::safe_join(install, rel)?;
                if target.exists() && !fresh {
                    continue;
                }
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut out = File::create(&target)?;
                io::copy(&mut f, &mut out)?;
                applied += 1;
            }
        }
    }
    if applied > 0 {
        rep.log(tf!("sync.overrides_applied_files", applied = applied));
    }

    let state = State { pack_id: pack.id(), pack_version: pack.version().to_string(), mods: wanted };
    fs::write(&state_path, serde_json::to_string_pretty(&state)?)?;
    rep.progress(1.0, t("sync.mods_ready"));
    Ok(())
}

/// Ensures the mod JAR is present and correct in `mods/`. Returns how it was obtained, or `None` if it was already correct.
fn ensure_mod<'a>(
    pack: &'a LoadedPack,
    archive: &mut Option<ZipArchive<Cursor<&'a [u8]>>>,
    agent: &ureq::Agent,
    install: &Path,
    rel: &str,
    m: &ModEntry,
) -> Result<Option<&'static str>> {
    let dest = util::safe_join(install, rel)?;
    if util::verify_file(&dest, &m.file)? {
        return Ok(None);
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let name = dest.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = dest.with_file_name(format!("{name}.part"));

    let result = (|| -> Result<&'static str> {
        let bundled = m.file.bundled_path.as_deref().filter(|p| !p.is_empty());
        let mut how = t("sync.extracted");
        let from_zip = match (bundled, archive.as_mut()) {
            (Some(b), Some(ar)) => match ar.by_name(&format!("{}{}", pack.root(), b)) {
                Ok(mut f) => {
                    let mut out = File::create(&tmp)?;
                    io::copy(&mut f, &mut out)?;
                    true
                }
                Err(_) => false,
            },
            _ => false,
        };
        if !from_zip {
            let url = m.file.download_url.as_deref().filter(|u| !u.is_empty());
            match url {
                Some(u) => {
                    util::download_to(agent, u, &tmp)?;
                    how = t("sync.downloaded");
                }
                None if m.resolved == Some(false) => {
                    bail!(t("sync.unresolved_mod_its_download_file"))
                }
                None => bail!(t("sync.has_neither_download_url_nor")),
            }
        }
        if !util::verify_file(&tmp, &m.file)? {
            return Err(anyhow!(t("sync.file_hash_or_size_does")));
        }
        if dest.exists() {
            fs::remove_file(&dest)?;
        }
        fs::rename(&tmp, &dest)?;
        Ok(how)
    })();

    if tmp.exists() {
        let _ = fs::remove_file(&tmp);
    }
    result.map(Some)
}
