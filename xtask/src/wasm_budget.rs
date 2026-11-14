//! `cargo xtask wasm-budget`: inspect `dist/web/wordcraft-web*_bg.wasm` and every
//! `dist/web/wordcraft-worker-*_bg.wasm`, report section sizes and the gzip/Brotli sizes, and
//! enforce size + symbol ceilings per artifact and for everything loaded together.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use sha2::{Digest as _, Sha384};

/// Main-module ceilings ratchet to measured size plus a small headroom (plan §3.5); lower them
/// whenever the artifact shrinks. Ratcheted 2026-11-14 (S1) on an optimized release build
/// (`trunk build --release`, so `wasm-opt` ran): 17,749,525 measured → 17.9 MB. The 2026-11-13
/// ratchet to 19.5 MB had measured an artifact `wasm-opt` had not run on (a plain `trunk build`
/// skips it outside release mode), which the `name`-section guard below now catches.
pub const MAX_UNCOMPRESSED_BYTES: usize = 17_900_000;
pub const MAX_GZIPPED_BYTES: usize = 7_100_000;
pub const MAX_DATA_SECTION_BYTES: usize = 2_750_000;
pub const CLOUDFLARE_MAX_FILE_BYTES: usize = 25 * 1024 * 1024;

/// "Everything loaded" ceilings (plan §3.5): the main module plus every dedicated worker a
/// session may fetch, raw and gzipped. Each worker links its own copy of std, alloc and the
/// document model, so the total grows as workers land even while the main module shrinks —
/// this is the row that catches a sum that quietly drifts past the platform's 25 MiB file
/// limit (still enforced per file by [`check_static_file_sizes`]) or the story the size work
/// tells. Ratcheted 2026-11-14 from the release build: 17,749,525 + 1,015,087 = 18,764,612
/// raw and 6,973,466 + 637,274 = 7,610,740 gzip measured.
pub const MAX_TOTAL_UNCOMPRESSED_BYTES: usize = 19_000_000;
pub const MAX_TOTAL_GZIPPED_BYTES: usize = 7_750_000;

/// One dedicated worker's ceilings, at the artifact's measured size plus headroom (plan §3.5:
/// workers get their own rows, not a shared provisional number). A worker artifact with no
/// row fails the budget — add one from a real build's measurements, never by copying another
/// worker's numbers. Ceilings ratchet down; raising one is a recorded decision in the plan.
pub struct WorkerCeiling {
    /// The artifact's file stem (`wordcraft-worker-proof` for `wordcraft-worker-proof_bg.wasm`).
    pub name: &'static str,
    pub max_raw: usize,
    pub max_gzip: usize,
}

/// Measured 2026-11-14 on the release build (`trunk build --release`), each at its size plus
/// headroom: proof 1,015,087 raw / 637,274 gzip; codecs 4,212,970 / 1,781,789; pdf 5,420,578 /
/// 2,537,711.
pub const WORKER_CEILINGS: &[WorkerCeiling] = &[
    WorkerCeiling { name: "wordcraft-worker-proof", max_raw: 1_100_000, max_gzip: 700_000 },
    WorkerCeiling { name: "wordcraft-worker-codecs", max_raw: 4_400_000, max_gzip: 1_900_000 },
    WorkerCeiling { name: "wordcraft-worker-pdf", max_raw: 5_600_000, max_gzip: 2_650_000 },
];

/// Offloaded symbols that must not appear in the *main module's* bytes: `krilla` (PDF export
/// lives in the PDF worker, §3.4) and `wordcraft-docbin` (legacy `.doc` lives in the codec
/// worker, §3.3). Workers get their own lists by [`forbidden_symbols_for`] — the crate a
/// worker exists to host is expected there, the other stays forbidden.
pub const FORBIDDEN_SYMBOLS: &[&str] = &["krilla::", "wordcraft_docbin::"];

