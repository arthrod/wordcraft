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

use crate::font_pack_manifest::{ManifestEntry, entries_for_family, parse_manifest, style_matches};

/// The manifest entry implements the crate's face-like trait, so the matching rule is
/// [`wordcraft_fonts::packs::pick_pack_face`]'s alone (family, italic, weight closest to the
/// wanted boldness) rather than a second copy in the shell.
impl wordcraft_fonts::packs::PackFaceLike for ManifestEntry {
    fn family(&self) -> &str {
        &self.family
    }
    fn weight(&self) -> u16 {
        self.weight
    }
    fn italic(&self) -> bool {
        self.italic
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
    /// Manual installs asked for before the manifest arrived, honoured once it loads.
    static PENDING_FAMILIES: RefCell<Vec<(Vec<String>, egui::Context)>> = RefCell::new(Vec::new());
    /// Wanted faces drained while the manifest was still loading (the record drains on read, so
    /// they're kept here rather than lost).
    static PENDING_WANTED: RefCell<Vec<wordcraft_fonts::WantedPackFace>> = RefCell::new(Vec::new());
    /// `assetBase`, set once when the shell starts.
    static ASSET_BASE: RefCell<String> = const { RefCell::new(String::new()) };
    /// Resolves of `installFonts` promises still waiting on their last in-flight fetch.
    static WAITERS: RefCell<Vec<Waiter>> = RefCell::new(Vec::new());
    /// Status-line messages from background fetches (surfaced by the shell's message queue).
    static STATUS: RefCell<Option<Rc<RefCell<Vec<String>>>>> = const { RefCell::new(None) };
}

/// An `installFonts(families)` call waiting on the pack files it asked for.
struct Waiter {
    resolve: js_sys::Function,
    /// The family names the call asked about (`None`: `installFonts()` — resolve when nothing is
    /// in flight at all; `Some`: resolve when the files those families need have finished).
    families: Option<Vec<String>>,
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

/// Fetch `url` and return its bytes (bounded by the caller's checks on `bytes` and the font
/// caps inside `install_pack_font`).
async fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    let window = web_sys::window().ok_or("no window")?;
    let resp_val = JsFuture::from(window.fetch_with_str(url)).await.map_err(|e| format!("{e:?}"))?;
    let resp: web_sys::Response = resp_val.dyn_into().map_err(|_| "invalid Response".to_string())?;
    let buf_prom = resp.array_buffer().map_err(|e| format!("{e:?}"))?;
    let buf_val = JsFuture::from(buf_prom).await.map_err(|e| format!("{e:?}"))?;
    Ok(js_sys::Uint8Array::new(&buf_val).to_vec())
}

/// Fetch `url` and return its text, or `None` on any failure.
async fn fetch_text(url: &str) -> Option<String> {
    let window = web_sys::window()?;
    let resp_val = JsFuture::from(window.fetch_with_str(url)).await.ok()?;
    let resp: web_sys::Response = resp_val.dyn_into().ok()?;
    let prom = resp.text().ok()?;
    JsFuture::from(prom).await.ok()?.as_string()
}

/// Load `fonts/manifest.json` once, on first use. Honours `PENDING_FAMILIES` when it arrives.
fn ensure_manifest(ctx: egui::Context) {
    let loading = MANIFEST_FETCHING.with(|c| c.get());
    let have = MANIFEST.with(|m| m.borrow().is_some());
    if loading || have {
        return;
    }
    MANIFEST_FETCHING.with(|c| c.set(true));
    let url = format!("{}fonts/manifest.json", asset_base());
    wasm_bindgen_futures::spawn_local(async move {
        let text = fetch_text(&url).await;
        let parsed = text.as_deref().and_then(parse_manifest).filter(|f| !f.is_empty());
        MANIFEST_FETCHING.with(|c| c.set(false));
        match parsed {
            Some(faces) => {
                MANIFEST.with(|m| *m.borrow_mut() = Some(Rc::new(faces)));
                MANIFEST_FAILED.with(|c| c.set(false));
                // Installs asked for while the manifest loaded can run now.
                for (families, ctx) in PENDING_FAMILIES.with(|p| std::mem::take(&mut *p.borrow_mut())) {
                    install_families(families, ctx);
                }
            }
            None => {
                log::warn!("font packs: could not read {url}");
                MANIFEST_FAILED.with(|c| c.set(true));
            }
        }
        // New wanted faces may have arrived while the manifest loaded.
        ctx.request_repaint();
        flush_waiters();
    });
}

fn manifest() -> Option<Rc<Vec<ManifestEntry>>> {
    MANIFEST.with(|m| m.borrow().clone())
}

fn in_flight(file: &str) -> bool {
    IN_FLIGHT.with(|s| s.borrow().contains(file))
}

fn attempted(file: &str) -> bool {
    ATTEMPTED.with(|s| s.borrow().contains(file))
}

/// The face `entry` names is already loaded (a pack install keeps its `(family, style)`).
fn is_installed(entry: &ManifestEntry) -> bool {
    wordcraft_fonts::FontDb::global()
        .pack_fonts()
        .iter()
        .any(|(family, style)| family.eq_ignore_ascii_case(&entry.family) && style_matches(style, entry))
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
            let result = async {
                let compressed = fetch_bytes(&url).await?;
                // Judge success by the decode and the font check, never by the HTTP status alone
                // (F12: a missing file on Pages can answer `index.html` with status 200).
                let ttf = wordcraft_fonts::decode_pack_bytes(&compressed, entry.bytes).map_err(|e| e.to_string())?;
                match wordcraft_fonts::FontDb::global().install_pack_font(&entry.file, &ttf) {
                    Ok(_) | Err(wordcraft_fonts::UserFontError::AlreadyAvailable(_)) => Ok(()),
                    Err(e) => Err(e.to_string()),
                }
            }
            .await;
            match result {
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
            let Some(families) = &waiter.families else {
                continue;
            };
            if manifest().is_some_and(|m| families.iter().any(|f| entries_for_family(&m, f).iter().any(|e| e.file == file))) {
                waiter.results.borrow_mut().push((file.to_string(), ok));
            }
        }
    });
}

