//! Changing the skin and the player name of an account.
//!
//! - Microsoft accounts: Mojang's services API, `POST https://api.minecraftservices.com/minecraft/profile/skins`
//!   (multipart: `variant` = classic|slim, `file` = PNG).
//! - Custom authentication server (authlib-injector):
//!   - skin: the texture upload of the Yggdrasil server specification, `PUT {root}api/user/profile/{uuid}/skin`
//!     (multipart: `model` = "" | slim, `file` = PNG);
//!   - name: the Mojang-compatible services API that authlib-injector maps `api.minecraftservices.com` to,
//!     `PUT {root}minecraftservices/minecraft/profile/name/{name}` (Drasl implements it; servers that do not are reported).
//!
//! Every request is authenticated with `Authorization: Bearer <access token>`.

use crate::i18n::t;
use crate::server::decode_png;
use crate::util;
use anyhow::{anyhow, Result};
use serde_json::Value;

/// Mojang's services API (Microsoft accounts).
pub const MOJANG_SERVICES: &str = "https://api.minecraftservices.com/";

/// Arm model of a skin.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Model {
    /// Classic arms (Steve).
    Classic,
    /// Slim arms (Alex).
    Slim,
}

// ───────── skin file ─────────

/// Checks that the file is a valid skin before uploading it: a PNG of 64×64 or 64×32 (or, if `allow_hd`, multiples).
pub fn check_skin(bytes: &[u8], allow_hd: bool) -> Result<()> {
    if bytes.len() > 1024 * 1024 {
        return Err(anyhow!(t("profile.skin_too_big")));
    }
    let icon = decode_png(bytes).ok_or_else(|| anyhow!(t("profile.skin_not_png")))?;
    let (w, h) = (icon.width, icon.height);
    let shape_ok = h == w || h * 2 == w;
    let size_ok = if allow_hd { w >= 64 && w % 64 == 0 } else { w == 64 };
    if shape_ok && size_ok {
        Ok(())
    } else {
        Err(anyhow!(tf!("profile.skin_bad_size", width = w, height = h)))
    }
}

// ───────── multipart/form-data ─────────

/// Builds a `multipart/form-data` body. Returns (Content-Type header, body).
fn multipart(text_fields: &[(&str, &str)], file_field: &str, file_name: &str, png: &[u8]) -> (String, Vec<u8>) {
    // A boundary that cannot appear in the PNG by accident is not required by the format as long as it is not
    // present in the data: derive it from the data length and a fixed prefix, and check.
    let mut n: u64 = 0x5f3759df ^ png.len() as u64;
    let boundary = loop {
        let b = format!("----ModpackLauncherBoundary{n:016x}");
        if !png.windows(b.len()).any(|w| w == b.as_bytes()) {
            break b;
        }
        n = n.wrapping_mul(6364136223846793005).wrapping_add(1);
    };
    let mut body = Vec::with_capacity(png.len() + 512);
    for (name, value) in text_fields {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
    }
    let safe_name: String = file_name.chars().filter(|c| *c != '"' && *c != '\r' && *c != '\n').collect();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{file_field}\"; filename=\"{safe_name}\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(png);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

// ───────── errors ─────────

/// Turns an HTTP error from the services / Yggdrasil API into a readable message.
fn api_error(e: ureq::Error, not_supported: &'static str) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, resp) => {
            let v: Value = resp.into_string().ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
            let message = ["errorMessage", "developerMessage", "error"]
                .iter()
                .find_map(|k| v.get(*k).and_then(|m| m.as_str()).filter(|m| !m.trim().is_empty()))
                .unwrap_or("")
                .to_string();
            let duplicate = v.get("details").and_then(|d| d.get("status")).and_then(|s| s.as_str()) == Some("DUPLICATE");
            match code {
                _ if duplicate => anyhow!(t("profile.name_taken")),
                401 => anyhow!(t("profile.session_expired")),
                404 | 405 | 501 => anyhow!(not_supported),
                429 => anyhow!(t("profile.too_many_requests")),
                _ if !message.is_empty() => anyhow!(tf!("profile.server_said", code = code, message = message)),
                _ => anyhow!(tf!("profile.http_error", code = code)),
            }
        }
        ureq::Error::Transport(e) => anyhow!(tf!("profile.could_not_contact", error = e)),
    }
}

// ───────── requests ─────────

/// Uploads a skin through Mojang's services API (or a compatible one at `base`).
pub fn services_upload_skin(base: &str, token: &str, png: &[u8], model: Model) -> Result<()> {
    let variant = if model == Model::Slim { "slim" } else { "classic" };
    let (content_type, body) = multipart(&[("variant", variant)], "file", "skin.png", png);
    util::agent_quick()
        .post(&format!("{base}minecraft/profile/skins"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", &content_type)
        .send_bytes(&body)
        .map_err(|e| api_error(e, t("profile.skin_not_supported")))?;
    Ok(())
}

/// Uploads a skin with the texture upload of the Yggdrasil server specification (authlib-injector servers).
pub fn yggdrasil_upload_skin(root: &str, uuid: &str, token: &str, png: &[u8], model: Model) -> Result<()> {
    let m = if model == Model::Slim { "slim" } else { "" };
    let (content_type, body) = multipart(&[("model", m)], "file", "skin.png", png);
    util::agent_quick()
        .put(&format!("{root}api/user/profile/{uuid}/skin"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", &content_type)
        .send_bytes(&body)
        .map_err(|e| api_error(e, t("profile.skin_not_supported")))?;
    Ok(())
}

/// Changes the player name through a Mojang-compatible services API (`{root}minecraftservices/` for
/// authlib-injector servers). Returns the name the server reports afterwards.
pub fn services_change_name(services_base: &str, token: &str, new_name: &str) -> Result<String> {
    let resp = util::agent_quick()
        .put(&format!("{services_base}minecraft/profile/name/{new_name}"))
        .set("Authorization", &format!("Bearer {token}"))
        .send_bytes(&[])
        .map_err(|e| api_error(e, t("profile.rename_not_supported")))?;
    let v: Value = resp.into_string().ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
    Ok(v.get("name").and_then(|n| n.as_str()).unwrap_or(new_name).to_string())
}

/// Player names allowed by Minecraft: 3–16 characters, letters, digits and underscores.
pub fn valid_player_name(name: &str) -> bool {
    (3..=16).contains(&name.chars().count()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipart_body() {
        let (ct, body) = multipart(&[("variant", "slim")], "file", "a\"b.png", b"\x89PNGDATA");
        let boundary = ct.strip_prefix("multipart/form-data; boundary=").unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"variant\"\r\n\r\nslim\r\n")));
        assert!(text.contains("name=\"file\"; filename=\"ab.png\"\r\nContent-Type: image/png\r\n\r\n"));
        assert!(text.ends_with(&format!("\r\n--{boundary}--\r\n")));
    }

    #[test]
    fn names() {
        assert!(valid_player_name("Pere_09"));
        assert!(!valid_player_name("ab"));
        assert!(!valid_player_name("with space"));
        assert!(!valid_player_name("seventeen_chars_x"));
    }
}