/// The byte-scan list for one artifact, by worker stem (empty stem = the main module). The
/// codec worker links `wordcraft-docbin` by design; the PDF worker links `krilla` by design.
fn forbidden_symbols_for(worker_stem: &str) -> &[&str] {
    match worker_stem {
        "wordcraft-worker-codecs" => &["krilla::"],
        "wordcraft-worker-pdf" => &["wordcraft_docbin::"],
        _ => FORBIDDEN_SYMBOLS,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmReport {
    pub path: PathBuf,
    pub total_bytes: usize,
    pub gzip_bytes: usize,
    /// Brotli quality 11, lgwin 24 — the `.wasm.br` sidecar size when one exists and matches
    /// (plan §2.2), else a fresh compression. Reported for every artifact (plan §3.5); the
    /// ceilings stay on raw and gzip because Brotli quality 11 takes ~50 s on the main module
    /// and a budget gate must stay runnable.
    pub brotli_bytes: usize,
    pub code_bytes: usize,
    pub data_bytes: usize,
    pub custom_bytes: usize,
    pub found_forbidden: Vec<String>,
    /// Whether the module carries a `name` custom section. Guard (plan S1): Trunk runs
    /// `wasm-opt` only in release mode, and `wasm-opt` drops the `name` section, so a module
    /// that still has one was built unoptimized — it must not be measured against a ratcheted
    /// ceiling (which would then fit only unoptimized builds) or shipped.
    pub has_name_section: bool,
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

/// Parse WASM sections and collect code/data/custom sizes and forbidden symbol markers,
/// scanning for `forbidden` (see [`forbidden_symbols_for`]).
pub fn analyze_wasm_bytes_with(path: PathBuf, bytes: &[u8], gzip_bytes: usize, forbidden: &[&str]) -> Result<WasmReport, String> {
    if bytes.len() < 8 || bytes.get(..8) != Some(&[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]) {
        return Err(format!("{}: not a valid WASM binary", path.display()));
    }
    let mut pos = 8usize;
    let mut code_bytes = 0usize;
    let mut data_bytes = 0usize;
    let mut custom_bytes = 0usize;
    let mut has_name_section = false;

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
            0 => {
                custom_bytes = custom_bytes.saturating_add(len);
                // A custom section is `name_len name contents…`; a `name` section means the
                // module skipped `wasm-opt` (see `WasmReport::has_name_section`). The LEB read
                // starts inside the section (`pos < end`) and the name must fit inside it, so an
                // empty or truncated custom section can never read the next section's bytes.
                if pos < end
                    && let Ok(name_len) = read_leb_u64(bytes, &mut pos)
                    && end.checked_sub(pos).is_some_and(|avail| avail >= name_len as usize)
                    && bytes.get(pos..pos + name_len as usize) == Some(b"name")
                {
                    has_name_section = true;
                }
            }
            10 => code_bytes = code_bytes.saturating_add(len),
            11 => data_bytes = data_bytes.saturating_add(len),
            _ => {}
        }
        pos = end;
    }

    let mut found_forbidden = Vec::new();
    for &pat in forbidden {
        if bytes.windows(pat.len()).any(|w| w == pat.as_bytes()) {
            found_forbidden.push(pat.to_string());
        }
    }

    Ok(WasmReport {
        path,
        total_bytes: bytes.len(),
        gzip_bytes,
        brotli_bytes: 0,
        code_bytes,
        data_bytes,
        custom_bytes,
        found_forbidden,
        has_name_section,
    })
}

/// [`analyze_wasm_bytes_with`] with the main module's list ([`FORBIDDEN_SYMBOLS`]).
pub fn analyze_wasm_bytes(path: PathBuf, bytes: &[u8], gzip_bytes: usize) -> Result<WasmReport, String> {
    analyze_wasm_bytes_with(path, bytes, gzip_bytes, FORBIDDEN_SYMBOLS)
}

impl WasmReport {
    /// Fill in the Brotli column (plan §3.5); measured beside the report, not inside the
    /// section walk.
    pub fn with_brotli(mut self, brotli_bytes: usize) -> WasmReport {
        self.brotli_bytes = brotli_bytes;
        self
    }
}

pub fn check_budget(r: &WasmReport) -> Vec<String> {
    let mut errs = Vec::new();
    if r.has_name_section {
        errs.push(String::from("module carries a `name` custom section: `wasm-opt` did not run (build with `trunk build --release`; Trunk only optimizes release builds), so this artifact is unoptimized and cannot be measured or shipped"));
    }
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

/// Every `data-type="worker"` artifact in `dist_web` (`wordcraft-worker-*_bg.wasm`; Trunk
/// keeps worker names un-hashed). The main module's `wordcraft-web*_bg.wasm` is matched by
/// [`find_core_wasm`] and never returned here.
pub fn find_worker_wasms(dist_web: &Path) -> Vec<PathBuf> {
    let mut matches: Vec<PathBuf> = std::fs::read_dir(dist_web)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("wordcraft-worker-") && n.ends_with("_bg.wasm")))
        .collect();
    matches.sort();
    matches
}

