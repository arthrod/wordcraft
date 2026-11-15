//! `cargo xtask wasm-attr [--wasm <path>]`: attribute the WASM code section of the
//! `wordcraft-web` build per crate, to decide whether cold-code splitting is worth it.
//!
//! The `wasm-release` profile sets `strip = "symbols"`, which removes the `name` custom
//! section rustc emits — so before analysing, the default path rebuilds with
//! `CARGO_PROFILE_WASM_RELEASE_STRIP=none` (the env override keeps the `name` section;
//! everything else about the build is unchanged). The analysis targets the raw rustc
//! artifact under `target/wasm32-unknown-unknown/wasm-release/`, not the wasm-bindgen
//! output in `dist/` (which renames and post-processes functions).
//!
//! Parsing is done here, without a wasmparser dependency: only two sections matter —
//! the code section (id 10) for per-function encoded sizes, and the `name` custom
//! section's function-name subsection for the symbol to attribute each size to. Every
//! read is bounded; malformed input is an error, never a panic.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::wasm_budget::read_leb_u64;

/// Where the report goes. `docs/plans/` already holds the implementation plan this
/// measurement informs.
const REPORT_REL: &str = "docs/plans/wasm-attribution.md";

/// Profile env override that keeps the `name` custom section (the profile's
/// `strip = "symbols"` would otherwise remove it and make attribution impossible).
const STRIP_ENV: &str = "CARGO_PROFILE_WASM_RELEASE_STRIP";

/// The bin target of `apps/wordcraft-web` (bins keep hyphens in their artifact name).
const WASM_ARTIFACT: &str = "wordcraft-web.wasm";

#[derive(Debug)]
pub struct Bucket {
    pub name: String,
    pub bytes: usize,
    pub funcs: usize,
}

#[derive(Debug)]
pub struct Attribution {
    pub path: PathBuf,
    pub wasm_bytes: usize,
    /// Payload bytes of the code section (vec count + all function entries).
    pub code_section_bytes: usize,
    /// Sum of the encoded function entries (length prefix + body per function).
    pub function_bytes: usize,
    pub functions: usize,
    /// Sorted by bytes descending, then name.
    pub buckets: Vec<Bucket>,
}

/// Read a length-prefixed UTF-8 name whose bytes must end within `end`.
fn read_name(bytes: &[u8], pos: &mut usize, end: usize) -> Result<String, String> {
    let len = usize::try_from(read_leb_u64(bytes, pos)?).map_err(|_| "name length too large for usize".to_string())?;
    let name_end = pos.checked_add(len).ok_or_else(|| "name offset overflow".to_string())?;
    if name_end > end {
        return Err("truncated name string".into());
    }
    let slice = bytes.get(*pos..name_end).ok_or_else(|| "name slice out of bounds".to_string())?;
    let name = std::str::from_utf8(slice).map_err(|e| format!("name is not valid UTF-8: {e}"))?;
    *pos = name_end;
    Ok(name.to_string())
}

/// Read a LEB128 that the spec types as u32 (counts, indices, section lengths).
fn read_leb_u32(bytes: &[u8], pos: &mut usize) -> Result<u32, String> {
    u32::try_from(read_leb_u64(bytes, pos)?).map_err(|_| "LEB128 exceeds u32".to_string())
}

/// The code section payload: `<count> <entry>*` where each entry is
/// `<body_size:leb> <body bytes>`. Returns each function's total encoded size
/// (its length prefix plus body), which is exactly what attribution cares about.
fn parse_code_section(bytes: &[u8], start: usize, end: usize) -> Result<Vec<usize>, String> {
    let mut pos = start;
    let count = read_leb_u32(bytes, &mut pos)?;
    let mut sizes = Vec::new();
    for _ in 0..count {
        let before = pos;
        let body = usize::try_from(read_leb_u64(bytes, &mut pos)?).map_err(|_| "function body too large for usize".to_string())?;
        let prefix = pos.saturating_sub(before);
        let body_end = pos.checked_add(body).ok_or_else(|| "function body offset overflow".to_string())?;
        if body_end > end {
            return Err("truncated function body in code section".into());
        }
        pos = body_end;
        sizes.push(prefix.saturating_add(body));
    }
    if pos != end {
        return Err("trailing bytes after the last function body in code section".into());
    }
    Ok(sizes)
}

