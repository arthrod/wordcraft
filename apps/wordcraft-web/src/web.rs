//! The browser shell: web `Services`, drag-and-drop, the unsaved-changes guard and the eframe
//! web runner.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wordcraft_engine::Session;
use wordcraft_ui_egui::async_saves::{AsyncSaves, SaveOutcome};
use wordcraft_ui_egui::{Inbox, Services, WordApp};

thread_local! {
    /// Locale fetches in flight, so concurrent asks don't duplicate one. Not a record of
    /// successes: a registered locale is guarded by `i18n::locale_registered`, and a failed
    /// or empty fetch removes its marker so a later ask can retry.
    static LOADING_LOCALES: RefCell<HashSet<&'static str>> = RefCell::new(HashSet::new());
}

fn default_asset_base(document: &web_sys::Document) -> String {
    document
        .base_uri()
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .and_then(|base| web_sys::Url::new_with_base("./", &base).ok().map(|u| u.href()))
        .map_or_else(|| "./".to_string(), |resolved| normalize_asset_base(&resolved))
}

pub(crate) fn normalize_asset_base(base: &str) -> String {
    let trimmed = base.trim();
    if trimmed.is_empty() {
        "./".to_string()
    } else if trimmed.ends_with('/') {
        trimmed.to_string()
    } else {
        format!("{trimmed}/")
    }
}

async fn fetch_text(window: &web_sys::Window, url: &str) -> Result<String, String> {
    let resp_val = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url)).await.map_err(|e| format!("{e:?}"))?;
    let resp: web_sys::Response = resp_val.dyn_into().map_err(|_| "fetch did not return a Response".to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {} for {url}", resp.status()));
    }
    let text_prom = resp.text().map_err(|e| format!("{e:?}"))?;
    let text_val = wasm_bindgen_futures::JsFuture::from(text_prom).await.map_err(|e| format!("{e:?}"))?;
    Ok(text_val.as_string().unwrap_or_default())
}

pub(crate) fn ensure_locale_loaded(pref: &str, asset_base: &str, ctx: egui::Context) {
    let code = wordcraft_ui_egui::i18n::target_locale_code(pref);
    if code == "en" || wordcraft_ui_egui::i18n::locale_registered(code) {
        return;
    }
    let in_flight = LOADING_LOCALES.with(|s| s.borrow_mut().insert(code));
    if !in_flight {
        return;
    }
    let base = normalize_asset_base(asset_base);
    wasm_bindgen_futures::spawn_local(async move {
        let Some(window) = web_sys::window() else {
            LOADING_LOCALES.with(|s| s.borrow_mut().remove(code));
            return;
        };
        let common_url = format!("{base}i18n/{code}/common.tsv");
        let app_url = format!("{base}i18n/{code}/wordcraft.tsv");
        let (common_res, app_res) = (fetch_text(&window, &common_url).await, fetch_text(&window, &app_url).await);
        match (common_res, app_res) {
            (Ok(common_tsv), Ok(app_tsv)) => {
                let has_entries = app_tsv.lines().any(|line| {
                    let t = line.trim();
                    !t.is_empty() && !t.starts_with('#')
                });
                if has_entries {
                    wordcraft_ui_egui::i18n::register_locale(code, &common_tsv, &app_tsv);
                    ctx.request_repaint();
                }
            }
            (Err(e), _) | (_, Err(e)) => {
                log::warn!("i18n: could not load locale `{code}` ({e}); keeping English");
            }
        }
        // Done one way or another: the marker only deduped this fetch. A registered locale is
        // protected by `locale_registered`; anything else may be retried by a later ask.
        LOADING_LOCALES.with(|s| s.borrow_mut().remove(code));
    });
}

const DOC_EXTS: &[&str] =
    &["docx", "docm", "dotx", "dotm", "doc", "dot", "odt", "ott", "fodt", "rtf", "txt", "md", "html", "htm", "mht", "mhtml", "tex", "json", "xml"];
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "wmf", "emf"];
const RECIPIENT_EXTS: &[&str] = wordcraft_engine::cmd::mailings::RECIPIENT_EXTENSIONS;
pub(crate) mod codec_worker;
pub(crate) mod font_packs;
mod font_store;
pub(crate) mod lazy_io;
pub(crate) mod pdf_worker;
pub(crate) mod proof_worker;

/// A dedicated worker's `error` event in words, for the log. A script error is an `ErrorEvent`
/// with a message and a place. A worker whose script could not be fetched (a 404, no network)
/// fires a plain `Event` with neither: reading them through `ErrorEvent`'s string getters
/// throws out of the handler, before it has failed the requests that were waiting — so the
/// fields are read as whatever they are.
pub(crate) fn worker_error_text(event: &web_sys::Event) -> String {
    let field = |key: &str| js_sys::Reflect::get(event, &wasm_bindgen::JsValue::from_str(key)).ok();
    match field("message").and_then(|v| v.as_string()).filter(|m| !m.is_empty()) {
        Some(message) => {
            let file = field("filename").and_then(|v| v.as_string()).unwrap_or_default();
            let line = field("lineno").and_then(|v| v.as_f64()).unwrap_or_default();
            format!("{message} ({file}:{line})")
        }
        None => "its script could not be loaded".to_string(),
    }
}

/// Why an on-demand worker failed its waiting requests: before its `ready` notice its files never
/// arrived or would not run; after it, it died with work in hand (out of memory on a large
/// document, a fault). `lazy_formats::failure_message` words the two differently.
pub(crate) fn worker_failure(worker: &str, ready: bool) -> String {
    if ready {
        format!("the {worker} worker {}", crate::lazy_formats::STOPPED_WORKING)
    } else {
        format!("the {worker} worker {}", crate::lazy_formats::NOT_LOADED)
    }
}

