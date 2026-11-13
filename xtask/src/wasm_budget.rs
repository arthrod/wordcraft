//! `cargo xtask wasm-budget`: inspect `dist/web/wordcraft-web*_bg.wasm`, report section
//! sizes and compressed size, and enforce size + symbol ceilings.

use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use sha2::{Digest as _, Sha384};

pub const MAX_UNCOMPRESSED_BYTES: usize = 18_500_000;
pub const MAX_GZIPPED_BYTES: usize = 7_100_000;
pub const MAX_DATA_SECTION_BYTES: usize = 2_750_000;
pub const CLOUDFLARE_MAX_FILE_BYTES: usize = 25 * 1024 * 1024;

pub const FORBIDDEN_SYMBOLS: &[&str] = &["krilla::", "wordcraft_docbin::"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmReport {
    pub path: PathBuf,
    pub total_bytes: usize,
    pub gzip_bytes: usize,
    pub code_bytes: usize,
    pub data_bytes: usize,
    pub custom_bytes: usize,
    pub found_forbidden: Vec<String>,
}

/** Read an unsigned LEB128 u64 starting at `*pos`, advancing `*pos`. Shared with the
 * wasm-attr analyzer, which needs the same bounded decoding. */
pub(crate) fn read_leb_u64(bytes: &[u8], pos: &mut usize) -> Result<u64, String> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let b = *bytes.get(*pos).ok_or_else(|| "unexpected EOF in LEB128".to_string())?;
        *pos += 1;
        if shift >= 64 {
            return Err("LEB128 overflow".into());
        }
        result |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
}

/// Parse WASM sections and collect code/data/custom sizes and forbidden symbol markers.
pub fn analyze_wasm_bytes(path: PathBuf, bytes: &[u8], gzip_bytes: usize) -> Result<WasmReport, String> {
    if bytes.len() < 8 || bytes.get(..8) != Some(&[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]) {
        return Err(format!("{}: not a valid WASM binary", path.display()));
    }
    let mut pos = 8usize;
    let mut code_bytes = 0usize;
    let mut data_bytes = 0usize;
    let mut custom_bytes = 0usize;

    while pos < bytes.len() {
        let id = *bytes.get(pos).ok_or_else(|| "unexpected EOF reading section id".to_string())?;
        pos += 1;
        let len_u64 = read_leb_u64(bytes, &mut pos)?;
        let len = usize::try_from(len_u64).map_err(|_| "section too large for usize".to_string())?;
        let end = pos.checked_add(len).ok_or_else(|| "section offset overflow".to_string())?;
        if end > bytes.len() {
            return Err(format!("truncated WASM section id={id}"));
        }
        match id {
            0 => custom_bytes = custom_bytes.saturating_add(len),
            10 => code_bytes = code_bytes.saturating_add(len),
            11 => data_bytes = data_bytes.saturating_add(len),
            _ => {}
        }
        pos = end;
    }

    let mut found_forbidden = Vec::new();
    for &pat in FORBIDDEN_SYMBOLS {
        if bytes.windows(pat.len()).any(|w| w == pat.as_bytes()) {
            found_forbidden.push(pat.to_string());
        }
    }

    Ok(WasmReport { path, total_bytes: bytes.len(), gzip_bytes, code_bytes, data_bytes, custom_bytes, found_forbidden })
}

pub fn check_budget(r: &WasmReport) -> Vec<String> {
    let mut errs = Vec::new();
    if r.total_bytes > MAX_UNCOMPRESSED_BYTES {
        errs.push(format!("uncompressed .wasm {} bytes > ceiling {} bytes", r.total_bytes, MAX_UNCOMPRESSED_BYTES));
    }
    if r.gzip_bytes > MAX_GZIPPED_BYTES {
        errs.push(format!("gzipped .wasm.gz {} bytes > ceiling {} bytes", r.gzip_bytes, MAX_GZIPPED_BYTES));
    }
    if r.data_bytes > MAX_DATA_SECTION_BYTES {
        errs.push(format!("WASM Data section (11) {} bytes > ceiling {} bytes", r.data_bytes, MAX_DATA_SECTION_BYTES));
    }
    if !r.found_forbidden.is_empty() {
        errs.push(format!("forbidden offloaded symbols found in core .wasm: {}", r.found_forbidden.join(", ")));
    }
    errs
}

