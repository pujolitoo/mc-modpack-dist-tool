//! Custom authentication server: client for the Yggdrasil API (the one authlib-injector uses), as implemented by
//! Drasl, Blessing Skin (Yggdrasil plugin) and similar servers.
//!
//! - Sign-in: `POST {root}authserver/authenticate` with the player's username/email and password. The password is
//!   never stored: only the access token, the client token and the profile (UUID and name).
//! - Before playing: `validate` the token and, if it expired, `refresh` it.
//! - The game is started with authlib-injector pointing to `{root}` (see game.rs), and the server metadata
//!   (`GET {root}`) is passed to it pre-fetched so the game does not have to download it again.

use crate::i18n::t;
use crate::util;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Header with which a server points to its real API root (authlib-injector's «API Location Indication»).
const ALI_HEADER: &str = "X-Authlib-Injector-API-Location";

/// A signed-in account of the custom server.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Account {
    /// Server it belongs to: the `[auth] url` of launcher.toml, normalized (accounts of another server are ignored).
    pub server: String,
    /// What the player typed to sign in (username or email), to show it and to sign in again.
    pub login: String,
    /// Minecraft profile: UUID without dashes and name.
    pub uuid: String,
    pub name: String,
    pub access_token: String,
    pub client_token: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Database {
    accounts: Vec<Account>,
}

// ───────── API root and metadata ─────────

/// Normalizes an API root: no surrounding spaces, always ending in `/`.
pub fn normalize_root(url: &str) -> String {
    let u = url.trim();
    if u.ends_with('/') { u.to_string() } else { format!("{u}/") }
}

/// Resolves the value of the ALI header (absolute URL, absolute path or relative path) against `base`.
fn resolve_ali(base: &str, location: &str) -> String {
    let loc = location.trim();
    if loc.starts_with("https://") || loc.starts_with("http://") {
        return normalize_root(loc);
    }
    // scheme://host[:port]
    let origin_end = base.find("://").map(|i| i + 3).and_then(|s| base[s..].find('/').map(|p| s + p)).unwrap_or(base.len());
    if loc.starts_with('/') {
        normalize_root(&format!("{}{}", &base[..origin_end], loc))
    } else {
        let dir_end = base.rfind('/').map(|i| i + 1).unwrap_or(base.len());
        normalize_root(&format!("{}{}", &base[..dir_end.max(origin_end)], loc))
    }
}

/// Server metadata (`GET {root}`): the real API root (after following the ALI header) and the raw JSON text.
pub struct Metadata {
    pub root: String,
    pub json: String,
}

impl Metadata {
    /// Server name from the metadata (`meta.serverName`), if it has one.
    pub fn server_name(&self) -> Option<String> {
        let v: Value = serde_json::from_str(&self.json).ok()?;
        v.get("meta")?.get("serverName")?.as_str().map(|s| s.to_string())
    }
}

fn cache_path() -> PathBuf {
    util::app_data_dir().join("yggdrasil_metadata.json")
}

/// Downloads the metadata, following the ALI header once. It is cached so the game can start with the last known
/// copy if the server cannot be reached right now (single-player still works; multiplayer will need the server).
pub fn fetch_metadata(url: &str) -> Result<Metadata> {
    let mut root = normalize_root(url);
    let mut followed = false;
    loop {
        let resp = util::agent_quick()
            .get(&root)
            .call()
            .map_err(|e| anyhow!(tf!("yggdrasil.could_not_contact", error = short_error(e))))?;
        if let (false, Some(loc)) = (followed, resp.header(ALI_HEADER)) {
            let target = resolve_ali(&root, loc);
            if target != root {
                root = target;
                followed = true;
                continue;
            }
        }
        let json = resp.into_string().context(t("yggdrasil.invalid_reply"))?;
        let v: Value = serde_json::from_str(&json).map_err(|_| anyhow!(t("yggdrasil.not_a_yggdrasil_server")))?;
        if v.get("meta").is_none() && v.get("signaturePublickey").is_none() {
            bail!(t("yggdrasil.not_a_yggdrasil_server"));
        }
        let _ = std::fs::create_dir_all(util::app_data_dir());
        let cache = json!({ "url": normalize_root(url), "root": root, "metadata": json });
        let _ = std::fs::write(cache_path(), cache.to_string());
        return Ok(Metadata { root, json });
    }
}

/// Like `fetch_metadata`, but falls back to the cached copy (of this same server) when it cannot be reached.
pub fn metadata_or_cached(url: &str) -> Result<(Metadata, bool)> {
    match fetch_metadata(url) {
        Ok(m) => Ok((m, false)),
        Err(e) => {
            let cached = std::fs::read_to_string(cache_path())
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .filter(|v| v.get("url").and_then(|u| u.as_str()) == Some(normalize_root(url).as_str()))
                .and_then(|v| Some((v.get("root")?.as_str()?.to_string(), v.get("metadata")?.as_str()?.to_string())));
            match cached {
                Some((root, json)) => Ok((Metadata { root, json }, true)),
                None => Err(e),
            }
        }
    }
}

// ───────── requests ─────────

/// Readable text of a transport error (without ureq's long prefixes).
fn short_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("HTTP {code}"),
        ureq::Error::Transport(t) => t.to_string(),
    }
}