/// Read one `[headerJson, payloadBuffer]` message from the `worker` worker, checked as the
/// workers check a request ([`crate::lazy_formats::inbound`]): it is an array, its header a
/// string no longer than a JSON payload, its payload an `ArrayBuffer` or `Uint8Array` within the
/// reply kind's bound — all before anything is copied. A reply that fails the checks comes back
/// as a typed `error` reply for its own job; a message that answers no job is logged and `None`.
pub(crate) fn read_worker_message(worker: &str, data: &wasm_bindgen::JsValue) -> Option<(wordcraft_worker_proto::Envelope, Vec<u8>)> {
    use crate::lazy_formats::{Inbound, inbound};
    // `is_array` before the cast: an array-like object never goes through `Array.from`.
    let (header, payload) = if js_sys::Array::is_array(data) {
        let arr: &js_sys::Array = data.unchecked_ref();
        (arr.get(0), arr.get(1))
    } else {
        (wasm_bindgen::JsValue::UNDEFINED, wasm_bindgen::JsValue::UNDEFINED)
    };
    let limit = u32::try_from(wordcraft_worker_proto::MAX_PAYLOAD_BYTES).unwrap_or(u32::MAX);
    let header = header.dyn_ref::<js_sys::JsString>().filter(|s| s.length() <= limit).and_then(|s| s.as_string());
    // A `Uint8Array` (what the workers post) is used as it is: `new Uint8Array(typedArray)` would
    // copy every byte (ECMA-262 InitializeTypedArrayFromTypedArray clones the buffer) before the
    // length check. An `ArrayBuffer` gets a view, which copies nothing.
    let bytes = match payload.dyn_into::<js_sys::Uint8Array>() {
        Ok(view) => Some(view),
        Err(other) if other.is_instance_of::<js_sys::ArrayBuffer>() => Some(js_sys::Uint8Array::new(&other)),
        Err(_) => None,
    };
    match inbound(header.as_deref(), bytes.as_ref().map(|b| b.length() as usize)) {
        Inbound::Route(env) => {
            // `ready` carries nothing read; anything else is within its bound by now.
            let copied = if env.kind == wordcraft_worker_proto::Kind::Ready { Vec::new() } else { bytes.map(|b| b.to_vec()).unwrap_or_default() };
            Some((env, copied))
        }
        Inbound::Refuse(env, reply) => {
            log::warn!("{worker} worker: {}", reply.message);
            let payload = wordcraft_worker_proto::encode_payload(&reply).ok()?;
            Some((env, payload))
        }
        Inbound::Drop(why) => {
            log::warn!("{worker} worker: dropped {why}");
            None
        }
    }
}

const CANVAS_ID: &str = "wordcraft_canvas";
const LOADING_ID: &str = "wordcraft_loading";

thread_local! {
    static RUNTIME_INIT: Cell<bool> = const { Cell::new(false) };
    static STARTED_CANVASES: RefCell<crate::launch::LaunchBook<web_sys::HtmlCanvasElement, js_sys::Promise>> =
        const { RefCell::new(crate::launch::LaunchBook::new()) };
}

/// Canvas identity for [`STARTED_CANVASES`]: the same DOM element, even across wrappers.
fn same_canvas(a: &web_sys::HtmlCanvasElement, b: &web_sys::HtmlCanvasElement) -> bool {
    js_sys::Object::is(a.as_ref(), b.as_ref())
}

struct StartConfig {
    canvas: web_sys::HtmlCanvasElement,
    asset_base: String,
    host_mode: Option<String>,
    author: Option<String>,
    install_global: bool,
    force_sample: Option<bool>,
}

fn init_runtime_once() {
    RUNTIME_INIT.with(|done| {
        if done.get() {
            return;
        }
        done.set(true);
        eframe::WebLogger::init(log::LevelFilter::Info).ok();
        // `wasm_now_ms` and `wasm_epoch_ms` both read `Date::now()` here. For `wasm_now_ms` that is
        // wall-clock time, not a monotonic clock: it can decrease (or jump forward) when the system
        // clock is adjusted, so elapsed-time readers must tolerate a negative difference.
        wordcraft_geom::register_wasm_clock(js_sys::Date::now, js_sys::Date::now, |unix| {
            let ms = (unix as f64) * 1000.0;
            if !ms.is_finite() {
                return 0;
            }
            let west_min = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms)).get_timezone_offset();
            if west_min.is_finite() && west_min.abs() <= 26.0 * 60.0 { -(west_min as i64) * 60 } else { 0 }
        });
        let now = js_sys::Date::now();
        wordcraft_geom::set_wasm_clock(now, now);
    });
}

fn opt_str_prop(obj: &wasm_bindgen::JsValue, key: &str) -> Result<Option<String>, wasm_bindgen::JsValue> {
    let val = js_sys::Reflect::get(obj, &wasm_bindgen::JsValue::from_str(key)).unwrap_or(wasm_bindgen::JsValue::UNDEFINED);
    if val.is_undefined() || val.is_null() {
        Ok(None)
    } else if let Some(s) = val.as_string() {
        Ok(Some(s))
    } else {
        Err(js_sys::Error::new(&format!("start(options): `{key}` must be a string")).into())
    }
}

fn opt_bool_prop(obj: &wasm_bindgen::JsValue, key: &str) -> Result<Option<bool>, wasm_bindgen::JsValue> {
    let val = js_sys::Reflect::get(obj, &wasm_bindgen::JsValue::from_str(key)).unwrap_or(wasm_bindgen::JsValue::UNDEFINED);
    if val.is_undefined() || val.is_null() {
        Ok(None)
    } else if let Some(b) = val.as_bool() {
        Ok(Some(b))
    } else {
        Err(js_sys::Error::new(&format!("start(options): `{key}` must be a boolean")).into())
    }
}

fn resolve_canvas(document: &web_sys::Document, value: wasm_bindgen::JsValue) -> Result<web_sys::HtmlCanvasElement, wasm_bindgen::JsValue> {
    if value.is_undefined() || value.is_null() {
        return document
            .get_element_by_id(CANVAS_ID)
            .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
            .ok_or_else(|| js_sys::Error::new(&format!("missing <canvas id=\"{CANVAS_ID}\">")).into());
    }
    if let Some(raw_id) = value.as_string() {
        let id = raw_id.strip_prefix('#').unwrap_or(&raw_id);
        return document
            .get_element_by_id(id)
            .and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok())
            .ok_or_else(|| js_sys::Error::new(&format!("missing canvas element `{raw_id}`")).into());
    }
    value
        .dyn_into::<web_sys::HtmlCanvasElement>()
        .map_err(|_| js_sys::Error::new("start(options): `canvas` must be an HTMLCanvasElement or element id string").into())
}

