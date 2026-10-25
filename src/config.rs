//! Configuration fixed AT BUILD TIME from `launcher.toml` (see build.rs).
//! The resulting executable does not read any configuration files: it is self-contained.

use crate::util;

// Generates: TITLE, MODPACK_URL, UPDATE_URL, LANGUAGE, MSA_APP_ID, MIN_RAM_MB, FORMAT, CF_API_KEY, CF_API_BASE,
// SERVER_ADDRESS, SERVER_AUTO_JOIN, AUTH_URL, AUTH_NAME, AUTH_ALLOW_MICROSOFT, AUTH_ALLOW_OFFLINE, AUTHLIB_INJECTOR,
// EMBEDDED_PACK and CLASSIC_ZLIB.
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

/// Modpack file format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// Detect from the ZIP contents.
    Auto,
    /// ZIP/JSON exported from Modpack Forge (modpack.json).
    Forge,
    /// ZIP exported from CurseForge (manifest.json).
    CurseForge,
    /// Modrinth .mrpack (modrinth.index.json).
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

/// CurseForge API key fixed at build time (it ends up inside the executable).
pub fn cf_api_key() -> Option<&'static str> {
    let k = CF_API_KEY.trim();
    if k.is_empty() { None } else { Some(k) }
}

/// CurseForge API base URL (the official one, or your proxy).
pub fn cf_api_base() -> &'static str {
    let b = CF_API_BASE.trim();
    if b.is_empty() { "https://api.curseforge.com" } else { b }
}

/// URL where the launcher .exe is published (self-update). `None` = disabled.
pub fn update_url() -> Option<&'static str> {
    let u = UPDATE_URL.trim();
    if u.is_empty() { None } else { Some(u) }
}

/// Default launcher language (`[launcher] language`); each player can change it in the launcher.
/// (build.rs checks that `lang/<code>.json` exists.)
pub fn default_language() -> &'static str {
    LANGUAGE
}

/// Modpack server for auto-join and the status box (empty = none). Only set in launcher.toml.
pub fn server_address() -> &'static str {
    SERVER_ADDRESS.trim()
}

/// Whether auto-join starts enabled (until the player changes it).
pub fn server_auto_join() -> bool {
    SERVER_AUTO_JOIN
}

/// Custom authentication server (Yggdrasil API root from `[auth] url`). `None` = disabled.
pub fn auth_url() -> Option<&'static str> {
    let u = AUTH_URL.trim();
    if u.is_empty() { None } else { Some(u) }
}

/// Name of the custom account type shown in the launcher (`[auth] name`).
pub fn auth_name() -> &'static str {
    AUTH_NAME
}

/// Whether Microsoft accounts are offered (`[auth] allow_microsoft`).
pub fn allow_microsoft() -> bool {
    AUTH_ALLOW_MICROSOFT
}

/// Whether offline accounts are offered (`[auth] allow_offline`).
pub fn allow_offline() -> bool {
    AUTH_ALLOW_OFFLINE
}

/// authlib-injector jar embedded at build time (only when `[auth] url` is set).
pub fn authlib_injector() -> Option<&'static [u8]> {
    AUTHLIB_INJECTOR
}

/// Classic zlib (`libz.so.1`) embedded in Linux builds; `compat` uses it to work around zlib-ng.
#[allow(dead_code)]
pub fn classic_zlib() -> Option<&'static [u8]> {
    CLASSIC_ZLIB
}
