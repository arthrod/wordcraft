//! On-the-spot font packs (§1 of the deferred-items plan): faces the `minimal-wasm-fonts` build
//! doesn't bundle ship as `fonts/<stem>.<hash8>.ttf.br` beside the app and are fetched on demand.
//!
//! Two ways a face gets requested: automatically, when layout reports a *wanted* face (resolution
//! synthesized bold/italic on a family that has packs, or walked a pack family that isn't loaded;
//! see [`wordcraft_fonts::FontDb::take_wanted_packs`]), and manually through `font.installPack` or
//! `installFonts(...)`. Every fetch is of a file named in `fonts/manifest.json` under `assetBase`
//! — a document can never supply a URL. A face that fails to fetch, decode or install is
//! remembered for the session and not retried on each relayout; the user-font IndexedDB store is
//! never touched (Q1), so a reload re-requests the face and the HTTP cache answers.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wasm_bindgen_futures::JsFuture;

/// One entry of `fonts/manifest.json` (staged by `cargo xtask web-assets`).
#[derive(Clone, Debug)]
pub(crate) struct ManifestEntry {
    pub family: String,
    pub weight: u16,
    pub italic: bool,
    /// `fonts/<file>`, e.g. `SourceSans3-Bold.9f27c1d0.ttf.br`.
    pub file: String,
    /// Decoded TTF size; the decode is capped at this and must match it exactly.
    pub bytes: usize,
}

fn parse_manifest(text: &str) -> Option<Vec<ManifestEntry>> {
    let json: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut out = Vec::new();
    for e in json.get("faces")?.as_array()?.iter().take(64) {
        let family = e.get("family")?.as_str()?;
        let file = e.get("file")?.as_str()?;
        // File names come from our own staging: no separators or query tricks.
        if family.is_empty()
            || file.is_empty()
            || file.len() > 128
            || !file.ends_with(".ttf.br")
            || file.chars().any(|c| matches!(c, '/' | '\\' | '?' | '#' | '%') || c.is_control())
        {
            continue;
        }
        let weight = e.get("weight")?.as_u64()?;
        let bytes = e.get("bytes")?.as_u64()?;
        let entry = ManifestEntry {
            family: family.chars().take(256).collect(),
            weight: u16::try_from(weight).ok()?,
            italic: e.get("italic")?.as_bool()?,
            file: file.to_string(),
            bytes: usize::try_from(bytes).ok()?,
        };
        if entry.bytes > wordcraft_fonts::MAX_FONT_FILE_BYTES {
            continue;
        }
        out.push(entry);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parsing_is_bounded_and_filters_junk() {
        let good = parse_manifest(
            r#"{"faces":[{"family":"Inter","weight":600,"italic":false,"file":"Inter-SemiBold.abcd1234.ttf.br","bytes":1234,"compressedBytes":600}]}"#,
        )
        .unwrap();
        assert_eq!(good.len(), 1);
        assert_eq!(good[0].file, "Inter-SemiBold.abcd1234.ttf.br");
        assert_eq!(good[0].bytes, 1234);

        // Entries with a path or query in `file`, missing fields, or an absurd size are dropped.
        let junk = parse_manifest(
            r#"{"faces":[
              {"family":"X","weight":400,"italic":false,"file":"../index.html","bytes":10,"compressedBytes":1},
              {"family":"Y","weight":400,"italic":false,"file":"a.ttf.br?x=1","bytes":10,"compressedBytes":1},
              {"family":"Z","weight":400,"italic":false,"file":"z.ttf.br","bytes":999999999999,"compressedBytes":1},
              {"family":"W","weight":400,"italic":false,"file":"w.ttf.br","bytes":10},
              {"family":"V","weight":400,"italic":false,"file":"v.ttf","bytes":10,"compressedBytes":1}
            ]}"#,
        )
        .unwrap();
        assert!(junk.is_empty(), "{junk:?}");

        assert!(parse_manifest("not json").is_none());
        assert!(parse_manifest("{}").is_none());
    }
}

thread_local! {
    /// The parsed `fonts/manifest.json` (fetched lazily, on the first wanted face or manual ask).
    static MANIFEST: RefCell<Option<Rc<Vec<ManifestEntry>>>> = const { RefCell::new(None) };
    /// A manifest fetch is in flight.
    static MANIFEST_FETCHING: Cell<bool> = const { Cell::new(false) };
    /// The manifest fetch failed this session: automatic installs stay off (a manual ask retries).
    static MANIFEST_FAILED: Cell<bool> = const { Cell::new(false) };
    /// Manifest file names being fetched right now (one fetch per file at a time).
    static IN_FLIGHT: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    /// Manifest file names already tried this session, whatever the result: never retried.
    static ATTEMPTED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    /// `assetBase`, set once when the shell starts.
    static ASSET_BASE: RefCell<String> = const { RefCell::new(String::new()) };
    /// Resolves of `installFonts` promises still waiting for their last in-flight fetch.
    static WAITERS: RefCell<Vec<Waiter>> = RefCell::new(Vec::new());
    /// Status-line messages from background fetches (surfaced by the shell's message queue).
    static STATUS: RefCell<Option<Rc<RefCell<Vec<String>>>>> = const { RefCell::new(None) };
}

