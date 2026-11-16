//! `cargo xtask wasm-budget`: inspect `dist/web/wordcraft-web*_bg.wasm` and every
//! `dist/web/wordcraft-worker-*_bg.wasm`, report section sizes and the gzip/Brotli sizes, and
//! enforce size + symbol ceilings per artifact and for the first load (plan 10.5).

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use sha2::{Digest as _, Sha384};

/// Main-module ceilings ratchet to measured size plus a small headroom (plan §3.5); lower them
/// whenever the artifact shrinks. Ratcheted 2026-11-14 on the SIMD release build
/// (`trunk build --release` with `-C target-feature=+simd128`, plan §4.2, so `wasm-opt` ran):
/// 16,289,048 measured → 16.5 MB raw / 6,750,000 gzip. This supersedes the same-day scalar
/// ratchet (17,749,525 → 17.9 MB); the 2026-11-13 19.5 MB ceiling had measured an artifact
/// `wasm-opt` had not run on (a plain `trunk build` skips it outside release mode), which the
/// `name`-section guard below now catches.
///
/// Re-set 2026-11-16 (plan 10.5) from 16,500,000 / 6,750,000 to the measured size plus about
/// 3% raw and 4% gzip: 17,695,535 raw / 7,205,429 gzip on the release build after the merge
/// of `main` (which is what grew it past the old ceiling) and the typed model bridge. The
/// editor is no longer on the first-load path (plan 10.3), so this ceiling watches the file a
/// reader fetches on the first edit action, not the first visit.
pub const MAX_UNCOMPRESSED_BYTES: usize = 18_227_000;
pub const MAX_GZIPPED_BYTES: usize = 7_494_000;
pub const MAX_DATA_SECTION_BYTES: usize = 2_750_000;
pub const CLOUDFLARE_MAX_FILE_BYTES: usize = 25 * 1024 * 1024;

/// The first-load ceiling (plan 10.5): the wasm a first visit fetches, which is the page
/// module ([`PAGE_STEM`]) plus the view-only worker ([`VIEWER_WORKER_STEM`]), raw and gzipped. Measured
/// 2026-11-16 on the release build: 6,051,105 raw / 2,876,521 gzip / 2,364,729 Brotli q11,
/// ceiling about 3% raw and 4% gzip above. Everything else is fetched on demand and has its
/// own row: the editor above, the workers in [`WORKER_CEILINGS`].
///
/// This replaces the "everything loaded" total (19,000,000 raw / 7,750,000 gzip, first cut
/// 2026-11-14), which summed the editor and every worker. No session loads them all at once
/// any more, and the sum had been over its ceiling since the third worker landed. The sum is
/// still printed, without a ceiling.
pub const MAX_FIRST_LOAD_UNCOMPRESSED_BYTES: usize = 6_233_000;
pub const MAX_FIRST_LOAD_GZIPPED_BYTES: usize = 2_992_000;

/// File-name stems the first-load page must not name outside a comment: the editor and the
/// three on-demand workers. `wordcraft-web` does not occur inside `wordcraft-viewer-web`.
pub const ON_DEMAND_STEMS: &[&str] = &["wordcraft-web", "wordcraft-worker-proof", "wordcraft-worker-codecs", "wordcraft-worker-pdf"];

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

/// Measured 2026-11-16 on the release build (plan 10.5), each at its size plus about 3% raw
/// and 4% gzip: proof 1,011,227 raw / 635,960 gzip (lowered from 1,100,000 / 700,000); codecs
/// 4,407,114 / 1,866,280 (from 4,300,000 / 1,850,000); pdf 6,444,918 / 3,000,608 (from
/// 5,550,000 / 2,625,000). Codecs and pdf grew with the merge of `main`; the typed model
/// bridge then took 2.3 MB and 1.0 MB raw back off them. All three load on first use, never
/// on a first visit. The view-only
/// worker (plan 10.3) re-measured 2026-11-16 after the typed model bridge, on the same kind of
/// build (no optional inputs): 5,845,030 raw / 2,784,071 gzip / 2,285,910 Brotli q11, ceilings
/// about 3% raw and 4% gzip above.
pub const WORKER_CEILINGS: &[WorkerCeiling] = &[
    WorkerCeiling { name: "wordcraft-worker-proof", max_raw: 1_042_000, max_gzip: 662_000 },
    WorkerCeiling { name: "wordcraft-worker-codecs", max_raw: 4_540_000, max_gzip: 1_941_000 },
    WorkerCeiling { name: "wordcraft-worker-pdf", max_raw: 6_639_000, max_gzip: 3_121_000 },
    WorkerCeiling { name: VIEWER_WORKER_STEM, max_raw: 6_021_000, max_gzip: 2_896_000 },
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
        VIEWER_WORKER_STEM => VIEWER_WORKER_FORBIDDEN_SYMBOLS,
        _ => FORBIDDEN_SYMBOLS,
    }
}

/// The view-only dedicated worker (plan 10.3): the view-only core behind the worker envelope,
/// fetched on a first visit. Its ceiling is a row in [`WORKER_CEILINGS`] like any worker's.
pub const VIEWER_WORKER_STEM: &str = "wordcraft-worker-viewer";

/// Crates the view-only worker must never link: it paints pages, so the editor and PDF export
/// stay out. Unlike the bare core ([`VIEWER_FORBIDDEN_CRATES`]) it is a browser worker and
/// links `web-sys` for its message port.
pub const VIEWER_WORKER_FORBIDDEN_CRATES: &[&str] = &["egui", "eframe", "wordcraft-engine", "wordcraft-ui-egui", "krilla"];

/// The byte-scan form of [`VIEWER_WORKER_FORBIDDEN_CRATES`] (supplementary; see
/// [`FORBIDDEN_SYMBOLS`]).
pub const VIEWER_WORKER_FORBIDDEN_SYMBOLS: &[&str] = &["egui::", "eframe::", "wordcraft_engine::", "wordcraft_ui_egui::", "krilla::"];

