//! Model of the modpack exported by Modpack Forge (`modpack-forge/1`) and loading from ZIP/JSON/URL.

use crate::i18n::t;
use crate::config::{self, Format};
use crate::external;
use crate::msg::Reporter;
use crate::util;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Cursor, Read};
use zip::ZipArchive;

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PackInfo {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub version: String,
    pub author: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Minecraft {
    pub version: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LoaderInfo {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Runtime {
    pub recommended_ram_mb: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct InstallInfo {
    pub mods_dir: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ModFile {
    pub name: Option<String>,
    pub size: Option<u64>,
    /// Text values (sha1, sha512...) or numeric values (CurseForge murmur2): both are accepted.
    pub hashes: HashMap<String, serde_json::Value>,
    pub download_url: Option<String>,
    pub bundled_path: Option<String>,
    /// Destination path relative to the installation (Modrinth modpacks: mods/, resourcepacks/, config/…).
    /// If missing, it goes to the mods folder under its file name.
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModVersion {
    pub number: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Side {
    pub client: Option<String>,
    pub server: Option<String>,
}

/// Dependency declared by a mod (comes from Modrinth/CurseForge).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Dep {
    pub source: String,
    pub project_id: Option<serde_json::Value>,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: ModVersion,
    #[serde(default)]
    pub side: Side,
    #[serde(default = "yes")]
    pub required: bool,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub file: ModFile,
    #[serde(default)]
    pub resolved: Option<bool>,
    #[serde(default, rename = "projectId")]
    pub project_id: Option<serde_json::Value>,
    #[serde(default)]
    pub dependencies: Vec<Dep>,
}

/// Stable project key: «source:id». Remains stable across mod version updates.
fn project_key(source: &str, id: Option<&serde_json::Value>) -> Option<String> {
    match id {
        Some(serde_json::Value::String(v)) if !v.trim().is_empty() => Some(format!("{source}:{}", v.trim())),
        Some(serde_json::Value::Number(n)) => Some(format!("{source}:{n}")),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModState {
    Active,
    Disabled,
    ServerOnly,
}

impl ModEntry {
    pub fn state(&self) -> ModState {
        if !self.enabled {
            ModState::Disabled
        } else if self.side.client.as_deref() == Some("unsupported") {
            ModState::ServerOnly
        } else {
            ModState::Active
        }
    }

    /// Mod that only works on the client (the server does not support it): the player can remove it.
    /// Only known from Modrinth data; for CurseForge and local JARs the side is «unknown».
    pub fn is_client_only(&self) -> bool {
        self.side.server.as_deref() == Some("unsupported") && self.side.client.as_deref() != Some("unsupported")
    }

    /// Key used to remember the player's choice.
    pub fn key(&self) -> String {
        project_key(&self.source, self.project_id.as_ref()).unwrap_or_else(|| {
            if !self.id.is_empty() {
                format!("id:{}", self.id)
            } else {
                format!("file:{}", self.file.name.clone().unwrap_or_default())
            }
        })
    }

    /// Relative path (with «/») where this file is installed.
    pub fn rel_path(&self, mods_dir: &str) -> Result<String> {
        if let Some(p) = self.file.path.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            return Ok(p.replace('\\', "/").trim_start_matches('/').to_string());
        }
        Ok(format!("{}/{}", mods_dir, self.file_name()?))
    }

    pub fn file_name(&self) -> Result<String> {
        util::safe_file_name(self.file.name.as_deref().unwrap_or(""))
            .with_context(|| format!("mod «{}»", self.name))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    pub schema: String,
    pub pack: PackInfo,
    pub minecraft: Minecraft,
    pub loader: LoaderInfo,
    pub runtime: Runtime,
    pub install: InstallInfo,
    pub mods: Vec<ModEntry>,
}

impl Manifest {
    pub fn mods_dir(&self) -> String {
        let d = self.install.mods_dir.trim().trim_matches(|c| c == '/' || c == '\\');
        if d.is_empty() { "mods".to_string() } else { d.to_string() }
    }
}

/// Which included (not excluded) mods each project requires: key → names of mods that require it.
pub fn required_by(m: &Manifest, excluded: &HashSet<String>) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for x in &m.mods {
        if x.state() != ModState::Active || (x.is_client_only() && excluded.contains(&x.key())) {
            continue;
        }
        for d in x.dependencies.iter().filter(|d| d.kind == "required") {
            let src = if d.source.is_empty() { x.source.as_str() } else { d.source.as_str() };
            if let Some(k) = project_key(src, d.project_id.as_ref()) {
                out.entry(k).or_default().push(x.name.clone());
            }
        }
    }
    out
}

/// Removed mods that can actually be removed: only «client only» ones that no other included mod needs.
/// (Even if the settings file says otherwise, a required mod or a dependency is never removed.)
pub fn effective_excluded(m: &Manifest, requested: &HashSet<String>) -> HashSet<String> {
    let mut eff: HashSet<String> = m
        .mods
        .iter()
        .filter(|x| x.state() == ModState::Active && x.is_client_only())
        .map(|x| x.key())
        .filter(|k| requested.contains(k))
        .collect();
    loop {
        let needed = required_by(m, &eff);
        let before = eff.len();
        eff.retain(|k| !needed.contains_key(k));
        if eff.len() == before {
            break;
        }
    }
    eff
}

/// A loaded modpack: its manifest and, if it came from a ZIP, the ZIP bytes (local JARs, overrides).
#[derive(Debug)]
pub struct LoadedPack {
    pub manifest: Manifest,
    zip: Option<Vec<u8>>,
    root: String,
    pub origin: String,
    override_dirs: Vec<String>,
    /// Fingerprint (SHA-1) of the modpack file, used to detect changes between checks.
    pub fingerprint: String,
    /// `true` if the server could not be reached and the cached or embedded copy was used.
    pub offline: bool,
}

impl LoadedPack {
    pub fn from_bytes(bytes: Vec<u8>, origin: String) -> Result<Self> {
        let (manifest, root, is_zip) = if bytes.starts_with(b"PK") {
            let mut ar = ZipArchive::new(Cursor::new(&bytes[..])).context(t("pack.file_not_valid_zip"))?;
            let name = ar
                .file_names()
                .filter(|n| *n == "modpack.json" || n.ends_with("/modpack.json"))
                .min_by_key(|n| n.len())
                .map(|n| n.to_string())
                .ok_or_else(|| anyhow!(t("pack.zip_does_not_contain_modpack")))?;
            let mut text = String::new();
            ar.by_name(&name)?.read_to_string(&mut text)?;
            let manifest: Manifest = serde_json::from_str(text.trim_start_matches('\u{feff}'))
                .context(t("pack.modpack_json_invalid"))?;
            let root = name[..name.len() - "modpack.json".len()].to_string();
            (manifest, root, true)
        } else {
            let text = String::from_utf8(bytes.clone()).context(t("pack.modpack_neither_zip_nor_json"))?;
            let manifest: Manifest = serde_json::from_str(text.trim_start_matches('\u{feff}'))
                .context(t("pack.file_not_modpack_forge_json"))?;
            (manifest, String::new(), false)
        };
        if !manifest.schema.starts_with("modpack-forge/") {
            bail!(t("pack.file_not_modpack_forge_modpack"));
        }
        Ok(Self {
            manifest,
            zip: if is_zip { Some(bytes) } else { None },
            root,
            origin,
            override_dirs: vec!["overrides/".to_string()],
            fingerprint: String::new(),
            offline: false,
        })
    }

    pub(crate) fn from_parts(manifest: Manifest, zip: Vec<u8>, root: String, origin: String, override_dirs: Vec<String>) -> Self {
        Self { manifest, zip: Some(zip), root, origin, override_dirs, fingerprint: String::new(), offline: false }
    }

    /// Loads a modpack detecting its format (Modpack Forge, CurseForge or Modrinth) or the one set in launcher.toml.
    pub fn load_bytes(bytes: Vec<u8>, origin: String, rep: &Reporter) -> Result<Self> {
        let fp = fingerprint(&bytes);
        let mut p = Self::load_bytes_inner(bytes, origin, rep)?;
        p.fingerprint = fp;
        Ok(p)
    }

    fn load_bytes_inner(bytes: Vec<u8>, origin: String, rep: &Reporter) -> Result<Self> {
        let want = config::format();
        if !bytes.starts_with(b"PK") {
            if matches!(want, Format::CurseForge | Format::Modrinth) {
                bail!(tf!("pack.launcher_toml_sets_format_but", format = want.name()));
            }
            return Self::from_bytes(bytes, origin);
        }
        let fmt = if want == Format::Auto { detect(&bytes)? } else { want };
        match fmt {
            Format::CurseForge => {
                let e = external::curseforge(&bytes, rep)?;
                Ok(Self::from_parts(e.manifest, bytes, e.root, origin, e.override_dirs))
            }
            Format::Modrinth => {
                let e = external::modrinth(&bytes, rep)?;
                Ok(Self::from_parts(e.manifest, bytes, e.root, origin, e.override_dirs))
            }
            Format::Forge | Format::Auto => Self::from_bytes(bytes, origin),
        }
    }

    /// ZIP folders (relative to the root) whose contents are copied to the installation, in order.
    pub fn override_dirs(&self) -> &[String] {
        &self.override_dirs
    }

    /// Prefix (folder) inside the ZIP containing modpack.json; usually empty.
    pub fn root(&self) -> &str {
        &self.root
    }

    /// Opens the ZIP (if any). Each thread opens its own: it is cheap because it reads from memory.
    pub fn archive(&self) -> Result<Option<ZipArchive<Cursor<&[u8]>>>> {
        match &self.zip {
            Some(z) => Ok(Some(ZipArchive::new(Cursor::new(z.as_slice()))?)),
            None => Ok(None),
        }
    }

    pub fn id(&self) -> String {
        let p = &self.manifest.pack;
        if !p.id.is_empty() { p.id.clone() } else { util::slugify(&p.name) }
    }

    pub fn version(&self) -> &str {
        &self.manifest.pack.version
    }

    pub fn display_name(&self) -> &str {
        if self.manifest.pack.name.is_empty() { "Modpack" } else { &self.manifest.pack.name }
    }

    pub fn slug(&self) -> String {
        let p = &self.manifest.pack;
        if !p.slug.is_empty() { util::slugify(&p.slug) } else { util::slugify(&p.name) }
    }

    pub fn subtitle(&self) -> String {
        let m = &self.manifest;
        let loader = match m.loader.kind.to_lowercase().as_str() {
            "" | "vanilla" => "Vanilla".to_string(),
            "neoforge" => "NeoForge".to_string(),
            other => {
                let mut c = other.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
            }
        };
        let mut s = format!("Minecraft {} · {}", m.minecraft.version, loader);
        if !m.loader.version.is_empty() {
            s.push(' ');
            s.push_str(&m.loader.version);
        }
        if !m.pack.author.is_empty() {
            s.push_str(&format!(" · by {}", m.pack.author));
        }
        s
    }
}

fn fingerprint(bytes: &[u8]) -> String {
    Sha1::digest(bytes).iter().map(|b| format!("{:02x}", b)).collect()
}

/// Compares two versions of a modpack: (new, removed, updated mods).
pub fn diff_mods(old: &Manifest, new: &Manifest) -> (usize, usize, usize) {
    let sig = |m: &ModEntry| {
        let hash = ["sha512", "sha256", "sha1", "md5"]
            .iter()
            .find_map(|a| m.file.hashes.get(*a).and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        format!(
            "{}|{}|{}",
            m.version.number.clone().unwrap_or_default(),
            m.file.name.clone().unwrap_or_default(),
            hash
        )
    };
    let to_map = |x: &Manifest| -> HashMap<String, String> { x.mods.iter().map(|m| (m.key(), sig(m))).collect() };
    let (a, b) = (to_map(old), to_map(new));
    let added = b.keys().filter(|k| !a.contains_key(*k)).count();
    let removed = a.keys().filter(|k| !b.contains_key(*k)).count();
    let updated = b.iter().filter(|(k, v)| a.get(*k).map_or(false, |o| o != *v)).count();
    (added, removed, updated)
}

/// Recognises the format of a ZIP from the files it contains.
fn detect(bytes: &[u8]) -> Result<Format> {
    let ar = ZipArchive::new(Cursor::new(bytes)).context(t("pack.file_not_valid_zip"))?;
    let has = |f: &str| ar.file_names().any(|n| n == f || n.ends_with(&format!("/{f}")));
    if has("modpack.json") {
        Ok(Format::Forge)
    } else if has("modrinth.index.json") {
        Ok(Format::Modrinth)
    } else if has("manifest.json") {
        Ok(Format::CurseForge)
    } else {
        bail!(t("pack.unrecognised_zip_format_contains_neither"))
    }
}

/// Loads the modpack according to the build-time settings (launcher.toml):
///  - with `url`: downloads it (with caching); if that fails and an embedded ZIP exists, uses it;
///  - without `url`: uses the ZIP embedded in the executable.
pub fn load(rep: &Reporter) -> Result<LoadedPack> {
    let embedded = config::embedded_pack();

    if let Some(url) = config::modpack_url() {
        rep.busy(t("pack.checking_modpack"));
        let mut offline = false;
        let from_url = fetch_cached(url, rep).and_then(|(bytes, off)| {
            offline = off;
            LoadedPack::load_bytes(bytes, url.to_string(), rep)
        });
        match from_url {
            Ok(mut p) => {
                p.offline = offline;
                return Ok(p);
            }
            Err(e) => match embedded {
                Some(bytes) => {
                    rep.log(tf!("pack.could_not_get_modpack_from", error = format!("{:#}", e)));
                    let mut p = LoadedPack::load_bytes(bytes.to_vec(), t("pack.embedded_executable").to_string(), rep)?;
                    p.offline = true;
                    return Ok(p);
                }
                None => return Err(e),
            },
        }
    }

    match embedded {
        Some(bytes) => LoadedPack::load_bytes(bytes.to_vec(), t("pack.embedded_executable").to_string(), rep),
        None => bail!(t("pack.this_executable_was_built_without")),
    }
}

/// Downloads the URL to a local cache using ETag; uses the cached copy when offline.
/// Returns the bytes and `true` if the server could not be reached and the cached copy was used.
fn fetch_cached(url: &str, rep: &Reporter) -> Result<(Vec<u8>, bool)> {
    let cache_dir = util::app_data_dir().join("cache");
    fs::create_dir_all(&cache_dir)?;
    let key: String = Sha1::digest(url.as_bytes()).iter().take(8).map(|b| format!("{:02x}", b)).collect();
    let file = cache_dir.join(format!("{key}.pack"));
    let etag_file = cache_dir.join(format!("{key}.etag"));

    let agent = util::agent();
    let mut req = agent.get(url);
    if file.is_file() {
        if let Ok(etag) = fs::read_to_string(&etag_file) {
            if !etag.trim().is_empty() {
                req = req.set("If-None-Match", etag.trim());
            }
        }
    }

    let mut offline = false;
    match req.call() {
        // ureq only treats 4xx/5xx as errors: a 304 is handled here as a successful response.
        Ok(resp) if resp.status() == 304 && file.is_file() => rep.log(t("pack.modpack_up_date_local_cache")),
        Ok(resp) => {
            let etag = resp.header("ETag").map(|s| s.to_string());
            let total = resp.header("Content-Length").and_then(|v| v.parse::<u64>().ok());
            let tmp = cache_dir.join(format!("{key}.part"));
            let rep2 = rep.clone();
            util::copy_stream(resp.into_reader(), &tmp, total, move |done, total| {
                let text = match total {
                    Some(t) if t > 0 => tf!("pack.downloading_modpack_mb", done_mb = format!("{:.1}", util::mb(done as f64)), total_mb = format!("{:.1}", util::mb(t as f64))),
                    _ => tf!("pack.downloading_modpack_mb_2", done_mb = format!("{:.1}", util::mb(done as f64))),
                };
                match total {
                    Some(t) if t > 0 => rep2.progress(done as f32 / t as f32, text),
                    _ => rep2.busy(text),
                }
            })?;
            if file.exists() {
                let _ = fs::remove_file(&file);
            }
            fs::rename(&tmp, &file)?;
            match etag {
                Some(e) => fs::write(&etag_file, e)?,
                None => {
                    let _ = fs::remove_file(&etag_file);
                }
            }
        }
        Err(ureq::Error::Status(304, _)) => rep.log(t("pack.modpack_up_date_local_cache")),
        Err(e) => {
            if file.is_file() {
                rep.log(tf!("pack.offline_while_accessing_modpack_url", error = e));
                offline = true;
            } else {
                bail!(tf!("pack.could_not_download_modpack_from", url = url, error = e));
            }
        }
    }
    Ok((fs::read(&file)?, offline))
}