fn parse_start_config(options: &wasm_bindgen::JsValue) -> Result<StartConfig, wasm_bindgen::JsValue> {
    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or_else(|| wasm_bindgen::JsValue::from(js_sys::Error::new("no browser document available")))?;

    if options.is_undefined() || options.is_null() {
        return Ok(StartConfig {
            canvas: resolve_canvas(&document, wasm_bindgen::JsValue::UNDEFINED)?,
            asset_base: default_asset_base(&document),
            host_mode: None,
            author: query_param("author"),
            install_global: false,
            force_sample: None,
        });
    }
    if !options.is_object() {
        return Err(js_sys::Error::new("start(options): expected an options object").into());
    }

    let canvas_val = js_sys::Reflect::get(options, &wasm_bindgen::JsValue::from_str("canvas")).unwrap_or(wasm_bindgen::JsValue::UNDEFINED);
    let canvas = resolve_canvas(&document, canvas_val)?;
    let asset_base = opt_str_prop(options, "assetBase")?.map(|b| normalize_asset_base(&b)).unwrap_or_else(|| default_asset_base(&document));
    let host_mode = opt_str_prop(options, "host")?;
    let author = opt_str_prop(options, "author")?.or_else(|| query_param("author"));
    let install_global = opt_bool_prop(options, "installGlobal")?.unwrap_or(false);
    let force_sample = opt_bool_prop(options, "sample")?;

    Ok(StartConfig { canvas, asset_base, host_mode, author, install_global, force_sample })
}

/// Start WordCraft automatically when the standalone `#wordcraft_canvas` element is present.
/// Importing the WASM module into a host page without `#wordcraft_canvas` is a harmless no-op;
/// programmatic callers use [`start`]. The launch itself is deferred to [`default_launch`]: a
/// page that calls `start(options)` — the npm wrapper awaits module init before forwarding
/// them — registers the canvas with its explicit options first, and the default launch then
/// stands down for it (the first launch of a canvas owns it).
pub fn start_default_if_present() {
    init_runtime_once();
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(document) = window.document() else {
        return;
    };
    if document.get_element_by_id(CANVAS_ID).is_none() {
        return;
    }
    let deferred = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(default_launch);
    let scheduled = window.set_timeout_with_callback(deferred.as_ref().unchecked_ref()).is_ok();
    // The closure lives as long as the page (and never fires if it could not be scheduled).
    deferred.forget();
    if !scheduled {
        default_launch();
    }
}

/// The deferred automatic launch ([`start_default_if_present`]): the default-id canvas, when no
/// explicit `start` claimed it while the launch was deferred.
fn default_launch() {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(canvas) = document.get_element_by_id(CANVAS_ID).and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok()) else {
        return;
    };
    if !STARTED_CANVASES.with(|book| book.borrow().default_launch_due(&canvas, same_canvas)) {
        return;
    }
    let asset_base = default_asset_base(&document);
    let config = StartConfig { canvas, asset_base, host_mode: None, author: query_param("author"), install_global: true, force_sample: None };
    let _ = launch_with_config(config);
}

/// Mount WordCraft on `options.canvas` and resolve to the `WordCraftApi` instance. Calling `start`
/// repeatedly for the same canvas resolves to its existing instance.
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn start(options: wasm_bindgen::JsValue) -> js_sys::Promise {
    init_runtime_once();
    match parse_start_config(&options) {
        Ok(cfg) => launch_with_config(cfg),
        Err(err) => js_sys::Promise::reject(&err),
    }
}

