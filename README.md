# Modpack Forge Launcher (Rust)

One-click launcher for modpacks exported from Modpack Forge.
Uses the `portablemc` library (Minecraft + Fabric/Quilt/Forge/NeoForge installation, with progress reporting)
and `egui/eframe` for the user interface.

## 1. Configure (BEFORE building)
Edit `launcher.toml`. All settings are embedded into the executable at build time:
  - `[modpack] path`  → ZIP file EMBEDDED in the .exe (path relative to this folder).
  - `[modpack] url`   → or a download URL (with caching and ETag-based updates).
  - `[modpack] format` → `auto` (default), `forge`, `curseforge` or `modrinth`: accepts a Modpack Forge ZIP,
    a ZIP exported from CurseForge, or a Modrinth `.mrpack`.
  - `[curseforge] api_key` → only required for CurseForge ZIPs (resolves IDs to downloads).
  - `[requirements] min_ram_mb` → minimum system RAM; 0 = the amount recommended by the modpack.
  - `[launcher] language` → default language of the launcher texts: a file of `lang/` without `.json` (`en`, `es`...).
  - `[microsoft] app_id` → your own Azure app ID (optional).
  - `[server] address` / `auto_join` → the modpack server (fixed in the executable) and whether
    "join automatically on PLAY" starts enabled (players can tick/untick it).

## 2. Build
    cargo build --release        # → target/release/modpack-launcher(.exe)

Linux: you need the usual windowing libraries (X11/Wayland and, for the folder picker,
xdg-desktop-portal or GTK3).

## 3. Distribute
Only the executable is needed. No ZIP or configuration files need to be placed alongside it.
If you change `launcher.toml`, build again.

## Modpack updates
With `[modpack] url`, the **Check for updates** button queries the server and, if a new version is available, loads it
and displays a summary (new, removed and updated mods). Changes are applied when **PLAY** is clicked.
With `[modpack] path`, the modpack is embedded in the .exe: distribute a new launcher to update it.
Increment the pack version when publishing changes: overrides (configs) are only reapplied when the version changes.

## Server status and auto-join
With `[server] address` set, the **Server** box (top right) shows the server icon (its `server-icon.png`), whether the server is online, how many players are connected (hover to see their
names, when the server shares them), latency, version and MOTD; it refreshes every 30 seconds. With
**Join automatically when clicking PLAY** enabled, PLAY starts Minecraft and joins that server directly (Quick Play on
1.20+, `--server/--port` on older versions). Addresses without a port use 25565 or the domain's SRV record.

## Languages
All launcher texts live in `lang/<code>.json` (`lang/en.json`, `lang/es.json`...), embedded in the executable at build
time. `[launcher] language` sets the default; each player can switch it with the **Language** selector under the title,
and the choice is remembered.

**Adding a language:** copy `lang/en.json` to e.g. `lang/fr.json`, set `"language_name": "Français"`, translate the
texts and rebuild. It appears in the selector automatically. Keep the `{placeholders}` exactly as they are (you can
move them inside the sentence). Untranslated texts fall back to English.

The build checks the texts against the code: a key used in the code that is missing from `en.json`, or a `{placeholder}`
the code does not fill, stops the build with the file and line; problems in translations are shown as warnings.
In the code, texts are used as `t("main.play")` or `tf!("main.signed_as", name = name)` (see `src/i18n.rs`).

## Launcher auto-update
With `[launcher] update_url` (a Dropbox link to the .exe), the launcher checks on startup whether the
published file has been replaced; if so, it downloads it, replaces itself and restarts. Upload the new version
using the SAME filename (Dropbox's **Replace** option) so the shared link does not change. Dropbox may limit
traffic from a single public link if you have many players.

## Linux: Forge/NeoForge on systems with zlib-ng
Distributions that replace zlib with zlib-ng (Fedora, openSUSE Tumbleweed, CachyOS...) make the Forge/NeoForge installer
fail with "installer processor output corrupted": zlib-ng compresses differently, so the checksums of the files the
installer generates do not match. Linux builds embed a classic `libz.so.1` (copied by CI into `assets/linux/`); when the
launcher detects zlib-ng it restarts itself once with that library preloaded (`LD_PRELOAD`). Nothing to do on your side.
`MODPACK_LAUNCHER_ZLIB=force` always does it, `MODPACK_LAUNCHER_ZLIB=off` never does. See `THIRD_PARTY_NOTICES.md`.