/// The `wordcraft-worker-<name>_bg.wasm` stem a [`WorkerCeiling`] is looked up by.
fn worker_stem(path: &Path) -> String {
    path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix("_bg.wasm")).unwrap_or_default().to_string()
}

/// One row of the worker section of the budget report (plan §3.5's per-artifact table:
/// raw, gzip and Brotli, against that worker's own measured ceiling; the per-file platform
/// ceiling is enforced by [`check_static_file_sizes`]).
fn worker_report_line(r: &WasmReport) -> String {
    let stem = worker_stem(&r.path);
    let (raw_budget, gz_budget) = WORKER_CEILINGS
        .iter()
        .find(|c| c.name == stem)
        .map(|c| (format!("{}", c.max_raw), format!("{}", c.max_gzip)))
        .unwrap_or_else(|| ("UNREGISTERED".into(), "UNREGISTERED".into()));
    format!(
        "  worker {stem}: uncompressed {:>10} bytes (budget {:>10}), gzip -9 {:>10} bytes (budget {:>10}), brotli q11 {:>10} bytes",
        r.total_bytes, raw_budget, r.gzip_bytes, gz_budget, r.brotli_bytes
    )
}

fn check_worker_budget(r: &WasmReport) -> Vec<String> {
    let stem = worker_stem(&r.path);
    let Some(ceiling) = WORKER_CEILINGS.iter().find(|c| c.name == stem) else {
        return vec![format!(
            "worker {stem} has no ceiling in WORKER_CEILINGS: measure it on a release build and add a row at measured size plus headroom (a shared or guessed number hides growth)"
        )];
    };
    let mut errs = Vec::new();
    if r.has_name_section {
        errs.push(format!("worker {stem} carries a `name` custom section: `wasm-opt` did not run (build with `trunk build --release`), so this artifact is unoptimized and cannot be measured or shipped"));
    }
    if r.total_bytes > ceiling.max_raw {
        errs.push(format!("worker {stem} uncompressed {} bytes > ceiling {} bytes", r.total_bytes, ceiling.max_raw));
    }
    if r.gzip_bytes > ceiling.max_gzip {
        errs.push(format!("worker {stem} gzipped {} bytes > ceiling {} bytes", r.gzip_bytes, ceiling.max_gzip));
    }
    if !r.found_forbidden.is_empty() {
        errs.push(format!("forbidden offloaded symbols found in worker {stem}: {}", r.found_forbidden.join(", ")));
    }
    errs
}

