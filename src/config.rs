//! Configuration fijada AT BUILD TIME a partir de `launcher.toml` (ver build.rs).
//! The resulting executable does not read any configuration files: it is self-contained.

use crate::util;

// Genera: TITLE, MODPACK_URL, UPDATE_URL, MSA_APP_ID, MIN_RAM_MB, FORMAT, CF_API_KEY, CF_API_BASE, EMBEDDED_PACK y CLASSIC_ZLIB.
include!(concat!(env!("OUT_DIR"), "/build_config.rs"));

/// Azure application ID for the PortableMC project (the same one used by its CLI).
const DEFAULT_MSA_APP_ID: &str = "708e91b5-99f8-4a1d-80ec-e746cbb24771";

pub fn title() -> String {
    if TITLE.trim().is_empty() {
        util::APP_NAME.to_string()
    } else {
        TITLE.to_string()
    }
}

pub fn app_id() -> String {
    if MSA_APP_ID.trim().is_empty() {
        DEFAULT_MSA_APP_ID.to_string()
    } else {
        MSA_APP_ID.to_string()
    }
}

pub fn modpack_url() -> Option<&'static str> {
    let u = MODPACK_URL.trim();
    if u.is_empty() { None } else { Some(u) }
}

/// Modpack ZIP (or JSON) embedded in the executable, if built with `path`.
pub fn embedded_pack() -> Option<&'static [u8]> {
    EMBEDDED_PACK
}

/// Manually configured minimum system RAM (0 in launcher.toml = use the pack's recommendation).
pub fn min_ram_override_mb() -> Option<u64> {
    if MIN_RAM_MB > 0 { Some(MIN_RAM_MB) } else { None }
}

/// Formato del archivo del modpack.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// Detectar por el contenido del ZIP.
    Auto,
    /// ZIP/JSON exportado desde Modpack Forge (modpack.json).
    Forge,
    /// ZIP exportado desde CurseForge (manifest.json).
    CurseForge,
    /// .mrpack de Modrinth (modrinth.index.json).
    Modrinth,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Auto => "auto",
            Format::Forge => "forge",
            Format::CurseForge => "curseforge",
            Format::Modrinth => "modrinth",
        }
    }
}

pub fn format() -> Format {
    match FORMAT.trim().to_lowercase().as_str() {
        "forge" => Format::Forge,
        "curseforge" => Format::CurseForge,
        "modrinth" => Format::Modrinth,
        _ => Format::Auto,
    }
}

/// API key de CurseForge fijada al compilar (queda dentro del ejecutable).
pub fn cf_api_key() -> Option<&'static str> {
    let k = CF_API_KEY.trim();
    if k.is_empty() { None } else { Some(k) }
}

/// URL base de la API de CurseForge (la oficial, o tu proxy).
pub fn cf_api_base() -> &'static str {
    let b = CF_API_BASE.trim();
    if b.is_empty() { "https://api.curseforge.com" } else { b }
}

/// URL where the launcher .exe is published (self-update). `None` = disabled.
pub fn update_url() -> Option<&'static str> {
    let u = UPDATE_URL.trim();
    if u.is_empty() { None } else { Some(u) }
}

/// Classic zlib (`libz.so.1`) embedded in Linux builds; `compat` uses it to work around zlib-ng.
#[allow(dead_code)]
pub fn classic_zlib() -> Option<&'static [u8]> {
    CLASSIC_ZLIB
}