fn launch_with_config(config: StartConfig) -> js_sys::Promise {
    if let Some(existing) = STARTED_CANVASES.with(|book| book.borrow().find(&config.canvas, same_canvas)) {
        return existing;
    }

    let canvas_key = config.canvas.clone();
    let promise = wasm_bindgen_futures::future_to_promise(async move {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            return Err(js_sys::Error::new("no document").into());
        };
        let canvas = config.canvas;
        let bridge = match crate::host::Bridge::detect_with_override(&canvas, config.host_mode.as_deref()) {
            Ok(b) => b,
            Err(msg) => {
                remove_started_canvas(&canvas);
                return Err(js_sys::Error::new(&msg).into());
            }
        };
        let app_bridge = bridge.clone();
        let dirty = Rc::new(Cell::new(false));
        let saves = AsyncSaves::default();
        if bridge.is_none()
            && let Err(e) = guard_unload(dirty.clone(), saves.clone())
        {
            log::error!("no unsaved-changes guard: {e}");
        }
        #[cfg_attr(not(feature = "wgpu"), allow(unused_mut))]
        let mut options = eframe::WebOptions::default();
        #[cfg(feature = "wgpu")]
        if query().contains("webgl")
            && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup
        {
            create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
        }
        let initial_asset_base = config.asset_base;
        let demo_asset_base = initial_asset_base.clone();
        let author_override = config.author;
        let install_global = config.install_global;
        let force_sample = config.force_sample;
        let created_api: Rc<RefCell<Option<js_sys::Object>>> = Rc::new(RefCell::new(None));
        let created_api_slot = created_api.clone();
        let shell_canvas = canvas.clone();

        let result = eframe::WebRunner::new()
            .start(
                canvas.clone(),
                options,
                Box::new(move |cc| {
                    #[cfg(feature = "wgpu")]
                    if let Some(rs) = &cc.wgpu_render_state {
                        log::info!("wordcraft-web: wgpu backend {:?}", rs.adapter.get_info().backend);
                    }
                    // Fonts the user added on an earlier visit (#582); the app notices them arrive.
                    font_store::load_all(cc.egui_ctx.clone());
                    let inbox: Inbox = Inbox::default();
                    let pasted: Inbox = Inbox::default();
                    if let Err(e) = listen_for_pasted_pictures(pasted.clone(), cc.egui_ctx.clone()) {
                        log::error!("pasting pictures is unavailable: {e}");
                    }
                    // The browser's locale picks the new document's paper, the units and the
                    // decimal separator (#838).
                    let region = wordcraft_ui_egui::i18n::system_region();
                    // In host mode the parent sends the document to edit; no sample meanwhile.
                    let sample = app_bridge.is_none() && force_sample.unwrap_or_else(|| query().contains("sample"));
                    let doc = if sample { wordcraft_engine::sample::sample_document() } else { wordcraft_engine::blank_document(&region) };
                    let mut session = Session::new(doc);
                    session.system_region = region;
                    let messages = Rc::new(RefCell::new(Vec::new()));
                    let mut services = services(inbox.clone(), cc.egui_ctx.clone(), dirty, saves.clone(), messages.clone(), &initial_asset_base);
                    if let Some(bridge) = &app_bridge {
                        bridge.connect(&mut services, inbox.clone(), cc.egui_ctx.clone());
                        // A worker's export goes to the host too, as a copy (the document keeps
                        // its unsaved changes, so it never becomes the document's file).
                        let host = bridge.clone();
                        let exporter = lazy_io::Exporter {
                            asset_base: initial_asset_base.clone(),
                            messages: messages.clone(),
                            ctx: cc.egui_ctx.clone(),
                            deliver: Rc::new(move |name: &str, bytes: &[u8]| host.post_copy(name, bytes)),
                        };
                        services.lazy_export = Some(lazy_export_hook(exporter));
                    }
                    let (control, api_obj) = crate::api::install_with_global(cc.egui_ctx.clone(), install_global, &initial_asset_base);
                    *created_api_slot.borrow_mut() = Some(api_obj);
                    let mut app = WordApp::new(session, services).with_control(control);
                    // Pasted pictures wait apart from dropped files: the app inserts one only when the page is the edit target (#1349).
                    app.services.pasted = Some(pasted);
                    app.autosave = false;
                    // Preferences from an earlier visit (#2132), as the desktop app reads `ui.json`.
                    let stored = stored_prefs();
                    crate::prefs::restore(&mut app, stored.as_deref());
                    let asset_base = initial_asset_base;
                    font_packs::init(&asset_base, messages.clone());
                    // §3.2's smoke path (`?proof-worker`): the flag adds console lines once the
                    // proofing worker starts, and never starts it. The worker starts on the first
                    // edit or proofing command (`proof_squiggles::StartGate`, plan 10.1), so a
                    // page nobody edits prints none of them; the smoke harnesses type first.
                    if query().contains("proof-worker") {
                        crate::proof_squiggles::enable_smoke();
                    }
                    let initial_lang = app.ui.language.clone();
                    ensure_locale_loaded(&initial_lang, &asset_base, cc.egui_ctx.clone());
                    // A region chosen in File › Options replaces the browser's for the first blank
                    // document too (#838).
                    if !sample && app.session.prefs.region.is_some() {
                        let doc = app.session.blank_document();
                        app.session.set_document(doc);
                    }
                    let mut prefs = crate::prefs::PrefsSync::new(stored);
                    // No document to show: start on File › Home, as the desktop app does (#766).
                    if !sample {
                        app.start_screen();
                    }
                    // The host's name for the user signs this visit's work, but isn't stored as the
                    // browser's own preference, which a visit outside the host would restore.
                    if let Some(author) = author_override {
                        let own = app.session.author.clone();
                        crate::host::Bridge::set_author(&mut app, &author);
                        prefs.keep_own_author(&app.session.author, &own);
                    }
                    Ok(Box::new(WebShell { app, inbox, messages, prefs, asset_base, last_lang_pref: initial_lang, canvas: shell_canvas.clone() }))
                }),
            )
            .await;

        if let Some(bridge) = &bridge {
            match &result {
                Ok(()) => bridge.post_ready(),
                Err(e) => bridge.post_failed(&format!("{e:?}")),
            }
        }
        if let Some(el) = document.get_element_by_id(LOADING_ID) {
            match &result {
                Ok(()) => el.remove(),
                Err(e) => el.set_inner_html(&format!(
                    "<p>WordCraft failed to start: {e:?}</p><p>A browser with {} is required.</p>",
                    if cfg!(feature = "wgpu") { "WebGPU or WebGL2" } else { "WebGL2" }
                )),
            }
        }

        match result {
            Ok(()) => {
                label_ime_input(&canvas);
                let api_val = created_api.borrow_mut().take().unwrap_or_default();
                crate::demo::mount_if_enabled(&document, &canvas, &demo_asset_base, &api_val);
                Ok(api_val.into())
            }
            Err(e) => {
                remove_started_canvas(&canvas);
                Err(js_sys::Error::new(&format!("WordCraft failed to start: {e:?}")).into())
            }
        }
    });

    STARTED_CANVASES.with(|book| {
        book.borrow_mut().register(canvas_key, promise.clone());
    });
    promise
}

/// Gives the hidden focusable `<input>` the text agent keeps next to the canvas an accessible
/// name, so assistive technology announces it instead of a bare "text field". Runs at mount and
/// again on interface-language changes so the label is re-translated. The agent's input is the
/// text-typed sibling it created with `autocapitalize="off"`; other controls are left alone.
fn label_ime_input(canvas: &web_sys::HtmlCanvasElement) {
    let Some(parent) = canvas.parent_element() else {
        return;
    };
    let label = wordcraft_ui_egui::i18n::t("Text input");
    let children = parent.children();
    for i in 0..children.length() {
        let Some(el) = children.item(i) else {
            continue;
        };
        if let Ok(input) = el.dyn_into::<web_sys::HtmlInputElement>()
            && input.type_() == "text"
            && input.get_attribute("autocapitalize").as_deref() == Some("off")
            && input.get_attribute("aria-label").as_deref() != Some(label)
            && let Err(e) = input.set_attribute("aria-label", label)
        {
            log::warn!("IME input aria-label: {e:?}");
        }
    }
}

fn remove_started_canvas(canvas: &web_sys::HtmlCanvasElement) {
    STARTED_CANVASES.with(|book| book.borrow_mut().remove(canvas, same_canvas));
}

fn query() -> String {
    web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default()
}

fn query_param(key: &str) -> Option<String> {
    web_sys::UrlSearchParams::new_with_str(&query()).ok()?.get(key)
}

/// Wraps the app to read dropped files asynchronously (browsers can't read them synchronously)
/// and feed them through the inbox.
struct WebShell {
    app: WordApp,
    inbox: Inbox,
    messages: Rc<RefCell<Vec<String>>>,
    prefs: crate::prefs::PrefsSync,
    asset_base: String,
    last_lang_pref: String,
    canvas: web_sys::HtmlCanvasElement,
}

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

/// The preferences stored on an earlier visit, if the browser allows storage.
fn stored_prefs() -> Option<String> {
    local_storage()?.get_item(crate::prefs::KEY).ok()?
}