/// Subsection 1 of the `name` custom section: a name-map of `<count> <funcidx, name>*`.
fn parse_name_map(bytes: &[u8], start: usize, end: usize) -> Result<Vec<(u32, String)>, String> {
    let mut pos = start;
    let count = read_leb_u32(bytes, &mut pos)?;
    let mut entries = Vec::new();
    for _ in 0..count {
        let idx = read_leb_u32(bytes, &mut pos)?;
        let name = read_name(bytes, &mut pos, end)?;
        entries.push((idx, name));
    }
    if pos != end {
        return Err("trailing bytes in function-name map".into());
    }
    Ok(entries)
}

/// A custom section (id 0) payload: `<section name> <subsection>*`. For the one named
/// "name", walk the subsections and return subsection 1's function-name map.
fn parse_custom_section(bytes: &[u8], start: usize, end: usize) -> Result<Option<Vec<(u32, String)>>, String> {
    let mut pos = start;
    let section_name = read_name(bytes, &mut pos, end)?;
    if section_name != "name" {
        return Ok(None); // some other custom section (e.g. "target_features"): not ours
    }
    let mut names = None;
    while pos < end {
        let id = *bytes.get(pos).ok_or_else(|| "unexpected EOF in name subsection".to_string())?;
        pos += 1;
        let size = usize::try_from(read_leb_u64(bytes, &mut pos)?).map_err(|_| "subsection too large for usize".to_string())?;
        let sub_end = pos.checked_add(size).ok_or_else(|| "subsection offset overflow".to_string())?;
        if sub_end > end {
            return Err("truncated name subsection".into());
        }
        if id == 1 {
            names = Some(parse_name_map(bytes, pos, sub_end)?);
        }
        pos = sub_end;
    }
    Ok(names)
}

/// The first `::`-separated path segment of a demangled name — the crate. For
/// `<path as trait>::…` forms, the first segment inside the angle brackets; the cut at
/// `:` or `>` keeps nested generics (`<crate::Foo<T> as …>`) from leaking past the crate.
fn first_crate_segment(demangled: &str) -> &str {
    let body = demangled.strip_prefix('<').unwrap_or(demangled);
    let cut = body.find([':', '>']).unwrap_or(body.len());
    body.get(..cut).unwrap_or("")
}

/// A plausible crate name: symbols come from rustc, but hostile or non-Rust names must not
/// create junk buckets.
fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Rust primitive type names appear as the first segment of `<str as core::fmt::Display>::fmt`-
/// style symbols; they name a type, not a crate, so they belong in `other`.
const PRIMITIVES: &[&str] = &[
    "bool",
    "char",
    "str",
    "u8",
    "u16",
    "u32",
    "u64",
    "u128",
    "usize",
    "i8",
    "i16",
    "i32",
    "i64",
    "i128",
    "isize",
    "f32",
    "f64",
    "array",
    "slice",
    "tuple",
    "reference",
];

/// Map a demangled first segment to a bucket: workspace crates keep a `wordcraft-*` name,
/// the four support crates group under `rust-std`, a plausible crate name buckets under
/// itself (third-party), anything else (primitives, generics, junk, nothing) is `other`.
fn normalize_bucket(segment: &str) -> String {
    // v0 disambiguators print as `crate[hash]`; strip them.
    let base = segment.split('[').next().unwrap_or(segment);
    if matches!(base, "std" | "core" | "alloc" | "compiler_builtins") {
        return "rust-std".into();
    }
    if base == "wordcraft" || base.starts_with("wordcraft_") {
        return base.replace('_', "-");
    }
    if !PRIMITIVES.contains(&base) && is_identifier(base) {
        return base.to_string();
    }
    "other".into()
}