/// The "everything loaded" totals (plan §3.5): the main module plus every worker, raw and
/// gzipped, against [`MAX_TOTAL_UNCOMPRESSED_BYTES`] / [`MAX_TOTAL_GZIPPED_BYTES`].
fn check_total_budget(main: &WasmReport, workers: &[WasmReport]) -> Vec<String> {
    let raw: usize = std::iter::once(main.total_bytes).chain(workers.iter().map(|w| w.total_bytes)).fold(0usize, usize::saturating_add);
    let gzip: usize = std::iter::once(main.gzip_bytes).chain(workers.iter().map(|w| w.gzip_bytes)).fold(0usize, usize::saturating_add);
    let mut errs = Vec::new();
    if raw > MAX_TOTAL_UNCOMPRESSED_BYTES {
        errs.push(format!("everything loaded: {raw} uncompressed bytes (main + workers) > ceiling {MAX_TOTAL_UNCOMPRESSED_BYTES}"));
    }
    if gzip > MAX_TOTAL_GZIPPED_BYTES {
        errs.push(format!("everything loaded: {gzip} gzipped bytes (main + workers) > ceiling {MAX_TOTAL_GZIPPED_BYTES}"));
    }
    errs
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

/// Whether the Brotli sidecar at `br_path` decompresses to exactly `wasm_bytes`.
fn br_sidecar_matches_wasm(br_path: &Path, wasm_bytes: &[u8]) -> bool {
    let Ok(file) = std::fs::File::open(br_path) else { return false };
    let mut reader = brotli::Decompressor::new(file, 1 << 16);
    let mut out = Vec::with_capacity(wasm_bytes.len());
    reader.read_to_end(&mut out).is_ok() && out == wasm_bytes
}

/// Brotli quality 11, lgwin 24 — the same numbers `xtask/src/brotli.rs` writes sidecars with
/// (plan §2.2). A matching `<path>.br` sidecar answers directly; otherwise the artifact is
/// compressed in-process (slow at this quality on the main module, but the sidecar is what a
/// release build leaves behind and the number must not be a lie).
fn brotli_size(path: &Path, wasm_bytes: &[u8]) -> Result<usize, String> {
    let br_path = PathBuf::from(format!("{}.br", path.display()));
    if br_path.is_file()
        && let Ok(meta) = std::fs::metadata(&br_path)
        && br_sidecar_matches_wasm(&br_path, wasm_bytes)
    {
        return usize::try_from(meta.len()).map_err(|_| "brotli file too large".to_string());
    }
    let params = brotli::enc::BrotliEncoderParams { quality: 11, lgwin: 24, ..brotli::enc::BrotliEncoderParams::default() };
    let mut out = Vec::new();
    brotli::BrotliCompress(&mut &wasm_bytes[..], &mut out, &params).map_err(|e| format!("brotli compress: {e}"))?;
    Ok(out.len())
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
    let br = brotli_size(&wasm_path, &bytes)?;
    let report = analyze_wasm_bytes(wasm_path, &bytes, gz)?.with_brotli(br);

    println!("WordCraft Web WASM Budget Report ({}):", report.path.display());
    if report.has_name_section {
        println!("  WARNING: module carries a `name` custom section (`wasm-opt` did not run)");
    }
    println!("  uncompressed: {:>10} bytes (budget {:>10})", report.total_bytes, MAX_UNCOMPRESSED_BYTES);
    println!("  gzip -9:      {:>10} bytes (budget {:>10})", report.gzip_bytes, MAX_GZIPPED_BYTES);
    println!("  brotli q11:   {:>10} bytes (sidecar size when it matches; report column, no ceiling)", report.brotli_bytes);
    println!("  code (sec10): {:>10} bytes", report.code_bytes);
    println!("  data (sec11): {:>10} bytes (budget {:>10})", report.data_bytes, MAX_DATA_SECTION_BYTES);
    println!("  custom (sec0):{:>10} bytes", report.custom_bytes);
    if !report.found_forbidden.is_empty() {
        println!("  forbidden symbols: {}", report.found_forbidden.join(", "));
    }

    // Per-artifact rows for the dedicated workers (plan §3.5). A dist built without the
    // worker link simply reports none.
    let worker_wasms = find_worker_wasms(&dist_web);
    let mut worker_reports = Vec::new();
    for path in &worker_wasms {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let gz = gzip_size(path, &bytes)?;
        let br = brotli_size(path, &bytes)?;
        worker_reports.push(analyze_wasm_bytes_with(path.clone(), &bytes, gz, forbidden_symbols_for(&worker_stem(path)))?.with_brotli(br));
    }
    if worker_reports.is_empty() {
        println!("  workers: no wordcraft-worker-*_bg.wasm in {}", dist_web.display());
    }
    for r in &worker_reports {
        println!("{}", worker_report_line(r));
    }
    let total_raw: usize =
        std::iter::once(report.total_bytes).chain(worker_reports.iter().map(|w| w.total_bytes)).fold(0usize, usize::saturating_add);
    let total_gz: usize = std::iter::once(report.gzip_bytes).chain(worker_reports.iter().map(|w| w.gzip_bytes)).fold(0usize, usize::saturating_add);
    println!(
        "  everything loaded: {total_raw:>10} bytes raw (budget {MAX_TOTAL_UNCOMPRESSED_BYTES:>10}), {total_gz:>10} gzip (budget {MAX_TOTAL_GZIPPED_BYTES:>10})"
    );

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
    for r in &worker_reports {
        errs.extend(check_worker_budget(r));
    }
    errs.extend(check_total_budget(&report, &worker_reports));
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
    fn find_worker_wasms_picks_only_worker_artifacts() {
        let dir = std::env::temp_dir().join(format!("wc_wasm_workers_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("wordcraft-web-abc_bg.wasm"), b"a").unwrap();
        std::fs::write(dir.join("wordcraft-worker-proof_bg.wasm"), b"b").unwrap();
        std::fs::write(dir.join("wordcraft-worker-codecs_bg.wasm"), b"c").unwrap();
        std::fs::write(dir.join("wordcraft-worker-proof.js"), b"d").unwrap();
        let found = find_worker_wasms(&dir);
        let names: Vec<String> = found.iter().filter_map(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).collect();
        assert_eq!(names, vec!["wordcraft-worker-codecs_bg.wasm", "wordcraft-worker-proof_bg.wasm"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worker_budget_flags_over_limit_and_forbidden() {
        let ceiling = WORKER_CEILINGS.iter().find(|c| c.name == "wordcraft-worker-proof").expect("the proofing worker is registered");
        let bytes = minimal_wasm(1024, 512, b"krilla::Page");
        let rep = analyze_wasm_bytes(PathBuf::from("wordcraft-worker-proof_bg.wasm"), &bytes, ceiling.max_gzip + 1).unwrap();
        let errs = check_worker_budget(&rep);
        assert!(errs.iter().any(|e| e.contains("gzipped")) && errs.iter().any(|e| e.contains("forbidden")), "{errs:?}");
        // Over the raw ceiling: a padded module, since sizes come from the bytes.
        let padded = minimal_wasm(ceiling.max_raw + 1, 512, b"safe_symbol");
        let rep = analyze_wasm_bytes(PathBuf::from("wordcraft-worker-proof_bg.wasm"), &padded, 400).unwrap();
        let errs = check_worker_budget(&rep);
        assert!(errs.iter().any(|e| e.contains("uncompressed")), "{errs:?}");
        let ok = analyze_wasm_bytes(PathBuf::from("wordcraft-worker-proof_bg.wasm"), &minimal_wasm(1024, 512, b"safe_symbol"), 400).unwrap();
        assert!(check_worker_budget(&ok).is_empty());
    }

    #[test]
    fn unregistered_worker_and_totals_flag() {
        // A worker with no measured ceiling row is an error, not a pass: guessed or shared
        // numbers hide growth.
        let rep = analyze_wasm_bytes(PathBuf::from("wordcraft-worker-future_bg.wasm"), &minimal_wasm(1024, 512, b"safe_symbol"), 400).unwrap();
        let errs = check_worker_budget(&rep);
        assert!(errs.iter().any(|e| e.contains("no ceiling") && e.contains("wordcraft-worker-future")), "{errs:?}");

        // The everything-loaded total: main + workers against the total ceilings.
        let main = analyze_wasm_bytes(PathBuf::from("m.wasm"), &minimal_wasm(1000, 100, b"x"), 500).unwrap();
        let w = analyze_wasm_bytes(PathBuf::from("wordcraft-worker-proof_bg.wasm"), &minimal_wasm(1000, 100, b"x"), 500).unwrap();
        assert!(check_total_budget(&main, std::slice::from_ref(&w)).is_empty());
        let huge_main = WasmReport { total_bytes: MAX_TOTAL_UNCOMPRESSED_BYTES, gzip_bytes: MAX_TOTAL_GZIPPED_BYTES, ..main.clone() };
        let worker = WasmReport { total_bytes: 1, gzip_bytes: 1, ..w };
        let errs = check_total_budget(&huge_main, &[worker]);
        assert!(errs.iter().any(|e| e.contains("uncompressed")) && errs.iter().any(|e| e.contains("gzipped")), "{errs:?}");
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

    /// A custom section with a proper `name_len name contents` payload, the way a real module
    /// frames its `name`/`producers` sections.
    fn named_custom_section(name: &str) -> Vec<u8> {
        let mut payload = vec![u8::try_from(name.len()).unwrap_or(255)];
        payload.extend_from_slice(name.as_bytes());
        payload
    }

    #[test]
    fn name_section_marks_an_unoptimized_module_and_fails_the_budget() {
        // A `name` custom section (function names survive a cargo build that `wasm-opt` never
        // ran) fails the budget for the main module and for a worker alike.
        let with_name = minimal_wasm(1024, 512, &named_custom_section("name"));
        let rep = analyze_wasm_bytes(PathBuf::from("test.wasm"), &with_name, 400).unwrap();
        assert!(rep.has_name_section);
        let errs = check_budget(&rep);
        assert!(errs.iter().any(|e| e.contains("`name` custom section")), "{errs:?}");
        let rep = analyze_wasm_bytes(PathBuf::from("wordcraft-worker-proof_bg.wasm"), &with_name, 400).unwrap();
        assert!(check_worker_budget(&rep).iter().any(|e| e.contains("`name` custom section")));

        // The sections an optimized release build does keep (`producers` only) stay fine.
        let optimized = minimal_wasm(1024, 512, &named_custom_section("producers"));
        let rep = analyze_wasm_bytes(PathBuf::from("test.wasm"), &optimized, 400).unwrap();
        assert!(!rep.has_name_section);
        assert!(check_budget(&rep).is_empty(), "{:?}", check_budget(&rep));

        // The guard never reads past a hostile section: an empty custom section, and one whose
        // name length runs past the section end, are not `name` sections and never panic.
        let empty = minimal_wasm(1024, 512, &[]);
        let rep = analyze_wasm_bytes(PathBuf::from("test.wasm"), &empty, 400).unwrap();
        assert!(!rep.has_name_section);
        let mut overrun = named_custom_section("producers");
        overrun[0] = 200; // declares 200 name bytes where the section holds eight
        let rep = analyze_wasm_bytes(PathBuf::from("test.wasm"), &minimal_wasm(1024, 512, &overrun), 400).unwrap();
        assert!(!rep.has_name_section);

        // The raw-payload custom sections the older tests used (no framing) are not `name`
        // sections either — even when they literally contain the bytes "name".
        let unframed = minimal_wasm(1024, 512, b"name");
        let rep = analyze_wasm_bytes(PathBuf::from("test.wasm"), &unframed, 400).unwrap();
        assert!(!rep.has_name_section);
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