/// The viewer-first page module (plan 10.3): the page's first module, content-hashed by Trunk
/// (`wordcraft-viewer-web-<hash>_bg.wasm`). What a reader waits for before anything else, so it
/// has its own row.
pub const PAGE_STEM: &str = "wordcraft-viewer-web";

/// The page module's ceilings, at measured size plus about 3% raw and 4% gzip (the headroom
/// the workers got). Re-measured 2026-11-16, with the in-page password form and the bounded
/// editor wait, on the SIMD release build (`trunk build --release`, no optional inputs):
/// 206,075 raw / 92,450 gzip -9 / 78,819 Brotli q11.
pub const PAGE_CEILING: WorkerCeiling = WorkerCeiling { name: PAGE_STEM, max_raw: 212_300, max_gzip: 96_200 };

/// Crates the page module must never link: it is `web-sys` glue that starts the view-only
/// worker and loads the editor on an edit action. The editor, PDF export and the whole document
/// stack (which lives in the worker) stay out.
pub const PAGE_FORBIDDEN_CRATES: &[&str] = &[
    "egui",
    "eframe",
    "wordcraft-engine",
    "wordcraft-ui-egui",
    "krilla",
    "wordcraft-docx",
    "wordcraft-layout",
    "wordcraft-render",
    "wordcraft-viewer",
];

/// The byte-scan form of [`PAGE_FORBIDDEN_CRATES`] (supplementary; see [`FORBIDDEN_SYMBOLS`]).
pub const PAGE_FORBIDDEN_SYMBOLS: &[&str] =
    &["egui::", "eframe::", "wordcraft_engine::", "wordcraft_ui_egui::", "krilla::", "wordcraft_docx::", "wordcraft_layout::", "wordcraft_render::"];

/// The view-only module's artifact stem (`wordcraft-viewer_bg.wasm`; plan Phase 8). It is not a
/// dedicated worker and no Trunk link builds it, so it has its own row and its own run
/// (`cargo xtask wasm-budget --viewer`) instead of a place in [`WORKER_CEILINGS`].
pub const VIEWER_STEM: &str = "wordcraft-viewer";

/// The view-only module's ceilings, at measured size plus the headroom the multi-megabyte
/// workers got (about 3% raw, 4% gzip). Measured 2026-11-16 on the SIMD release build
/// (`cargo build -p wordcraft-viewer --bin wordcraft-viewer --target wasm32-unknown-unknown
/// --profile wasm-release`, `wasm-bindgen --target web --weak-refs`, then `wasm-opt -Oz` with
/// the flags `apps/wordcraft-web/index.html` gives the workers): 7,496,105 raw / 3,198,016
/// gzip / 2,537,321 Brotli q11. The first measurement (8,159,381 / 3,440,073) was of a `cdylib`
/// built beside an `rlib`, which cargo builds without link-time optimisation; the module is a
/// bin since. Measured again 2026-11-16 after `wordcraft_docx::read_with_password` stopped
/// linking the write direction of the model bridge: 6,809,726 raw / 3,106,857 gzip /
/// 2,486,942 Brotli q11. Measured again 2026-11-16 after the model bridge stopped going through
/// JSON (`wordcraft-model-bridge`): 5,804,744 raw / 2,770,514 gzip / 2,272,553 Brotli q11. The
/// baseline for that change was a fresh build of the tree just before it, 6,809,172 raw /
/// 3,106,872 gzip: the same tree as the 6,809,726 / 3,106,857 measurement, built again, and two
/// builds of one tree differ by a few hundred bytes. Ceilings ratchet down; raising one is a
/// recorded decision in the plan.
pub const VIEWER_CEILING: WorkerCeiling = WorkerCeiling { name: VIEWER_STEM, max_raw: 5_980_000, max_gzip: 2_881_000 };

/// Crates the view-only module must never link (plan 8.2): it opens a document and paints its
/// pages, so the editor (`wordcraft-engine`, `wordcraft-ui-egui`, `egui`, `eframe`), PDF export
/// (`krilla`) and the DOM (`web-sys`: the host passes bytes in, which is what lets one module
/// serve a worker, the main thread or a server) stay out of its dependency graph.
pub const VIEWER_FORBIDDEN_CRATES: &[&str] = &["egui", "eframe", "wordcraft-engine", "wordcraft-ui-egui", "krilla", "web-sys"];

/// The byte-scan form of [`VIEWER_FORBIDDEN_CRATES`], supplementary like [`FORBIDDEN_SYMBOLS`]:
/// the dependency graph is the check that holds on a stripped module.
pub const VIEWER_FORBIDDEN_SYMBOLS: &[&str] = &["egui::", "eframe::", "wordcraft_engine::", "wordcraft_ui_egui::", "krilla::", "web_sys::"];

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
        // At shift 63 only one payload bit still fits in a u64; a larger payload needs bit 64
        // and the shift below would silently discard it, so reject it before shifting.
        if shift == 63 && b & 0x7f > 1 {
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
/// A folder or an entry that cannot be read is an error, never fewer workers: one behind it
/// would be left out of the totals, which would then pass with less counted than is shipped.
pub fn find_worker_wasms(dist_web: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(dist_web).map_err(|e| format!("{}: {e}", dist_web.display()))?.map(|e| e.map(|e| e.path()));
    worker_wasms_in(dist_web, entries)
}

/// [`find_worker_wasms`] over the listing `entries` of `dist_web`.
fn worker_wasms_in(dist_web: &Path, entries: impl Iterator<Item = std::io::Result<PathBuf>>) -> Result<Vec<PathBuf>, String> {
    let mut matches = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dist_web.display()))?;
        if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("wordcraft-worker-") && n.ends_with("_bg.wasm")) {
            matches.push(path);
        }
    }
    matches.sort();
    Ok(matches)
}

