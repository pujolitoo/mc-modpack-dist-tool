//! Reads `launcher.toml` AT BUILD TIME y generates `build_config.rs`:
//! title, modpack URL, Microsoft ID, minimum RAM and (if `path` is specified) the embedded ZIP
//! en el propio ejecutable con `include_bytes!`.

use std::{env, fs, path::PathBuf};

fn get<'a>(cfg: &'a toml::Table, section: &str, key: &str) -> Option<&'a toml::Value> {
    cfg.get(section)?.as_table()?.get(key)
}

fn get_str(cfg: &toml::Table, section: &str, key: &str) -> String {
    get(cfg, section, key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn main() {
    println!("cargo:rerun-if-changed=launcher.toml");
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let cfg_path = manifest_dir.join("launcher.toml");
    let text = fs::read_to_string(&cfg_path)
        .unwrap_or_else(|e| panic!("No se pudo leer {}: {e}", cfg_path.display()));
    let cfg: toml::Table = text
        .parse()
        .unwrap_or_else(|e| panic!("launcher.toml is not valid TOML: {e}"));

    let title = get_str(&cfg, "launcher", "title");
    let update_url = get_str(&cfg, "launcher", "update_url");
    let url = get_str(&cfg, "modpack", "url");
    let path = get_str(&cfg, "modpack", "path");
    let app_id = get_str(&cfg, "microsoft", "app_id");
    let format = {
        let f = get_str(&cfg, "modpack", "format").to_lowercase();
        if f.is_empty() { "auto".to_string() } else { f }
    };
    if !["auto", "forge", "curseforge", "modrinth"].contains(&format.as_str()) {
        panic!("\n\nlauncher.toml: [modpack] format = \"{format}\" is invalid. Use: auto, forge, curseforge o modrinth.\n");
    }
    let cf_key = get_str(&cfg, "curseforge", "api_key");
    let cf_base = get_str(&cfg, "curseforge", "api_base");
    let min_ram = get(&cfg, "requirements", "min_ram_mb")
        .and_then(|v| v.as_integer())
        .unwrap_or(0)
        .max(0);

    if url.is_empty() && path.is_empty() {
        panic!(
            "\n\nlauncher.toml: rellena [modpack] url = \"…\" o path = \"…\" antes de compilar.\n\
             (path is relative to the project folder, where this launcher.toml is located)\n"
        );
    }

    let mut out = String::new();
    out.push_str(&format!("pub const TITLE: &str = {:?};\n", title));
    out.push_str(&format!("pub const MODPACK_URL: &str = {:?};\n", url));
    out.push_str(&format!("pub const UPDATE_URL: &str = {:?};\n", update_url));
    out.push_str(&format!("pub const MSA_APP_ID: &str = {:?};\n", app_id));
    out.push_str(&format!("pub const MIN_RAM_MB: u64 = {};\n", min_ram));
    out.push_str(&format!("pub const FORMAT: &str = {:?};\n", format));
    out.push_str(&format!("pub const CF_API_KEY: &str = {:?};\n", cf_key));
    out.push_str(&format!("pub const CF_API_BASE: &str = {:?};\n", cf_base));

    if path.is_empty() {
        out.push_str("pub static EMBEDDED_PACK: Option<&[u8]> = None;\n");
    } else {
        let p = PathBuf::from(&path);
        let abs = if p.is_absolute() { p } else { manifest_dir.join(p) };
        if !abs.is_file() {
            panic!(
                "\n\nlauncher.toml: no existe el modpack en «{}».\n\
                 Revisa [modpack] path (relativo a {}).\n",
                abs.display(),
                manifest_dir.display()
            );
        }
        println!("cargo:rerun-if-changed={}", abs.display());
        let abs_str = abs.to_str().expect("la ruta del modpack debe ser UTF-8");
        out.push_str(&format!(
            "pub static EMBEDDED_PACK: Option<&[u8]> = Some(include_bytes!({:?}));\n",
            abs_str
        ));
    }

    let dest = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("build_config.rs");
    fs::write(&dest, out).expect("no se pudo escribir build_config.rs");
}
