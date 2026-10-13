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
  - `[microsoft] app_id` → your own Azure app ID (optional).

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

## Launcher auto-update
With `[launcher] update_url` (a Dropbox link to the .exe), the launcher checks on startup whether the
published file has been replaced; if so, it downloads it, replaces itself and restarts. Upload the new version
using the SAME filename (Dropbox's **Replace** option) so the shared link does not change. Dropbox may limit
traffic from a single public link if you have many players.

## Error "failed to find tool \"cc\"" when building
A C compiler is missing (required by dependencies such as aws-lc-sys or ring).
  macOS:  xcode-select --install
  Debian/Ubuntu:  sudo apt install build-essential pkg-config libxkbcommon-dev libwayland-dev libx11-dev libssl-dev
Cross-compiling for Linux/macOS from Windows requires cross-compilation tools. Use
`.github/workflows/build.yml` (GitHub Actions) to obtain binaries for each operating system.