impl eframe::App for WebShell {
    /// Runs one browser frame: forwards queued host messages to the status bar, advances the app,
    /// then writes changed preferences to browser storage immediately, because a reload or closed
    /// tab never runs the exit path.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = js_sys::Date::now();
        wordcraft_geom::set_wasm_clock(now, now);
        let msgs = std::mem::take(&mut *self.messages.borrow_mut());
        for msg in msgs {
            self.app.status(msg);
        }
        if self.app.ui.language != self.last_lang_pref {
            self.last_lang_pref = self.app.ui.language.clone();
            ensure_locale_loaded(&self.last_lang_pref, &self.asset_base, ctx.clone());
            label_ime_input(&self.canvas);
        }
        // Picked, dropped or host-sent files only the codecs worker reads leave the inbox here
        // and come back as Word packages on a later frame (plan 10.2); the app opens the rest.
        lazy_io::take_foreign_opens(&self.inbox, &self.asset_base, &self.messages, ctx, || self.app.session.region());
        self.app.logic(ctx);
        // Wanted pack faces are recorded during layout, so drain them after the app ran.
        font_packs::drain_wanted(ctx);
        // §3.2: worker proofing — send what changed, apply what arrived, repaint if it drew.
        crate::proof_squiggles::tick(&self.app.session, &self.asset_base, ctx);
        crate::api::wake_waiters();
        // Store changed preferences straight away: a reload or closed tab runs no exit code (#2132).
        if let Some(json) = self.prefs.changed(&self.app) {
            match local_storage() {
                Some(store) => {
                    if let Err(e) = store.set_item(crate::prefs::KEY, &json) {
                        log::warn!("preferences: cannot save: {e:?}");
                    }
                }
                None => log::warn!("preferences: browser storage is unavailable"),
            }
        }
    }

    /// Forwards this frame's raw browser input to the app before widgets run.
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.app.raw_input_hook(ctx, raw);
    }

    /// Draws the app. On the first pass of a visible frame it takes the browser's dropped files and
    /// spawns asynchronous byte reads into the inbox; later passes only draw.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Browser drops are input: take them only in the first real UI pass, never in a hidden
        // `run_logic` tick that still sees the previous shown frame's RawInput.
        if ui.ctx().current_pass_index() == 0 {
            let ctx = ui.ctx().clone();
            let dropped = ctx.input_mut(|i| std::mem::take(&mut i.raw.dropped_files));
            for f in dropped {
                let inbox = self.inbox.clone();
                let ctx = ctx.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let name = f.path().file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "dropped".into());
                    match f.bytes_async().await {
                        Ok(bytes) => {
                            inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
                            ctx.request_repaint();
                        }
                        Err(e) => log::error!("couldn't read dropped file {name}: {e}"),
                    }
                });
            }
        }
        self.app.ui(ui);
        crate::api::wake_waiters();
    }
}

/// Largest picture pasted from the browser's clipboard (as `insert.picture` allows).
const MAX_PASTED_BYTES: f64 = (200u32 << 20) as f64;
/// Most pictures one paste inserts.
const MAX_PASTED_PICTURES: u32 = 20;

/// Pasting a picture (#45): eframe's paste handler only reads text, so a clipboard holding just
/// pictures (a screenshot, a browser's Copy Image) is read here and the pictures arrive through
/// the app's `pasted` queue, which inserts them only when the page is the edit target (#1349). A clipboard with text is left to eframe.
fn listen_for_pasted_pictures(inbox: Inbox, ctx: egui::Context) -> Result<(), String> {
    let document = web_sys::window().and_then(|w| w.document()).ok_or("no document")?;
    let on_paste = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::ClipboardEvent)>::new(move |e: web_sys::ClipboardEvent| {
        // The target is always eframe's hidden text input while the page has focus; the app decides (#1349).
        let Some(data) = e.clipboard_data() else { return };
        if !data.get_data("text").unwrap_or_default().is_empty() {
            return;
        }
        let Some(files) = data.files() else { return };
        for i in 0..files.length().min(MAX_PASTED_PICTURES) {
            let Some(file) = files.get(i) else { continue };
            if !file.type_().starts_with("image/") {
                continue;
            }
            if file.size() > MAX_PASTED_BYTES {
                log::warn!("pasted picture is larger than 200 MB; not inserted");
                continue;
            }
            let inbox = inbox.clone();
            let ctx = ctx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match wasm_bindgen_futures::JsFuture::from(file.array_buffer()).await {
                    Ok(buf) => {
                        let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                        // The engine reads the format from the bytes; the name is ignored.
                        inbox.lock().unwrap_or_else(|e| e.into_inner()).push(("pasted.png".into(), bytes));
                        ctx.request_repaint();
                    }
                    Err(e) => log::error!("couldn't read the pasted picture: {e:?}"),
                }
            });
        }
    });
    document.add_event_listener_with_callback("paste", on_paste.as_ref().unchecked_ref()).map_err(|e| format!("{e:?}"))?;
    // The listener lives as long as the page.
    on_paste.forget();
    Ok(())
}

/// [`wordcraft_ui_egui::Services::lazy_export`] answered by `exporter`.
fn lazy_export_hook(exporter: lazy_io::Exporter) -> wordcraft_ui_egui::LazyExportFn {
    Box::new(
        move |name: &str,
              password: Option<&str>,
              opts: &wordcraft_engine::io::ExportOptions,
              docx: &dyn Fn() -> Result<Vec<u8>, String>,
              used: &dyn Fn() -> Vec<(String, String)>| exporter.export(name, password, opts, docx, used),
    )
}

