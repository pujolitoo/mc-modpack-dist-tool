//! Utilities: safe paths, hashes and downloads.

use crate::i18n::t;
use crate::pack::ModFile;
use anyhow::{anyhow, bail, Result};
use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const APP_NAME: &str = "Modpack Forge Launcher";
const USER_AGENT: &str = concat!("ModpackForgeLauncher/", env!("CARGO_PKG_VERSION"));

pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Launcher data directory (accounts, modpack cache, settings, instances).
pub fn app_data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(exe_dir)
        .join("ModpackForgeLauncher")
}

pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars().flat_map(|c| c.to_lowercase()) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "modpack".to_string()
    } else {
        out
    }
}

/// Joins a relative path to `root`, rejecting `..`, absolute paths and drive letters (anti zip-slip).
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let rel = rel.replace('\\', "/");
    let mut out = root.to_path_buf();
    for comp in rel.split('/') {
        match comp {
            "" | "." => {}
            ".." => bail!(tf!("util.unsafe_path_modpack", path = rel)),
            c if c.contains(':') => bail!(tf!("util.unsafe_path_modpack", path = rel)),
            c => out.push(c),
        }
    }
    Ok(out)
}

/// Just the file name (no directories) of a name that comes from the modpack.
pub fn safe_file_name(name: &str) -> Result<String> {
    let n = name.rsplit(|c| c == '/' || c == '\\').next().unwrap_or("").trim();
    if n.is_empty() || n == "." || n == ".." || n.contains(':') {
        bail!(tf!("util.invalid_filename", name = name));
    }
    Ok(n.to_string())
}

pub fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .user_agent(USER_AGENT)
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .build()
}

/// Agent with short timeouts for quick checks (so offline startup is not delayed).
pub fn agent_quick() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .user_agent(USER_AGENT)
        .timeout_connect(Duration::from_secs(6))
        .timeout_read(Duration::from_secs(10))
        .build()
}

/// Copies a stream to disk reporting progress (bytes done, total if known).
pub fn copy_stream(
    mut reader: impl Read,
    dest: &Path,
    total: Option<u64>,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<()> {
    let mut out = File::create(dest)?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        done += n as u64;
        on_progress(done, total);
    }
    out.flush()?;
    Ok(())
}

/// Downloads with 3 attempts. Does not retry on 4xx errors (except 429).
pub fn download_to(agent: &ureq::Agent, url: &str, dest: &Path) -> Result<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!(tf!("util.unsupported_url", url = url));
    }
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..3u64 {
        match agent.get(url).call() {
            Ok(resp) => {
                let total = resp.header("Content-Length").and_then(|v| v.parse::<u64>().ok());
                match copy_stream(resp.into_reader(), dest, total, |_, _| {}) {
                    Ok(()) => return Ok(()),
                    Err(e) => last = Some(e),
                }
            }
            Err(ureq::Error::Status(code, _)) if (400..500).contains(&code) && code != 429 => {
                bail!(tf!("util.server_responded_with_http", code = code))
            }
            Err(e) => last = Some(anyhow!(e.to_string())),
        }
        std::thread::sleep(Duration::from_millis(800 * (attempt + 1)));
    }
    Err(last.unwrap_or_else(|| anyhow!(t("util.download_failed"))))
}

/// The strongest available hash: (algorithm, lowercase value).
fn strongest(hashes: &HashMap<String, serde_json::Value>) -> Option<(&'static str, String)> {
    for algo in ["sha512", "sha256", "sha1", "md5"] {
        if let Some(v) = hashes.get(algo).and_then(|v| v.as_str()) {
            if !v.trim().is_empty() {
                return Some((algo, v.trim().to_lowercase()));
            }
        }
    }
    None
}

pub fn hash_file(path: &Path, algo: &str) -> Result<String> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; 64 * 1024];
    macro_rules! run {
        ($hasher:expr) => {{
            let mut h = $hasher;
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
            }
            h.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>()
        }};
    }
    Ok(match algo {
        "sha512" => run!(Sha512::new()),
        "sha256" => run!(Sha256::new()),
        "sha1" => run!(Sha1::new()),
        "md5" => run!(Md5::new()),
        other => bail!(tf!("util.unsupported_hash_algorithm", algorithm = other)),
    })
}

/// Does the file exist and match the modpack's size and hash?
pub fn verify_file(path: &Path, file: &ModFile) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    if let Some(size) = file.size {
        if fs::metadata(path)?.len() != size {
            return Ok(false);
        }
    }
    if let Some((algo, expected)) = strongest(&file.hashes) {
        return Ok(hash_file(path, algo)? == expected);
    }
    Ok(true)
}

pub fn mb(bytes: f64) -> f64 {
    bytes / 1_048_576.0
}
