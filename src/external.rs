//! Importers for native modpacks: CurseForge (`manifest.json`) and Modrinth (`.mrpack`, `modrinth.index.json`).
//! They are converted into the same internal `Manifest` used by the rest of the launcher.
//!
//! - Modrinth: the ZIP already contains URLs and hashes. Names, versions, and dependencies are filled in (when
//!   network access is available) using the public Modrinth API; when offline, filenames are used.
//! - CurseForge: the ZIP only contains IDs (`projectID`/`fileID`). Resolving them into downloadable files requires
//!   the CurseForge API, which needs an API key (`[curseforge] api_key` in `launcher.toml`, set at compile time).

use crate::i18n::t;
use crate::config;
use crate::msg::Reporter;
use crate::pack::{Dep, InstallInfo, LoaderInfo, Manifest, Minecraft, ModEntry, ModFile, ModVersion, PackInfo, Runtime, Side};
use crate::util;
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::fs;
use std::io::{Cursor, Read};
use zip::ZipArchive;

pub struct External {
    pub manifest: Manifest,
    /// Folder inside the ZIP containing the index (usually empty).
    pub root: String,
    /// Override folders, in the order they are applied (later ones override earlier ones).
    pub override_dirs: Vec<String>,
}

fn yes() -> bool {
    true
}

/// Reads a file from the ZIP root (or its only containing folder). Returns (prefix, text).
fn read_entry(bytes: &[u8], file: &str) -> Result<(String, String)> {
    let mut ar = ZipArchive::new(Cursor::new(bytes)).context(t("external.file_not_valid_zip"))?;
    let name = ar
        .file_names()
        .filter(|n| *n == file || n.ends_with(&format!("/{file}")))
        .min_by_key(|n| n.len())
        .map(|n| n.to_string())
        .ok_or_else(|| anyhow!(tf!("external.zip_does_not_contain", file = file)))?;
    let mut text = String::new();
    ar.by_name(&name)?.read_to_string(&mut text)?;
    Ok((name[..name.len() - file.len()].to_string(), text))
}

/// Requests data from the API and uses the last cached response if it fails.
/// `required = true`: missing API and cache is an error; `false`: continue with whatever is available.
fn resolve_with_cache(
    bytes: &[u8],
    tag: &str,
    rep: &Reporter,
    mut manifest: Manifest,
    resolve: impl FnOnce(&mut Manifest) -> Result<()>,
    required: bool,
) -> Result<Manifest> {
    let key: String = Sha1::digest(bytes).iter().take(8).map(|b| format!("{:02x}", b)).collect();
    let dir = util::app_data_dir().join("cache");
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("resolved-{tag}-{key}.json"));

    rep.busy(t("external.fetching_modpack_data"));
    match resolve(&mut manifest) {
        Ok(()) => {
            if let Ok(json) = serde_json::to_vec(&manifest) {
                let _ = fs::write(&path, json);
            }
            Ok(manifest)
        }
        Err(e) => {
            let cached = fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Manifest>(&t).ok());
            if let Some(c) = cached {
                rep.log(tf!("external.using_data_saved_from_previous", error = format!("{:#}", e)));
                return Ok(c);
            }
            if required {
                Err(e)
            } else {
                rep.log(tf!("external.using_filenames", error = format!("{:#}", e)));
                Ok(manifest)
            }
        }
    }
}