/// `dirty` is the flag the `beforeunload` guard reads ([`guard_unload`]).
fn services(
    inbox: Inbox,
    ctx: egui::Context,
    dirty: Rc<Cell<bool>>,
    saves: AsyncSaves,
    messages: Rc<RefCell<Vec<String>>>,
    asset_base: &str,
) -> Services {
    // Save As and Export in a format a worker writes (plan 10.2): the worker is fetched on the
    // first such save, and the finished file is an anchor download (the host page's, when one
    // hosts the app: see the bridge's wiring).
    let exporter = lazy_io::Exporter {
        asset_base: asset_base.to_string(),
        messages: messages.clone(),
        ctx: ctx.clone(),
        deliver: Rc::new(lazy_io::anchor_download),
    };
    let open_inbox = inbox.clone();
    let save_ctx = ctx.clone();
    let font_ctx = ctx.clone();
    let font_messages = messages.clone();
    let pack_ctx = ctx.clone();
    Services {
        open_async: Some(Box::new(move |purpose: &str| {
            let inbox = open_inbox.clone();
            let ctx = ctx.clone();
            let dialog = if purpose == "picture" {
                rfd::AsyncFileDialog::new().add_filter("Pictures", IMAGE_EXTS)
            } else if purpose == "font" {
                rfd::AsyncFileDialog::new().add_filter("Fonts", wordcraft_fonts::FONT_FILE_EXTS)
            } else if purpose == "recipients" {
                rfd::AsyncFileDialog::new().add_filter("Recipient lists", RECIPIENT_EXTS)
            } else {
                rfd::AsyncFileDialog::new().add_filter("Documents", DOC_EXTS)
            };
            let many = purpose == "font";
            wasm_bindgen_futures::spawn_local(async move {
                // Add Font takes every selected file (#1789); the others one.
                let files = if many { dialog.pick_files().await.unwrap_or_default() } else { dialog.pick_file().await.into_iter().collect() };
                for file in files.into_iter().take(MAX_PICKED_FILES) {
                    let mut bytes = file.read().await;
                    let mut pending = inbox.lock().unwrap_or_else(|e| e.into_inner());
                    // Fonts waiting for the app to drain them stay under a byte cap. A file
                    // over it is queued empty, so Add Font reports that file on its own.
                    let held: usize = pending.iter().map(|(_, b)| b.len()).fold(0, usize::saturating_add);
                    if many && held.saturating_add(bytes.len()) > MAX_PENDING_PICKED_BYTES {
                        bytes = Vec::new();
                    }
                    pending.push((file.file_name(), bytes));
                    drop(pending);
                    ctx.request_repaint();
                }
            });
        })),
        // Exports ask for a name; the browser decides where the download goes.
        pick_save: Some(Box::new(|name: &str| Some(name.to_string()))),
        async_saves: Some(saves.clone()),
        lazy_export: Some(lazy_export_hook(exporter)),
        download: Some(Box::new(move |path: &str, bytes: &[u8]| download(&saves, &save_ctx, path, bytes))),
        print: Some(Box::new(print_pdf)),
        inbox: Some(inbox),
        on_dirty: Some(Box::new(move |d| dirty.set(d))),
        // The proofing worker starts on the first proofing command too, heard as it runs (plan 10.1).
        on_command: Some(Box::new(crate::proof_squiggles::observe_command)),
        animations_enabled: Some(animations_enabled()),
        store_font: Some(Box::new(font_store::store)),
        forget_font: Some(Box::new(font_store::forget)),
        load_system_fonts: Some(Box::new(move || {
            load_system_fonts(font_ctx.clone(), font_messages.clone());
        })),
        install_font_packs: Some(Box::new(move |families: Vec<String>| {
            font_packs::install_families(families, pack_ctx.clone());
        })),
        ..Default::default()
    }
}

/// Most files one multi-select pick (Add Font) reads.
const MAX_PICKED_FILES: usize = 2000;
/// Most bytes of picked fonts held in the inbox before the app drains them.
const MAX_PENDING_PICKED_BYTES: usize = 256 << 20;
/// Most local fonts one Load System Installed Fonts installs (a designer's machine has thousands).
const MAX_SYSTEM_FONTS: usize = 500;
/// Most font bytes one Load System Installed Fonts reads into memory (wasm memory is small).
const MAX_SYSTEM_FONT_BYTES: usize = 256 << 20;
/// Most local fonts looked at, installed or not (already available, unreadable, too big).
const MAX_SYSTEM_FONTS_TRIED: u32 = 4000;

/// Query the browser's Local Font Access API (`window.queryLocalFonts()`) and install the faces
/// into the global font database, up to [`MAX_SYSTEM_FONTS`] faces and [`MAX_SYSTEM_FONT_BYTES`].
fn load_system_fonts(ctx: egui::Context, messages: Rc<RefCell<Vec<String>>>) {
    wasm_bindgen_futures::spawn_local(async move {
        let Some(window) = web_sys::window() else {
            return;
        };
        let query_fn = match js_sys::Reflect::get(&window, &wasm_bindgen::JsValue::from_str("queryLocalFonts")) {
            Ok(val) if val.is_function() => js_sys::Function::from(val),
            _ => {
                messages.borrow_mut().push(wordcraft_ui_egui::i18n::t("Local font access is not supported by your browser.").to_string());
                ctx.request_repaint();
                return;
            }
        };
        let fonts_promise = match query_fn.call0(&window) {
            Ok(p) => js_sys::Promise::from(p),
            Err(e) => {
                log::warn!("queryLocalFonts call failed: {e:?}");
                messages.borrow_mut().push(wordcraft_ui_egui::i18n::t("Local font access was denied.").to_string());
                ctx.request_repaint();
                return;
            }
        };
        let fonts_val = match wasm_bindgen_futures::JsFuture::from(fonts_promise).await {
            Ok(val) => val,
            Err(e) => {
                log::warn!("queryLocalFonts promise rejected: {e:?}");
                messages.borrow_mut().push(wordcraft_ui_egui::i18n::t("Local font access was denied.").to_string());
                ctx.request_repaint();
                return;
            }
        };
        let font_list = js_sys::Array::from(&fonts_val);
        let len = font_list.length();
        let mut loaded = 0usize;
        let mut total_bytes = 0usize;
        for i in 0..len.min(MAX_SYSTEM_FONTS_TRIED) {
            if loaded >= MAX_SYSTEM_FONTS || total_bytes >= MAX_SYSTEM_FONT_BYTES {
                log::warn!("local fonts: stopped after {loaded} fonts ({total_bytes} bytes)");
                break;
            }
            let font_data = font_list.get(i);
            let Ok(blob_fn) = js_sys::Reflect::get(&font_data, &wasm_bindgen::JsValue::from_str("blob")) else {
                continue;
            };
            if !blob_fn.is_function() {
                continue;
            }
            let Ok(blob_val) = js_sys::Function::from(blob_fn).call0(&font_data) else {
                continue;
            };
            let Ok(blob_js) = wasm_bindgen_futures::JsFuture::from(js_sys::Promise::from(blob_val)).await else {
                continue;
            };
            let Ok(blob) = blob_js.dyn_into::<web_sys::Blob>() else {
                continue;
            };
            // Skip a face too big on its own or for what is left of the budget, before reading it.
            let size = blob.size().clamp(0.0, usize::MAX as f64) as usize;
            if size > wordcraft_fonts::MAX_FONT_FILE_BYTES || total_bytes.saturating_add(size) > MAX_SYSTEM_FONT_BYTES {
                continue;
            }
            let Ok(array_buffer) = wasm_bindgen_futures::JsFuture::from(blob.array_buffer()).await else {
                continue;
            };
            let bytes = js_sys::Uint8Array::new(&array_buffer).to_vec();
            if bytes.len() > wordcraft_fonts::MAX_FONT_FILE_BYTES {
                continue;
            }
            total_bytes = total_bytes.saturating_add(bytes.len());
            let ps_name = js_sys::Reflect::get(&font_data, &wasm_bindgen::JsValue::from_str("postscriptName"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| format!("local-font-{i}"));
            let name = format!("{ps_name}.ttf");
            match wordcraft_fonts::FontDb::global().install_font(&name, &bytes) {
                Ok(_) => loaded += 1,
                Err(wordcraft_fonts::UserFontError::AlreadyAvailable(_)) => {}
                Err(e) => log::warn!("local font {name} skipped: {e}"),
            }
        }
        if loaded > 0 {
            let msg = wordcraft_ui_egui::i18n::fmt(wordcraft_ui_egui::i18n::t("Loaded {count} system fonts."), &[("count", &loaded.to_string())]);
            messages.borrow_mut().push(msg);
        } else {
            messages.borrow_mut().push(wordcraft_ui_egui::i18n::t("All system fonts are already available.").to_string());
        }
        ctx.request_repaint();
    });
}

/// Whether the system has animation effects on, as the browser passes it on: off while the
/// `prefers-reduced-motion: reduce` media query matches. `None` when the browser can't say. The
/// query is made once and asked every frame; it follows the system's setting by itself.
fn animations_enabled() -> Box<dyn Fn() -> Option<bool>> {
    let reduce = web_sys::window().and_then(|w| w.match_media("(prefers-reduced-motion: reduce)").ok().flatten());
    Box::new(move || reduce.as_ref().map(|query| !query.matches()))
}

/// Closing, reloading or leaving the tab with unsaved changes asks first. Browsers show their own
/// confirmation (a page can only ask for it), so there is no Save button: Cancel, then File › Save.
fn guard_unload(dirty: Rc<Cell<bool>>, saves: AsyncSaves) -> Result<(), String> {
    let window = web_sys::window().ok_or("no window")?;
    let on_unload = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::BeforeUnloadEvent)>::new(move |e: web_sys::BeforeUnloadEvent| {
        // A save still in the browser's picker hasn't written anything yet, nor has an export a
        // worker is still writing.
        if dirty.get() || saves.pending() > 0 || lazy_io::exports_in_flight() > 0 {
            e.prevent_default();
            // Older browsers ask only when `returnValue` is set.
            e.set_return_value("unsaved changes");
        }
    });
    window.add_event_listener_with_callback("beforeunload", on_unload.as_ref().unchecked_ref()).map_err(|e| format!("{e:?}"))?;
    // The listener lives as long as the page.
    on_unload.forget();
    Ok(())
}