/// The second `::`-separated path segment of a demangled name, if any, with the same
/// leading-`<` and trailing-generics handling as [`first_crate_segment`] (`<a::b::T<…> as …>`
/// names `b`).
fn second_segment(demangled: &str) -> &str {
    let body = demangled.strip_prefix('<').unwrap_or(demangled);
    let Some(first_cut) = body.find("::") else { return "" };
    let rest = body.get(first_cut + 2..).unwrap_or("");
    let cut = rest.find([':', '>']).unwrap_or(rest.len());
    rest.get(..cut).unwrap_or("")
}

/// Demangle and attribute one symbol. Symbols that demangle to a single word carry no
/// crate path (C shims, link-time thunks), so they land in `other`.
fn bucket_for_name(mangled: &str) -> String {
    let demangled = rustc_demangle::demangle(mangled).to_string();
    if !demangled.contains("::") {
        return "other".into();
    }
    // The one module-level bucket (plan S6): the dialogs are the split-spike's candidate cold
    // region, so they are measured apart from the rest of the UI crate rather than claimed.
    // The first segment carries a v0 disambiguator hash in mangled form
    // (`wordcraft_ui_egui[6c571ced…]::dialogs`), so compare it stripped, as the crate buckets do.
    let first = first_crate_segment(&demangled);
    if first.split('[').next().unwrap_or(first) == "wordcraft_ui_egui" && second_segment(&demangled) == "dialogs" {
        return "wordcraft-ui-egui::dialogs".into();
    }
    normalize_bucket(first_crate_segment(&demangled))
}

/// Attribute each function entry to a bucket by its name in the function-name map;
/// functions without a name entry (or with an unusable one) go to `other`.
pub fn attribute(sizes: &[usize], names: &[(u32, String)]) -> Vec<Bucket> {
    let mut map: BTreeMap<u32, &str> = BTreeMap::new();
    for (idx, name) in names {
        map.insert(*idx, name.as_str());
    }
    let mut buckets: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (i, &size) in sizes.iter().enumerate() {
        // Function indices come from a u32 count in the code section, so try_from cannot
        // fail in practice — but hostile bytes never get to panic on the conversion.
        let idx = u32::try_from(i).ok();
        let bucket = idx.and_then(|idx| map.get(&idx)).map_or_else(|| "other".to_string(), |n| bucket_for_name(n));
        let entry = buckets.entry(bucket).or_insert((0, 0));
        entry.0 = entry.0.saturating_add(size);
        entry.1 = entry.1.saturating_add(1);
    }
    let mut buckets: Vec<Bucket> = buckets.into_iter().map(|(name, (bytes, funcs))| Bucket { name, bytes, funcs }).collect();
    buckets.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    buckets
}

fn strip_error(path: &Path) -> String {
    format!(
        "the `name` custom section is missing from {}: the binary was stripped (the \
         `wasm-release` profile sets `strip = \"symbols\"`), so function names are gone. \
         Rebuild unstripped with `{STRIP_ENV}=none cargo build -p wordcraft-web \
         --target wasm32-unknown-unknown --profile wasm-release`, or just run \
         `cargo xtask wasm-attr` without `--wasm`, which sets that env var itself.",
        path.display()
    )
}

