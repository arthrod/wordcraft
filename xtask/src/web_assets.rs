//! `cargo xtask web-assets`: stage externalized runtime i18n TSV catalogs (and manifest) plus
//! browser showcase samples into `apps/wordcraft-web/target-assets/` for Trunk `copy-dir`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

const STUB_TSV: &str = "# craft-i18n stub (built without CRAFT_I18N_DIR)\n";

/// The site is a public product page as well as an app: crawlers may index it.
const ROBOTS_TXT: &str = "User-agent: *\nAllow: /\n";

/// Served by static hosts (Cloudflare Pages serves a root `404.html` with status 404) when a path
/// matches no deployed file, so missing assets are a real error rather than a silent `index.html`
/// with status 200.
const NOT_FOUND_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>Page not found — WordCraft</title>
<style>
  body { margin: 0; min-height: 100vh; display: flex; align-items: center; justify-content: center; background: #e6e6e6; color: #334155; font-family: system-ui, -apple-system, sans-serif; }
  main { max-width: 34rem; padding: 2rem; }
  h1 { font-size: 1.4rem; margin: 0 0 0.75rem; }
  p { line-height: 1.5; }
  a { color: #2b47b5; }
</style>
</head>
<body>
<main>
  <h1>Page not found</h1>
  <p>The page you requested does not exist or has moved.</p>
  <p><a href="/">Open WordCraft</a></p>
</main>
</body>
</html>
"#;

const SAMPLE_SPEC_MD: &str = r#"# WordCraftWeb Architecture & Browser Capabilities

WordCraftWeb runs the clean-room Rust **WordCraft** document engine, layout engine, OOXML codec, and GPU canvas UI completely inside WebAssembly.

## Key Highlights

- **Zero-copy local editing**: `.docx`, `.md`, and `.txt` files are parsed and saved client-side in your browser without uploading document contents.
- **Microsoft Word parity**: full paragraph styles, tables, headers/footers, tracked changes, comments, and math typesetting.
- **Scriptable Promise API**: control the live document from JavaScript via `start(options)` or `window.wordcraft`.

> Edit this Markdown live in the Demo Bar split-pane and click **Apply Markdown** to update the formatted canvas immediately.
"#;

const SAMPLE_WELCOME_MD: &str = r#"# Welcome to WordCraftWeb

**WordCraftWeb** is an open-source, clean-room word processor compiled to WebAssembly (`wordcraft-web`).

## Try It Now

1. Switch samples from the Demo Bar above or drag any `.docx` / `.md` file onto the editor.
2. Edit in the formatted canvas *or* open the live Markdown split-pane.
3. Export back to `.docx` directly in your browser.
"#;

const SAMPLE_LETTER_MD: &str = r#"# Studio Collaborator Agreement

November 13, 2026

Dear Maker,

Welcome to the Alder Street Open Studio. Please remember to label shared containers, wipe workbenches after kiln sessions, and lock the north studio gate if you are the last person out.

Warm regards,

The Studio Crew
"#;

/// The document the viewer-first page shows when it is given none: the three sample texts
/// above, as one `.docx` (`apps/wordcraft-viewer-web` fetches `samples/welcome.docx`).
pub const VIEWER_SAMPLE_DOCX: &str = "welcome.docx";

/// Parse every non-English `LangInfo { code: "..." }` entry from `crates/ui-egui/src/i18n.rs`.
pub fn offered_non_english_locales(root: &Path) -> Result<Vec<String>, String> {
    let i18n_path = root.join("crates/ui-egui/src/i18n.rs");
    let src = std::fs::read_to_string(&i18n_path).map_err(|e| format!("{}: {e}", i18n_path.display()))?;
    parse_offered_non_english_locales(&src)
}

fn parse_offered_non_english_locales(src: &str) -> Result<Vec<String>, String> {
    let mut locales = Vec::new();
    let mut in_table = false;
    for line in src.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("pub static LANGUAGES:") {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        if trimmed.starts_with("];") {
            break;
        }
        if let Some(after) = trimmed.split("code: \"").nth(1)
            && let Some(end) = after.find('"')
        {
            let code = &after[..end];
            if code != "en" && !code.is_empty() && !locales.iter().any(|c| c == code) {
                locales.push(code.to_string());
            }
        }
    }
    if locales.is_empty() {
        return Err("could not parse any non-English locales from crates/ui-egui/src/i18n.rs".into());
    }
    Ok(locales)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Emit `i18n/<lang>/{common.tsv,wordcraft.tsv}`, `i18n/manifest.json`, and `samples/*` under `out_dir`.
pub fn emit_web_assets(root: &Path, out_dir: &Path, i18n_dir: Option<&Path>, required: bool) -> Result<usize, String> {
    let locales = offered_non_english_locales(root)?;
    let source_locales = match i18n_dir.filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => {
            let locales_root = dir.join("locales");
            if !locales_root.is_dir() {
                let msg = format!("CRAFT_I18N_DIR ({}) does not contain a locales/ directory", dir.display());
                if required {
                    return Err(msg);
                }
                eprintln!("warning: {msg}; emitting stub web i18n catalogs");
                None
            } else {
                Some(locales_root)
            }
        }
        None => {
            if required {
                return Err("CRAFT_I18N_REQUIRED=1 but CRAFT_I18N_DIR is unset: web release requires translations".into());
            }
            None
        }
    };

    let i18n_out = out_dir.join("i18n");
    std::fs::create_dir_all(&i18n_out).map_err(|e| format!("{}: {e}", i18n_out.display()))?;

    let mut manifest_locales = BTreeMap::new();
    for code in &locales {
        let loc_dir = i18n_out.join(code);
        std::fs::create_dir_all(&loc_dir).map_err(|e| format!("{}: {e}", loc_dir.display()))?;

        let (common_bytes, wordcraft_bytes, populated) = if let Some(src_root) = &source_locales {
            let src_loc = src_root.join(code);
            let app_path = src_loc.join("wordcraft.tsv");
            if !app_path.is_file() {
                if required {
                    return Err(format!("missing required catalog {}", app_path.display()));
                }
                (STUB_TSV.as_bytes().to_vec(), STUB_TSV.as_bytes().to_vec(), false)
            } else {
                let app = std::fs::read(&app_path).map_err(|e| format!("{}: {e}", app_path.display()))?;
                let common_path = src_loc.join("common.tsv");
                let common = if common_path.is_file() {
                    std::fs::read(&common_path).map_err(|e| format!("{}: {e}", common_path.display()))?
                } else {
                    Vec::new()
                };
                (common, app, true)
            }
        } else {
            (STUB_TSV.as_bytes().to_vec(), STUB_TSV.as_bytes().to_vec(), false)
        };

        std::fs::write(loc_dir.join("common.tsv"), &common_bytes).map_err(|e| format!("{}: {e}", loc_dir.join("common.tsv").display()))?;
        std::fs::write(loc_dir.join("wordcraft.tsv"), &wordcraft_bytes).map_err(|e| format!("{}: {e}", loc_dir.join("wordcraft.tsv").display()))?;

        manifest_locales.insert(
            code.clone(),
            serde_json::json!({
                "common": format!("{code}/common.tsv"),
                "wordcraft": format!("{code}/wordcraft.tsv"),
                "sha256_common": sha256_hex(&common_bytes),
                "sha256_wordcraft": sha256_hex(&wordcraft_bytes),
                "populated": populated,
            }),
        );
    }

    let manifest = serde_json::json!({
        "locales": manifest_locales,
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| format!("serialize manifest: {e}"))?;
    std::fs::write(i18n_out.join("manifest.json"), manifest_bytes).map_err(|e| format!("{}: {e}", i18n_out.join("manifest.json").display()))?;

    let samples_out = out_dir.join("samples");
    std::fs::create_dir_all(&samples_out).map_err(|e| format!("{}: {e}", samples_out.display()))?;
    for (name, content) in [("welcome.md", SAMPLE_WELCOME_MD), ("spec.md", SAMPLE_SPEC_MD), ("letter.md", SAMPLE_LETTER_MD)] {
        std::fs::write(samples_out.join(name), content).map_err(|e| format!("{}: {e}", samples_out.join(name).display()))?;
    }

    // The viewer-first page's built-in sample (plan 10.3). The view-only module reads `.docx`
    // and nothing else, so the same sample text is staged as a package too.
    let viewer_sample = [SAMPLE_WELCOME_MD, SAMPLE_SPEC_MD, SAMPLE_LETTER_MD].join("\n");
    let docx =
        wordcraft_docx::write(&wordcraft_formats::markdown::import(&viewer_sample)).map_err(|e| format!("samples/{VIEWER_SAMPLE_DOCX}: {e}"))?;
    std::fs::write(samples_out.join(VIEWER_SAMPLE_DOCX), docx).map_err(|e| format!("{}: {e}", samples_out.join(VIEWER_SAMPLE_DOCX).display()))?;

    for (name, content) in [("robots.txt", ROBOTS_TXT), ("404.html", NOT_FOUND_HTML)] {
        std::fs::write(out_dir.join(name), content).map_err(|e| format!("{}: {e}", out_dir.join(name).display()))?;
    }

    emit_font_packs(root, out_dir)?;

    Ok(locales.len())
}

/// A downloadable pack face: the `assets/fonts/` faces that `minimal-wasm-fonts` builds do not
/// bundle. This table must agree with `CORE_WASM_FONTS` in `crates/fonts/build.rs` (the bundled
/// four) and with `PACK_FACES` in `crates/fonts/src/packs.rs`; `tests::font_packs_cover_every_non_bundled_face`
/// checks the first, the manifest-vs-PACK_FACES agreement is asserted by the family/weight/italic
/// fields the web shell's `packs::pick_pack_face` matches on.
struct PackSpec {
    /// File stem in `assets/fonts/` ("SourceSans3-Bold"); the shipped file is `<stem>.<hash8>.ttf.br`.
    stem: &'static str,
    /// Typographic family the face reports ("Source Sans 3").
    family: &'static str,
    /// usWeightClass (400 regular, 600 semibold, 700 bold).
    weight: u16,
    italic: bool,
}

const PACK_FACES: &[PackSpec] = &[
    PackSpec { stem: "Inter-SemiBold", family: "Inter", weight: 600, italic: false },
    PackSpec { stem: "JetBrainsMono-Regular", family: "JetBrains Mono", weight: 400, italic: false },
    PackSpec { stem: "SourceSans3-Bold", family: "Source Sans 3", weight: 700, italic: false },
    PackSpec { stem: "SourceSans3-It", family: "Source Sans 3", weight: 400, italic: true },
    PackSpec { stem: "SourceSans3-Semibold", family: "Source Sans 3", weight: 600, italic: false },
    PackSpec { stem: "SourceSerif4-Bold", family: "Source Serif 4", weight: 700, italic: false },
    PackSpec { stem: "SourceSerif4-BoldIt", family: "Source Serif 4", weight: 700, italic: true },
    PackSpec { stem: "SourceSerif4-It", family: "Source Serif 4", weight: 400, italic: true },
    PackSpec { stem: "SourceSerif4-Semibold", family: "Source Serif 4", weight: 600, italic: false },
];

/// The four faces `crates/fonts/build.rs::CORE_WASM_FONTS` embeds in `minimal-wasm-fonts` builds:
/// everything else in `assets/fonts/` is a pack. Kept in sync by
/// `tests::font_packs_cover_every_non_bundled_face`, which parses `build.rs` rather than trusting
/// this copy. Only the drift test reads it.
#[cfg(test)]
const CORE_WASM_FONTS: &[&str] = &["SourceSans3-Regular", "SourceSerif4-Regular", "Inter-Regular", "Inter-Medium"];

/// Stage `fonts/<stem>.<hash8>.ttf.br` (Brotli quality 11; `<hash8>` is the first 8 hex chars of the
/// SHA-256 of the *uncompressed* TTF, so the name changes exactly when the font does) plus
/// `fonts/manifest.json` under `out_dir`. Manifest entries carry `family`, `weight`, `italic`,
/// `file`, `bytes` (decoded size) and `compressedBytes`; the URL is always `assetBase + "fonts/" + file`.
fn emit_font_packs(root: &Path, out_dir: &Path) -> Result<(), String> {
    let fonts_dir = root.join("assets/fonts");
    let fonts_out = out_dir.join("fonts");
    std::fs::create_dir_all(&fonts_out).map_err(|e| format!("{}: {e}", fonts_out.display()))?;

    let mut faces = Vec::with_capacity(PACK_FACES.len());
    let mut written = std::collections::BTreeSet::new();
    for spec in PACK_FACES {
        let path = fonts_dir.join(format!("{}.ttf", spec.stem));
        let ttf = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let hash8 = sha256_hex(&ttf).get(..8).map(str::to_string).unwrap_or_default();
        let params = brotli::enc::BrotliEncoderParams { quality: 11, lgwin: 22, ..Default::default() };
        let mut compressed = Vec::new();
        brotli::BrotliCompress(&mut ttf.as_slice(), &mut compressed, &params).map_err(|e| format!("brotli {}: {e}", spec.stem))?;
        let file = format!("{}.{hash8}.ttf.br", spec.stem);
        std::fs::write(fonts_out.join(&file), &compressed).map_err(|e| format!("{}: {e}", fonts_out.join(&file).display()))?;
        written.insert(file.clone());
        faces.push(serde_json::json!({
            "family": spec.family,
            "weight": spec.weight,
            "italic": spec.italic,
            "file": file,
            "bytes": ttf.len(),
            "compressedBytes": compressed.len(),
        }));
    }

    let manifest = serde_json::json!({ "faces": faces });
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| format!("serialize font manifest: {e}"))?;
    std::fs::write(fonts_out.join("manifest.json"), bytes).map_err(|e| format!("{}: {e}", fonts_out.join("manifest.json").display()))?;

    // Keep only this run's files plus the manifest: a file whose stem still matches a spec but
    // whose hash is stale (the TTF changed since the last staging) must not ship either —
    // deployed static-file limits count every file left behind.
    let entries = std::fs::read_dir(&fonts_out).map_err(|e| format!("{}: {e}", fonts_out.display()))?;
    remove_stale_pack_files(&fonts_out, entries.map(|entry| entry.map(|e| e.file_name().to_string_lossy().to_string())), &written)
}

pub fn run(root: &Path) -> Result<(), String> {
    let out_dir = root.join("apps/wordcraft-web/target-assets");
    let i18n_dir = std::env::var_os("CRAFT_I18N_DIR").filter(|v| !v.is_empty()).map(PathBuf::from);
    let required = std::env::var("CRAFT_I18N_REQUIRED").is_ok_and(|v| v.trim() == "1");
    let count = emit_web_assets(root, &out_dir, i18n_dir.as_deref(), required)?;
    println!("web-assets: staged {count} locale catalog directories + samples into {}", out_dir.display());
    Ok(())
}

/// Removes every file of `dir` named by `names` that this run did not write (`manifest.json`
/// stays). A name the directory could not give is an error, not a file to skip: a listing that
/// stops early would leave stale pack files in the output and still report success.
fn remove_stale_pack_files(
    dir: &Path,
    names: impl Iterator<Item = std::io::Result<String>>,
    written: &std::collections::BTreeSet<String>,
) -> Result<(), String> {
    for name in names {
        let name = name.map_err(|e| format!("{}: {e}", dir.display()))?;
        if name != "manifest.json" && !written.contains(&name) {
            let path = dir.join(&name);
            std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_offered_locales() {
        let root = crate::root();
        let locales = offered_non_english_locales(&root).unwrap();
        assert!(locales.contains(&"fr".to_string()));
        assert!(locales.contains(&"ar".to_string()));
        assert!(locales.contains(&"pt-br".to_string()));
        assert!(!locales.contains(&"en".to_string()));
        assert_eq!(locales.len(), 28);
    }

    #[test]
    fn emit_fails_when_required_and_missing_and_copies_real_catalogs() {
        let root = crate::root();
        let dir = std::env::temp_dir().join(format!("wc_web_assets_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join("out");

        let err = emit_web_assets(&root, &out, None, true).unwrap_err();
        assert!(err.contains("CRAFT_I18N_REQUIRED=1"), "{err}");

        // Stub emission succeeds when optional.
        let count = emit_web_assets(&root, &out, None, false).unwrap();
        assert_eq!(count, 28);
        assert!(out.join("i18n/manifest.json").is_file());
        assert!(out.join("i18n/fr/wordcraft.tsv").is_file());
        assert!(out.join("samples/spec.md").is_file());
        assert!(out.join("robots.txt").is_file());
        let not_found = std::fs::read_to_string(out.join("404.html")).unwrap();
        assert!(not_found.contains("Page not found"), "{not_found}");

        // Synthetic craft-i18n checkout copies real bytes when present.
        let fake_i18n = dir.join("craft-i18n");
        let fr_dir = fake_i18n.join("locales/fr");
        std::fs::create_dir_all(&fr_dir).unwrap();
        std::fs::write(fr_dir.join("common.tsv"), "\t@id\tlanguage.name\n\tOK\tOK\n").unwrap();
        std::fs::write(fr_dir.join("wordcraft.tsv"), "\t@id\tlanguage.name\n\tFile\tFichier\n").unwrap();
        emit_web_assets(&root, &out, Some(&fake_i18n), false).unwrap();
        let copied = std::fs::read_to_string(out.join("i18n/fr/wordcraft.tsv")).unwrap();
        assert!(copied.contains("Fichier"), "{copied}");
        let manifest: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(out.join("i18n/manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["locales"]["fr"]["populated"], true);
        assert_eq!(manifest["locales"]["de"]["populated"], false);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Parse the `CORE_WASM_FONTS` stem list out of `crates/fonts/build.rs`.
    fn core_wasm_fonts(root: &Path) -> Vec<String> {
        let src = std::fs::read_to_string(root.join("crates/fonts/build.rs")).unwrap();
        let start = src.find("CORE_WASM_FONTS").unwrap();
        let src = &src[start..];
        let list = src.split("= &[").nth(1).and_then(|s| s.split(']').next()).unwrap_or_default();
        list.split('"').filter(|s| s.chars().next().is_some_and(|c| c.is_alphabetic())).map(str::to_string).collect()
    }

    /// Every `assets/fonts/*.ttf` that `minimal-wasm-fonts` does not bundle is a pack face, no
    /// more and no fewer — a drifted `CORE_WASM_FONTS` or a new font file fails here.
    #[test]
    fn font_packs_cover_every_non_bundled_face() {
        let root = crate::root();
        let core = core_wasm_fonts(&root);
        assert_eq!(core.len(), 4, "CORE_WASM_FONTS: {core:?}");
        assert_eq!(core, CORE_WASM_FONTS, "xtask's copy of CORE_WASM_FONTS drifted from crates/fonts/build.rs");
        let pack_stems: Vec<&str> = PACK_FACES.iter().map(|p| p.stem).collect();
        let mut ttf_stems = Vec::new();
        for e in std::fs::read_dir(root.join("assets/fonts")).unwrap().flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "ttf")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
            {
                ttf_stems.push(stem.to_string());
            }
        }
        assert_eq!(ttf_stems.len(), 13, "assets/fonts TTF count changed: {ttf_stems:?}");
        for stem in &ttf_stems {
            if core.iter().any(|c| c == stem) {
                assert!(!pack_stems.contains(&stem.as_str()), "bundled face {stem} is also a pack");
            } else {
                assert!(pack_stems.contains(&stem.as_str()), "{stem} is neither bundled nor a pack");
            }
        }
        assert_eq!(pack_stems.len(), 9);
    }

    /// The local table and `wordcraft_fonts::packs::PACK_FACES` must name the same faces with the
    /// same family/weight/italic: the manifest is matched by those fields (`pick_pack_face`), so a
    /// drifted entry here would fetch a face resolution never asks for, or vice versa.
    #[test]
    fn pack_faces_match_wordcraft_fonts() {
        let theirs = wordcraft_fonts::packs::PACK_FACES;
        assert_eq!(PACK_FACES.len(), theirs.len(), "pack face count drifted");
        for (ours, theirs) in PACK_FACES.iter().zip(theirs) {
            assert_eq!(ours.stem, theirs.stem);
            assert_eq!(ours.family, theirs.family);
            assert_eq!(ours.weight, theirs.weight);
            assert_eq!(ours.italic, theirs.italic);
        }
    }

    #[test]
    fn emit_font_packs_writes_decodable_files_and_manifest() {
        let root = crate::root();
        let dir = std::env::temp_dir().join(format!("wc_font_packs_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join("out");
        emit_font_packs(&root, &out).unwrap();

        let manifest: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(out.join("fonts/manifest.json")).unwrap()).unwrap();
        let faces = manifest["faces"].as_array().unwrap();
        assert_eq!(faces.len(), 9, "{faces:?}");
        for (spec, entry) in PACK_FACES.iter().zip(faces) {
            assert_eq!(entry["family"], spec.family);
            assert_eq!(entry["weight"], spec.weight);
            assert_eq!(entry["italic"], spec.italic);
            let file = entry["file"].as_str().unwrap();
            assert!(file.starts_with(&format!("{}.", spec.stem)) && file.ends_with(".ttf.br"), "{file}");
            // The hash in the name is the sha256 of the uncompressed TTF.
            let ttf = std::fs::read(root.join("assets/fonts").join(format!("{}.ttf", spec.stem))).unwrap();
            let want = sha256_hex(&ttf);
            assert!(file.contains(&want[..8]), "{file} lacks sha256 {}", &want[..8]);
            assert_eq!(entry["bytes"].as_u64().unwrap() as usize, ttf.len());
            let packed = std::fs::read(out.join("fonts").join(file)).unwrap();
            assert_eq!(entry["compressedBytes"].as_u64().unwrap() as usize, packed.len());
            assert!(!packed.is_empty());
            use std::io::Read as _;
            let mut decoded = Vec::new();
            brotli_decompressor::Decompressor::new(packed.as_slice(), 4096).take(ttf.len() as u64 + 1).read_to_end(&mut decoded).unwrap();
            assert_eq!(decoded, ttf, "{} does not decode to the TTF", spec.stem);
        }
        // Stale pack files are removed on the next staging run: one whose stem left the spec
        // set, and one whose stem still matches but whose hash is out of date.
        std::fs::write(out.join("fonts/RemovedFace-Regular.deadbeef.ttf.br"), b"x").unwrap();
        std::fs::write(out.join("fonts/SourceSans3-Bold.deadbeef.ttf.br"), b"x").unwrap();
        emit_font_packs(&root, &out).unwrap();
        assert!(!out.join("fonts/RemovedFace-Regular.deadbeef.ttf.br").exists());
        assert!(!out.join("fonts/SourceSans3-Bold.deadbeef.ttf.br").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory entry that cannot be read stops the cleanup with an error; it is not skipped.
    #[test]
    fn stale_pack_cleanup_reports_an_unreadable_entry() {
        let dir = std::env::temp_dir().join(format!("wordcraft-xtask-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("old.ttf.br"), b"x").unwrap();
        std::fs::write(dir.join("kept.ttf.br"), b"x").unwrap();
        let written = std::collections::BTreeSet::from(["kept.ttf.br".to_string()]);
        let names = vec![Ok("kept.ttf.br".to_string()), Err(std::io::Error::other("unreadable entry")), Ok("old.ttf.br".to_string())];
        let err = remove_stale_pack_files(&dir, names.into_iter(), &written).unwrap_err();
        assert!(err.contains("unreadable entry"), "{err}");
        let names = vec![Ok("kept.ttf.br".to_string()), Ok("manifest.json".to_string()), Ok("old.ttf.br".to_string())];
        remove_stale_pack_files(&dir, names.into_iter(), &written).unwrap();
        assert!(dir.join("kept.ttf.br").exists() && !dir.join("old.ttf.br").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