/// Trigger a browser save dialog or download of `bytes` named after the last component of `path`.
fn download(saves: &AsyncSaves, ctx: &egui::Context, path: &str, bytes: &[u8]) -> Result<(), String> {
    let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "document".into());
    let window = web_sys::window().ok_or("no window")?;

    // On browsers supporting the File System Access API (Chromium: Chrome, Edge, etc.),
    // `showSaveFilePicker` lets the user choose the save location and filename natively.
    if let Ok(picker_val) = js_sys::Reflect::get(&window, &wasm_bindgen::JsValue::from_str("showSaveFilePicker"))
        && let Ok(picker_fn) = picker_val.dyn_into::<js_sys::Function>()
    {
        match trigger_save_file_picker(saves, ctx, &window, &picker_fn, &name, bytes) {
            Ok(()) => return Ok(()),
            Err(e) => {
                log::warn!("showSaveFilePicker failed: {e}; falling back to anchor download");
            }
        }
    }

    // Fallback for browsers without showSaveFilePicker (e.g. Firefox, Safari):
    trigger_anchor_download(&window, &name, bytes)
}

fn trigger_save_file_picker(
    saves: &AsyncSaves,
    ctx: &egui::Context,
    window: &web_sys::Window,
    picker_fn: &js_sys::Function,
    name: &str,
    bytes: &[u8],
) -> Result<(), String> {
    let js = |e: wasm_bindgen::JsValue| format!("{e:?}");
    let ext = std::path::Path::new(name).extension().and_then(std::ffi::OsStr::to_str).unwrap_or("docx").to_ascii_lowercase();
    let dot_ext = format!(".{ext}");
    let mime = mime_for(name);

    let accept_obj = js_sys::Object::new();
    let exts_arr = js_sys::Array::new();
    exts_arr.push(&wasm_bindgen::JsValue::from_str(&dot_ext));
    let _ = js_sys::Reflect::set(&accept_obj, &wasm_bindgen::JsValue::from_str(mime), &exts_arr);

    // Our own neutral description: the format's name, as its extension says it.
    let type_desc = match ext.as_str() {
        "htm" => "HTML".to_string(),
        "txt" => "Text".to_string(),
        e if !e.is_empty() && e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric()) => e.to_ascii_uppercase(),
        _ => "Document".to_string(),
    };

    let type_obj = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&type_obj, &wasm_bindgen::JsValue::from_str("description"), &wasm_bindgen::JsValue::from_str(&type_desc));
    let _ = js_sys::Reflect::set(&type_obj, &wasm_bindgen::JsValue::from_str("accept"), &accept_obj);

    let types_arr = js_sys::Array::new();
    types_arr.push(&type_obj);

    let picker_opts = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&picker_opts, &wasm_bindgen::JsValue::from_str("suggestedName"), &wasm_bindgen::JsValue::from_str(name));
    let _ = js_sys::Reflect::set(&picker_opts, &wasm_bindgen::JsValue::from_str("types"), &types_arr);

    // Call showSaveFilePicker synchronously while transient user activation is active:
    let prom_val = picker_fn.call1(window, &picker_opts).map_err(js)?;
    let prom: js_sys::Promise = prom_val.dyn_into().map_err(|_| "not a promise".to_string())?;

    let bytes_vec = bytes.to_vec();
    let window = window.clone();
    let saves = saves.clone();
    let ctx = ctx.clone();
    let name = name.to_string();
    // The app holds back a Save Changes prompt's New/Open until this ticket ends (#1315).
    let ticket = saves.start();
    wasm_bindgen_futures::spawn_local(async move {
        // The document counts as saved only once the file is written and closed.
        let outcome = match wasm_bindgen_futures::JsFuture::from(prom).await {
            Ok(handle_val) => match write_file_handle(&handle_val, &bytes_vec).await {
                Ok(()) => {
                    log::info!("saved {name} through the save picker");
                    SaveOutcome::Saved
                }
                Err(e) => {
                    log::error!("couldn't write {name}: {e}");
                    SaveOutcome::Failed(e)
                }
            },
            Err(e) => {
                let err_name =
                    js_sys::Reflect::get(&e, &wasm_bindgen::JsValue::from_str("name")).ok().and_then(|v| v.as_string()).unwrap_or_default();
                // A browser can expose the picker but reject its promise (for example in
                // a cross-origin iframe). Use the same download path as an absent API;
                // cancelling a picker must never start an unwanted download (#1246).
                crate::save_fallback::picker_rejected(&err_name, || trigger_anchor_download(&window, &name, &bytes_vec))
            }
        };
        saves.finish(ticket, outcome);
        ctx.request_repaint();
    });
    Ok(())
}

