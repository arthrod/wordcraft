//! `cargo xtask wasm-budget`: inspect `dist/web/wordcraft-web*_bg.wasm`, report section
//! sizes and compressed size, and enforce size + symbol ceilings.

use std::path::{Path, PathBuf};
use std::process::Command;

pub const MAX_UNCOMPRESSED_BYTES: usize = 7_500_000;
pub const MAX_GZIPPED_BYTES: usize = 2_750_000;
pub const MAX_DATA_SECTION_BYTES: usize = 1_200_000;

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

/// Read an unsigned LEB128 u64 starting at `*pos`, advancing `*pos`.
fn read_leb_u64(bytes: &[u8], pos: &mut usize) -> Result<u64, String> {
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
    matches.sort();
    matches.into_iter().next().ok_or_else(|| format!("no wordcraft-web*_bg.wasm found in {}", dist_web.display()))
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

    if report_only {
        return Ok(());
    }
    let errs = check_budget(&report);
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
}