/// Walk the sections of a wasm binary and attribute its code section. `path` is used in
/// error messages only.
pub fn analyze(path: PathBuf, bytes: &[u8]) -> Result<Attribution, String> {
    if bytes.len() < 8 || bytes.get(..8) != Some(&[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]) {
        return Err(format!("{}: not a valid WASM binary", path.display()));
    }
    let mut pos = 8usize;
    let mut code: Option<(usize, usize)> = None;
    let mut names: Option<Vec<(u32, String)>> = None;
    while pos < bytes.len() {
        let id = *bytes.get(pos).ok_or_else(|| "unexpected EOF reading section id".to_string())?;
        pos += 1;
        let len = usize::try_from(read_leb_u64(bytes, &mut pos)?).map_err(|_| "section too large for usize".to_string())?;
        let end = pos.checked_add(len).ok_or_else(|| "section offset overflow".to_string())?;
        if end > bytes.len() {
            return Err(format!("truncated WASM section id={id}"));
        }
        match id {
            0 => {
                if let Some(parsed) = parse_custom_section(bytes, pos, end)? {
                    names = Some(parsed);
                }
            }
            10 => {
                if code.is_some() {
                    return Err(format!("{}: duplicate code section", path.display()));
                }
                code = Some((pos, end));
            }
            _ => {}
        }
        pos = end;
    }
    let Some((code_start, code_end)) = code else {
        return Err(format!("{}: no code section (id 10) found", path.display()));
    };
    let Some(names) = names else {
        return Err(strip_error(&path));
    };
    let sizes = parse_code_section(bytes, code_start, code_end)?;
    let functions = sizes.len();
    let function_bytes = sizes.iter().fold(0usize, |acc, s| acc.saturating_add(*s));
    let buckets = attribute(&sizes, &names);
    Ok(Attribution { path, wasm_bytes: bytes.len(), code_section_bytes: code_end.saturating_sub(code_start), function_bytes, functions, buckets })
}

/// Bytes in buckets matching `pred` (used for the plan-focused group summary).
fn group_bytes(a: &Attribution, pred: impl Fn(&Bucket) -> bool) -> usize {
    a.buckets.iter().filter(|b| pred(b)).fold(0usize, |acc, b| acc.saturating_add(b.bytes))
}

fn pct(part: usize, whole: usize) -> String {
    if whole == 0 {
        return "0.0%".into();
    }
    format!("{:.1}%", part as f64 / whole as f64 * 100.0)
}

/// The plan cares about three groups: our crates (cold-code splitting targets), rust-std
/// (only linker GC moves it), and third-party crates (only dependency/feature cuts move them).
fn group_note(a: &Attribution) -> Vec<String> {
    let ours = group_bytes(a, |b| b.name.starts_with("wordcraft-"));
    let std = group_bytes(a, |b| b.name == "rust-std");
    let other = group_bytes(a, |b| b.name == "other");
    let third_party = a.function_bytes.saturating_sub(ours).saturating_sub(std).saturating_sub(other);
    vec![
        format!("- `wordcraft-*` (ours to split or slim): {} bytes ({})", ours, pct(ours, a.function_bytes)),
        format!("- `rust-std` (shrinks via linker GC / panic paths only): {} bytes ({})", std, pct(std, a.function_bytes)),
        format!("- third-party crates (shrink via dependency and feature cuts): {} bytes ({})", third_party, pct(third_party, a.function_bytes)),
        format!("- `other` (unnamed or foreign symbols): {} bytes ({})", other, pct(other, a.function_bytes)),
    ]
}

fn render_text(a: &Attribution) -> String {
    let mut out = format!("WordCraft Web WASM code attribution ({}):\n", a.path.display());
    out.push_str(&format!("  wasm total:    {} bytes\n", a.wasm_bytes));
    out.push_str(&format!(
        "  code section:  {} bytes across {} functions (entries: {} bytes)\n\n",
        a.code_section_bytes, a.functions, a.function_bytes
    ));
    out.push_str(&format!("{:<34} {:>12} {:>9} {:>8}\n", "bucket", "code bytes", "% code", "funcs"));
    for b in &a.buckets {
        out.push_str(&format!("{:<34} {:>12} {:>9} {:>8}\n", b.name, b.bytes, pct(b.bytes, a.function_bytes), b.funcs));
    }
    out.push_str(&format!("{:<34} {:>12} {:>9} {:>8}\n", "TOTAL", a.function_bytes, pct(a.function_bytes, a.function_bytes), a.functions));
    out
}