/// POSTs JSON to `{root}{path}`. `Ok(None)` for 204 No Content.
fn post(root: &str, path: &str, body: &Value) -> Result<Option<Value>, ApiError> {
    let resp = util::agent_quick()
        .post(&format!("{root}{path}"))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string());
    match resp {
        Ok(r) if r.status() == 204 => Ok(None),
        Ok(r) => {
            let text = r.into_string().map_err(|e| ApiError::Network(e.to_string()))?;
            if text.trim().is_empty() {
                return Ok(None);
            }
            serde_json::from_str(&text).map(Some).map_err(|_| ApiError::Network(t("yggdrasil.invalid_reply").to_string()))
        }
        Err(ureq::Error::Status(code, r)) => {
            let v: Value = r.into_string().ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
            let kind = v.get("error").and_then(|e| e.as_str()).unwrap_or("").to_string();
            let message = v.get("errorMessage").and_then(|e| e.as_str()).unwrap_or("").to_string();
            Err(ApiError::Server { code, kind, message })
        }
        Err(e) => Err(ApiError::Network(short_error(e))),
    }
}

/// Error from the authentication server.
#[derive(Debug)]
enum ApiError {
    /// The server answered with an error (`error` / `errorMessage` of the Yggdrasil API).
    Server { code: u16, kind: String, message: String },
    /// It could not be reached or its reply is not valid.
    Network(String),
}

impl ApiError {
    /// `forbidden`: text for a 401/403 (ForbiddenOperationException: wrong credentials, token no longer valid…).
    fn into_anyhow(self, forbidden: &'static str) -> anyhow::Error {
        match self {
            ApiError::Server { code: 401 | 403, message, .. } if message.is_empty() => anyhow!(forbidden),
            ApiError::Server { code: 401 | 403, message, .. } => {
                anyhow!(tf!("yggdrasil.forbidden_with_reason", text = forbidden, reason = message))
            }
            ApiError::Server { code, kind, message } => {
                let reason = if message.is_empty() { kind } else { message };
                anyhow!(tf!("yggdrasil.server_error", code = code, reason = reason))
            }
            ApiError::Network(e) => anyhow!(tf!("yggdrasil.could_not_contact", error = e)),
        }
    }
}

/// Random client identifier (32 hex characters). `RandomState` is seeded randomly by the standard library.
fn new_client_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let part = |salt: u64| {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(salt);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        h.finish()
    };
    format!("{:016x}{:016x}", part(1), part(2))
}

/// Profile `{id, name}` of a reply.
fn profile_of(v: &Value) -> Option<(String, String)> {
    Some((v.get("id")?.as_str()?.replace('-', ""), v.get("name")?.as_str()?.to_string()))
}

