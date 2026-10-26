//! Command line (no window):
//!
//!   modpack-launcher --sync-server <server folder> [--no-config]
//!
//! Keeps a dedicated server in sync with the modpack: it gets the modpack from the same place as the launcher
//! (launcher.toml) and, in the server folder, installs the mods that run on a server (not the client-only ones),
//! removes the ones it installed before that are no longer in the modpack and applies the configuration (overrides)
//! except the client-only files. Mods added to the server by hand are never touched.
//! Meant for the server start script, before starting Forge. Output also goes to `<server>/modpack-sync.log`.

use crate::i18n::{self, t};
use crate::{config, msg::Reporter, pack, sync};
use std::path::PathBuf;

/// Runs the command line if the arguments ask for it. `None` = no command: the launcher window opens.
pub fn run(args: &[String]) -> Option<i32> {
    let first = args.first()?.as_str();
    if !matches!(first, "--sync-server" | "--help" | "-h" | "/?") {
        return None;
    }
    attach_console();
    i18n::set(config::default_language());
    Some(match first {
        "--sync-server" => sync_server(&args[1..]),
        _ => {
            println!("{}", usage());
            0
        }
    })
}

fn usage() -> String {
    let exe = std::env::args()
        .next()
        .and_then(|p| PathBuf::from(p).file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "modpack-launcher".to_string());
    tf!("cli.usage", exe = exe)
}

fn sync_server(args: &[String]) -> i32 {
    let mut dir: Option<PathBuf> = None;
    let mut apply_config = true;
    for a in args {
        match a.as_str() {
            "--no-config" => apply_config = false,
            s if s.starts_with("--") || dir.is_some() => {
                eprintln!("{}\n\n{}", tf!("cli.unknown_option", option = s), usage());
                return 2;
            }
            s => dir = Some(PathBuf::from(s)),
        }
    }
    let Some(dir) = dir else {
        eprintln!("{}", usage());
        return 2;
    };
    let dir = if dir.is_absolute() { dir } else { std::env::current_dir().map(|c| c.join(&dir)).unwrap_or(dir) };
    if !dir.is_dir() {
        eprintln!("{}", tf!("cli.folder_missing", path = dir.display()));
        return 1;
    }

    let rep = Reporter::console(std::fs::File::create(dir.join("modpack-sync.log")).ok());
    rep.log(tf!("cli.syncing", path = dir.display()));
    if !["server.properties", "run.sh", "run.bat", "libraries"].iter().any(|f| dir.join(f).exists()) {
        rep.log(t("cli.not_a_server_warning"));
    }
    if !apply_config {
        rep.log(t("cli.config_skipped"));
    }
    let result = (|| -> anyhow::Result<usize> {
        let p = pack::load(&rep)?;
        if p.offline {
            rep.log(t("cli.using_cached_pack"));
        }
        rep.log(tf!("cli.modpack", name = p.display_name(), version = p.version()));
        sync::sync_server(&p, &dir, apply_config, &rep)
    })();
    match result {
        Ok(count) => {
            rep.log(tf!("cli.done", count = count));
            0
        }
        Err(e) => {
            rep.log(tf!("cli.failed", error = format!("{e:#}")));
            1
        }
    }
}

/// Windows: the launcher is a window application (no console of its own); write to the console it was started from.
#[cfg(windows)]
fn attach_console() {
    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    // SAFETY: plain Win32 call without pointers; failing (no parent console) is harmless.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
fn attach_console() {}