pub fn find_core_wasm(dist_web: &Path) -> Result<PathBuf, String> {
    let entries = std::fs::read_dir(dist_web).map_err(|e| format!("{}: {e}", dist_web.display()))?;
    let mut matches: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if let Some(name) = p.file_name().and_then(|n| n.to_str())
            && name.starts_with("wordcraft-web")
            && name.ends_with("_bg.wasm")
        {
            matches.push(p);
        }
    }
    match matches.len() {
        0 => Err(format!("no wordcraft-web*_bg.wasm found in {}", dist_web.display())),
        1 => matches.pop().ok_or_else(|| format!("no wordcraft-web*_bg.wasm found in {}", dist_web.display())),
        _ => {
            matches.sort();
            let names: Vec<String> = matches.iter().map(|p| p.display().to_string()).collect();
            Err(format!(
                "ambiguous: {} wordcraft-web*_bg.wasm files in {} ({}); clean the directory and rebuild",
                matches.len(),
                dist_web.display(),
                names.join(", ")
            ))
        }
    }
}

/// Crates that must not be in the web target's dependency graph. The byte scan for
/// [`FORBIDDEN_SYMBOLS`] is only supplementary: `wasm-release` strips symbols, so literals may
/// not survive, and Rust v0 mangling would not contain the `crate::` form anyway.
pub const FORBIDDEN_CRATES: &[&str] = &["krilla", "wordcraft-docbin", "wgpu", "naga"];

/// Forbidden crate names found in `cargo tree --prefix none --format {p}` output.
pub fn forbidden_crates_in_tree(tree: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for line in tree.lines() {
        let Some(name) = line.split_whitespace().next() else { continue };
        if FORBIDDEN_CRATES.contains(&name) && !found.iter().any(|f| f == name) {
            found.push(name.to_string());
        }
    }
    found
}

fn wasm_dependency_tree(root: &Path) -> Result<String, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let out = Command::new(cargo)
        .current_dir(root)
        .args(["tree", "-p", "wordcraft-web", "--target", "wasm32-unknown-unknown", "-e", "normal"])
        .args(["--prefix", "none", "--format", "{p}"])
        .output()
        .map_err(|e| format!("cargo tree failed: {e}"))?;
    if !out.status.success() {
        return Err(format!("cargo tree exited with {}: {}", out.status, String::from_utf8_lossy(&out.stderr)));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("cargo tree output is not UTF-8: {e}"))
}

fn sidecar_matches_wasm(gz_path: &Path, wasm_bytes: &[u8]) -> bool {
    Command::new("gzip").args(["-d", "-c"]).arg(gz_path).output().is_ok_and(|out| out.status.success() && out.stdout == wasm_bytes)
}

fn gzip_size(path: &Path, wasm_bytes: &[u8]) -> Result<usize, String> {
    let gz_path = PathBuf::from(format!("{}.gz", path.display()));
    if gz_path.is_file()
        && let Ok(meta) = std::fs::metadata(&gz_path)
        && sidecar_matches_wasm(&gz_path, wasm_bytes)
    {
        return usize::try_from(meta.len()).map_err(|_| "gzip file too large".to_string());
    }
    let out = Command::new("gzip").args(["-9", "-c"]).arg(path).output().map_err(|e| format!("gzip failed: {e}"))?;
    if !out.status.success() {
        return Err(format!("gzip exited with {}", out.status));
    }
    Ok(out.stdout.len())
}

fn extract_attr<'a>(tag: &'a str, attr: &str) -> Option<&'a str> {
    for quote in ['"', '\''] {
        let needle = format!("{attr}={quote}");
        if let Some(idx) = tag.find(&needle) {
            let after = tag.get(idx + needle.len()..)?;
            let end = after.find(quote)?;
            return after.get(..end);
        }
    }
    None
}

fn sha384_integrity(bytes: &[u8]) -> String {
    let digest = Sha384::digest(bytes);
    format!("sha384-{}", base64::engine::general_purpose::STANDARD.encode(digest))
}

