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
  - `[auth]` → your own authentication server (see below) and which account types are offered.
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

## Dedicated server: syncing its mods

The same executable keeps a dedicated server in sync with the modpack, without opening a window:

```
modpack-launcher --sync-server <server folder> [--no-config]
```

It gets the modpack from the same place as the launcher (`launcher.toml`) and, in the server folder:

- installs the mods that run on a server (and the server-only ones), skipping the client-only ones;
- removes the mods **it** installed before that are no longer in the modpack (mods you added by hand are kept);
- applies the modpack configuration (overrides) like the launcher does for players: only files that are missing, or
  all of them when the modpack version changes. Client-only files (`options.txt`, `servers.dat`, shader and resource
  packs) are skipped, and Modrinth `server-overrides/` is used instead of `client-overrides/`. `--no-config` skips
  the configuration entirely.

Output is printed and saved to `<server folder>/modpack-sync.log`. Run it in the server start script, before Forge,
so every restart picks up the latest modpack:

```bash
# run.sh (Linux)
./modpack-launcher --sync-server . || exit 1
# …the usual Forge command (java @user_jvm_args.txt @libraries/…/unix_args.txt nogui)
```

```bat
rem run.bat (Windows)
start /wait "" modpack-launcher.exe --sync-server .
if errorlevel 1 exit /b 1
rem …the usual Forge command
```

Which mods are client-only comes from the modpack data (Modrinth reports it; for CurseForge and local JARs the side is
unknown and the mod is installed on the server). Make sure client-only mods are marked as such, or the server will
fail to start. Forge itself must be installed on the server once (with the Forge installer). The Linux build needs
the GTK 3 libraries (`libgtk-3-0`), also on a server.

## Custom authentication server
Players can sign in with accounts of **your own authentication server** instead of (or besides) Microsoft accounts.
Any server implementing the Yggdrasil API used by authlib-injector works, for example
[Drasl](https://github.com/unmojang/drasl) or Blessing Skin with its Yggdrasil plugin.

1. Host the server behind **HTTPS** and note its API root (e.g. `https://auth.mydomain.com/api/yggdrasil/`).
   A root that announces the real API with the `X-Authlib-Injector-API-Location` header also works.
2. Download the official `authlib-injector-<version>.jar` from
   <https://github.com/yushijinhun/authlib-injector/releases> and save it as `assets/authlib-injector.jar`
   (commit it, or let CI download it: the workflow fetches the pinned version when the file is missing).
   It is **embedded in the executable** at build time; use the unaltered official jar (see `THIRD_PARTY_NOTICES.md`).
3. In `launcher.toml`, set `[auth] url` to the API root, `name` to the name players will see, and choose whether
   Microsoft/offline accounts stay available (`allow_microsoft`, `allow_offline`). Build again.
4. **Your Minecraft server must authenticate against the same server**: start it (or your Velocity/BungeeCord proxy)
   with `-javaagent:authlib-injector.jar=<the same URL>` and keep `online-mode=true`. Otherwise joining fails with
   "Failed to verify username". Microsoft accounts can only join if your auth server falls back to Mojang (Drasl can).

In the launcher, the account type appears with the configured name: players type their username/email and password,
and the launcher saves only the session (access token, client token and profile) in `yggdrasil_accounts.json` in its
data folder; the password is never stored. Before playing it validates the token and refreshes it if it expired; if the
server cannot be reached, the game still starts with the saved session (single-player works). The game is launched
with `-javaagent:authlib-injector.jar=<url>` and the server metadata pre-fetched (`-Dauthlibinjector.yggdrasil.prefetched`).
`http://` URLs are rejected at build time, except for `localhost` (testing).

## Application icon

Put a square PNG (256×256 or larger) at `assets/icon.png`, or set its path in `[launcher] icon`. At build time it
becomes the icon of the launcher window and taskbar and, on Windows, the icon of the `.exe` itself (every size from
16 to 256 px is generated). Embedding it in the `.exe` uses `rc.exe` from the Windows SDK, which comes with the
Visual Studio C++ build tools that Rust already needs on Windows.

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