/// An `installFonts(...)` call waiting on the pack files it asked for.
struct Waiter {
    resolve: js_sys::Function,
    /// The manifest file names this call requested. The promise resolves when none of them is in
    /// flight any more and no fetch is in flight at all.
    files: HashSet<String>,
    /// (file, ok) outcomes recorded as this call's fetches finished.
    results: Rc<RefCell<Vec<(String, bool)>>>,
}

/// Record `asset_base` and the status queue at shell start.
pub(crate) fn init(asset_base: &str, messages: Rc<RefCell<Vec<String>>>) {
    ASSET_BASE.with(|b| *b.borrow_mut() = asset_base.to_string());
    STATUS.with(|s| *s.borrow_mut() = Some(messages));
}

fn asset_base() -> String {
    ASSET_BASE.with(|b| b.borrow().clone())
}

fn status(msg: String) {
    STATUS.with(|s| {
        if let Some(m) = s.borrow().as_ref() {
            m.borrow_mut().push(msg);
        }
    });
}

/// Fetch `url` and return its bytes (bounded: the caller checks the size before use; pack files
/// are capped by [`wordcraft_fonts::MAX_FONT_FILE_BYTES`] and the manifest's `bytes`).
async fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    let window = web_sys::window().ok_or("no window")?;
    let resp_val = JsFuture::from(window.fetch_with_str(url)).await.map_err(|e| format!("{e:?}"))?;
    let resp: web_sys::Response = resp_val.dyn_into().map_err(|_| "invalid Response".to_string())?;
    // The status alone proves nothing (a missing file may answer index.html, F12): the caller
    // judges success by the decode and the font check, never by HTTP status alone.
    let buf_prom = resp.array_buffer().map_err(|e| format!("{e:?}"))?;
    let buf_val = JsFuture::from(buf_prom).await.map_err(|e| format!("{e:?}"))?;
    Ok(js_sys::Uint8Array::new(&buf_val).to_vec())
}

/// Load `fonts/manifest.json` once, on first use.
fn ensure_manifest(ctx: egui::Context) {
    let loading = MANIFEST_FETCHING.with(|c| c.get());
    let failed = MANIFEST_FAILED.with(|c| c.get());
    let have = MANIFEST.with(|m| m.borrow().is_some());
    if loading || failed || have {
        return;
    }
    MANIFEST_FETCHING.with(|c| c.set(true));
    let url = format!("{}fonts/manifest.json", asset_base());
    wasm_bindgen_futures::spawn_local(async move {
        let window = web_sys::window();
        let text = match window {
            Some(w) => {
                let resp_val = JsFuture::from(w.fetch_with_str(&url)).await;
                match resp_val {
                    Ok(v) => v.dyn_into::<web_sys::Response>().ok().and_then(|r| r.text().ok()).map(|p| {
                        // A future is not Send; await inside the same async block.
                        p
                    }),
                    Err(_) => None,
                }
            }
            None => None,
        };
        let text = match text {
            Some(prom) => JsFuture::from(prom).await.ok().and_then(|v| v.as_string()),
            None => None,
        };
        let parsed = text.as_deref().and_then(parse_manifest).filter(|f| !f.is_empty());
        MANIFEST_FETCHING.with(|c| c.set(false));
        match parsed {
            Some(faces) => {
                MANIFEST.with(|m| *m.borrow_mut() = Some(Rc::new(faces)));
                MANIFEST_FAILED.with(|c| c.set(false));
            }
            None => {
                log::warn!("font packs: could not read {url}");
                MANIFEST_FAILED.with(|c| c.set(true));
            }
        }
        // New wanted faces may have arrived while the manifest loaded.
        ctx.request_repaint();
    });
}

fn manifest() -> Option<Rc<Vec<ManifestEntry>>> {
    MANIFEST.with(|m| m.borrow().clone())
}