/// Recompute SHA-384 for every `integrity="sha384-..."` tag in `index.html` and verify it matches
/// the bytes on disk under `dist_web`.
pub fn verify_sri_digests(dist_web: &Path) -> Result<usize, String> {
    let index_path = dist_web.join("index.html");
    let html = std::fs::read_to_string(&index_path).map_err(|e| format!("{}: {e}", index_path.display()))?;
    let mut checked = 0usize;
    let mut errs = Vec::new();
    let mut cursor = html.as_str();
    while let Some(start) = cursor.find('<') {
        let rest = cursor.get(start..).unwrap_or("");
        let Some(end) = rest.find('>') else { break };
        let tag = rest.get(..=end).unwrap_or("");
        cursor = rest.get(end + 1..).unwrap_or("");
        if let Some(expected) = extract_attr(tag, "integrity") {
            let Some(raw_url) = extract_attr(tag, "href").or_else(|| extract_attr(tag, "src")) else {
                errs.push(format!("integrity attribute without href/src in tag: {tag}"));
                continue;
            };
            let trimmed = raw_url.split(['?', '#']).next().unwrap_or(raw_url).trim_start_matches("./").trim_start_matches('/');
            let file_path = dist_web.join(trimmed);
            match std::fs::read(&file_path) {
                Ok(bytes) => {
                    let actual = sha384_integrity(&bytes);
                    checked += 1;
                    if actual != expected {
                        errs.push(format!("SRI mismatch for {trimmed}: index.html declares {expected}, actual file is {actual}"));
                    }
                }
                Err(e) => errs.push(format!("SRI target missing ({trimmed}): {e}")),
            }
        }
    }
    if errs.is_empty() { Ok(checked) } else { Err(errs.join("; ")) }
}

fn check_static_sizes_in(dir: &Path, root: &Path, errs: &mut Vec<String>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let p = entry.path();
        if p.is_dir() {
            check_static_sizes_in(&p, root, errs)?;
        } else if p.is_file() {
            let meta = std::fs::metadata(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            let size = usize::try_from(meta.len()).unwrap_or(usize::MAX);
            if size > CLOUDFLARE_MAX_FILE_BYTES {
                let rel = p.strip_prefix(root).unwrap_or(&p);
                errs.push(format!("static file {} is {} bytes > 25 MiB platform limit ({} bytes)", rel.display(), size, CLOUDFLARE_MAX_FILE_BYTES));
            }
        }
    }
    Ok(())
}

/// Verify that `dist_web/i18n/` contains `manifest.json` and `common.tsv` + `wordcraft.tsv` for every
/// offered non-English locale in `crates/ui-egui/src/i18n.rs`. When `require_populated` is true
/// (`CRAFT_I18N_REQUIRED=1`), every locale in `manifest.json` must also have `populated == true`.
pub fn verify_i18n_dist(root: &Path, dist_web: &Path, require_populated: bool) -> Result<usize, String> {
    let i18n_dir = dist_web.join("i18n");
    let manifest_path = i18n_dir.join("manifest.json");
    let manifest_bytes = std::fs::read(&manifest_path).map_err(|e| format!("missing {}: {e}", manifest_path.display()))?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).map_err(|e| format!("invalid {}: {e}", manifest_path.display()))?;
    let locales_obj =
        manifest.get("locales").and_then(|v| v.as_object()).ok_or_else(|| format!("{}: missing `locales` object", manifest_path.display()))?;

    let offered = crate::web_assets::offered_non_english_locales(root)?;
    let mut errs = Vec::new();
    for code in &offered {
        let loc_dir = i18n_dir.join(code);
        for file in ["common.tsv", "wordcraft.tsv"] {
            let p = loc_dir.join(file);
            if !p.is_file() {
                errs.push(format!("missing i18n/{code}/{file}"));
            }
        }
        match locales_obj.get(code) {
            None => errs.push(format!("manifest.json missing locale `{code}`")),
            Some(entry) if require_populated && entry.get("populated").and_then(|v| v.as_bool()) != Some(true) => {
                errs.push(format!("locale `{code}` has populated != true under CRAFT_I18N_REQUIRED=1"));
            }
            Some(_) => {}
        }
    }
    if errs.is_empty() { Ok(offered.len()) } else { Err(errs.join("; ")) }
}

/// Verify no file in `dist_web` exceeds Cloudflare's 25 MiB single-file static asset limit.
pub fn check_static_file_sizes(dist_web: &Path) -> Result<(), String> {
    let mut errs = Vec::new();
    check_static_sizes_in(dist_web, dist_web, &mut errs)?;
    if errs.is_empty() { Ok(()) } else { Err(errs.join("; ")) }
}

/// The report line for the static-file-size check, printed like the SRI and i18n results so a
/// violation is visible even when `--report-only` still returns success.
fn static_size_report_line(res: &Result<(), String>) -> String {
    match res {
        Ok(()) => format!("  static sizes: every file is within the {}-byte platform limit", CLOUDFLARE_MAX_FILE_BYTES),
        Err(e) => format!("  static sizes: ERROR ({e})"),
    }
}

