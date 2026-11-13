//! `cargo xtask web-assets`: stage externalized runtime i18n TSV catalogs (and manifest) plus
//! browser showcase samples into `apps/wordcraft-web/target-assets/` for Trunk `copy-dir`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

const STUB_TSV: &str = "# craft-i18n stub (built without CRAFT_I18N_DIR)\n";

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

    Ok(locales.len())
}

pub fn run(root: &Path) -> Result<(), String> {
    let out_dir = root.join("apps/wordcraft-web/target-assets");
    let i18n_dir = std::env::var_os("CRAFT_I18N_DIR").filter(|v| !v.is_empty()).map(PathBuf::from);
    let required = std::env::var("CRAFT_I18N_REQUIRED").is_ok_and(|v| v.trim() == "1");
    let count = emit_web_assets(root, &out_dir, i18n_dir.as_deref(), required)?;
    println!("web-assets: staged {count} locale catalog directories + samples into {}", out_dir.display());
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
        assert_eq!(locales.len(), 27);
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
        assert_eq!(count, 27);
        assert!(out.join("i18n/manifest.json").is_file());
        assert!(out.join("i18n/fr/wordcraft.tsv").is_file());
        assert!(out.join("samples/spec.md").is_file());

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
}
