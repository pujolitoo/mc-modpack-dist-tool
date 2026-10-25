//! Skin head of the selected account, shown in the account box.
//!
//! The profile's `textures` property (base64 JSON) gives the skin URL:
//!   - Microsoft accounts: `https://sessionserver.mojang.com/session/minecraft/profile/<uuid>`;
//!   - accounts of the custom server: `{api root}sessionserver/session/minecraft/profile/<uuid>` (Yggdrasil API).
//! From the skin (64×64, legacy 64×32 or HD multiples) the 8×8 face is cut out and the hat layer drawn over it.
//! The downloaded skin is cached per UUID, so the head is still shown when offline.

use crate::server::{base64_decode, decode_png, Icon};
use crate::{config, util, yggdrasil};
use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;

/// Whose head to show.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Who {
    /// Microsoft account (username, as saved by the launcher).
    Microsoft(String),
    /// Account of the custom authentication server (profile name).
    Custom(String),
}

impl Who {
    /// Key used by the UI to remember which head it already has.
    pub fn key(&self) -> String {
        match self {
            Who::Microsoft(n) => format!("ms:{n}"),
            Who::Custom(n) => format!("custom:{n}"),
        }
    }
}

const MAX_SKIN_BYTES: u64 = 2 * 1024 * 1024;

fn cache_file(uuid: &str) -> PathBuf {
    util::app_data_dir().join("skins").join(format!("{}.png", uuid.replace('-', "").to_lowercase()))
}

/// UUID (no dashes) and profile URL of the account. `None` if the account is not known.
fn profile_url(who: &Who) -> Option<(String, String)> {
    match who {
        Who::Microsoft(name) => {
            let uuid = crate::game::ms_account_uuid(&crate::game::accounts_db_path(), name)?;
            let url = format!("https://sessionserver.mojang.com/session/minecraft/profile/{uuid}");
            Some((uuid, url))
        }
        Who::Custom(name) => {
            let url = config::auth_url()?;
            let acc = yggdrasil::load_account(&yggdrasil::db_path(), &yggdrasil::normalize_root(url), name)?;
            let root = yggdrasil::metadata_or_cached(url).ok()?.0.root;
            let profile = format!("{root}sessionserver/session/minecraft/profile/{}?unsigned=true", acc.uuid);
            Some((acc.uuid, profile))
        }
    }
}

/// Skin URL from a profile reply (`properties[name=textures].value` → base64 JSON → `textures.SKIN.url`).
/// `None` if the player uses a default skin.
pub fn skin_url_from_profile(profile_json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(profile_json).ok()?;
    let value = v
        .get("properties")?
        .as_array()?
        .iter()
        .find(|p| p.get("name").and_then(|n| n.as_str()) == Some("textures"))?
        .get("value")?
        .as_str()?;
    let decoded = base64_decode(value)?;
    let textures: Value = serde_json::from_slice(&decoded).ok()?;
    textures.get("textures")?.get("SKIN")?.get("url")?.as_str().map(|s| s.to_string())
}

/// Downloads the skin of `who` (or uses the cached one) and returns its head. `None` = no skin / not available.
/// Blocking: call it from a worker thread.
pub fn head(who: &Who) -> Option<Icon> {
    let (uuid, profile) = profile_url(who)?;
    let cache = cache_file(&uuid);
    let downloaded = (|| {
        let agent = util::agent_quick();
        let text = agent.get(&profile).call().ok()?.into_string().ok()?;
        let url = skin_url_from_profile(&text)?;
        let mut bytes = Vec::new();
        agent.get(&url).call().ok()?.into_reader().take(MAX_SKIN_BYTES).read_to_end(&mut bytes).ok()?;
        let _ = std::fs::create_dir_all(cache.parent()?);
        let _ = std::fs::write(&cache, &bytes);
        Some(bytes)
    })();
    let bytes = downloaded.or_else(|| std::fs::read(&cache).ok())?;
    face_from_skin(&decode_png(&bytes)?)
}