fn render_markdown(a: &Attribution, date: &str) -> String {
    let mut out = String::new();
    out.push_str("# WASM code-section attribution (`wordcraft-web`)\n\n");
    out.push_str(&format!("- wasm: `{}` ({} bytes)\n", a.path.display(), a.wasm_bytes));
    out.push_str(&format!("- date: {date}\n"));
    out.push_str(&format!(
        "- code section (id 10): {} bytes across {} function bodies ({} bytes of entries)\n",
        a.code_section_bytes, a.functions, a.function_bytes
    ));
    out.push_str(
        "- method: each function's encoded size (length prefix + body) from the code section, \
         attributed by crate via the `name` custom section's function names, demangled \
         with `rustc-demangle`; functions without a name entry (LTO internals) fall to `other`. \
         Produced by `cargo xtask wasm-attr`.\n\n",
    );
    out.push_str("| bucket | code bytes | % of code | functions |\n");
    out.push_str("| --- | ---: | ---: | ---: |\n");
    for b in &a.buckets {
        out.push_str(&format!("| `{}` | {} | {} | {} |\n", b.name, b.bytes, pct(b.bytes, a.function_bytes), b.funcs));
    }
    out.push_str(&format!("| **TOTAL** | {} | {} | {} |\n\n", a.function_bytes, pct(a.function_bytes, a.function_bytes), a.functions));
    out.push_str("## What the plan cares about\n\n");
    out.push_str(&group_note(a).join("\n"));
    out.push('\n');
    out
}

/// `--wasm <path>` or none; anything else is a usage error.
fn parse_args(args: &[&str]) -> Result<Option<PathBuf>, String> {
    const USAGE: &str = "usage: cargo xtask wasm-attr [--wasm <path>]";
    let mut wasm = None;
    let mut i = 0usize;
    while let Some(arg) = args.get(i) {
        match *arg {
            "--wasm" => {
                let value = args.get(i + 1).ok_or_else(|| format!("--wasm needs a path\n{USAGE}"))?;
                wasm = Some(PathBuf::from(value));
                i += 2;
            }
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        }
    }
    Ok(wasm)
}

/// Rebuild the wasm unstripped (the env var keeps the `name` section) and return the
/// artifact path under the target dir cargo reports.
fn build_and_find() -> Result<PathBuf, String> {
    let mut cmd = crate::cargo();
    cmd.args(["build", "-p", "wordcraft-web", "--target", "wasm32-unknown-unknown", "--profile", "wasm-release"]).env(STRIP_ENV, "none");
    eprintln!("$ cargo build -p wordcraft-web --target wasm32-unknown-unknown --profile wasm-release ({STRIP_ENV}=none)");
    let status = cmd.status().map_err(|e| format!("cargo build failed to spawn: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build exited with {status}"));
    }
    let meta = crate::metadata()?;
    let target_dir = meta.get("target_directory").and_then(|v| v.as_str()).ok_or_else(|| "cargo metadata: no target_directory".to_string())?;
    let wasm = Path::new(target_dir).join("wasm32-unknown-unknown/wasm-release").join(WASM_ARTIFACT);
    if !wasm.is_file() {
        return Err(format!("expected {} after the build", wasm.display()));
    }
    Ok(wasm)
}