pub fn run(root: &Path, report_only: bool) -> Result<(), String> {
    let dist_web = root.join("dist/web");
    let wasm_path = find_core_wasm(&dist_web)?;
    let bytes = std::fs::read(&wasm_path).map_err(|e| format!("{}: {e}", wasm_path.display()))?;
    let gz = gzip_size(&wasm_path, &bytes)?;
    let report = analyze_wasm_bytes(wasm_path, &bytes, gz)?;

    println!("WordCraft Web WASM Budget Report ({}):", report.path.display());
    println!("  uncompressed: {:>10} bytes (budget {:>10})", report.total_bytes, MAX_UNCOMPRESSED_BYTES);
    println!("  gzip -9:      {:>10} bytes (budget {:>10})", report.gzip_bytes, MAX_GZIPPED_BYTES);
    println!("  code (sec10): {:>10} bytes", report.code_bytes);
    println!("  data (sec11): {:>10} bytes (budget {:>10})", report.data_bytes, MAX_DATA_SECTION_BYTES);
    println!("  custom (sec0):{:>10} bytes", report.custom_bytes);
    if !report.found_forbidden.is_empty() {
        println!("  forbidden symbols: {}", report.found_forbidden.join(", "));
    }
    let sri_res = verify_sri_digests(&dist_web);
    match &sri_res {
        Ok(n) => println!("  SRI digests:  {n} verified cleanly in index.html"),
        Err(e) => println!("  SRI digests:  ERROR ({e})"),
    }
    let require_populated = std::env::var("CRAFT_I18N_REQUIRED").is_ok_and(|v| v.trim() == "1");
    let i18n_res = verify_i18n_dist(root, &dist_web, require_populated);
    match &i18n_res {
        Ok(n) => println!("  i18n assets:  {n} non-English locale bundles + manifest.json present"),
        Err(e) => println!("  i18n assets:  ERROR ({e})"),
    }
    let static_size_res = check_static_file_sizes(&dist_web);
    println!("{}", static_size_report_line(&static_size_res));
    let forbidden_crates = forbidden_crates_in_tree(&wasm_dependency_tree(root)?);
    if !forbidden_crates.is_empty() {
        println!("  forbidden crates in wasm dependency graph: {}", forbidden_crates.join(", "));
    }

    if report_only {
        return Ok(());
    }
    let mut errs = check_budget(&report);
    if let Err(e) = sri_res {
        errs.push(e);
    }
    if let Err(e) = i18n_res {
        errs.push(e);
    }
    if let Err(e) = static_size_res {
        errs.push(e);
    }
    if !forbidden_crates.is_empty() {
        errs.push(format!("forbidden crates in wordcraft-web wasm32 dependency graph: {}", forbidden_crates.join(", ")));
    }
    if errs.is_empty() {
        println!("OK: wordcraft-web satisfies all WASM budgets.");
        Ok(())
    } else {
        Err(errs.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_wasm(code_len: usize, data_len: usize, extra_custom: &[u8]) -> Vec<u8> {
        let mut out = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
        let mut push_sec = |id: u8, payload: &[u8]| {
            out.push(id);
            let mut n = payload.len();
            loop {
                let mut byte = (n & 0x7f) as u8;
                n >>= 7;
                if n != 0 {
                    byte |= 0x80;
                }
                out.push(byte);
                if n == 0 {
                    break;
                }
            }
            out.extend_from_slice(payload);
        };
        push_sec(10, &vec![0u8; code_len]);
        push_sec(11, &vec![1u8; data_len]);
        if !extra_custom.is_empty() {
            push_sec(0, extra_custom);
        }
        out
    }

    #[test]
    fn parses_sections_and_passes_within_budget() {
        let bytes = minimal_wasm(1024, 512, b"safe_symbol");
        let rep = analyze_wasm_bytes(PathBuf::from("test.wasm"), &bytes, 400).unwrap();
        assert_eq!(rep.code_bytes, 1024);
        assert_eq!(rep.data_bytes, 512);
        assert!(rep.found_forbidden.is_empty());
        assert!(check_budget(&rep).is_empty());
    }

    #[test]
    fn fails_when_data_or_gzip_or_symbols_exceed_budget() {
        let bytes = minimal_wasm(1024, MAX_DATA_SECTION_BYTES + 1, b"krilla::Page");
        let rep = analyze_wasm_bytes(PathBuf::from("big.wasm"), &bytes, MAX_GZIPPED_BYTES + 1).unwrap();
        let errs = check_budget(&rep);
        assert_eq!(errs.len(), 3, "{errs:?}");
    }

    #[test]
    fn find_core_wasm_rejects_ambiguous_and_missing() {
        let dir = std::env::temp_dir().join(format!("wc_wasm_find_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(find_core_wasm(&dir).unwrap_err().contains("no wordcraft-web"));
        let a = dir.join("wordcraft-web-aaa_bg.wasm");
        std::fs::write(&a, b"a").unwrap();
        assert_eq!(find_core_wasm(&dir).unwrap(), a);
        std::fs::write(dir.join("wordcraft-web-bbb_bg.wasm"), b"b").unwrap();
        assert!(find_core_wasm(&dir).unwrap_err().contains("ambiguous"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_static_file_is_reported() {
        let dir = std::env::temp_dir().join(format!("wc_wasm_static_size_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::File::create(dir.join("oversized.bin")).unwrap().set_len((CLOUDFLARE_MAX_FILE_BYTES + 1) as u64).unwrap();
        let line = static_size_report_line(&check_static_file_sizes(&dir));
        assert!(line.contains("ERROR"), "{line}");
        assert!(line.contains("oversized.bin"), "{line}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forbidden_crates_detected_in_dependency_tree() {
        let clean = "wordcraft-web v0.1.0 (/x)\nwordcraft-engine v0.1.0 (/x)\nkrilla-svg v0.1.0\n";
        assert!(forbidden_crates_in_tree(clean).is_empty());
        let dirty = "wordcraft-web v0.1.0 (/x)\nkrilla v0.4.0 (https://x)\nwordcraft-docbin v0.1.0 (/x)\nwgpu v24.0.0\nnaga v24.0.0\nkrilla v0.4.0 (https://x) (*)\n";
        assert_eq!(
            forbidden_crates_in_tree(dirty),
            vec!["krilla".to_string(), "wordcraft-docbin".to_string(), "wgpu".to_string(), "naga".to_string()]
        );
    }

    #[test]
    fn gzip_size_ignores_stale_sidecar_and_uses_valid_sidecar() {
        let dir = std::env::temp_dir().join(format!("wc_wasm_budget_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wasm_path = dir.join("wordcraft-web-test_bg.wasm");
        let gz_path = dir.join("wordcraft-web-test_bg.wasm.gz");

        let selected = minimal_wasm(4096, 2048, b"selected_artifact");
        std::fs::write(&wasm_path, &selected).unwrap();

        // Write a stale .gz sidecar compressed from a different payload.
        let stale_path = dir.join("stale.wasm");
        std::fs::write(&stale_path, b"stale payload with different contents").unwrap();
        let stale_gz = Command::new("gzip").args(["-9", "-c"]).arg(&stale_path).output().unwrap();
        assert!(stale_gz.status.success());
        std::fs::write(&gz_path, &stale_gz.stdout).unwrap();

        // Fresh compression size for the selected artifact.
        let expected_gz = Command::new("gzip").args(["-9", "-c"]).arg(&wasm_path).output().unwrap();
        assert!(expected_gz.status.success());

        // Stale sidecar must be ignored in favor of the selected artifact's compressed size.
        let measured_stale = gzip_size(&wasm_path, &selected).unwrap();
        assert_eq!(measured_stale, expected_gz.stdout.len());
        assert_ne!(measured_stale, stale_gz.stdout.len());

        // Write a matching sidecar compressed with `-1` so its size differs from `-9`, proving
        // the verified sidecar is actually used when it matches `selected`.
        let valid_sidecar = Command::new("gzip").args(["-1", "-c"]).arg(&wasm_path).output().unwrap();
        assert!(valid_sidecar.status.success());
        std::fs::write(&gz_path, &valid_sidecar.stdout).unwrap();

        let measured_valid = gzip_size(&wasm_path, &selected).unwrap();
        assert_eq!(measured_valid, valid_sidecar.stdout.len());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_sri_digests_accepts_matching_and_rejects_mismatched() {
        let dir = std::env::temp_dir().join(format!("wc_sri_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wasm_bytes = b"\0asm\x01\0\0\0demo";
        std::fs::write(dir.join("app_bg.wasm"), wasm_bytes).unwrap();
        let good = sha384_integrity(wasm_bytes);
        std::fs::write(dir.join("index.html"), format!(r#"<link rel="preload" href="./app_bg.wasm" integrity="{good}">"#)).unwrap();
        assert_eq!(verify_sri_digests(&dir).unwrap(), 1);

        std::fs::write(
            dir.join("index.html"),
            r#"<link rel="preload" href="./app_bg.wasm" integrity="sha384-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA">"#,
        )
        .unwrap();
        let err = verify_sri_digests(&dir).unwrap_err();
        assert!(err.contains("SRI mismatch for app_bg.wasm"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
