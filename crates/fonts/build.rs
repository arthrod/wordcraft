//! Optional craft-fonts build input (https://github.com/storytold/craft-fonts, recipe from its
//! `docs/integration.md`). With `CRAFT_FONTS_DIR=<checkout>` the fonts in its
//! `fonts/manifest.txt` are embedded as `CRAFT_FONTS`; unset, `CRAFT_FONTS` is empty. Web
//! (wasm32) builds embed only the Georgian interface face (about 230 KB), to stay within the web
//! size budget; the Japanese one is 4.7 MB. With `CRAFT_FONTS_REQUIRED` (releases), a native
//! build also fails when the checkout has no font for a CJK interface language (Japanese,
//! Simplified Chinese), as an outdated pin once did (#241). It only reads the local checkout: no
//! network.
use std::fmt::Write as _;
use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-env-changed=CRAFT_FONTS_DIR");
    println!("cargo::rerun-if-env-changed=CRAFT_FONTS_REQUIRED");
    println!("cargo::rerun-if-env-changed=CARGO_FEATURE_MINIMAL_WASM_FONTS");
    let minimal_wasm_fonts = std::env::var_os("CARGO_FEATURE_MINIMAL_WASM_FONTS").is_some();
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default());

    if let Err(e) = write_bundled_wasm_fonts(&out_dir, minimal_wasm_fonts) {
        println!("cargo::error=writing bundled_fonts.rs: {e}");
    }

    let mut src = String::from("pub static CRAFT_FONTS: &[CraftFont] = &[\n");
    let wasm = std::env::var("CARGO_CFG_TARGET_ARCH").is_ok_and(|a| a == "wasm32");
    // Native minimal builds skip craft-fonts; minimal wasm builds keep the small Arabic/Georgian UI faces.
    if (!minimal_wasm_fonts || wasm)
        && let Some(dir) = std::env::var_os("CRAFT_FONTS_DIR").map(PathBuf::from)
    {
        match craft_fonts(&dir) {
            Ok((entries, covered)) => {
                src.push_str(&entries);
                // Each CJK interface language needs its own face: without the Chinese one, Chinese
                // menus fall back to the Japanese face and show boxes for simplified hanzi (#241).
                if std::env::var_os("CRAFT_FONTS_REQUIRED").is_some() && !wasm {
                    for s in REQUIRED_SCRIPTS.iter().filter(|s| !covered.iter().any(|c| c == *s)) {
                        println!("cargo::error=CRAFT_FONTS_DIR={}: no {s} font in fonts/manifest.txt (craft-fonts too old?)", dir.display());
                    }
                }
            }
            Err(e) if std::env::var_os("CRAFT_FONTS_REQUIRED").is_some() => {
                println!("cargo::error=CRAFT_FONTS_DIR={}: {e}", dir.display());
            }
            Err(e) => println!("cargo::warning=building without craft-fonts: CRAFT_FONTS_DIR={}: {e}", dir.display()),
        }
    }
    src.push_str("];\n");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default()).join("craft_fonts.rs");
    if let Err(e) = std::fs::write(&out, src) {
        println!("cargo::error=writing {}: {e}", out.display());
    }
}

/// Scripts a release build (`CRAFT_FONTS_REQUIRED`, native) must embed a font for: the CJK
/// interface languages' (Japanese, Simplified Chinese).
const REQUIRED_SCRIPTS: &[&str] = &["Jpan", "Hans"];

/// One `CraftFont { .. }` initialiser per manifest line, and the scripts the embedded fonts cover.
fn craft_fonts(dir: &std::path::Path) -> Result<(String, Vec<String>), String> {
    let manifest = dir.join("fonts/manifest.txt");
    println!("cargo::rerun-if-changed={}", manifest.display());
    let text = std::fs::read_to_string(&manifest).map_err(|e| format!("{}: {e}", manifest.display()))?;
    let wasm = std::env::var("CARGO_CFG_TARGET_ARCH").is_ok_and(|a| a == "wasm32");
    let mut out = String::new();
    let mut covered = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let f: Vec<&str> = line.split(" | ").map(str::trim).collect();
        let [family, style, file, scripts, ..] = f.as_slice() else {
            return Err(format!("malformed manifest line: {line}"));
        };
        // Arabic interface faces are small, so the web build keeps them too.
        let small_ui_face = scripts.split(',').any(|s| matches!(s.trim(), "Arab" | "Geor"));
        if wasm && !small_ui_face {
            continue;
        }
        let path = dir.join(file).canonicalize().map_err(|e| format!("{file}: {e}"))?;
        println!("cargo::rerun-if-changed={}", path.display());
        covered.extend(scripts.split(',').map(|s| s.trim().to_string()));
        let scripts: Vec<String> = scripts.split(',').map(|s| format!("{:?}", s.trim())).collect();
        let _ = writeln!(
            out,
            "    CraftFont {{ family: {family:?}, style: {style:?}, scripts: &[{}], bytes: include_bytes!({:?}) }},",
            scripts.join(", "),
            path.display().to_string(),
        );
    }
    Ok((out, covered))
}

const CORE_WASM_FONTS: &[&str] = &["SourceSans3-Regular", "SourceSerif4-Regular", "Inter-Regular", "Inter-Medium"];

fn write_bundled_wasm_fonts(out_dir: &std::path::Path, minimal: bool) -> Result<(), String> {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let assets_dir = manifest_dir.join("../../assets/fonts");
    let br_dir = out_dir.join("bundled_br");
    let mut rs = String::new();
    if minimal {
        std::fs::create_dir_all(&br_dir).map_err(|e| format!("{}: {e}", br_dir.display()))?;
        rs.push_str("pub(super) static BUNDLED_BR: &[(&str, &[u8])] = &[\n");
        let params = brotli::enc::BrotliEncoderParams { quality: 9, lgwin: 22, ..Default::default() };
        for stem in CORE_WASM_FONTS {
            let src = assets_dir.join(format!("{stem}.ttf"));
            println!("cargo::rerun-if-changed={}", src.display());
            let raw = std::fs::read(&src).map_err(|e| format!("{}: {e}", src.display()))?;
            let mut compressed = Vec::new();
            let mut reader = std::io::Cursor::new(raw);
            brotli::BrotliCompress(&mut reader, &mut compressed, &params).map_err(|e| format!("brotli {stem}: {e}"))?;
            let dst = br_dir.join(format!("{stem}.ttf.br"));
            std::fs::write(&dst, compressed).map_err(|e| format!("{}: {e}", dst.display()))?;
            let _ = writeln!(rs, "    ({stem:?}, include_bytes!({:?})),", dst.display().to_string());
        }
        rs.push_str("];\n");
    }
    let out_file = out_dir.join("bundled_fonts.rs");
    std::fs::write(&out_file, rs).map_err(|e| format!("{}: {e}", out_file.display()))
}