async fn write_file_handle(handle: &wasm_bindgen::JsValue, bytes: &[u8]) -> Result<(), String> {
    let js = |e: wasm_bindgen::JsValue| format!("{e:?}");
    let create_fn: js_sys::Function = js_sys::Reflect::get(handle, &wasm_bindgen::JsValue::from_str("createWritable"))
        .map_err(js)?
        .dyn_into()
        .map_err(|_| "createWritable is not a function".to_string())?;
    let stream_prom: js_sys::Promise = create_fn.call0(handle).map_err(js)?.dyn_into().map_err(|_| "not a promise".to_string())?;
    let stream = wasm_bindgen_futures::JsFuture::from(stream_prom).await.map_err(js)?;

    let write_fn: js_sys::Function = js_sys::Reflect::get(&stream, &wasm_bindgen::JsValue::from_str("write"))
        .map_err(js)?
        .dyn_into()
        .map_err(|_| "write is not a function".to_string())?;
    let uint8 = js_sys::Uint8Array::from(bytes);
    let write_prom: js_sys::Promise = write_fn.call1(&stream, &uint8).map_err(js)?.dyn_into().map_err(|_| "not a promise".to_string())?;
    wasm_bindgen_futures::JsFuture::from(write_prom).await.map_err(js)?;

    let close_fn: js_sys::Function = js_sys::Reflect::get(&stream, &wasm_bindgen::JsValue::from_str("close"))
        .map_err(js)?
        .dyn_into()
        .map_err(|_| "close is not a function".to_string())?;
    let close_prom: js_sys::Promise = close_fn.call0(&stream).map_err(js)?.dyn_into().map_err(|_| "not a promise".to_string())?;
    wasm_bindgen_futures::JsFuture::from(close_prom).await.map_err(js)?;
    Ok(())
}

pub(crate) fn trigger_anchor_download(window: &web_sys::Window, name: &str, bytes: &[u8]) -> Result<(), String> {
    let js = |e: wasm_bindgen::JsValue| format!("{e:?}");
    let document = window.document().ok_or("no document")?;
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type(mime_for(name));
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).map_err(js)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(js)?;
    let a: web_sys::HtmlAnchorElement = document.create_element("a").map_err(js)?.dyn_into().map_err(|_| "not an anchor".to_string())?;
    a.set_href(&url);
    a.set_download(name);
    a.style().set_property("display", "none").map_err(js)?;
    let body = document.body().ok_or("no body")?;
    body.append_child(&a).map_err(js)?;
    a.click();
    a.remove();
    // Revoke after the click has been dispatched; the download keeps its own reference.
    let revoke = wasm_bindgen::closure::Closure::once_into_js(move || {
        web_sys::Url::revoke_object_url(&url).ok();
    });
    window.set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), 10_000).map_err(js)?;
    Ok(())
}

/// The hidden iframe the last print loaded its PDF into.
const PRINT_FRAME_ID: &str = "wordcraft-print-frame";

/// Opens `bytes` (a PDF) as an object URL in a hidden iframe and calls the
/// iframe's own `print()` once it has finished loading — this is the
/// standard way to drive the browser's native print dialog on a PDF without
/// a download or a popup window the browser might block. The previous
/// print's iframe and object URL are removed first, so only one copy of the
/// document's PDF stays in the page (the last one may still be printing).
fn print_pdf(bytes: &[u8]) -> Result<(), String> {
    let js = |e: wasm_bindgen::JsValue| format!("{e:?}");
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;

    if let Some(old) = document.get_element_by_id(PRINT_FRAME_ID) {
        if let Ok(frame) = old.clone().dyn_into::<web_sys::HtmlIFrameElement>() {
            let src = frame.src();
            if src.starts_with("blob:") {
                web_sys::Url::revoke_object_url(&src).ok();
            }
        }
        old.remove();
    }

    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type("application/pdf");
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).map_err(js)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(js)?;

    let iframe: web_sys::HtmlIFrameElement =
        match document.create_element("iframe").map_err(js).and_then(|e| e.dyn_into().map_err(|_| "not an iframe".to_string())) {
            Ok(f) => f,
            Err(e) => {
                web_sys::Url::revoke_object_url(&url).ok();
                return Err(e);
            }
        };
    iframe.set_id(PRINT_FRAME_ID);
    let attached = iframe
        .style()
        .set_property("display", "none")
        .map_err(js)
        .and_then(|()| document.body().ok_or_else(|| "no body".to_string()))
        .and_then(|body| body.append_child(&iframe).map_err(js));
    if let Err(e) = attached {
        iframe.remove();
        web_sys::Url::revoke_object_url(&url).ok();
        return Err(e);
    }

    // `onload` fires once the PDF has actually rendered inside the iframe;
    // printing before that would show a blank page. Set the handler BEFORE
    // `src` so a fast/cached load can't fire before we're listening.
    let iframe_for_load = iframe.clone();
    let on_load = wasm_bindgen::closure::Closure::once_into_js(move || {
        if let Some(w) = iframe_for_load.content_window() {
            // Ignore errors: some browsers (notably Firefox with a built-in
            // PDF viewer) print fine but report a benign cross-origin-style
            // error back on this call.
            let _ = w.print();
        }
    });
    iframe.set_onload(Some(on_load.unchecked_ref()));
    iframe.set_src(&url);
    Ok(())
}

fn mime_for(name: &str) -> &'static str {
    match name.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("pdf") => "application/pdf",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("odt") => "application/vnd.oasis.opendocument.text",
        Some("ott") => "application/vnd.oasis.opendocument.text-template",
        Some("fodt") => "application/vnd.oasis.opendocument.text-flat-xml",
        Some("rtf") => "application/rtf",
        Some("html") => "text/html",
        Some("md" | "txt") => "text/plain",
        Some("tex") => "application/x-tex",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        _ => "application/octet-stream",
    }
}