/// The manifest entry for a wanted face, or `None` when no pack can satisfy it (e.g. bold italic
/// sans — there is no `SourceSans3-BoldIt`, so it stays synthesized).
fn match_entry(manifest: &[ManifestEntry], wanted: &wordcraft_fonts::WantedPackFace) -> Option<ManifestEntry> {
    let pack = wordcraft_fonts::PACK_FACES
        .iter()
        .filter(|f| f.family.eq_ignore_ascii_case(&wanted.family))
        .find(|f| {
            // Match the rule `packs::pick_pack_face` applies to manifest faces.
            let wanted_weight = if wanted.bold { 700u16 } else { 400 };
            f.italic == wanted.italic && wordcraft_fonts::PACK_FACES.iter().all(|g| {
                g.family.eq_ignore_ascii_case(&wanted.family)
                    && !(g.italic == wanted.italic && (g.weight as i32 - wanted_weight as i32).abs() < (f.weight as i32 - wanted_weight as i32).abs()
                        || g.italic == wanted.italic && (g.weight as i32 - wanted_weight as i32).abs() == (f.weight as i32 - wanted_weight as i32).abs() && g.weight > f.weight)
            })
        })?;
    manifest
        .iter()
        .find(|e| e.family.eq_ignore_ascii_case(pack.family) && e.weight == pack.weight && e.italic == pack.italic)
        .cloned()
}

/// Manifest entries for a requested family name (all faces of that family), or `None` when the
/// family has no packs.
fn entries_for_family(manifest: &[ManifestEntry], family: &str) -> Vec<ManifestEntry> {
    manifest.iter().filter(|e| e.family.eq_ignore_ascii_case(family)).cloned().collect()
}

fn in_flight(file: &str) -> bool {
    IN_FLIGHT.with(|s| s.borrow().contains(file))
}

fn attempted(file: &str) -> bool {
    ATTEMPTED.with(|s| s.borrow().contains(file))
}

fn is_installed(entry: &ManifestEntry) -> bool {
    wordcraft_fonts::FontDb::global().pack_fonts().iter().any(|(f, _)| f.eq_ignore_ascii_case(&entry.family))
        && installed_face(entry)
}

/// The face `entry` names is loaded: its (family, style-ish) weight/italic already resolves.
fn installed_face(entry: &ManifestEntry) -> bool {
    wordcraft_fonts::FontDb::global()
        .pack_fonts()
        .iter()
        .any(|(f, s)| f.eq_ignore_ascii_case(&entry.family) && style_matches(s, entry.weight, entry.italic))
}

/// Does an installed face's style name match (weight, italic)? Pack faces carry standard style
/// names from their `name` table (Regular, Italic, Bold, Bold Italic, Semibold).
fn style_matches(style: &str, weight: u16, italic: bool) -> bool {
    let s = style.to_ascii_lowercase();
    let has_italic = s.contains("italic") || s == "it";
    if has_italic != italic {
        return false;
    }
    match weight {
        w if w >= 700 => s.contains("bold") || s == "it" && false,
        600..=699 => s.contains("semibold") || s.contains("demibold") || s.contains("bold"),
        _ => true,
    }
}

/// Fetch, decode and install `entries` in one task turn: one repaint per batch, not per face
/// (`WordApp::watch_fonts` lays out once on the generation bump the installs cause).
fn fetch_and_install(entries: Vec<ManifestEntry>, ctx: egui::Context) {
    let files: HashSet<String> = entries.iter().map(|e| e.file.clone()).collect();
    IN_FLIGHT.with(|s| s.borrow_mut().extend(files.clone()));
    ATTEMPTED.with(|s| s.borrow_mut().extend(files));
    let base = asset_base();
    wasm_bindgen_futures::spawn_local(async move {
        let mut failed = 0usize;
        for entry in &entries {
            let url = format!("{base}fonts/{}", entry.file);
            let ok = async {
                let compressed = fetch_bytes(&url).await?;
                // Judge by the decode and the font check, never by the HTTP status alone (F12).
                let ttf = wordcraft_fonts::decode_pack_bytes(&compressed, entry.bytes).map_err(|e| format!("{e}"))?;
                match wordcraft_fonts::FontDb::global().install_pack_font(&entry.file, &ttf) {
                    Ok(_) => Ok(()),
                    Err(wordcraft_fonts::UserFontError::AlreadyAvailable(_)) => Ok(()),
                    Err(e) => Err(e.to_string()),
                }
            }
            .await;
            match ok {
                Ok(()) => record_result(&entry.file, true),
                Err(e) => {
                    failed += 1;
                    log::warn!("font pack {}: {e}", entry.file);
                    record_result(&entry.file, false);
                }
            }
            IN_FLIGHT.with(|s| s.borrow_mut().remove(&entry.file));
        }
        if failed > 0 {
            status(wordcraft_ui_egui::i18n::t("Some fonts could not be downloaded.").to_string());
        }
        ctx.request_repaint();
        flush_waiters();
    });
}

fn record_result(file: &str, ok: bool) {
    WAITERS.with(|w| {
        for waiter in w.borrow().iter() {
            if waiter.files.contains(file) {
                waiter.results.borrow_mut().push((file.to_string(), ok));
            }
        }
    });
}

