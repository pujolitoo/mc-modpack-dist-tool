//! Reads `launcher.toml` AT BUILD TIME and generates `build_config.rs`:
//! title, modpack URL, Microsoft ID, minimum RAM and (if `path` is specified) the embedded ZIP
//! inside the executable itself with `include_bytes!`.
//!
//! It also embeds the languages of `lang/*.json` (generates `languages.rs`, see src/i18n.rs) and checks them:
//! every key used in the code must exist in `lang/en.json`, and `tf!` must pass every placeholder of its text.

use std::collections::{BTreeMap, BTreeSet};
use std::{env, fs, path::Path, path::PathBuf};

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
        .unwrap_or_else(|e| panic!("Could not read {}: {e}", cfg_path.display()));
    let cfg: toml::Table = text
        .parse()
        .unwrap_or_else(|e| panic!("launcher.toml is not valid TOML: {e}"));

    let title = get_str(&cfg, "launcher", "title");
    let update_url = get_str(&cfg, "launcher", "update_url");
    let language = {
        let l = get_str(&cfg, "launcher", "language").to_lowercase();
        if l.is_empty() { "en".to_string() } else { l }
    };
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let codes = embed_languages(&manifest_dir, &out_dir);
    if !codes.contains(&language) {
        panic!(
            "\n\nlauncher.toml: [launcher] language = \"{language}\" has no lang/{language}.json. Available: {}.\n",
            codes.join(", ")
        );
    }
    let url = get_str(&cfg, "modpack", "url");
    let path = get_str(&cfg, "modpack", "path");
    let app_id = get_str(&cfg, "microsoft", "app_id");
    let format = {
        let f = get_str(&cfg, "modpack", "format").to_lowercase();
        if f.is_empty() { "auto".to_string() } else { f }
    };
    if !["auto", "forge", "curseforge", "modrinth"].contains(&format.as_str()) {
        panic!("\n\nlauncher.toml: [modpack] format = \"{format}\" is invalid. Use: auto, forge, curseforge or modrinth.\n");
    }
    let cf_key = get_str(&cfg, "curseforge", "api_key");
    let cf_base = get_str(&cfg, "curseforge", "api_base");
    let min_ram = get(&cfg, "requirements", "min_ram_mb")
        .and_then(|v| v.as_integer())
        .unwrap_or(0)
        .max(0);

    let server_address = get_str(&cfg, "server", "address");
    let server_auto_join = get(&cfg, "server", "auto_join").and_then(|v| v.as_bool()).unwrap_or(false);
    if server_address.contains("://") || server_address.contains('/') || server_address.chars().any(char::is_whitespace) {
        panic!(
            "\n\nlauncher.toml: [server] address = \"{server_address}\" is invalid.\n\
             Use only the host and optionally the port, e.g. \"play.mydomain.com\" or \"123.45.67.89:25565\".\n"
        );
    }

    if url.is_empty() && path.is_empty() {
        panic!(
            "\n\nlauncher.toml: fill in [modpack] url = \"…\" or path = \"…\" before building.\n\
             (path is relative to the project folder, where this launcher.toml is located)\n"
        );
    }

    let mut out = String::new();
    out.push_str(&format!("pub const TITLE: &str = {:?};\n", title));
    out.push_str(&format!("pub const MODPACK_URL: &str = {:?};\n", url));
    out.push_str(&format!("pub const UPDATE_URL: &str = {:?};\n", update_url));
    out.push_str(&format!("pub const LANGUAGE: &str = {:?};\n", language));
    out.push_str(&format!("pub const MSA_APP_ID: &str = {:?};\n", app_id));
    out.push_str(&format!("pub const MIN_RAM_MB: u64 = {};\n", min_ram));
    out.push_str(&format!("pub const FORMAT: &str = {:?};\n", format));
    out.push_str(&format!("pub const CF_API_KEY: &str = {:?};\n", cf_key));
    out.push_str(&format!("pub const CF_API_BASE: &str = {:?};\n", cf_base));
    out.push_str(&format!("pub const SERVER_ADDRESS: &str = {:?};\n", server_address));
    out.push_str(&format!("pub const SERVER_AUTO_JOIN: bool = {};\n", server_auto_join));

    if path.is_empty() {
        out.push_str("pub static EMBEDDED_PACK: Option<&[u8]> = None;\n");
    } else {
        let p = PathBuf::from(&path);
        let abs = if p.is_absolute() { p } else { manifest_dir.join(p) };
        if !abs.is_file() {
            panic!(
                "\n\nlauncher.toml: no modpack found at «{}».\n\
                 Check [modpack] path (relative to {}).\n",
                abs.display(),
                manifest_dir.display()
            );
        }
        println!("cargo:rerun-if-changed={}", abs.display());
        let abs_str = abs.to_str().expect("the modpack path must be UTF-8");
        out.push_str(&format!(
            "pub static EMBEDDED_PACK: Option<&[u8]> = Some(include_bytes!({:?}));\n",
            abs_str
        ));
    }

    // Classic zlib for Linux builds (see src/compat.rs). CI copies it to assets/linux/libz.so.1 before building;
    // without that file the build still works, it just cannot fix the zlib-ng problem by itself.
    println!("cargo:rerun-if-changed=assets/linux/libz.so.1");
    let zlib = manifest_dir.join("assets/linux/libz.so.1");
    if env::var("CARGO_CFG_TARGET_OS").map(|os| os == "linux").unwrap_or(false) && zlib.is_file() {
        let zlib_str = zlib.to_str().expect("the zlib path must be UTF-8");
        out.push_str(&format!("pub static CLASSIC_ZLIB: Option<&[u8]> = Some(include_bytes!({:?}));\n", zlib_str));
    } else {
        out.push_str("pub static CLASSIC_ZLIB: Option<&[u8]> = None;\n");
    }

    let dest = out_dir.join("build_config.rs");
    fs::write(&dest, out).expect("could not write build_config.rs");
}