/// Signs in with username/email and password against the API `root`. Returns the account (not saved yet); its
/// `server` is set to `server` (the configured URL that identifies the saved accounts).
pub fn authenticate(root: &str, server: &str, login: &str, password: &str) -> Result<Account> {
    let client_token = new_client_token();
    let body = json!({
        "agent": { "name": "Minecraft", "version": 1 },
        "username": login,
        "password": password,
        "clientToken": client_token,
        "requestUser": true,
    });
    let v = post(root, "authserver/authenticate", &body)
        .map_err(|e| e.into_anyhow(t("yggdrasil.invalid_credentials")))?
        .ok_or_else(|| anyhow!(t("yggdrasil.invalid_reply")))?;
    let mut token = v.get("accessToken").and_then(|x| x.as_str()).ok_or_else(|| anyhow!(t("yggdrasil.invalid_reply")))?.to_string();
    let client_token = v.get("clientToken").and_then(|x| x.as_str()).unwrap_or(&client_token).to_string();

    let profile = match v.get("selectedProfile").and_then(profile_of) {
        Some(p) => p,
        None => {
            // No profile selected: take the first one and bind it to the token (the API does it with refresh).
            let first = v
                .get("availableProfiles")
                .and_then(|a| a.as_array())
                .and_then(|a| a.first())
                .and_then(profile_of)
                .ok_or_else(|| anyhow!(t("yggdrasil.no_profile")))?;
            let body = json!({
                "accessToken": token,
                "clientToken": client_token,
                "selectedProfile": { "id": first.0, "name": first.1 },
            });
            let r = post(root, "authserver/refresh", &body)
                .map_err(|e| e.into_anyhow(t("yggdrasil.session_expired")))?
                .ok_or_else(|| anyhow!(t("yggdrasil.invalid_reply")))?;
            if let Some(new) = r.get("accessToken").and_then(|x| x.as_str()) {
                token = new.to_string();
            }
            r.get("selectedProfile").and_then(profile_of).unwrap_or(first)
        }
    };
    Ok(Account {
        server: server.to_string(),
        login: login.trim().to_string(),
        uuid: profile.0,
        name: profile.1,
        access_token: token,
        client_token,
    })
}

/// Result of checking a saved session before playing.
pub enum Session {
    /// Valid (possibly refreshed: save it again).
    Valid(Account),
    /// The server could not be reached: the saved token is used as is (single-player works; multiplayer will not
    /// until the server is back).
    Unverified(String),
}

/// Makes sure the token is still valid against the API `root`, refreshing it if needed.
pub fn ensure_valid(root: &str, acc: &Account) -> Result<Session> {
    let body = json!({ "accessToken": acc.access_token, "clientToken": acc.client_token });
    match post(root, "authserver/validate", &body) {
        Ok(_) => return Ok(Session::Valid(acc.clone())),
        Err(ApiError::Server { .. }) => {} // not valid any more: refresh
        Err(ApiError::Network(e)) => return Ok(Session::Unverified(e)),
    }
    refresh(root, acc).map(Session::Valid)
}

/// Gets a new access token (and the current profile name, e.g. after renaming it).
pub fn refresh(root: &str, acc: &Account) -> Result<Account> {
    let body = json!({ "accessToken": acc.access_token, "clientToken": acc.client_token, "requestUser": true });
    let v = post(root, "authserver/refresh", &body)
        .map_err(|e| e.into_anyhow(t("yggdrasil.session_expired")))?
        .ok_or_else(|| anyhow!(t("yggdrasil.invalid_reply")))?;
    let mut out = acc.clone();
    out.access_token = v.get("accessToken").and_then(|x| x.as_str()).ok_or_else(|| anyhow!(t("yggdrasil.invalid_reply")))?.to_string();
    if let Some((uuid, name)) = v.get("selectedProfile").and_then(profile_of) {
        out.uuid = uuid;
        out.name = name;
    }
    Ok(out)
}

/// Signs out on the server (best effort: the account is forgotten locally anyway).
pub fn invalidate(root: &str, acc: &Account) {
    let body = json!({ "accessToken": acc.access_token, "clientToken": acc.client_token });
    let _ = post(root, "authserver/invalidate", &body);
}

// ───────── saved accounts ─────────

pub fn db_path() -> PathBuf {
    util::app_data_dir().join("yggdrasil_accounts.json")
}