/// Cuts the 8×8 face out of a skin and draws the hat layer over it (scaled for HD skins: 128×128 → 16×16…).
pub fn face_from_skin(skin: &Icon) -> Option<Icon> {
    let w = skin.width;
    // 64×64 (current), 64×32 (legacy) or an HD multiple of them.
    if w < 64 || w % 64 != 0 || (skin.height != w && skin.height * 2 != w) {
        return None;
    }
    let s = w / 64; // scale
    let size = 8 * s;
    let px = |x: usize, y: usize| -> [u8; 4] {
        let i = (y * w + x) * 4;
        [skin.rgba[i], skin.rgba[i + 1], skin.rgba[i + 2], skin.rgba[i + 3]]
    };
    // Old skins sometimes have a fully opaque (unused) hat layer: like the game, ignore it then.
    let hat_used = (0..size).any(|y| (0..size).any(|x| px(40 * s + x, 8 * s + y)[3] < 255));
    let mut out = Vec::with_capacity(size * size * 4);
    for y in 0..size {
        for x in 0..size {
            let mut c = px(8 * s + x, 8 * s + y);
            c[3] = 255; // the face layer is always opaque
            if hat_used {
                let h = px(40 * s + x, 8 * s + y);
                let a = h[3] as u32;
                for k in 0..3 {
                    c[k] = ((h[k] as u32 * a + c[k] as u32 * (255 - a)) / 255) as u8;
                }
            }
            out.extend_from_slice(&c);
        }
    }
    Some(Icon { width: size, height: size, rgba: out })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skin(w: usize, h: usize, f: impl Fn(usize, usize) -> [u8; 4]) -> Icon {
        let mut rgba = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&f(x, y));
            }
        }
        Icon { width: w, height: h, rgba }
    }

    #[test]
    fn face_and_hat() {
        // Face red; hat: transparent except one blue pixel at (41, 9) → face pixel (1, 1).
        let s = skin(64, 64, |x, y| {
            if (8..16).contains(&x) && (8..16).contains(&y) {
                [255, 0, 0, 255]
            } else if x == 41 && y == 9 {
                [0, 0, 255, 255]
            } else {
                [0, 0, 0, 0]
            }
        });
        let f = face_from_skin(&s).unwrap();
        assert_eq!((f.width, f.height), (8, 8));
        assert_eq!(&f.rgba[0..4], &[255, 0, 0, 255]);
        let i = (8 + 1) * 4;
        assert_eq!(&f.rgba[i..i + 4], &[0, 0, 255, 255]);
    }

    #[test]
    fn opaque_hat_is_ignored_and_formats() {
        let s = skin(64, 32, |x, y| if (40..48).contains(&x) && (8..16).contains(&y) { [0, 0, 0, 255] } else { [9, 9, 9, 255] });
        assert_eq!(&face_from_skin(&s).unwrap().rgba[0..4], &[9, 9, 9, 255]);
        assert_eq!(face_from_skin(&skin(128, 128, |_, _| [1, 2, 3, 255])).unwrap().width, 16);
        assert!(face_from_skin(&skin(32, 32, |_, _| [0; 4])).is_none());
    }

    #[test]
    fn skin_url() {
        // {"textures":{"SKIN":{"url":"http://textures.example/skin.png"}}}
        let b64 = "eyJ0ZXh0dXJlcyI6eyJTS0lOIjp7InVybCI6Imh0dHA6Ly90ZXh0dXJlcy5leGFtcGxlL3NraW4ucG5nIn19fQ==";
        let profile = format!(r#"{{"id":"x","name":"P","properties":[{{"name":"textures","value":"{b64}"}}]}}"#);
        assert_eq!(skin_url_from_profile(&profile).as_deref(), Some("http://textures.example/skin.png"));
        assert_eq!(skin_url_from_profile(r#"{"id":"x","name":"P","properties":[]}"#), None);
    }
}