// ═══════════════════════════════ languages (lang/*.json) ═══════════════════════════════

/// `{name}` placeholders of a text (`{{` / `}}` are literal braces).
fn placeholders(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = text;
    while let Some(i) = rest.find('{') {
        let tail = &rest[i..];
        if tail.starts_with("{{") {
            rest = &tail[2..];
            continue;
        }
        match tail.find('}') {
            Some(end) => {
                out.insert(tail[1..end].to_string());
                rest = &tail[end + 1..];
            }
            None => break,
        }
    }
    out
}

struct LangFile {
    code: String,
    path: PathBuf,
    texts: BTreeMap<String, String>,
}

fn bad(path: &Path, what: &str) -> ! {
    panic!("\n\n{}: {what}\n", path.display())
}

/// Reads and validates one `lang/<code>.json`: `{ "language_name": "...", "texts": { "key": "text", ... } }`.
fn read_lang(path: &Path) -> LangFile {
    let code = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_lowercase();
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("Could not read {}: {e}", path.display()));
    let v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .unwrap_or_else(|e| panic!("\n\n{} is not valid JSON: {e}\n", path.display()));
    match v.get("language_name").and_then(|n| n.as_str()) {
        Some(n) if !n.trim().is_empty() => {}
        _ => bad(path, "missing \"language_name\" (the language name shown in the selector, e.g. \"Français\")"),
    }
    let Some(obj) = v.get("texts").and_then(|t| t.as_object()) else { bad(path, "missing the \"texts\" object") };
    let mut texts = BTreeMap::new();
    for (k, val) in obj {
        match val.as_str() {
            Some(s) => {
                texts.insert(k.clone(), s.to_string());
            }
            None => bad(path, &format!("\"{k}\" must be a text (string)")),
        }
    }
    LangFile { code, path: path.to_path_buf(), texts }
}

/// A `t("key")` or `tf!("key", name = …)` call found in the source code.
struct Use {
    key: String,
    /// `Some(names)` for `tf!`; `None` for `t`.
    args: Option<BTreeSet<String>>,
    place: String,
}

/// Finds the `t("…")` / `tf!("…", …)` calls in the `.rs` files of `dir` (comments are skipped).
fn scan_uses(dir: &Path, uses: &mut Vec<Use>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            scan_uses(&path, uses);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") || path.file_name().and_then(|n| n.to_str()) == Some("i18n.rs") {
            continue;
        }
        let src = fs::read_to_string(&path).unwrap_or_default();
        let b = src.as_bytes();
        let mut line_starts = vec![0usize];
        line_starts.extend(src.match_indices('\n').map(|(i, _)| i + 1));
        for pat in ["tf!(\"", "t(\""] {
            for (i, _) in src.match_indices(pat) {
                let prev = if i == 0 { b' ' } else { b[i - 1] };
                if prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'.' || prev == b':' {
                    continue;
                }
                let line_no = line_starts.partition_point(|&s| s <= i);
                let line = &src[line_starts[line_no - 1]..];
                if line.trim_start().starts_with("//") {
                    continue;
                }
                let start = i + pat.len();
                let Some(len) = src[start..].find('"') else { continue };
                let key = src[start..start + len].to_string();
                let args = if pat.starts_with("tf") { Some(tf_arg_names(&src[start + len + 1..])) } else { None };
                let place = format!("{}:{line_no}", path.display());
                uses.push(Use { key, args, place });
            }
        }
    }
}