fn load_db(path: &Path) -> Database {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_db(path: &Path, db: &Database) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.part");
    std::fs::write(&tmp, serde_json::to_string_pretty(db)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Saved accounts of `server` (profile names).
pub fn list_accounts(path: &Path, server: &str) -> Vec<String> {
    load_db(path).accounts.into_iter().filter(|a| a.server == server).map(|a| a.name).collect()
}

pub fn load_account(path: &Path, server: &str, name: &str) -> Option<Account> {
    load_db(path).accounts.into_iter().find(|a| a.server == server && a.name == name)
}

/// Saves (or replaces) an account.
pub fn store_account(path: &Path, acc: &Account) -> Result<()> {
    let mut db = load_db(path);
    db.accounts.retain(|a| !(a.server == acc.server && (a.name == acc.name || a.uuid == acc.uuid)));
    db.accounts.push(acc.clone());
    save_db(path, &db).map_err(|e| anyhow!(tf!("yggdrasil.could_not_save_account", error = e)))
}

pub fn remove_account(path: &Path, server: &str, name: &str) -> Result<()> {
    let mut db = load_db(path);
    db.accounts.retain(|a| !(a.server == server && a.name == name));
    save_db(path, &db).map_err(|e| anyhow!(tf!("yggdrasil.could_not_save_account", error = e)))
}

// ───────── launch arguments ─────────

/// Standard base64 (for `-Dauthlibinjector.yggdrasil.prefetched`).
pub fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Puts the real access token in the game arguments (portablemc fills them as an offline session).
/// Modern versions use `--accessToken` / `--userType`; very old ones use `--session token:<token>:<uuid>`.
/// The user type is `msa`, like HMCL does for authlib-injector accounts: current Minecraft only knows `msa` and
/// `legacy` (the old `mojang` type is gone and makes the game treat the session as legacy, so the player's own
/// profile/skin is not loaded).
pub fn patch_game_args(args: &mut [String], acc: &Account) {
    let mut i = 0;
    while i + 1 < args.len() {
        match args[i].as_str() {
            "--accessToken" => args[i + 1] = acc.access_token.clone(),
            "--userType" => args[i + 1] = "msa".to_string(),
            "--session" => args[i + 1] = format!("token:{}:{}", acc.access_token, acc.uuid),
            "--uuid" => args[i + 1] = acc.uuid.clone(),
            "--username" => args[i + 1] = acc.name.clone(),
            _ => {
                i += 1;
                continue;
            }
        }
        i += 2;
    }
}

/// Parses a UUID without dashes (or with them) into the form portablemc expects.
pub fn parse_uuid(s: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(s).map_err(|_| anyhow!(tf!("yggdrasil.invalid_uuid", uuid = s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_and_ali() {
        assert_eq!(normalize_root(" https://a.com/api/yggdrasil "), "https://a.com/api/yggdrasil/");
        assert_eq!(resolve_ali("https://a.com/", "/api/yggdrasil"), "https://a.com/api/yggdrasil/");
        assert_eq!(resolve_ali("https://a.com/x/", "https://b.com/y"), "https://b.com/y/");
        assert_eq!(resolve_ali("https://a.com/x/", "api/"), "https://a.com/x/api/");
        assert_eq!(resolve_ali("https://a.com:8443/", "/ygg/"), "https://a.com:8443/ygg/");
    }

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("{\"meta\":{}}".as_bytes()), "eyJtZXRhIjp7fX0=");
    }

    #[test]
    fn game_args() {
        let acc = Account {
            server: "s/".into(),
            login: "pere".into(),
            uuid: "0123456789abcdef0123456789abcdef".into(),
            name: "Pere".into(),
            access_token: "TOKEN".into(),
            client_token: "C".into(),
        };
        let mut a: Vec<String> =
            ["--username", "x", "--uuid", "y", "--accessToken", "", "--userType", "", "--version", "1.20.1"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        patch_game_args(&mut a, &acc);
        assert_eq!(a, ["--username", "Pere", "--uuid", "0123456789abcdef0123456789abcdef", "--accessToken", "TOKEN", "--userType", "msa", "--version", "1.20.1"]);
        let mut legacy: Vec<String> = ["Pere", "--session", ""].iter().map(|s| s.to_string()).collect();
        patch_game_args(&mut legacy, &acc);
        assert_eq!(legacy[2], "token:TOKEN:0123456789abcdef0123456789abcdef");
    }
}