/// The page module's `wordcraft-viewer-web-<hash>_bg.wasm` in `dist_web`: exactly one, or an
/// error (a page built from `apps/wordcraft-web/index.html` always has it).
pub fn find_page_wasm(dist_web: &Path) -> Result<PathBuf, String> {
    let entries = std::fs::read_dir(dist_web).map_err(|e| format!("{}: {e}", dist_web.display()))?.map(|e| e.map(|e| e.path()));
    unique_page_wasm(dist_web, entries)
}

/// [`find_page_wasm`] over the listing `entries` of `dist_web`. An entry that cannot be read is
/// an error: a second page module may be behind it, so "exactly one" would not be known.
fn unique_page_wasm(dist_web: &Path, entries: impl Iterator<Item = std::io::Result<PathBuf>>) -> Result<PathBuf, String> {
    let prefix = format!("{PAGE_STEM}-");
    let mut matches = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dist_web.display()))?;
        if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(&prefix) && n.ends_with("_bg.wasm")) {
            matches.push(path);
        }
    }
    matches.sort();
    match matches.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("no {PAGE_STEM}-*_bg.wasm (the viewer-first page module) in {}", dist_web.display())),
        many => Err(format!(
            "ambiguous: {} {PAGE_STEM}-*_bg.wasm files in {} ({}); clean the directory and rebuild",
            many.len(),
            dist_web.display(),
            many.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// The page module's budget errors: its ceilings, the `name`-section guard and the byte scan.
fn check_page_budget(r: &WasmReport) -> Vec<String> {
    let mut errs = Vec::new();
    if r.has_name_section {
        errs.push(format!(
            "{PAGE_STEM} carries a `name` custom section: `wasm-opt` did not run (build with `trunk build --release`), so this artifact is unoptimized and cannot be measured or shipped"
        ));
    }
    if r.total_bytes > PAGE_CEILING.max_raw {
        errs.push(format!("{PAGE_STEM} uncompressed {} bytes > ceiling {} bytes", r.total_bytes, PAGE_CEILING.max_raw));
    }
    if r.gzip_bytes > PAGE_CEILING.max_gzip {
        errs.push(format!("{PAGE_STEM} gzipped {} bytes > ceiling {} bytes", r.gzip_bytes, PAGE_CEILING.max_gzip));
    }
    if !r.found_forbidden.is_empty() {
        errs.push(format!("forbidden symbols found in {PAGE_STEM}: {}", r.found_forbidden.join(", ")));
    }
    errs
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

/// The first-load total (plan 10.5): the page module plus the view-only worker, raw and
/// gzipped, against [`MAX_FIRST_LOAD_UNCOMPRESSED_BYTES`] / [`MAX_FIRST_LOAD_GZIPPED_BYTES`].
/// A build without the view-only worker is an error: the page cannot show a document.
fn check_first_load_budget(page: &WasmReport, viewer_worker: Option<&WasmReport>) -> Vec<String> {
    let Some(worker) = viewer_worker else {
        return vec![format!("first load: no {VIEWER_WORKER_STEM}_bg.wasm beside the page module")];
    };
    let raw = page.total_bytes.saturating_add(worker.total_bytes);
    let gzip = page.gzip_bytes.saturating_add(worker.gzip_bytes);
    let mut errs = Vec::new();
    if raw > MAX_FIRST_LOAD_UNCOMPRESSED_BYTES {
        errs.push(format!("first load: {raw} uncompressed bytes (page module + view-only worker) > ceiling {MAX_FIRST_LOAD_UNCOMPRESSED_BYTES}"));
    }
    if gzip > MAX_FIRST_LOAD_GZIPPED_BYTES {
        errs.push(format!("first load: {gzip} gzipped bytes (page module + view-only worker) > ceiling {MAX_FIRST_LOAD_GZIPPED_BYTES}"));
    }
    errs
}

/// The [`ON_DEMAND_STEMS`] the first-load page names outside a comment. Any hit means a first
/// visit fetches, preloads or at least points at a file that should load on demand: the page
/// module names those files at run time, from its own code, never from the page.
///
/// Only HTML comments are set aside. A name inside a `<script>` or `<style>` block counts even
/// in a `//` or `/* */` comment there: telling such a comment from a string or a URL takes a
/// script parser, and a check that guards the first load should fail on a doubtful page, not
/// pass it. Notes about the lazy files belong in HTML comments, where the page keeps them.
pub fn on_demand_files_named_by_page(html: &str) -> Vec<&'static str> {
    let live = crate::web_page::without_comments(html);
    ON_DEMAND_STEMS.iter().copied().filter(|stem| live.contains(stem)).collect()
}

/// The local files `html` names in `href`, `src` or a quoted `./` path of its inline module
/// script, comments excluded, sorted and without duplicates.
pub fn files_named_by_page(html: &str) -> Vec<String> {
    let live = crate::web_page::without_comments(html);
    let mut names: Vec<String> = Vec::new();
    for (i, _) in live.match_indices("./") {
        let tail = live.get(i.saturating_add(2)..).unwrap_or_default();
        let end = tail.find(['"', '\'', ' ', '>', ')']).unwrap_or(tail.len());
        // The name on disk ends where a query or a fragment begins.
        let name = tail.get(..end).unwrap_or_default().split(['?', '#']).next().unwrap_or_default();
        if !name.is_empty() && !name.contains("..") {
            names.push(name.to_string());
        }
    }
    names.sort();
    names.dedup();
    names
}

/// The first-load file list for the report: `index.html`, the files it names, and the
/// view-only worker's three files, each with its size on disk; then the page's on-demand
/// names, which must be none.
fn first_load_files(dist_web: &Path) -> Result<(Vec<(String, u64)>, Vec<&'static str>), String> {
    let index = dist_web.join("index.html");
    let html = std::fs::read_to_string(&index).map_err(|e| format!("{}: {e}", index.display()))?;
    let mut names = vec!["index.html".to_string()];
    names.extend(files_named_by_page(&html));
    for suffix in ["_loader.js", ".js", "_bg.wasm"] {
        names.push(format!("{VIEWER_WORKER_STEM}{suffix}"));
    }
    let mut files = Vec::new();
    for name in names {
        let path = dist_web.join(&name);
        let len = std::fs::metadata(&path).map_err(|e| format!("first-load file {}: {e}", path.display()))?.len();
        files.push((name, len));
    }
    Ok((files, on_demand_files_named_by_page(&html)))
}

/// Crates that must not be in the web target's dependency graph. The byte scan for
/// [`FORBIDDEN_SYMBOLS`] is only supplementary: `wasm-release` strips symbols, so literals may
/// not survive, and Rust v0 mangling would not contain the `crate::` form anyway.
pub const FORBIDDEN_CRATES: &[&str] = &["krilla", "wordcraft-docbin", "wgpu", "naga"];

/// Forbidden crate names found in `cargo tree --prefix none --format {p}` output.
pub fn forbidden_crates_in_tree(tree: &str) -> Vec<String> {
    crates_in_tree(tree, FORBIDDEN_CRATES)
}

/// The crates of `forbidden` named in `cargo tree --prefix none --format {p}` output, each once,
/// in the order first met. Names match whole: `krilla-svg` is not `krilla`.
pub fn crates_in_tree(tree: &str, forbidden: &[&str]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for line in tree.lines() {
        let Some(name) = line.split_whitespace().next() else { continue };
        if forbidden.contains(&name) && !found.iter().any(|f| f == name) {
            found.push(name.to_string());
        }
    }
    found
}

fn wasm_dependency_tree(root: &Path) -> Result<String, String> {
    wasm_dependency_tree_of(root, "wordcraft-web")
}

/// The wasm32 normal-dependency tree of workspace package `package`, one `name version` a line.
fn wasm_dependency_tree_of(root: &Path, package: &str) -> Result<String, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let out = Command::new(cargo)
        .current_dir(root)
        .args(["tree", "-p", package, "--target", "wasm32-unknown-unknown", "-e", "normal"])
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
    let worker_wasms = find_worker_wasms(&dist_web)?;
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
    println!("  editor + every worker: {total_raw:>10} bytes raw, {total_gz:>10} gzip (reported; no session loads them all, so no ceiling)");

    // The viewer-first page module (plan 10.3), and the first-load modules: it and the
    // view-only worker, against the first-load ceiling (plan 10.5).
    let page_report = match find_page_wasm(&dist_web) {
        Ok(path) => {
            let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let gz = gzip_size(&path, &bytes)?;
            let br = brotli_size(&path, &bytes)?;
            Ok(analyze_wasm_bytes_with(path, &bytes, gz, PAGE_FORBIDDEN_SYMBOLS)?.with_brotli(br))
        }
        Err(e) => Err(e),
    };
    match &page_report {
        Ok(r) => {
            println!(
                "  page {PAGE_STEM}: uncompressed {:>10} bytes (budget {:>10}), gzip -9 {:>10} bytes (budget {:>10}), brotli q11 {:>10} bytes",
                r.total_bytes, PAGE_CEILING.max_raw, r.gzip_bytes, PAGE_CEILING.max_gzip, r.brotli_bytes
            );
            let viewer_worker = worker_reports.iter().find(|w| worker_stem(&w.path) == VIEWER_WORKER_STEM);
            let first = std::iter::once(r).chain(viewer_worker);
            let (raw, gz, br) = first.fold((0usize, 0usize, 0usize), |(a, b, c), w| {
                (a.saturating_add(w.total_bytes), b.saturating_add(w.gzip_bytes), c.saturating_add(w.brotli_bytes))
            });
            println!(
                "  first load (wasm: {PAGE_STEM} + {}): {raw:>10} bytes raw (budget {MAX_FIRST_LOAD_UNCOMPRESSED_BYTES:>10}), {gz:>10} gzip (budget {MAX_FIRST_LOAD_GZIPPED_BYTES:>10}), {br:>10} brotli q11",
                if viewer_worker.is_some() { VIEWER_WORKER_STEM } else { "NO VIEWER WORKER" }
            );
        }
        Err(e) => println!("  page {PAGE_STEM}: ERROR ({e})"),
    }

    let first_load_res = first_load_files(&dist_web);
    match &first_load_res {
        Ok((files, on_demand)) => {
            let sum = files.iter().map(|(_, len)| *len).fold(0u64, u64::saturating_add);
            println!("  first-load files: {} files, {sum} bytes raw", files.len());
            for (name, len) in files {
                println!("    {len:>10}  {name}");
            }
            if on_demand.is_empty() {
                println!("  first-load page names none of: {}", ON_DEMAND_STEMS.join(", "));
            } else {
                println!("  first-load page names on-demand files: {}", on_demand.join(", "));
            }
        }
        Err(e) => println!("  first-load files: ERROR ({e})"),
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
    let mut graph_errs = Vec::new();
    for (package, forbidden) in [(PAGE_STEM, PAGE_FORBIDDEN_CRATES), (VIEWER_WORKER_STEM, VIEWER_WORKER_FORBIDDEN_CRATES)] {
        let found = crates_in_tree(&wasm_dependency_tree_of(root, package)?, forbidden);
        if found.is_empty() {
            println!("  {package}: none of {} in its wasm32 dependency graph", forbidden.join(", "));
        } else {
            println!("  {package}: forbidden crates in its wasm32 dependency graph: {}", found.join(", "));
            graph_errs.push(format!("forbidden crates in the {package} wasm32 dependency graph: {}", found.join(", ")));
        }
    }

    if report_only {
        return Ok(());
    }
    let mut errs = check_budget(&report);
    for r in &worker_reports {
        errs.extend(check_worker_budget(r));
    }
    match &page_report {
        Ok(r) => {
            errs.extend(check_page_budget(r));
            errs.extend(check_first_load_budget(r, worker_reports.iter().find(|w| worker_stem(&w.path) == VIEWER_WORKER_STEM)));
        }
        Err(e) => errs.push(e.clone()),
    }
    errs.extend(graph_errs);
    match first_load_res {
        Ok((_, on_demand)) if on_demand.is_empty() => {}
        Ok((_, on_demand)) => errs.push(format!("the first-load page names on-demand files outside a comment: {}", on_demand.join(", "))),
        Err(e) => errs.push(e),
    }
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

/// The view-only module's budget errors: its own ceilings, the `name`-section guard and the
/// byte scan, as [`check_worker_budget`] does for a worker.
fn check_viewer_budget(r: &WasmReport) -> Vec<String> {
    let mut errs = Vec::new();
    if r.has_name_section {
        errs.push(format!(
            "{VIEWER_STEM} carries a `name` custom section: `wasm-opt` did not run, so this artifact is unoptimized and cannot be measured or shipped"
        ));
    }
    if r.total_bytes > VIEWER_CEILING.max_raw {
        errs.push(format!("{VIEWER_STEM} uncompressed {} bytes > ceiling {} bytes", r.total_bytes, VIEWER_CEILING.max_raw));
    }
    if r.gzip_bytes > VIEWER_CEILING.max_gzip {
        errs.push(format!("{VIEWER_STEM} gzipped {} bytes > ceiling {} bytes", r.gzip_bytes, VIEWER_CEILING.max_gzip));
    }
    if !r.found_forbidden.is_empty() {
        errs.push(format!("forbidden symbols found in {VIEWER_STEM}: {}", r.found_forbidden.join(", ")));
    }
    errs
}

/// The path after `--wasm` in `args`, if any.
fn wasm_arg(args: &[&str]) -> Option<PathBuf> {
    args.iter().position(|a| *a == "--wasm").and_then(|i| args.get(i + 1)).map(PathBuf::from)
}

/// `cargo xtask wasm-budget --viewer [--wasm <path>] [--report-only]`: the view-only module's
/// row (plan Phase 8), alone. It needs no web build: the artifact is `--wasm <path>`, else
/// `dist/web/wordcraft-viewer_bg.wasm`, and a missing one is an error, not a pass. The
/// forbidden-crate check reads the module's wasm32 dependency graph, so it reports whether or
/// not the artifact exists.
pub fn run_viewer(root: &Path, args: &[&str]) -> Result<(), String> {
    let report_only = args.contains(&"--report-only");
    let path = wasm_arg(args).unwrap_or_else(|| root.join("dist/web").join(format!("{VIEWER_STEM}_bg.wasm")));
    let mut errs = Vec::new();

    println!("WordCraft view-only module WASM Budget Report ({}):", path.display());
    match std::fs::read(&path) {
        Ok(bytes) => {
            let gz = gzip_size(&path, &bytes)?;
            let br = brotli_size(&path, &bytes)?;
            let report = analyze_wasm_bytes_with(path.clone(), &bytes, gz, VIEWER_FORBIDDEN_SYMBOLS)?.with_brotli(br);
            println!("  uncompressed: {:>10} bytes (budget {:>10})", report.total_bytes, VIEWER_CEILING.max_raw);
            println!("  gzip -9:      {:>10} bytes (budget {:>10})", report.gzip_bytes, VIEWER_CEILING.max_gzip);
            println!("  brotli q11:   {:>10} bytes (report column, no ceiling)", report.brotli_bytes);
            println!("  code (sec10): {:>10} bytes", report.code_bytes);
            println!("  data (sec11): {:>10} bytes", report.data_bytes);
            println!("  custom (sec0):{:>10} bytes", report.custom_bytes);
            errs.extend(check_viewer_budget(&report));
        }
        Err(e) => {
            println!("  artifact:     MISSING ({e})");
            errs.push(format!(
                "{VIEWER_STEM}: no artifact at {} ({e}); build it (cargo build -p {VIEWER_STEM} --bin {VIEWER_STEM} --target wasm32-unknown-unknown --profile wasm-release, then wasm-bindgen --target web and wasm-opt -Oz) or pass --wasm <path>",
                path.display()
            ));
        }
    }

    let forbidden_crates = crates_in_tree(&wasm_dependency_tree_of(root, VIEWER_STEM)?, VIEWER_FORBIDDEN_CRATES);
    if forbidden_crates.is_empty() {
        println!("  forbidden crates: none of {} in the wasm32 dependency graph", VIEWER_FORBIDDEN_CRATES.join(", "));
    } else {
        println!("  forbidden crates in the wasm32 dependency graph: {}", forbidden_crates.join(", "));
        errs.push(format!("forbidden crates in the {VIEWER_STEM} wasm32 dependency graph: {}", forbidden_crates.join(", ")));
    }

    if report_only {
        return Ok(());
    }
    if errs.is_empty() {
        println!("OK: {VIEWER_STEM} satisfies its WASM budget.");
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
    fn leb128_rejects_a_payload_past_bit_64() {
        // Nine 0x80 bytes leave the decoder at shift 63; a tenth byte with payload 2 needs
        // bit 64, which must be an overflow error, not a silently truncated value.
        let mut bytes = [0x80u8; 10];
        bytes[9] = 0x02;
        let mut pos = 0usize;
        let err = read_leb_u64(&bytes, &mut pos).unwrap_err();
        assert!(err.contains("overflow"), "{err}");
        // The largest payload that does fit (payload 1 at shift 63) still decodes.
        bytes[9] = 0x01;
        let mut pos = 0usize;
        assert_eq!(read_leb_u64(&bytes, &mut pos).unwrap(), 1 << 63);
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
        let found = find_worker_wasms(&dir).unwrap();
        let names: Vec<String> = found.iter().filter_map(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()).collect();
        assert_eq!(names, vec!["wordcraft-worker-codecs_bg.wasm", "wordcraft-worker-proof_bg.wasm"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_page_module_and_the_un_hashed_editor_are_told_apart() {
        let dir = std::env::temp_dir().join(format!("wc_wasm_page_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(find_page_wasm(&dir).unwrap_err().contains("no wordcraft-viewer-web"));
        // The viewer-first build: the hashed page module, the editor un-hashed, the workers.
        let page = dir.join("wordcraft-viewer-web-4a1d7206a6ed210d_bg.wasm");
        let editor = dir.join("wordcraft-web_bg.wasm");
        for p in [&page, &editor, &dir.join("wordcraft-worker-viewer_bg.wasm"), &dir.join("wordcraft-viewer-web-8adef863fe79cba4.js")] {
            std::fs::write(p, b"x").unwrap();
        }
        assert_eq!(find_page_wasm(&dir).unwrap(), page);
        assert_eq!(find_core_wasm(&dir).unwrap(), editor, "the page module is not the editor");
        let workers: Vec<String> = find_worker_wasms(&dir).unwrap().iter().map(|p| worker_stem(p)).collect();
        assert_eq!(workers, vec![VIEWER_WORKER_STEM]);
        std::fs::write(dir.join("wordcraft-viewer-web-0000000000000000_bg.wasm"), b"y").unwrap();
        assert!(find_page_wasm(&dir).unwrap_err().contains("ambiguous"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_listing_that_fails_part_way_is_an_error_not_a_unique_page_module() {
        // One page module read, then the listing fails: a second one may be behind the failure,
        // so "exactly one" is not known.
        let dir = Path::new("dist/web");
        let page = dir.join("wordcraft-viewer-web-4a1d7206a6ed210d_bg.wasm");
        let failed = || std::io::Error::other("the listing failed");
        let err = unique_page_wasm(dir, [Ok(page.clone()), Err(failed())].into_iter()).unwrap_err();
        assert!(err.contains("the listing failed") && err.contains("dist/web"), "{err}");
        let err = unique_page_wasm(dir, [Err(failed()), Ok(page.clone())].into_iter()).unwrap_err();
        assert!(err.contains("the listing failed"), "{err}");
        assert_eq!(unique_page_wasm(dir, [Ok(page.clone()), Ok(dir.join("wordcraft-web_bg.wasm"))].into_iter()).unwrap(), page);
    }

    #[test]
    fn a_worker_listing_that_fails_part_way_is_an_error_not_fewer_workers() {
        // A worker behind the failure would be left out of the budget, which would then pass
        // with less counted than is shipped.
        let dir = Path::new("dist/web");
        let viewer = dir.join("wordcraft-worker-viewer_bg.wasm");
        let failed = || std::io::Error::other("the listing failed");
        let err = worker_wasms_in(dir, [Ok(viewer.clone()), Err(failed())].into_iter()).unwrap_err();
        assert!(err.contains("the listing failed") && err.contains("dist/web"), "{err}");
        assert!(worker_wasms_in(dir, [Err(failed())].into_iter()).is_err());
        let found = worker_wasms_in(
            dir,
            [Ok(dir.join("wordcraft-worker-pdf_bg.wasm")), Ok(viewer.clone()), Ok(dir.join("wordcraft-web_bg.wasm"))].into_iter(),
        )
        .unwrap();
        assert_eq!(found, vec![dir.join("wordcraft-worker-pdf_bg.wasm"), viewer]);
        // A folder that cannot be listed at all is an error too, not "no workers".
        assert!(find_worker_wasms(Path::new("/nonexistent/wordcraft/dist/web")).is_err());
    }

    #[test]
    fn page_budget_flags_over_limit_forbidden_and_unoptimized() {
        let path = || PathBuf::from("wordcraft-viewer-web-4a1d7206a6ed210d_bg.wasm");
        let ok = analyze_wasm_bytes_with(path(), &minimal_wasm(1024, 512, b"safe"), 400, PAGE_FORBIDDEN_SYMBOLS).unwrap();
        assert!(check_page_budget(&ok).is_empty(), "{:?}", check_page_budget(&ok));
        let linked = analyze_wasm_bytes_with(path(), &minimal_wasm(1024, 512, b"wordcraft_layout::Layout"), 400, PAGE_FORBIDDEN_SYMBOLS).unwrap();
        assert!(check_page_budget(&linked).iter().any(|e| e.contains("forbidden symbols")));
        let big =
            analyze_wasm_bytes_with(path(), &minimal_wasm(PAGE_CEILING.max_raw + 1, 512, b"safe"), PAGE_CEILING.max_gzip + 1, PAGE_FORBIDDEN_SYMBOLS)
                .unwrap();
        let errs = check_page_budget(&big);
        assert!(errs.iter().any(|e| e.contains("uncompressed")) && errs.iter().any(|e| e.contains("gzipped")), "{errs:?}");
        let named = analyze_wasm_bytes_with(path(), &minimal_wasm(1024, 512, &named_custom_section("name")), 400, PAGE_FORBIDDEN_SYMBOLS).unwrap();
        assert!(named.has_name_section, "the fixture carries a `name` section");
        assert!(check_page_budget(&named).iter().any(|e| e.contains("`name` custom section")));
    }

    #[test]
    fn page_and_viewer_worker_forbidden_crates_detected_in_dependency_tree() {
        let page = "wordcraft-viewer-web v0.18.1 (/x)\nwordcraft-worker-proto v0.18.1 (/x)\nweb-sys v0.3.0\nserde v1.0.0\n";
        assert!(crates_in_tree(page, PAGE_FORBIDDEN_CRATES).is_empty());
        let heavy = "wordcraft-viewer-web v0.18.1 (/x)\nwordcraft-layout v0.18.1 (/x)\nwordcraft-docx v0.18.1 (/x)\nwordcraft-render v0.18.1 (/x)\negui v0.37.0\n";
        assert_eq!(crates_in_tree(heavy, PAGE_FORBIDDEN_CRATES), vec!["wordcraft-layout", "wordcraft-docx", "wordcraft-render", "egui"]);
        // The worker hosts the document stack and a message port; only the editor and PDF are out.
        let worker = "wordcraft-worker-viewer v0.18.1 (/x)\nwordcraft-viewer v0.18.1 (/x)\nwordcraft-layout v0.18.1 (/x)\nweb-sys v0.3.0\n";
        assert!(crates_in_tree(worker, VIEWER_WORKER_FORBIDDEN_CRATES).is_empty());
        let dirty = "wordcraft-worker-viewer v0.18.1 (/x)\nkrilla v0.4.0\nwordcraft-engine v0.18.1 (/x)\n";
        assert_eq!(crates_in_tree(dirty, VIEWER_WORKER_FORBIDDEN_CRATES), vec!["krilla", "wordcraft-engine"]);
        // And it has a measured row like every worker.
        assert!(WORKER_CEILINGS.iter().any(|c| c.name == VIEWER_WORKER_STEM));
        assert_eq!(forbidden_symbols_for(VIEWER_WORKER_STEM), VIEWER_WORKER_FORBIDDEN_SYMBOLS);
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
    fn unregistered_worker_flags() {
        // A worker with no measured ceiling row is an error, not a pass: guessed or shared
        // numbers hide growth.
        let rep = analyze_wasm_bytes(PathBuf::from("wordcraft-worker-future_bg.wasm"), &minimal_wasm(1024, 512, b"safe_symbol"), 400).unwrap();
        let errs = check_worker_budget(&rep);
        assert!(errs.iter().any(|e| e.contains("no ceiling") && e.contains("wordcraft-worker-future")), "{errs:?}");
    }

    #[test]
    fn first_load_total_is_the_page_plus_the_view_only_worker() {
        let page = analyze_wasm_bytes(PathBuf::from("wordcraft-viewer-web-abc_bg.wasm"), &minimal_wasm(1000, 100, b"x"), 500).unwrap();
        let worker = analyze_wasm_bytes(PathBuf::from("wordcraft-worker-viewer_bg.wasm"), &minimal_wasm(1000, 100, b"x"), 500).unwrap();
        assert!(check_first_load_budget(&page, Some(&worker)).is_empty());

        // One byte over either ceiling fails that ceiling and only that one.
        let at_raw = WasmReport { total_bytes: MAX_FIRST_LOAD_UNCOMPRESSED_BYTES, gzip_bytes: 0, ..worker.clone() };
        let one = WasmReport { total_bytes: 1, gzip_bytes: 0, ..page.clone() };
        let errs = check_first_load_budget(&one, Some(&at_raw));
        assert!(errs.len() == 1 && errs[0].contains("uncompressed"), "{errs:?}");
        let at_gzip = WasmReport { total_bytes: 0, gzip_bytes: MAX_FIRST_LOAD_GZIPPED_BYTES, ..worker.clone() };
        let one = WasmReport { total_bytes: 0, gzip_bytes: 1, ..page.clone() };
        let errs = check_first_load_budget(&one, Some(&at_gzip));
        assert!(errs.len() == 1 && errs[0].contains("gzipped"), "{errs:?}");
        // Exactly at the ceilings passes.
        let zero = WasmReport { total_bytes: 0, gzip_bytes: 0, ..page.clone() };
        let at_both = WasmReport { total_bytes: MAX_FIRST_LOAD_UNCOMPRESSED_BYTES, gzip_bytes: MAX_FIRST_LOAD_GZIPPED_BYTES, ..worker };
        assert!(check_first_load_budget(&zero, Some(&at_both)).is_empty());

        // No view-only worker: the page cannot show a document.
        let errs = check_first_load_budget(&page, None);
        assert!(errs.iter().any(|e| e.contains("wordcraft-worker-viewer")), "{errs:?}");
    }

    #[test]
    fn the_first_load_ceiling_is_no_looser_than_its_parts() {
        let viewer_worker = WORKER_CEILINGS.iter().find(|c| c.name == VIEWER_WORKER_STEM).expect("the view-only worker is registered");
        assert!(MAX_FIRST_LOAD_UNCOMPRESSED_BYTES <= PAGE_CEILING.max_raw + viewer_worker.max_raw);
        assert!(MAX_FIRST_LOAD_GZIPPED_BYTES <= PAGE_CEILING.max_gzip + viewer_worker.max_gzip);
    }

    #[test]
    fn a_first_load_page_that_names_an_on_demand_file_is_caught() {
        let clean = r#"<head><!-- the editor: wordcraft-web_loader.js, and wordcraft-worker-pdf -->
<script type="module">import init from './wordcraft-viewer-web-8a80.js';
await init({ module_or_path: './wordcraft-viewer-web-7858_bg.wasm' });</script>
<link rel="icon" href="./wordcraft-fa59.ico"/></head>"#;
        assert!(on_demand_files_named_by_page(clean).is_empty(), "comments and the page module's own names are fine");
        assert_eq!(files_named_by_page(clean), ["wordcraft-fa59.ico", "wordcraft-viewer-web-7858_bg.wasm", "wordcraft-viewer-web-8a80.js"]);

        for (tag, stem) in [
            (r#"<link rel="modulepreload" href="./wordcraft-web.js">"#, "wordcraft-web"),
            (r#"<link rel="preload" href="./wordcraft-web_bg.wasm" as="fetch">"#, "wordcraft-web"),
            (r#"<script type="module" src="wordcraft-web_loader.js"></script>"#, "wordcraft-web"),
            (r#"<link rel="prefetch" href="/wordcraft-worker-proof_bg.wasm">"#, "wordcraft-worker-proof"),
            (r#"<script>new Worker("wordcraft-worker-codecs_loader.js")</script>"#, "wordcraft-worker-codecs"),
            (r#"<link rel="preload" href="wordcraft-worker-pdf.js">"#, "wordcraft-worker-pdf"),
        ] {
            let html = format!("{clean}{tag}");
            assert_eq!(on_demand_files_named_by_page(&html), [stem], "{tag}");
        }
        // A script or style comment is not set aside: the check fails closed on those.
        assert_eq!(on_demand_files_named_by_page("<script>// see wordcraft-worker-pdf_loader.js\n</script>"), ["wordcraft-worker-pdf"]);
        assert_eq!(on_demand_files_named_by_page("<style>/* wordcraft-web.js */</style>"), ["wordcraft-web"]);
        // A comment that never closes hides nothing before it and cannot panic.
        assert_eq!(on_demand_files_named_by_page(r#"<script src="wordcraft-web.js"></script><!-- open"#), ["wordcraft-web"]);
        assert!(on_demand_files_named_by_page("<!-- wordcraft-web.js").is_empty());
        assert!(files_named_by_page("./").is_empty() && files_named_by_page("<!--").is_empty());
        // A query or a fragment is not part of the file's name on disk.
        assert_eq!(files_named_by_page(r#"<script src="./p-1.js?v=1"></script><link href="./q.css#top"><a href="./?x">"#), ["p-1.js", "q.css"]);
    }

    #[test]
    fn the_first_load_file_list_reads_a_built_page() {
        let dir = std::env::temp_dir().join(format!("wc-first-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, body: &[u8]| std::fs::write(dir.join(name), body).unwrap();
        write("p-1.js", b"12345");
        write("wordcraft-worker-viewer_loader.js", b"1");
        write("wordcraft-worker-viewer.js", b"12");
        write("wordcraft-worker-viewer_bg.wasm", b"123");
        write("index.html", br#"<script type="module">import './p-1.js';</script>"#);
        let (files, on_demand) = first_load_files(&dir).unwrap();
        assert!(on_demand.is_empty());
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            ["index.html", "p-1.js", "wordcraft-worker-viewer_loader.js", "wordcraft-worker-viewer.js", "wordcraft-worker-viewer_bg.wasm"]
        );
        assert_eq!(files.iter().map(|(_, l)| *l).skip(1).collect::<Vec<_>>(), [5, 1, 2, 3]);

        // A page that names the editor is reported; a named file that is missing is an error.
        write("index.html", br#"<script type="module" src="./wordcraft-web_loader.js"></script>"#);
        assert!(first_load_files(&dir).is_err(), "the named file does not exist");
        write("wordcraft-web_loader.js", b"x");
        assert_eq!(first_load_files(&dir).unwrap().1, ["wordcraft-web"]);
        std::fs::remove_dir_all(&dir).unwrap();
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
    fn viewer_forbidden_crates_detected_in_dependency_tree() {
        let clean = "wordcraft-viewer v0.1.0 (/x)\nwordcraft-render v0.1.0 (/x)\nwasm-bindgen v0.2.0\negui_extras v0.37.0\n";
        assert!(crates_in_tree(clean, VIEWER_FORBIDDEN_CRATES).is_empty());
        let dirty = "wordcraft-viewer v0.1.0 (/x)\nwordcraft-engine v0.1.0 (/x)\negui v0.37.0\neframe v0.37.0\nwordcraft-ui-egui v0.1.0 (/x)\nkrilla v0.4.0\nweb-sys v0.3.0\negui v0.37.0 (*)\n";
        assert_eq!(
            crates_in_tree(dirty, VIEWER_FORBIDDEN_CRATES),
            vec!["wordcraft-engine", "egui", "eframe", "wordcraft-ui-egui", "krilla", "web-sys"]
        );
    }

    #[test]
    fn viewer_budget_flags_over_limit_forbidden_and_unoptimized() {
        let path = || PathBuf::from("wordcraft-viewer_bg.wasm");
        let forbidden = minimal_wasm(1024, 512, b"wordcraft_engine::Session");
        let rep = analyze_wasm_bytes_with(path(), &forbidden, 400, VIEWER_FORBIDDEN_SYMBOLS).unwrap();
        assert!(check_viewer_budget(&rep).iter().any(|e| e.contains("forbidden symbols")), "{:?}", check_viewer_budget(&rep));

        let padded = minimal_wasm(VIEWER_CEILING.max_raw + 1, 512, b"safe_symbol");
        let rep = analyze_wasm_bytes_with(path(), &padded, VIEWER_CEILING.max_gzip + 1, VIEWER_FORBIDDEN_SYMBOLS).unwrap();
        let errs = check_viewer_budget(&rep);
        assert!(errs.iter().any(|e| e.contains("uncompressed")) && errs.iter().any(|e| e.contains("gzipped")), "{errs:?}");

        let named = minimal_wasm(1024, 512, &named_custom_section("name"));
        let rep = analyze_wasm_bytes_with(path(), &named, 400, VIEWER_FORBIDDEN_SYMBOLS).unwrap();
        assert!(check_viewer_budget(&rep).iter().any(|e| e.contains("`name` custom section")));
    }

    #[test]
    fn viewer_run_fails_without_an_artifact() {
        let missing = std::env::temp_dir().join(format!("wc_viewer_missing_{}_bg.wasm", std::process::id()));
        let _ = std::fs::remove_file(&missing);
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
        let err = run_viewer(&root, &["--viewer", "--wasm", &missing.to_string_lossy()]).unwrap_err();
        assert!(err.contains("no artifact at"), "{err}");
        assert_eq!(wasm_arg(&["--viewer"]), None);
        assert_eq!(wasm_arg(&["--wasm"]), None);
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