/// Resolve every waiter whose requested files are no longer in flight. A waiter with no file
/// list (`installFonts()` with no argument — "wait for the downloads to settle") resolves when
/// nothing is in flight at all.
fn flush_waiters() {
    let in_flight_empty = IN_FLIGHT.with(|s| s.borrow().is_empty());
    let mut ready = Vec::new();
    WAITERS.with(|w| {
        let mut keep = Vec::new();
        for waiter in w.borrow_mut().drain(..) {
            let done = if waiter.files.is_empty() { in_flight_empty } else { waiter.files.iter().all(|f| !in_flight(f)) };
            if done {
                ready.push(waiter);
            } else {
                keep.push(waiter);
            }
        }
        *w.borrow_mut() = keep;
    });
    for waiter in ready {
        let results = waiter.results.borrow();
        let installed: Vec<&str> = results.iter().filter(|(_, ok)| *ok).map(|(f, _)| f.as_str()).collect();
        let failed: Vec<&str> = results.iter().filter(|(_, ok)| !*ok).map(|(f, _)| f.as_str()).collect();
        let out = js_sys::Object::new();
        let to_arr = |v: &[&str]| v.iter().fold(js_sys::Array::new(), |a, s| {
            a.push(&wasm_bindgen::JsValue::from_str(s));
            a
        });
        let _ = js_sys::Reflect::set(&out, &wasm_bindgen::JsValue::from_str("installed"), &to_arr(&installed));
        let _ = js_sys::Reflect::set(&out, &wasm_bindgen::JsValue::from_str("failed"), &to_arr(&failed));
        let _ = waiter.resolve.call1(&wasm_bindgen::JsValue::NULL, &out);
    }
}

/// Drain the wanted-face record and fetch anything new. Called once per frame after layout ran:
/// the record changes only then, so this is not a poll loop.
pub(crate) fn drain_wanted(ctx: &egui::Context) {
    let wanted = wordcraft_fonts::FontDb::global().take_wanted_packs();
    if wanted.is_empty() {
        return;
    }
    let Some(m) = manifest() else {
        ensure_manifest(ctx.clone());
        return;
    };
    let needed: Vec<ManifestEntry> = wanted
        .iter()
        .filter_map(|w| match_entry(&m, w))
        .filter(|e| !attempted(&e.file) && !in_flight(&e.file) && !is_installed(e))
        .collect();
    if !needed.is_empty() {
        fetch_and_install(needed, ctx.clone());
    }
}

/// The `install_font_packs` service callback and `font.installPack`'s work: fetch every pack face
/// of `families` (all pack faces when empty). Unknown family names are reported, not fatal.
pub(crate) fn install_families(families: Vec<String>, ctx: egui::Context) {
    let Some(m) = manifest() else {
        // A manual ask retries a manifest that failed earlier.
        MANIFEST_FAILED.with(|c| c.set(false));
        ensure_manifest(ctx.clone());
        status(wordcraft_ui_egui::i18n::t("Downloading fonts…").to_string());
        ctx.request_repaint();
        return;
    };
    let entries: Vec<ManifestEntry> = if families.is_empty() {
        m.iter().cloned().collect()
    } else {
        let mut out = Vec::new();
        for family in &families {
            out.extend(entries_for_family(&m, family));
        }
        out
    };
    let needed: Vec<ManifestEntry> = entries.into_iter().filter(|e| !attempted(&e.file) && !in_flight(&e.file) && !is_installed(e)).collect();
    if needed.is_empty() {
        status(wordcraft_ui_egui::i18n::t("All fonts are already available.").to_string());
        ctx.request_repaint();
        return;
    }
    fetch_and_install(needed, ctx);
}

/// `installFonts(families?)` on the JavaScript API: resolves when no pack fetch is in flight.
/// With `families` it requests those families' pack faces first; without it is the wait layout
/// calls need (a bold face arriving can move line and page breaks).
pub(crate) fn install_fonts_promise(families: Option<Vec<String>>, ctx: egui::Context) -> js_sys::Promise {
    let files: HashSet<String> = match &families {
        Some(list) => manifest()
            .map(|m| list.iter().flat_map(|f| entries_for_family(&m, f)).map(|e| e.file).collect())
            .unwrap_or_default(),
        None => HashSet::new(),
    };
    if let Some(list) = &families {
        install_families(list.clone(), ctx);
    } else {
        drain_wanted(&ctx);
    }
    js_sys::Promise::new(&mut |resolve, _reject| {
        let waiter = Waiter { resolve, files, results: Rc::new(RefCell::new(Vec::new())) };
        WAITERS.with(|w| w.borrow_mut().push(waiter));
        // Maybe nothing needs fetching at all.
        flush_waiters();
    })
}