/// Resolve every waiter whose work has settled. A waiter with `families: None`
/// (`installFonts()` — "wait for the downloads to settle") resolves when no fetch is in flight
/// and nothing is queued behind the manifest; a `Some(families)` waiter resolves when the files
/// its families need are no longer in flight and the manifest answered (or failed for good).
fn flush_waiters() {
    let in_flight_empty = IN_FLIGHT.with(|s| s.borrow().is_empty());
    let pending_empty = PENDING_FAMILIES.with(|p| p.borrow().is_empty());
    let manifest_done = MANIFEST.with(|m| m.borrow().is_some()) || MANIFEST_FAILED.with(|c| c.get());
    let mut ready = Vec::new();
    WAITERS.with(|w| {
        let mut keep = Vec::new();
        for waiter in w.borrow_mut().drain(..) {
            let done = match &waiter.families {
                None => in_flight_empty && pending_empty,
                Some(families) => {
                    manifest_done
                        && pending_empty
                        && manifest().is_none_or(|m| {
                            families
                                .iter()
                                .flat_map(|f| entries_for_family(&m, f))
                                .all(|e| !in_flight(&e.file) && (attempted(&e.file) || is_installed(e)))
                        })
                }
            };
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
        let to_arr = |v: &[&str]| {
            v.iter().fold(js_sys::Array::new(), |a, s| {
                a.push(&wasm_bindgen::JsValue::from_str(s));
                a
            })
        };
        let _ = js_sys::Reflect::set(&out, &wasm_bindgen::JsValue::from_str("installed"), &to_arr(&installed));
        let _ = js_sys::Reflect::set(&out, &wasm_bindgen::JsValue::from_str("failed"), &to_arr(&failed));
        let _ = waiter.resolve.call1(&wasm_bindgen::JsValue::NULL, &out);
    }
}

/// Drain the wanted-face record and fetch anything new. Called once per frame after layout ran:
/// the record changes only then, so this is not a poll loop.
pub(crate) fn drain_wanted(ctx: &egui::Context) {
    let wanted = wordcraft_fonts::FontDb::global().take_wanted_packs();
    PENDING_WANTED.with(|p| {
        let mut p = p.borrow_mut();
        for w in wanted {
            if !p.iter().any(|q| q.family.eq_ignore_ascii_case(&w.family) && q.bold == w.bold && q.italic == w.italic) && p.len() < 64 {
                p.push(w);
            }
        }
    });
    let Some(m) = manifest() else {
        if PENDING_WANTED.with(|p| !p.borrow().is_empty()) {
            ensure_manifest(ctx.clone());
        }
        return;
    };
    let wanted = PENDING_WANTED.with(|p| std::mem::take(&mut *p.borrow_mut()));
    let needed: Vec<ManifestEntry> = wanted
        .iter()
        .filter_map(|w| wordcraft_fonts::packs::pick_pack_face(&m, w).map(|i| m[i].clone()))
        .filter(|e| !attempted(&e.file) && !in_flight(&e.file) && !is_installed(e))
        .collect();
    if !needed.is_empty() {
        fetch_and_install(needed, ctx.clone());
    }
}

/// The `install_font_packs` service callback and `font.installPack`'s work: fetch every pack face
/// of `families` (all pack faces when empty). Unknown family names fetch nothing and are left to
/// the result/status reporting.
pub(crate) fn install_families(families: Vec<String>, ctx: egui::Context) {
    let Some(m) = manifest() else {
        // A manual ask retries a manifest whose fetch failed earlier.
        MANIFEST_FAILED.with(|c| c.set(false));
        PENDING_FAMILIES.with(|p| p.borrow_mut().push((families, ctx.clone())));
        ensure_manifest(ctx.clone());
        status(wordcraft_ui_egui::i18n::t("Downloading fonts…").to_string());
        ctx.request_repaint();
        return;
    };
    let needed: Vec<ManifestEntry> =
        if families.is_empty() { m.iter().collect::<Vec<_>>() } else { families.iter().flat_map(|f| entries_for_family(&m, f)).collect() }
            .into_iter()
            .filter(|e| !attempted(&e.file) && !in_flight(&e.file) && !is_installed(e))
            .cloned()
            .collect();
    if needed.is_empty() {
        status(wordcraft_ui_egui::i18n::t("All fonts are already available.").to_string());
        ctx.request_repaint();
        flush_waiters();
        return;
    }
    fetch_and_install(needed, ctx);
}

/// `installFonts(families?)` on the JavaScript API: resolves when no pack fetch is in flight.
/// With `families` it requests those families' pack faces first; without it is the wait layout
/// calls need (a bold face arriving can move line and page breaks).
pub(crate) fn install_fonts_promise(families: Option<Vec<String>>, ctx: egui::Context) -> js_sys::Promise {
    if let Some(list) = &families {
        install_families(list.clone(), ctx);
    } else {
        drain_wanted(&ctx);
    }
    js_sys::Promise::new(&mut |resolve, _reject| {
        WAITERS.with(|w| w.borrow_mut().push(Waiter { resolve, families: families.clone(), results: Rc::new(RefCell::new(Vec::new())) }));
        // Maybe nothing needs fetching at all.
        flush_waiters();
    })
}