/// Names of the `name = value` arguments of a `tf!` call, from the text right after its key up to the closing `)`.
fn tf_arg_names(rest: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let (mut depth, mut cur, mut chars) = (0i32, String::new(), rest.chars().peekable());
    let mut push = |arg: &str| {
        if let Some((name, _)) = arg.split_once('=') {
            let name = name.trim();
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                names.insert(name.to_string());
            }
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                cur.push('"');
                while let Some(d) = chars.next() {
                    if d == '\\' {
                        chars.next();
                    } else if d == '"' {
                        break;
                    }
                }
                cur.push('"');
            }
            '(' | '[' | '{' => {
                depth += 1;
                cur.push(c);
            }
            ')' | ']' | '}' if depth == 0 => {
                push(&cur);
                break;
            }
            ')' | ']' | '}' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => {
                push(&cur);
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    names
}

/// Embeds every `lang/*.json` (English first) into `languages.rs` and checks texts against the code.
/// Returns the available language codes.
fn embed_languages(manifest_dir: &Path, out_dir: &Path) -> Vec<String> {
    let lang_dir = manifest_dir.join("lang");
    println!("cargo:rerun-if-changed={}", lang_dir.display());
    println!("cargo:rerun-if-changed={}", manifest_dir.join("src").display());

    let mut files: Vec<LangFile> = fs::read_dir(&lang_dir)
        .unwrap_or_else(|e| panic!("\n\nCould not read the language folder {}: {e}\n", lang_dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("json")).unwrap_or(false))
        .map(|p| read_lang(&p))
        .collect();
    files.sort_by(|a, b| (a.code != "en", &a.code).cmp(&(b.code != "en", &b.code)));
    if files.first().map(|f| f.code.as_str()) != Some("en") {
        panic!("\n\nlang/en.json is required: it is the reference language and the fallback for missing translations.\n");
    }
    for f in &files {
        println!("cargo:rerun-if-changed={}", f.path.display());
    }

    // ── Keys used in the code must exist in en.json, and tf! must fill every placeholder ──
    let mut uses = Vec::new();
    scan_uses(&manifest_dir.join("src"), &mut uses);
    let en = &files[0].texts;
    let mut errors = Vec::new();
    let mut used = BTreeSet::new();
    for u in &uses {
        used.insert(u.key.clone());
        let Some(text) = en.get(&u.key) else {
            errors.push(format!("{}: key \"{}\" does not exist in lang/en.json", u.place, u.key));
            continue;
        };
        let ph = placeholders(text);
        match &u.args {
            None if !ph.is_empty() => errors.push(format!(
                "{}: \"{}\" has placeholders {:?}: use tf!(\"{}\", name = value, …) instead of t()",
                u.place, u.key, ph, u.key
            )),
            Some(args) => {
                for p in ph.difference(args) {
                    errors.push(format!("{}: tf!(\"{}\") does not pass {{{p}}}", u.place, u.key));
                }
            }
            None => {}
        }
    }
    if !errors.is_empty() {
        panic!("\n\nLanguage texts do not match the code:\n  - {}\n", errors.join("\n  - "));
    }
    for k in en.keys().filter(|k| !used.contains(*k)) {
        println!("cargo:warning=lang/en.json: \"{k}\" is not used in the code");
    }

    // ── Translations: missing texts fall back to English (warning), placeholders must be the same ──
    for f in &files[1..] {
        let missing: Vec<&String> = en.keys().filter(|k| !f.texts.contains_key(*k)).collect();
        if !missing.is_empty() {
            println!(
                "cargo:warning=lang/{}.json: {} untranslated text(s), English is used for them (e.g. \"{}\")",
                f.code,
                missing.len(),
                missing[0]
            );
        }
        for (k, text) in &f.texts {
            match en.get(k) {
                None => println!("cargo:warning=lang/{}.json: \"{k}\" does not exist in en.json (ignored)", f.code),
                Some(base) if placeholders(base) != placeholders(text) => println!(
                    "cargo:warning=lang/{}.json: \"{k}\" uses placeholders {:?}, but en.json uses {:?}",
                    f.code,
                    placeholders(text),
                    placeholders(base)
                ),
                Some(_) => {}
            }
        }
    }

    let mut out = String::from("pub static LANGUAGE_FILES: &[(&str, &str)] = &[\n");
    for f in &files {
        let p = f.path.to_str().expect("the lang/ path must be UTF-8");
        out.push_str(&format!("    ({:?}, include_str!({:?})),\n", f.code, p));
    }
    out.push_str("];\n");
    fs::write(out_dir.join("languages.rs"), out).expect("could not write languages.rs");
    files.into_iter().map(|f| f.code).collect()
}