/// UTC date for the report header; `date` is ubiquitous where cargo runs, and a missing
/// it must not fail the analysis (a placeholder is enough — no chrono dependency here).
fn utc_date() -> String {
    Command::new("date")
        .args(["-u", "+%Y-%m-%d"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown (`date` unavailable)".into())
}

pub fn run(root: &Path, args: &[&str]) -> Result<(), String> {
    let wasm_path = match parse_args(args)? {
        Some(p) => p,
        None => build_and_find()?,
    };
    let bytes = std::fs::read(&wasm_path).map_err(|e| format!("{}: {e}", wasm_path.display()))?;
    let attribution = analyze(wasm_path, &bytes)?;
    print!("{}", render_text(&attribution));
    let report = root.join(REPORT_REL);
    if let Some(parent) = report.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(&report, render_markdown(&attribution, &utc_date())).map_err(|e| format!("{}: {e}", report.display()))?;
    println!("\nwrote {}", report.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leb(n: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut n = n;
        loop {
            let mut b = (n & 0x7f) as u8;
            n >>= 7;
            if n != 0 {
                b |= 0x80;
            }
            out.push(b);
            if n == 0 {
                break;
            }
        }
        out
    }

    fn section(id: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![id];
        out.extend(leb(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn wasm_bytes(sections: &[Vec<u8>]) -> Vec<u8> {
        let mut out = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
        for s in sections {
            out.extend_from_slice(s);
        }
        out
    }

    /// A code section payload with one body of `len` padding bytes per function. Entry
    /// sizes are therefore `len` + 1 (the single-byte size prefix).
    fn code_payload(body_lens: &[usize]) -> Vec<u8> {
        let mut p = leb(body_lens.len() as u64);
        for &len in body_lens {
            p.extend(leb(len as u64));
            p.extend(std::iter::repeat_n(0u8, len));
        }
        p
    }

    /// The `name` custom section with subsection 1 (function names) mapping `entries`.
    fn name_section(entries: &[(u32, &str)]) -> Vec<u8> {
        let mut sub = leb(entries.len() as u64);
        for &(idx, name) in entries {
            sub.extend(leb(u64::from(idx)));
            sub.extend(leb(name.len() as u64));
            sub.extend_from_slice(name.as_bytes());
        }
        let mut payload = leb(4);
        payload.extend_from_slice(b"name");
        payload.push(1);
        payload.extend(leb(sub.len() as u64));
        payload.extend(sub);
        section(0, &payload)
    }

    fn sample(names: &[(u32, &str)]) -> Vec<u8> {
        wasm_bytes(&[section(10, &code_payload(&[10, 20, 5])), name_section(names)])
    }

    #[test]
    fn attributes_functions_to_crates_and_totals_match() {
        let bytes = sample(&[(0, "wordcraft_engine::x::f"), (1, "core::fmt::g"), (2, "egui::ui::h")]);
        let a = analyze(PathBuf::from("t.wasm"), &bytes).unwrap();
        assert_eq!(a.functions, 3);
        // entries: 10+1, 20+1, 5+1 prefix bytes
        assert_eq!(a.function_bytes, 11 + 21 + 6);
        let bucket = |n: &str| a.buckets.iter().find(|b| b.name == n);
        assert_eq!(bucket("wordcraft-engine").map(|b| (b.bytes, b.funcs)), Some((11, 1)));
        assert_eq!(bucket("rust-std").map(|b| (b.bytes, b.funcs)), Some((21, 1)));
        assert_eq!(bucket("egui").map(|b| (b.bytes, b.funcs)), Some((6, 1)));
        // every function lands somewhere, and the buckets are heaviest first
        let total: usize = a.buckets.iter().map(|b| b.bytes).sum();
        assert_eq!(total, a.function_bytes);
        assert!(a.buckets.windows(2).all(|w| w[0].bytes >= w[1].bytes));
    }

    /// S6: the dialogs get their own module-level bucket, so "the dialogs are hot/cold" is a
    /// measured row instead of a claim buried in the crate's total. Only that module splits out;
    /// every other `wordcraft_ui_egui` module stays in the crate bucket.
    #[test]
    fn ui_egui_dialogs_get_their_own_bucket() {
        // Five entries (sizes 10, 20, 5, 7, 7 + a length prefix each): three dialog forms —
        // plain, trait-impl, and the v0-mangled shape rustc emits in the name section — and
        // two non-dialog forms to prove the crate bucket keeps them.
        let bytes = wasm_bytes(&[
            section(10, &code_payload(&[10, 20, 5, 7, 7])),
            name_section(&[
                (0, "wordcraft_ui_egui::dialogs::text_dialogs::show"),
                (1, "<wordcraft_ui_egui::dialogs::para::D as core::fmt::Debug>::fmt"),
                (2, "wordcraft_ui_egui::ribbon::draw"),
                (3, "wordcraft_ui_egui[6c571ced3128981]::dialogs::dispatch::body"),
                (4, "wordcraft_ui_egui[6c571ced3128981]::ribbon::x"),
            ]),
        ]);
        let a = analyze(PathBuf::from("t.wasm"), &bytes).unwrap();
        let bucket = |n: &str| a.buckets.iter().find(|b| b.name == n);
        assert_eq!(bucket("wordcraft-ui-egui::dialogs").map(|b| (b.funcs, b.bytes)), Some((3, 11 + 21 + 8)), "{:?}", a.buckets);
        assert_eq!(bucket("wordcraft-ui-egui").map(|b| (b.funcs, b.bytes)), Some((2, 6 + 8)), "{:?}", a.buckets);
    }

    #[test]
    fn names_that_are_already_demangled_and_mangled_forms_bucket_the_same() {
        let bytes = sample(&[
            (0, "_ZN16wordcraft_engine1x1f17h0123456789abcdefE"),
            (1, "<wordcraft_engine::Foo<core::cell::Cell<u8>> as core::fmt::Debug>::fmt"),
        ]);
        let a = analyze(PathBuf::from("t.wasm"), &bytes).unwrap();
        // 10+1 and 20+1 both land in wordcraft-engine, regardless of mangling form; the
        // unnamed third function falls to `other`.
        assert_eq!(a.buckets.first().map(|b| (b.name.as_str(), b.bytes, b.funcs)), Some(("wordcraft-engine", 32, 2)));
        assert_eq!(a.buckets.get(1).map(|b| (b.name.as_str(), b.bytes)), Some(("other", 6)));
    }

    #[test]
    fn unnamed_unusable_and_foreign_names_go_to_other() {
        let bytes = sample(&[(0, "malloc"), (1, "not a crate name::leading spaces are not identifiers")]);
        let a = analyze(PathBuf::from("t.wasm"), &bytes).unwrap();
        // function 2 has no name entry at all; the two named ones carry no usable crate
        assert_eq!(a.buckets.first().map(|b| (b.name.as_str(), b.bytes, b.funcs)), Some(("other", 11 + 21 + 6, 3)));
    }

    #[test]
    fn v0_disambiguators_are_stripped_from_crate_names() {
        assert_eq!(bucket_for_name("wordcraft_engine[1f2a3b4c]::layout::break_lines"), "wordcraft-engine");
        assert_eq!(bucket_for_name("compiler_builtins::mem::memcpy"), "rust-std");
        assert_eq!(bucket_for_name("alloc::vec::Vec<T>::push"), "rust-std");
        assert_eq!(bucket_for_name("serde::ser::serialize"), "serde");
        assert_eq!(bucket_for_name("js_sys::Reflect::get"), "js_sys");
        assert_eq!(bucket_for_name("<str as core::fmt::Display>::fmt"), "other");
        assert_eq!(bucket_for_name("<usize as core::ops::Add>::add"), "other");
        assert_eq!(bucket_for_name(""), "other");
        assert_eq!(bucket_for_name("__rust_alloc"), "other");
    }

    #[test]
    fn missing_or_stripped_name_section_is_a_clear_error() {
        let bytes = wasm_bytes(&[section(10, &code_payload(&[8]))]);
        let err = analyze(PathBuf::from("t.wasm"), &bytes).unwrap_err();
        assert!(err.contains("stripped"), "{err}");
        assert!(err.contains("CARGO_PROFILE_WASM_RELEASE_STRIP=none"), "{err}");
    }

    #[test]
    fn malformed_and_truncated_inputs_error_rather_than_panic() {
        let cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"\0asX\x01\0\0\0".to_vec(),
            // section length beyond EOF
            wasm_bytes(&[vec![10, 0xff, 0x01]]),
            // code section: count says 2, data for 1
            wasm_bytes(&[section(10, &[2, 4, 0, 0, 0, 0])]),
            // code section: trailing bytes after the declared functions
            wasm_bytes(&[section(10, &[1, 1, 0, 0])]),
            // name string runs past the end of the name section
            wasm_bytes(&[section(10, &code_payload(&[1])), section(0, &[4, b'n', b'a', b'm', b'e', 1, 5, 1, 99, 5, b'f', b'u'])]),
            // name-map count says 2 but only one entry is present
            wasm_bytes(&[section(10, &code_payload(&[1])), section(0, &[4, b'n', b'a', b'm', b'e', 1, 5, 2, 0, 3, b'a', b':', b':', b'b'])]),
            // unterminated LEB128 runs into EOF
            wasm_bytes(&[vec![10, 0x80, 0x80, 0x80]]),
            // duplicate code sections
            wasm_bytes(&[section(10, &code_payload(&[1])), section(10, &code_payload(&[1])), name_section(&[(0, "a::b")])]),
            // non-UTF-8 function name
            wasm_bytes(&[section(10, &code_payload(&[1])), section(0, &[4, b'n', b'a', b'm', b'e', 1, 7, 1, 2, 0xff, 0xfe, 0x01])]),
        ];
        for (i, case) in cases.iter().enumerate() {
            let res = analyze(PathBuf::from(format!("case{i}.wasm")), case);
            assert!(res.is_err(), "case {i} should error: {case:02x?} -> {res:?}");
        }
    }

    #[test]
    fn missing_code_section_is_an_error() {
        let bytes = wasm_bytes(&[name_section(&[(0, "a::b")])]);
        let err = analyze(PathBuf::from("t.wasm"), &bytes).unwrap_err();
        assert!(err.contains("no code section"), "{err}");
    }

    #[test]
    fn zero_function_code_section_attributes_nothing() {
        let bytes = wasm_bytes(&[section(10, &code_payload(&[])), name_section(&[])]);
        let a = analyze(PathBuf::from("t.wasm"), &bytes).unwrap();
        assert_eq!(a.functions, 0);
        assert_eq!(a.function_bytes, 0);
        assert!(a.buckets.is_empty());
        assert!(render_text(&a).contains("TOTAL"));
        assert!(render_markdown(&a, "2025-01-01").contains("**TOTAL**"));
    }

    #[test]
    fn empty_function_still_counts_with_its_prefix() {
        let bytes = wasm_bytes(&[section(10, &code_payload(&[0, 3])), name_section(&[(0, "std::f"), (1, "wordcraft_doc::g")])]);
        let a = analyze(PathBuf::from("t.wasm"), &bytes).unwrap();
        assert_eq!(a.function_bytes, 1 + 4);
        assert_eq!(a.buckets.iter().map(|b| b.bytes).collect::<Vec<_>>(), vec![4, 1]);
    }

    #[test]
    fn parse_args_takes_exactly_one_wasm_path() {
        assert!(parse_args(&[]).unwrap().is_none());
        assert_eq!(parse_args(&["--wasm", "/tmp/a.wasm"]).unwrap(), Some(PathBuf::from("/tmp/a.wasm")));
        assert!(parse_args(&["--wasm"]).is_err());
        assert!(parse_args(&["--wasm", "a", "extra"]).is_err());
    }
}