// ════════════════════════════════ Modrinth (.mrpack) ════════════════════════════════

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct MrEnv {
    client: Option<String>,
    server: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MrFile {
    path: String,
    hashes: HashMap<String, String>,
    env: Option<MrEnv>,
    downloads: Vec<String>,
    #[serde(rename = "fileSize")]
    file_size: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MrIndex {
    name: String,
    #[serde(rename = "versionId")]
    version_id: String,
    files: Vec<MrFile>,
    dependencies: HashMap<String, String>,
}

/// The .mrpack specification requires rejecting downloads from outside these hosts.
fn allowed_host(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let host = rest.split('/').next().unwrap_or("").split(':').next().unwrap_or("").to_lowercase();
    ["cdn.modrinth.com", "github.com", "raw.githubusercontent.com", "gitlab.com"].contains(&host.as_str())
}

pub fn modrinth(bytes: &[u8], rep: &Reporter) -> Result<External> {
    let (root, text) = read_entry(bytes, "modrinth.index.json")?;
    let idx: MrIndex = serde_json::from_str(text.trim_start_matches('\u{feff}')).context(t("external.modrinth_index_json_invalid"))?;

    let mc = idx.dependencies.get("minecraft").cloned().unwrap_or_default();
    let (kind, lver) = [("fabric-loader", "fabric"), ("quilt-loader", "quilt"), ("neoforge", "neoforge"), ("forge", "forge")]
        .iter()
        .find_map(|(k, kind)| idx.dependencies.get(*k).map(|v| (kind.to_string(), v.clone())))
        .unwrap_or_else(|| ("vanilla".to_string(), String::new()));

    let mut mods = Vec::new();
    for f in &idx.files {
        let path = f.path.replace('\\', "/");
        let file_name = path.rsplit('/').next().unwrap_or("").to_string();
        if file_name.is_empty() {
            continue;
        }
        let hashes: HashMap<String, Value> =
            f.hashes.iter().map(|(k, v)| (k.to_lowercase(), Value::String(v.clone()))).collect();
        let stem = file_name.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or_else(|| file_name.clone());
        let env = f.env.clone().unwrap_or_default();
        mods.push(ModEntry {
            id: path.clone(),
            source: "modrinth".to_string(),
            name: stem,
            version: ModVersion::default(),
            side: Side { client: env.client, server: env.server },
            required: true,
            enabled: true,
            file: ModFile {
                name: Some(file_name),
                size: f.file_size,
                hashes,
                download_url: f.downloads.iter().find(|u| allowed_host(u)).cloned(),
                bundled_path: None,
                path: Some(path),
            },
            resolved: None,
            project_id: None,
            dependencies: Vec::new(),
        });
    }

    let name = if idx.name.trim().is_empty() { "Modpack".to_string() } else { idx.name.clone() };
    let slug = util::slugify(&name);
    let manifest = Manifest {
        schema: "external/modrinth".to_string(),
        pack: PackInfo { id: format!("modrinth:{slug}"), slug, name, version: idx.version_id.clone(), author: String::new() },
        minecraft: Minecraft { version: mc },
        loader: LoaderInfo { kind, version: lver },
        runtime: Runtime::default(),
        install: InstallInfo { mods_dir: "mods".to_string() },
        mods,
    };
    let manifest = resolve_with_cache(bytes, "mr", rep, manifest, enrich_modrinth, false)?;
    Ok(External { manifest, root, override_dirs: vec!["overrides/".to_string(), "client-overrides/".to_string()] })
}

/// Fills in name, version, project and dependencies by querying the Modrinth API with each file's SHA-1 hash.
fn enrich_modrinth(m: &mut Manifest) -> Result<()> {
    let sha1_of = |x: &ModEntry| x.file.hashes.get("sha1").and_then(|v| v.as_str()).map(|s| s.to_lowercase());
    let hashes: Vec<String> = m.mods.iter().filter_map(|x| sha1_of(x)).collect();
    if hashes.is_empty() {
        return Ok(());
    }
    let agent = util::agent();
    let body = json!({ "hashes": hashes, "algorithm": "sha1" }).to_string();
    let resp = agent
        .post("https://api.modrinth.com/v2/version_files")
        .set("Content-Type", "application/json")
        .send_string(&body)
        .map_err(|e| anyhow!(tf!("external.could_not_query_modrinth", error = e)))?;
    let versions: HashMap<String, Value> = serde_json::from_str(&resp.into_string()?)?;

    let mut ids: Vec<String> =
        versions.values().filter_map(|v| v.get("project_id").and_then(|p| p.as_str()).map(String::from)).collect();
    ids.sort();
    ids.dedup();
    let mut titles: HashMap<String, String> = HashMap::new();
    for chunk in ids.chunks(50) {
        let q = serde_json::to_string(chunk)?;
        let resp = agent
            .get("https://api.modrinth.com/v2/projects")
            .query("ids", &q)
            .call()
            .map_err(|e| anyhow!(tf!("external.could_not_query_modrinth", error = e)))?;
        let list: Vec<Value> = serde_json::from_str(&resp.into_string()?)?;
        for p in list {
            if let (Some(id), Some(t)) = (p.get("id").and_then(|v| v.as_str()), p.get("title").and_then(|v| v.as_str())) {
                titles.insert(id.to_string(), t.to_string());
            }
        }
    }

    for x in m.mods.iter_mut() {
        let Some(h) = sha1_of(x) else { continue };
        let Some(v) = versions.get(&h) else { continue };
        if let Some(pid) = v.get("project_id").and_then(|p| p.as_str()) {
            x.project_id = Some(Value::String(pid.to_string()));
            if let Some(t) = titles.get(pid) {
                x.name = t.clone();
            }
        }
        if let Some(n) = v.get("version_number").and_then(|n| n.as_str()) {
            x.version.number = Some(n.to_string());
        }
        x.dependencies = v
            .get("dependencies")
            .and_then(|d| d.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|d| {
                        let pid = d.get("project_id").and_then(|p| p.as_str())?;
                        Some(Dep {
                            source: "modrinth".to_string(),
                            project_id: Some(Value::String(pid.to_string())),
                            kind: d.get("dependency_type").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
    }
    Ok(())
}

// ════════════════════════════════ CurseForge (manifest.json) ════════════════════════════════

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CfLoader {
    id: String,
    primary: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CfMinecraft {
    version: String,
    #[serde(rename = "modLoaders")]
    mod_loaders: Vec<CfLoader>,
    #[serde(rename = "recommendedRam")]
    recommended_ram: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct CfFile {
    #[serde(rename = "projectID")]
    project_id: u64,
    #[serde(rename = "fileID")]
    file_id: u64,
    #[serde(default = "yes")]
    required: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CfManifest {
    #[serde(rename = "manifestType")]
    manifest_type: String,
    minecraft: CfMinecraft,
    name: String,
    version: String,
    author: String,
    files: Vec<CfFile>,
    overrides: String,
}

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{:02X}", b)
            }
        })
        .collect()
}

/// CurseForge CDN URL for mods whose API does not return `downloadUrl`.
fn edge_url(id: u64, name: &str) -> String {
    format!("https://edge.forgecdn.net/files/{}/{}/{}", id / 1000, id % 1000, pct(name))
}

pub fn curseforge(bytes: &[u8], rep: &Reporter) -> Result<External> {
    let (root, text) = read_entry(bytes, "manifest.json")?;
    let cm: CfManifest = serde_json::from_str(text.trim_start_matches('\u{feff}')).context(t("external.manifest_json_invalid"))?;
    if !cm.manifest_type.is_empty() && cm.manifest_type != "minecraftModpack" {
        bail!(tf!("external.unsupported_manifest_type", kind = cm.manifest_type));
    }

    // «forge-47.1.0» → (forge, 47.1.0)
    let primary = cm.minecraft.mod_loaders.iter().find(|l| l.primary).or_else(|| cm.minecraft.mod_loaders.first());
    let (kind, lver) = match primary {
        Some(l) => {
            let (k, v) = l.id.split_once('-').unwrap_or((l.id.as_str(), ""));
            (k.to_lowercase(), v.to_string())
        }
        None => ("vanilla".to_string(), String::new()),
    };

    let mut mods = Vec::new();
    let mut refs: Vec<(usize, u64, u64)> = Vec::new();
    for (i, f) in cm.files.iter().enumerate() {
        mods.push(ModEntry {
            id: format!("cf:{}:{}", f.project_id, f.file_id),
            source: "curseforge".to_string(),
            name: format!("CurseForge #{}", f.project_id),
            version: ModVersion::default(),
            side: Side::default(),
            required: f.required,
            enabled: true,
            file: ModFile::default(),
            resolved: Some(false),
            project_id: Some(json!(f.project_id)),
            dependencies: Vec::new(),
        });
        refs.push((i, f.project_id, f.file_id));
    }

    let name = if cm.name.trim().is_empty() { "Modpack".to_string() } else { cm.name.clone() };
    let slug = util::slugify(&name);
    let manifest = Manifest {
        schema: "external/curseforge".to_string(),
        pack: PackInfo { id: format!("curseforge:{slug}"), slug, name, version: cm.version.clone(), author: cm.author.clone() },
        minecraft: Minecraft { version: cm.minecraft.version.clone() },
        loader: LoaderInfo { kind, version: lver },
        runtime: Runtime { recommended_ram_mb: cm.minecraft.recommended_ram },
        install: InstallInfo { mods_dir: "mods".to_string() },
        mods,
    };
    let manifest = resolve_with_cache(bytes, "cf", rep, manifest, |m| resolve_curseforge(m, &refs), true)?;

    let dir = if cm.overrides.trim().is_empty() { "overrides" } else { cm.overrides.trim() };
    Ok(External { manifest, root, override_dirs: vec![format!("{}/", dir.trim_matches('/'))] })
}

/// Converts CurseForge IDs into filenames, URLs, sizes, hashes and dependencies.
fn resolve_curseforge(m: &mut Manifest, refs: &[(usize, u64, u64)]) -> Result<()> {
    let key = config::cf_api_key();
    let base = config::cf_api_base();
    if key.is_none() && base.trim_end_matches('/') == "https://api.curseforge.com" {
        bail!(
            "This is a CurseForge modpack and an API key is required. Fill in [curseforge] api_key in launcher.toml \
             (or api_base with your proxy) and build again"
        );
    }
    let agent = util::agent();
    let post = |path: &str, body: &Value| -> Result<Value> {
        let mut req = agent
            .post(&format!("{}{}", base.trim_end_matches('/'), path))
            .set("Content-Type", "application/json")
            .set("Accept", "application/json");
        if let Some(k) = key {
            req = req.set("x-api-key", k);
        }
        let resp = req.send_string(&body.to_string()).map_err(|e| anyhow!(tf!("external.could_not_query_curseforge", error = e)))?;
        Ok(serde_json::from_str(&resp.into_string()?)?)
    };

    let mut files: HashMap<u64, Value> = HashMap::new();
    for chunk in refs.chunks(100) {
        let ids: Vec<u64> = chunk.iter().map(|r| r.2).collect();
        let r = post("/v1/mods/files", &json!({ "fileIds": ids }))?;
        for f in r.get("data").and_then(|d| d.as_array()).into_iter().flatten() {
            if let Some(id) = f.get("id").and_then(|v| v.as_u64()) {
                files.insert(id, f.clone());
            }
        }
    }

    let mut names: HashMap<u64, String> = HashMap::new();
    let mut pids: Vec<u64> = refs.iter().map(|r| r.1).collect();
    pids.sort();
    pids.dedup();
    for chunk in pids.chunks(100) {
        let r = post("/v1/mods", &json!({ "modIds": chunk }))?;
        for p in r.get("data").and_then(|d| d.as_array()).into_iter().flatten() {
            if let (Some(id), Some(n)) = (p.get("id").and_then(|v| v.as_u64()), p.get("name").and_then(|v| v.as_str())) {
                names.insert(id, n.to_string());
            }
        }
    }

    for &(idx, pid, fid) in refs {
        let x = &mut m.mods[idx];
        if let Some(n) = names.get(&pid) {
            x.name = n.clone();
        }
        let Some(f) = files.get(&fid) else { continue }; // stays «unresolved»
        let fname = f.get("fileName").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if fname.is_empty() {
            continue;
        }
        let url = f
            .get("downloadUrl")
            .and_then(|v| v.as_str())
            .filter(|u| !u.is_empty())
            .map(String::from)
            .unwrap_or_else(|| edge_url(fid, &fname));
        let mut hashes: HashMap<String, Value> = HashMap::new();
        for h in f.get("hashes").and_then(|v| v.as_array()).into_iter().flatten() {
            let val = h.get("value").and_then(|v| v.as_str()).map(|s| s.to_lowercase());
            match (h.get("algo").and_then(|v| v.as_u64()), val) {
                (Some(1), Some(v)) => {
                    hashes.insert("sha1".to_string(), Value::String(v));
                }
                (Some(2), Some(v)) => {
                    hashes.insert("md5".to_string(), Value::String(v));
                }
                _ => {}
            }
        }
        x.file = ModFile {
            name: Some(fname),
            size: f.get("fileLength").and_then(|v| v.as_u64()),
            hashes,
            download_url: Some(url),
            bundled_path: None,
            path: None,
        };
        x.version.number = f.get("displayName").and_then(|v| v.as_str()).map(String::from);
        x.dependencies = f
            .get("dependencies")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|d| {
                        let mid = d.get("modId").and_then(|v| v.as_u64())?;
                        let kind = match d.get("relationType").and_then(|v| v.as_u64()) {
                            Some(3) => "required",
                            Some(2) => "optional",
                            Some(1) => "embedded",
                            Some(5) => "incompatible",
                            _ => "other",
                        };
                        Some(Dep { source: "curseforge".to_string(), project_id: Some(json!(mid)), kind: kind.to_string() })
                    })
                    .collect()
            })
            .unwrap_or_default();
        x.resolved = Some(true);
    }
    Ok(())
}
