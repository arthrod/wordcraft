//! The page itself: the DOM the viewer lives in, the listeners, and the actions the state
//! machine asks for. Everything that decides something is in the pure modules; this file
//! reads numbers from the browser, hands them over, and applies the answers.
//!
//! What the page looks like to a host or a test:
//!
//! - `<html data-wordcraft-phase>` is `viewing`, `loading-editor`, `handing-over`, `editing`
//!   or `viewer-only`; `data-wordcraft-notice` names why the editor did not take over
//!   (`no-webgl`, `load-failed`, `start-failed`, `hand-over-failed`); `data-wordcraft-error`
//!   is set when the document or the viewer itself failed.
//! - The mount element (`#wordcraft_viewer`) carries `data-mode` and, once a document is
//!   open, `data-pages`. Each page in or near the window is a `<canvas data-page="N">`; a
//!   painted one has `data-scale` (pixels per point) and its bitmap is exactly the RGBA the
//!   view-only core returned for that page at that scale.
//! - An edit action is the Edit control, a double click on a page, or Enter or F2 while the
//!   pages have focus.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;
use wordcraft_worker_proto as proto;

use crate::cache::{Arrival, PageCache, PageKey};
use crate::handover::{self, Action, Event, Handover, Phase, Reason};
use crate::mode::Mode;
use crate::pages::{self, Sheet};
use crate::worker::{self, ViewerWorker};

/// The element the viewer mounts in when a page does not name one.
const ROOT_ID: &str = "wordcraft_viewer";
/// The static loading skeleton of the demo page, removed at the first painted page.
const SKELETON_ID: &str = "wordcraft_viewer_loading";
/// The canvas the editor starts on by itself when it finds it (`apps/wordcraft-web`).
const EDITOR_CANVAS_ID: &str = "wordcraft_canvas";
/// The editor's Trunk loader shim: `import init from './wordcraft-web.js'; await init();`.
/// Un-hashed, so it can be named here.
const EDITOR_LOADER_FILE: &str = "wordcraft-web_loader.js";
/// The editor's page API object, and the function the document is handed over through.
const EDITOR_GLOBAL: &str = "wordcraft";
const EDITOR_LOAD_FN: &str = "loadDocx";
/// The text the editor rejects its launch with when it cannot start.
const EDITOR_START_FAILURE: &str = "WordCraft failed to start";
/// The built-in sample, staged by `cargo xtask web-assets`.
const SAMPLE_FILE: &str = "samples/welcome.docx";
/// This module's own page API object.
const VIEWER_GLOBAL: &str = "wordcraftViewer";
/// Pixels kept in painted pages outside the window, in bytes of RGBA.
const CACHE_BUDGET_BYTES: u64 = 192 << 20;
/// Where the document comes from.
enum Source {
    /// Nothing was given: the viewer shows the built-in sample, the editor what it shows by
    /// itself.
    Unset,
    Url(String),
    Bytes(js_sys::Uint8Array),
}

struct PageCanvas {
    el: web_sys::HtmlCanvasElement,
    /// Width, height and scale of the bitmap on it, once painted.
    bitmap: Option<(u32, u32, f32)>,
}

struct State {
    handover: Handover,
    worker: Option<ViewerWorker>,
    source: Source,
    /// The open document's bytes, kept for the hand-over to the editor.
    docx: Option<js_sys::Uint8Array>,
    password: Option<String>,
    sizes: Vec<proto::ViewPageSize>,
    page_count: u32,
    sheet: Sheet,
    cache: PageCache,
    /// Counts documents and layouts: a reply made for an earlier one is dropped.
    generation: u32,
    canvases: HashMap<u32, PageCanvas>,
    missing_fonts: Vec<String>,
    refresh_queued: bool,
    painted_once: bool,
    /// The viewer's DOM is gone (the editor took over).
    removed: bool,
    /// The editor's page API was seen: the wait for it is over.
    editor_seen: bool,
    /// `resolve`/`reject` of `open()` calls waiting for the worker's answer.
    open_waiters: Vec<(js_sys::Function, js_sys::Function)>,
}

struct Page {
    window: web_sys::Window,
    document: web_sys::Document,
    asset_base: String,
    mode: Mode,
    root: web_sys::HtmlElement,
    status: web_sys::HtmlElement,
    edit: web_sys::HtmlElement,
    notice: web_sys::HtmlElement,
    password: PasswordAsk,
    scroller: web_sys::HtmlElement,
    sheet: web_sys::HtmlElement,
    /// How long to wait for the editor's page API ([`handover::editor_wait_ms`]).
    editor_wait_ms: u32,
    state: RefCell<State>,
}

/// The in-page ask for a protected document's password: a form under the notice line, hidden
/// until the viewer worker answers that the document needs one. (Not `window.prompt`, which
/// blocks the page and anything driving it.)
struct PasswordAsk {
    form: web_sys::HtmlElement,
    message: web_sys::HtmlElement,
    input: web_sys::HtmlInputElement,
    cancel: web_sys::HtmlElement,
}

thread_local! {
    /// The one viewer of this page, and its API object.
    static STARTED: RefCell<Option<(Rc<Page>, js_sys::Object)>> = const { RefCell::new(None) };
}

// ---- starting ----

/// Start by itself when the page has a `#wordcraft_viewer` element and no `start(options)`
/// call claimed it first (the launch is deferred one task for that, as the editor's is).
pub(crate) fn start_default_if_present() {
    let Some(window) = web_sys::window() else { return };
    let Some(document) = window.document() else { return };
    if document.get_element_by_id(ROOT_ID).is_none() {
        return;
    }
    let deferred = Closure::once_into_js(|| {
        if STARTED.with(|s| s.borrow().is_none()) {
            let _ = launch(&JsValue::UNDEFINED, true);
        }
    });
    if window.set_timeout_with_callback(deferred.unchecked_ref()).is_err() {
        let _ = launch(&JsValue::UNDEFINED, true);
    }
}

/// Mount the viewer and resolve to its API object. Options, all optional: `container` (an
/// element or an element id; default `#wordcraft_viewer`), `mode` (`"view"`, `"viewer-first"`
/// or `"edit"`), `document` (a `Uint8Array` or `ArrayBuffer` holding a `.docx`), `url` (a
/// same-origin `.docx`), `password`, `assetBase` (where this module's sibling files are
/// served from), `installGlobal` (also publish the API at `window.wordcraftViewer`) and
/// `editorWaitMs` (how long to wait for the editor after an edit action before saying it did
/// not start; also read from the container's `data-editor-wait-ms`). A page has one viewer: a
/// second call resolves to the first one's API.
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn start(options: JsValue) -> js_sys::Promise {
    match launch(&options, false) {
        Ok(api) => js_sys::Promise::resolve(&api),
        Err(e) => js_sys::Promise::reject(&e),
    }
}

fn js_error(message: &str) -> JsValue {
    js_sys::Error::new(message).into()
}

fn prop(obj: &JsValue, key: &str) -> JsValue {
    if obj.is_object() { js_sys::Reflect::get(obj, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED) } else { JsValue::UNDEFINED }
}

fn str_prop(obj: &JsValue, key: &str) -> Result<Option<String>, JsValue> {
    let val = prop(obj, key);
    if val.is_undefined() || val.is_null() {
        Ok(None)
    } else {
        val.as_string().map(Some).ok_or_else(|| js_error(&format!("start(options): `{key}` must be a string")))
    }
}

/// The bytes a host passed: a `Uint8Array` as it is, an `ArrayBuffer` viewed.
fn bytes_of(val: &JsValue) -> Option<js_sys::Uint8Array> {
    if val.is_instance_of::<js_sys::Uint8Array>() {
        val.clone().dyn_into().ok()
    } else if val.is_instance_of::<js_sys::ArrayBuffer>() {
        Some(js_sys::Uint8Array::new(val))
    } else {
        None
    }
}

fn normalize_asset_base(base: &str) -> String {
    let trimmed = base.trim();
    if trimmed.is_empty() {
        "./".to_string()
    } else if trimmed.ends_with('/') {
        trimmed.to_string()
    } else {
        format!("{trimmed}/")
    }
}

/// The directory of the page, where Trunk puts this module's sibling files.
fn default_asset_base(document: &web_sys::Document) -> String {
    document
        .base_uri()
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .and_then(|base| web_sys::Url::new_with_base("./", &base).ok().map(|u| u.href()))
        .map_or_else(|| "./".to_string(), |resolved| normalize_asset_base(&resolved))
}

fn launch(options: &JsValue, auto: bool) -> Result<JsValue, JsValue> {
    if let Some(api) = STARTED.with(|s| s.borrow().as_ref().map(|(_, api)| api.clone())) {
        return Ok(api.into());
    }
    if !(options.is_undefined() || options.is_null() || options.is_object()) {
        return Err(js_error("start(options): expected an options object"));
    }
    let window = web_sys::window().ok_or_else(|| js_error("no browser window available"))?;
    let document = window.document().ok_or_else(|| js_error("no browser document available"))?;

    let container = prop(options, "container");
    let root: web_sys::HtmlElement = if let Some(id) = container.as_string() {
        let id = id.strip_prefix('#').unwrap_or(&id).to_string();
        document.get_element_by_id(&id).and_then(|e| e.dyn_into().ok()).ok_or_else(|| js_error(&format!("missing element `{id}`")))?
    } else if container.is_undefined() || container.is_null() {
        document.get_element_by_id(ROOT_ID).and_then(|e| e.dyn_into().ok()).ok_or_else(|| js_error(&format!("missing <div id=\"{ROOT_ID}\">")))?
    } else {
        container.dyn_into().map_err(|_| js_error("start(options): `container` must be an element or an element id"))?
    };

    let search = window.location().search().unwrap_or_default();
    let query = web_sys::UrlSearchParams::new_with_str(&search).ok();
    let query_get = |key: &str| query.as_ref().and_then(|q| q.get(key));
    let explicit = str_prop(options, "mode")?.or_else(|| root.get_attribute("data-mode"));
    let mode = Mode::resolve(explicit.as_deref(), query_get("mode").as_deref(), query.as_ref().is_some_and(|q| q.has("host")));

    let asset_base = str_prop(options, "assetBase")?.map(|b| normalize_asset_base(&b)).unwrap_or_else(|| default_asset_base(&document));
    let given = prop(options, "document");
    let source = if !(given.is_undefined() || given.is_null()) {
        Source::Bytes(bytes_of(&given).ok_or_else(|| js_error("start(options): `document` must be a Uint8Array or an ArrayBuffer"))?)
    } else if let Some(url) = str_prop(options, "url")?.or_else(|| query_get("doc")) {
        Source::Url(url)
    } else {
        Source::Unset
    };
    let password = str_prop(options, "password")?;
    let install_global = auto || prop(options, "installGlobal").as_bool().unwrap_or(false);
    let editor_wait_ms = handover::editor_wait_ms(
        // The attribute is read by JavaScript's `parseFloat`, which keeps Rust's float parser
        // (about 17 KB) out of the page module; `editor_wait_ms` drops a NaN.
        prop(options, "editorWaitMs").as_f64().or_else(|| root.get_attribute("data-editor-wait-ms").map(|v| js_sys::Number::parse_float(&v))),
    );

    // An editor-first page with the editor's host bridge (`?host=`) is the editor's page: see
    // `Handover::hosted`. Otherwise a browser without WebGL, which the editor draws with, is
    // told so before anything of the editor is fetched.
    let hosted = mode == Mode::EditorFirst && query.as_ref().is_some_and(|q| q.has("host"));
    let (handover, first) = if hosted { Handover::hosted() } else { Handover::start(mode, mode != Mode::ViewOnly && has_webgl(&document)) };

    let Built { status, edit, notice, password: password_ask, scroller, sheet } =
        build_dom(&document, &root).ok_or_else(|| js_error("could not build the viewer's elements"))?;
    let _ = root.set_attribute("data-mode", mode.name());
    let page = Rc::new(Page {
        window,
        document,
        asset_base,
        mode,
        root,
        status,
        edit,
        notice,
        password: password_ask,
        scroller,
        sheet,
        editor_wait_ms,
        state: RefCell::new(State {
            handover,
            worker: None,
            source,
            docx: None,
            password,
            sizes: Vec::new(),
            page_count: 0,
            sheet: pages::lay_out(&[], 1.0),
            cache: PageCache::new(CACHE_BUDGET_BYTES),
            generation: 0,
            canvases: HashMap::new(),
            missing_fonts: Vec::new(),
            refresh_queued: false,
            painted_once: false,
            removed: false,
            editor_seen: false,
            open_waiters: Vec::new(),
        }),
    });
    let api = build_api(&page);
    if install_global {
        let _ = js_sys::Reflect::set(&page.window, &JsValue::from_str(VIEWER_GLOBAL), &api);
    }
    STARTED.with(|s| *s.borrow_mut() = Some((page.clone(), api.clone())));
    wire_listeners(&page);
    run(&page, first);
    Ok(api.into())
}

/// Whether a canvas can give a WebGL context here (`webgl2`, or `webgl`, which the editor's
/// renderer falls back to).
fn has_webgl(document: &web_sys::Document) -> bool {
    let Some(canvas) = document.create_element("canvas").ok().and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok()) else {
        return false;
    };
    ["webgl2", "webgl"].iter().any(|kind| matches!(canvas.get_context(kind), Ok(Some(_))))
}

// ---- DOM ----

fn element(document: &web_sys::Document, tag: &str, css: &str) -> Option<web_sys::HtmlElement> {
    let el: web_sys::HtmlElement = document.create_element(tag).ok()?.dyn_into().ok()?;
    // Through the CSSOM, which a `style-src` policy does not restrict.
    el.style().set_css_text(css);
    Some(el)
}

struct Built {
    status: web_sys::HtmlElement,
    edit: web_sys::HtmlElement,
    notice: web_sys::HtmlElement,
    password: PasswordAsk,
    scroller: web_sys::HtmlElement,
    sheet: web_sys::HtmlElement,
}

/// The viewer's elements under `root`: a bar (name, status, Edit), a notice line, the password
/// form (hidden), and the scrolling sheet the page canvases sit in.
fn build_dom(document: &web_sys::Document, root: &web_sys::HtmlElement) -> Option<Built> {
    root.style().set_css_text(
        "position:absolute;inset:0;display:flex;flex-direction:column;background:#e6e6e6;color:#334155;z-index:400;font-family:system-ui,-apple-system,sans-serif;",
    );
    let bar = element(
        document,
        "div",
        "flex:none;height:34px;background:#2b47b5;color:#fff;display:flex;align-items:center;gap:12px;padding:0 14px;font-size:13px;",
    )?;
    let name = element(document, "span", "font-weight:600;")?;
    name.set_inner_text("WordCraft");
    let status = element(document, "span", "flex:1;opacity:0.9;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;")?;
    let _ = status.set_attribute("role", "status");
    let _ = status.set_attribute("aria-live", "polite");
    status.set_inner_text("Opening the document\u{2026}");
    let edit = element(
        document,
        "button",
        "flex:none;font:inherit;font-weight:600;color:#2b47b5;background:#fff;border:0;border-radius:4px;padding:4px 14px;cursor:pointer;",
    )?;
    let _ = edit.set_attribute("type", "button");
    edit.set_id("wordcraft_viewer_edit");
    edit.set_inner_text("Edit");
    edit.set_hidden(true);
    let notice =
        element(document, "div", "flex:none;background:#fef3c7;color:#713f12;border-bottom:1px solid #f1d38a;padding:8px 14px;font-size:13px;")?;
    notice.set_id("wordcraft_viewer_notice");
    let _ = notice.set_attribute("role", "alert");
    notice.set_hidden(true);
    let password = build_password_ask(document)?;
    let scroller = element(document, "div", "flex:1;overflow:auto;position:relative;outline:none;")?;
    scroller.set_tab_index(0);
    let _ = scroller.set_attribute("role", "document");
    let _ = scroller.set_attribute("aria-label", "Document");
    let sheet = element(document, "div", "position:relative;margin:0 auto;")?;
    for child in [&name, &status, &edit] {
        bar.append_child(child).ok()?;
    }
    scroller.append_child(&sheet).ok()?;
    for child in [&bar, &notice, &password.form, &scroller] {
        root.append_child(child).ok()?;
    }
    Some(Built { status, edit, notice, password, scroller, sheet })
}

fn build_password_ask(document: &web_sys::Document) -> Option<PasswordAsk> {
    let form = element(
        document,
        "form",
        // No `display` here: an inline `display` would override the `hidden` attribute.
        "flex:none;background:#fff;border-bottom:1px solid #d4d4d8;padding:8px 14px;font-size:13px;",
    )?;
    form.set_id("wordcraft_viewer_password");
    let _ = form.set_attribute("aria-label", "Password");
    form.set_hidden(true);
    let label = element(document, "label", "margin-right:8px;")?;
    let message = element(document, "span", "margin-right:8px;")?;
    let input: web_sys::HtmlInputElement =
        element(document, "input", "font:inherit;padding:3px 6px;border:1px solid #94a3b8;border-radius:4px;min-width:14em;")?.dyn_into().ok()?;
    input.set_type("password");
    input.set_id("wordcraft_viewer_password_input");
    input.set_autocomplete("current-password");
    let open = element(
        document,
        "button",
        "font:inherit;font-weight:600;color:#fff;background:#2b47b5;border:0;border-radius:4px;padding:4px 14px;cursor:pointer;",
    )?;
    let _ = open.set_attribute("type", "submit");
    open.set_inner_text("Open");
    let cancel = element(
        document,
        "button",
        "font:inherit;margin-left:8px;color:#2b47b5;background:transparent;border:1px solid #94a3b8;border-radius:4px;padding:3px 12px;cursor:pointer;",
    )?;
    let _ = cancel.set_attribute("type", "button");
    cancel.set_id("wordcraft_viewer_password_cancel");
    cancel.set_inner_text("Cancel");
    label.append_child(&message).ok()?;
    label.append_child(&input).ok()?;
    form.append_child(&label).ok()?;
    form.append_child(&open).ok()?;
    form.append_child(&cancel).ok()?;
    Some(PasswordAsk { form, message, input, cancel })
}

fn listen(target: &web_sys::EventTarget, kind: &str, handler: impl FnMut(web_sys::Event) + 'static) {
    let closure = Closure::<dyn FnMut(web_sys::Event)>::new(handler);
    let _ = target.add_event_listener_with_callback(kind, closure.as_ref().unchecked_ref());
    // The page has one viewer and it lives as long as the page does.
    closure.forget();
}

fn wire_listeners(page: &Rc<Page>) {
    let p = page.clone();
    listen(&page.password.form, "submit", move |event| {
        event.prevent_default();
        submit_password(&p);
    });
    let p = page.clone();
    listen(&page.password.cancel, "click", move |_| cancel_password(&p));
    let p = page.clone();
    listen(&page.scroller, "scroll", move |_| schedule_refresh(&p));
    let p = page.clone();
    listen(&page.window, "resize", move |_| relayout(&p));
    let p = page.clone();
    listen(&page.edit, "click", move |_| dispatch(&p, Event::EditRequested));
    let p = page.clone();
    listen(&page.scroller, "dblclick", move |_| dispatch(&p, Event::EditRequested));
    let p = page.clone();
    listen(&page.scroller, "keydown", move |event| {
        if let Some(key) = event.dyn_ref::<web_sys::KeyboardEvent>()
            && matches!(key.key().as_str(), "Enter" | "F2")
            && !(key.ctrl_key() || key.meta_key() || key.alt_key())
        {
            event.prevent_default();
            dispatch(&p, Event::EditRequested);
        }
    });
}

// ---- the state machine's actions ----

fn dispatch(page: &Rc<Page>, event: Event) {
    let actions = page.state.borrow_mut().handover.on(event);
    run(page, actions);
}

fn run(page: &Rc<Page>, actions: Vec<Action>) {
    for action in actions {
        match action {
            Action::StartViewer => start_viewer(page),
            Action::ShowLoading => page.status.set_inner_text("Loading the editor\u{2026}"),
            Action::LoadEditor => load_editor(page),
            Action::HandOver => hand_over(page),
            Action::AwaitPaint => await_paint(page),
            Action::HideLoading => show_page_count(page),
            Action::RemoveViewer => remove_viewer(page),
            Action::ShowNotice(reason) => show_notice(page, reason),
            Action::HideNotice => hide_notice(page),
        }
    }
    sync_phase(page);
}

fn html(page: &Page) -> Option<web_sys::Element> {
    page.document.document_element()
}

fn sync_phase(page: &Page) {
    let (phase, offered) = {
        let state = page.state.borrow();
        (state.handover.phase(), state.handover.edit_offered())
    };
    let name = match phase {
        Phase::Viewing => "viewing",
        Phase::LoadingEditor => "loading-editor",
        Phase::HandingOver => "handing-over",
        Phase::Editing => "editing",
        Phase::ViewerOnly(_) => "viewer-only",
    };
    if let Some(html) = html(page) {
        let _ = html.set_attribute("data-wordcraft-phase", name);
    }
    page.edit.set_hidden(!offered);
}

fn show_page_count(page: &Page) {
    let count = page.state.borrow().page_count;
    page.status.set_inner_text(&match count {
        0 => String::new(),
        1 => "1 page".to_string(),
        n => format!("{n} pages"),
    });
}

fn show_notice(page: &Page, reason: Reason) {
    let (code, text) = match reason {
        Reason::NoWebGl => ("no-webgl", "Editing needs WebGL, which this browser does not provide. The document stays open for reading."),
        Reason::LoadFailed => {
            ("load-failed", "The editor could not be downloaded. The document stays open for reading; reload the page to try again.")
        }
        Reason::StartFailed => ("start-failed", "The editor could not start in this browser. The document stays open for reading."),
        Reason::HandOverFailed => ("hand-over-failed", "The editor could not open this document. It stays open here for reading."),
        Reason::TimedOut => ("timed-out", "The editor took too long to start. The document stays open for reading; reload the page to try again."),
    };
    page.notice.set_inner_text(text);
    page.notice.set_hidden(false);
    if let Some(html) = html(page) {
        let _ = html.set_attribute("data-wordcraft-notice", code);
    }
}

fn hide_notice(page: &Page) {
    page.notice.set_hidden(true);
    if let Some(html) = html(page) {
        let _ = html.remove_attribute("data-wordcraft-notice");
    }
}

/// The document, or the viewer itself, failed: say so in the notice line.
fn show_error(page: &Page, code: &str, text: &str) {
    page.notice.set_inner_text(text);
    page.notice.set_hidden(false);
    page.status.set_inner_text("");
    if let Some(html) = html(page) {
        let _ = html.set_attribute("data-wordcraft-error", code);
    }
    remove_skeleton(page);
    let waiters = std::mem::take(&mut page.state.borrow_mut().open_waiters);
    for (_, reject) in waiters {
        let _ = reject.call1(&JsValue::NULL, &js_error(text));
    }
}

fn remove_skeleton(page: &Page) {
    if let Some(skeleton) = page.document.get_element_by_id(SKELETON_ID) {
        skeleton.remove();
    }
}

// ---- the viewer ----

fn start_viewer(page: &Rc<Page>) {
    let failed = page.clone();
    let started = ViewerWorker::start(&page.asset_base, move |why| {
        web_sys::console::warn_1(&format!("wordcraft viewer: {why}").into());
        show_error(&failed, "viewer-failed", "The document viewer could not start in this browser.");
    });
    match started {
        Ok(worker) => page.state.borrow_mut().worker = Some(worker),
        Err(why) => {
            web_sys::console::warn_1(&format!("wordcraft viewer: {why}").into());
            show_error(page, "viewer-failed", "The document viewer could not start in this browser.");
            return;
        }
    }
    let page = page.clone();
    wasm_bindgen_futures::spawn_local(async move {
        match document_bytes(&page, true).await {
            Ok(Some(bytes)) => {
                let password = page.state.borrow().password.clone();
                open_document(&page, bytes, password);
            }
            Ok(None) => {}
            Err(why) => show_error(&page, "document-failed", &why),
        }
    });
}

/// The document's bytes: the ones already held, else fetched from where the host said. With
/// nothing said, the viewer gets the built-in sample and the editor gets nothing.
async fn document_bytes(page: &Rc<Page>, for_viewer: bool) -> Result<Option<js_sys::Uint8Array>, String> {
    let source = {
        let mut state = page.state.borrow_mut();
        if let Some(docx) = &state.docx {
            return Ok(Some(docx.clone()));
        }
        std::mem::replace(&mut state.source, Source::Unset)
    };
    let bytes = match source {
        Source::Bytes(bytes) => bytes,
        Source::Url(url) => fetch_same_origin(page, &url).await?,
        Source::Unset if for_viewer => fetch_same_origin(page, &format!("{}{SAMPLE_FILE}", page.asset_base)).await?,
        Source::Unset => return Ok(None),
    };
    page.state.borrow_mut().docx = Some(bytes.clone());
    Ok(Some(bytes))
}

/// Fetch `url` (relative to the page) as bytes. Only this origin: a document address comes
/// from the page's URL, which anyone can write.
async fn fetch_same_origin(page: &Rc<Page>, url: &str) -> Result<js_sys::Uint8Array, String> {
    let base = page.document.base_uri().ok().flatten().unwrap_or_default();
    let resolved = web_sys::Url::new_with_base(url, &base).map_err(|_| "The document's address is not valid.".to_string())?;
    if page.window.location().origin().ok().as_deref() != Some(resolved.origin().as_str()) {
        return Err("The document is on another site, which this viewer does not open.".to_string());
    }
    let failed = || "The document could not be downloaded.".to_string();
    let response = wasm_bindgen_futures::JsFuture::from(page.window.fetch_with_str(&resolved.href())).await.map_err(|_| failed())?;
    let response: web_sys::Response = response.dyn_into().map_err(|_| failed())?;
    if !response.ok() {
        return Err(failed());
    }
    let buffer = wasm_bindgen_futures::JsFuture::from(response.array_buffer().map_err(|_| failed())?).await.map_err(|_| failed())?;
    Ok(js_sys::Uint8Array::new(&buffer))
}

/// Send `bytes` to the worker as the document to show. The bytes stay in JavaScript's memory:
/// only the few bytes of framing come from this module.
fn open_document(page: &Rc<Page>, bytes: js_sys::Uint8Array, password: Option<String>) {
    let length = bytes.length();
    let prefix = match proto::view_open_request_prefix(password.as_deref(), length as usize) {
        Ok(prefix) => prefix,
        Err(proto::ProtoError::TooLarge { .. }) => return show_error(page, "document-failed", "The document is too large for this viewer."),
        Err(_) => return show_error(page, "document-failed", "The document is empty."),
    };
    let Ok(prefix_len) = u32::try_from(prefix.len()) else { return };
    let Some(total) = prefix_len.checked_add(length) else { return };
    let frame = js_sys::Uint8Array::new_with_length(total);
    frame.set(&js_sys::Uint8Array::from(prefix.as_slice()), 0);
    frame.set(&bytes, prefix_len);

    let stale = forget_pages(page);
    let generation = {
        let mut state = page.state.borrow_mut();
        state.docx = Some(bytes);
        state.password = password;
        state.generation
    };
    drop(stale);
    let sink_page = page.clone();
    let sent = page
        .state
        .borrow()
        .worker
        .as_ref()
        .map(|w| w.request(proto::Kind::ViewOpen, frame, Box::new(move |env, payload| on_opened(&sink_page, generation, env, payload))));
    if !matches!(sent, Some(Ok(_))) {
        show_error(page, "viewer-failed", "The document viewer could not start in this browser.");
    }
}

/// Start a new generation: every page painted or awaited belongs to the old document or
/// layout. Their canvases are returned so they are released outside the state borrow.
fn forget_pages(page: &Page) -> Vec<PageCanvas> {
    let mut state = page.state.borrow_mut();
    state.generation = state.generation.wrapping_add(1);
    state.cache.clear();
    let canvases: Vec<PageCanvas> = state.canvases.drain().map(|(_, c)| c).collect();
    for canvas in &canvases {
        canvas.el.remove();
    }
    canvases
}

fn on_opened(page: &Rc<Page>, generation: u32, env: &proto::Envelope, payload: &js_sys::Uint8Array) {
    if page.state.borrow().generation != generation {
        return;
    }
    if env.kind == proto::Kind::ViewOpened {
        return match worker::small_payload(payload).and_then(|b| proto::decode_payload::<proto::ViewOpenedReply>(&b).ok()) {
            Some(opened) if proto::view_opened_reply_error(&opened).is_none() => apply_opened(page, opened),
            _ => show_error(page, "viewer-failed", "The document viewer answered with something unreadable."),
        };
    }
    let reply = worker::error_reply(payload);
    match reply.code {
        proto::ErrorCode::PasswordRequired => ask_password(page, "password-required", "This document is protected with a password."),
        proto::ErrorCode::WrongPassword => ask_password(page, "wrong-password", "That password does not open this document."),
        proto::ErrorCode::TooLarge => show_error(page, "document-failed", "The document is too large for this viewer."),
        _ => show_error(page, "document-failed", "This file is not a Word document this viewer can read."),
    }
}

/// The document needs a password: say so in the notice line (as any error, which also answers
/// waiting `open()` calls) and show the password form. The bytes stay in `state.docx`, so the
/// form only has to send the password.
/// Until a document opens, the edit actions are withdrawn (`Event::PasswordAsked`): the
/// editor would be handed bytes it cannot read.
fn ask_password(page: &Rc<Page>, code: &str, text: &str) {
    show_error(page, code, text);
    dispatch(page, Event::PasswordAsked);
    page.password.message.set_inner_text("Password:");
    page.password.input.set_value("");
    page.password.form.set_hidden(false);
    let _ = page.password.input.focus();
}

/// Not now: the form goes, the notice says the document is protected, and editing stays
/// withdrawn (there is still no document the editor could take).
fn cancel_password(page: &Page) {
    page.password.form.set_hidden(true);
    page.password.input.set_value("");
    page.notice.set_inner_text("This document is protected with a password.");
    page.notice.set_hidden(false);
}

fn submit_password(page: &Rc<Page>) {
    let given = page.password.input.value();
    let bytes = page.state.borrow().docx.clone();
    let Some(bytes) = bytes.filter(|_| !given.is_empty()) else { return };
    page.password.form.set_hidden(true);
    page.notice.set_hidden(true);
    page.status.set_inner_text("Opening the document\u{2026}");
    open_document(page, bytes, Some(given));
}

/// The worker holds a document (newly opened, or laid out again after a font arrived): take
/// its page list and paint.
fn apply_opened(page: &Rc<Page>, opened: proto::ViewOpenedReply) {
    let waiters = {
        let mut state = page.state.borrow_mut();
        state.page_count = opened.page_count;
        state.sizes = opened.sizes;
        std::mem::take(&mut state.open_waiters)
    };
    let _ = page.root.set_attribute("data-pages", &opened.page_count.to_string());
    let _ = page.scroller.set_attribute("aria-label", &format!("Document, {} pages", opened.page_count));
    if let Some(html) = html(page) {
        let _ = html.remove_attribute("data-wordcraft-error");
    }
    if !page.password.form.hidden() {
        page.password.form.set_hidden(true);
        page.password.input.set_value("");
        page.notice.set_hidden(true);
    }
    if !matches!(page.state.borrow().handover.phase(), Phase::LoadingEditor | Phase::HandingOver) {
        show_page_count(page);
    }
    dispatch(page, Event::DocumentOpened);
    relayout(page);
    ask_missing_fonts(page);
    let answer = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&answer, &JsValue::from_str("pageCount"), &JsValue::from(opened.page_count));
    for (resolve, _) in waiters {
        let _ = resolve.call1(&JsValue::NULL, &answer);
    }
}

fn ask_missing_fonts(page: &Rc<Page>) {
    let generation = page.state.borrow().generation;
    let sink_page = page.clone();
    let state = page.state.borrow();
    let Some(worker) = state.worker.as_ref() else { return };
    let _ = worker.request_json(
        proto::Kind::ViewMissingFonts,
        b"{}",
        Box::new(move |env, payload| {
            if env.kind != proto::Kind::ViewFonts || sink_page.state.borrow().generation != generation {
                return;
            }
            let Some(listed) = worker::small_payload(payload).and_then(|b| proto::decode_payload::<proto::ViewFontsReply>(&b).ok()) else { return };
            let _ = sink_page.root.set_attribute("data-missing-fonts", &listed.missing.len().to_string());
            sink_page.state.borrow_mut().missing_fonts = listed.missing;
        }),
    );
}

/// Lay the pages out for the window as it is now, move the canvases, and paint what shows.
fn relayout(page: &Rc<Page>) {
    {
        let mut state = page.state.borrow_mut();
        if state.removed {
            return;
        }
        state.sheet = pages::lay_out(&state.sizes, f64::from(page.scroller.client_width()));
        let style = page.sheet.style();
        let _ = style.set_property("width", &format!("{}px", state.sheet.width));
        let _ = style.set_property("height", &format!("{}px", state.sheet.height));
        let state = &*state;
        for (index, canvas) in &state.canvases {
            place(canvas, state.sheet.pages.get(*index as usize), state.sheet.zoom);
        }
    }
    refresh(page);
}

/// Put `canvas` at its page's box. A bitmap painted for the current zoom is shown at exactly
/// its own size in device pixels (the core rounds a page's raster up, so it can be a fraction
/// of a pixel larger than the box); one from another zoom is stretched to the box until its
/// replacement arrives.
fn place(canvas: &PageCanvas, at: Option<&pages::PageBox>, zoom: f64) {
    let Some(at) = at else { return };
    let (mut width, mut height) = (at.width, at.height);
    if let Some((w, h, scale)) = canvas.bitmap {
        let per_pixel = zoom / f64::from(scale);
        let exact = f64::from(w) * per_pixel;
        if per_pixel.is_finite() && (exact - at.width).abs() <= 1.5 {
            (width, height) = (exact, f64::from(h) * per_pixel);
        }
    }
    let style = canvas.el.style();
    for (name, value) in [("left", at.left.floor()), ("top", at.top.floor()), ("width", width), ("height", height)] {
        let _ = style.set_property(name, &format!("{value}px"));
    }
}

fn schedule_refresh(page: &Rc<Page>) {
    if std::mem::replace(&mut page.state.borrow_mut().refresh_queued, true) {
        return;
    }
    let later = page.clone();
    let callback = Closure::once_into_js(move || {
        later.state.borrow_mut().refresh_queued = false;
        refresh(&later);
    });
    if page.window.request_animation_frame(callback.unchecked_ref()).is_err() {
        page.state.borrow_mut().refresh_queued = false;
        refresh(page);
    }
}

/// The pages in the window, the ones the reader sees first, then the half window above and
/// below.
fn wanted_pages(page: &Page, sheet: &Sheet) -> Vec<u32> {
    let top = f64::from(page.scroller.scroll_top());
    let height = f64::from(page.scroller.client_height());
    let seen = pages::visible(sheet, top, height, 0.0);
    let near = pages::visible(sheet, top, height, height / 2.0);
    let index = |i: usize| u32::try_from(i).ok();
    seen.clone().chain(near.filter(|i| !seen.contains(i))).filter_map(index).collect()
}

/// Ask the worker for the pages the window shows and does not have, and release the canvases
/// of pages that are neither near the window nor painted.
fn refresh(page: &Rc<Page>) {
    let mut guard = page.state.borrow_mut();
    let state = &mut *guard;
    if state.removed || state.worker.is_none() || state.sheet.pages.is_empty() {
        return;
    }
    let ratio = page.window.device_pixel_ratio();
    let near = wanted_pages(page, &state.sheet);
    let mut wanted = Vec::with_capacity(near.len());
    for index in &near {
        let (Some(size), Some(at)) = (state.sizes.get(*index as usize), state.sheet.pages.get(*index as usize)) else { continue };
        if !state.canvases.contains_key(index)
            && let Some(canvas) = new_canvas(page, *index)
        {
            place(&canvas, Some(at), state.sheet.zoom);
            state.canvases.insert(*index, canvas);
        }
        if let Some(scale) = pages::render_scale(state.sheet.zoom, ratio, size) {
            wanted.push(PageKey::new(*index, scale));
        }
    }
    let cache = &state.cache;
    state.canvases.retain(|index, canvas| {
        let keep = near.contains(index) || cache.painted(*index).is_some();
        if !keep {
            canvas.el.remove();
        }
        keep
    });
    let generation = state.generation;
    for key in state.cache.plan(&wanted) {
        let request = proto::ViewRenderRequest { page: key.page, scale: key.scale() };
        let sink_page = page.clone();
        let sent = proto::encode_payload(&request).map_err(|e| e.to_string()).and_then(|payload| match state.worker.as_ref() {
            Some(worker) => worker.request_json(
                proto::Kind::ViewRender,
                &payload,
                Box::new(move |env, payload| on_page(&sink_page, generation, key, env, payload)),
            ),
            None => Err("no worker".to_string()),
        });
        if sent.is_err() {
            state.cache.failed(key);
        }
    }
}

/// A blank sheet of paper where page `index` will be painted.
fn new_canvas(page: &Page, index: u32) -> Option<PageCanvas> {
    let el: web_sys::HtmlCanvasElement = page.document.create_element("canvas").ok()?.dyn_into().ok()?;
    el.style().set_css_text("position:absolute;display:block;background:#fff;box-shadow:0 1px 4px rgba(15,23,42,0.25);");
    let _ = el.set_attribute("data-page", &index.to_string());
    let _ = el.set_attribute("role", "img");
    let _ = el.set_attribute("aria-label", &format!("Page {}", u64::from(index) + 1));
    page.sheet.append_child(&el).ok()?;
    Some(PageCanvas { el, bitmap: None })
}

/// A page arrived from the worker: `payload` is its pixels and the trailer that describes
/// them, still in the buffer the worker transferred.
fn on_page(page: &Rc<Page>, generation: u32, key: PageKey, env: &proto::Envelope, payload: &js_sys::Uint8Array) {
    let first_paint = {
        let mut guard = page.state.borrow_mut();
        let state = &mut *guard;
        if state.removed || state.generation != generation {
            return;
        }
        let header = (env.kind == proto::Kind::ViewPage).then(|| view_page_header(payload)).flatten();
        match header.filter(|h| h.page == key.page && h.scale.to_bits() == key.scale().to_bits()) {
            None => {
                state.cache.failed(key);
                false
            }
            Some(header) => {
                let keep = wanted_pages(page, &state.sheet);
                let pixels = payload.length().saturating_sub(proto::VIEW_PAGE_TRAILER_BYTES as u32);
                match state.cache.arrived(key, u64::from(pixels), &keep) {
                    Arrival::Stale => false,
                    Arrival::Paint { evict } => {
                        for gone in evict {
                            if let Some(canvas) = state.canvases.remove(&gone) {
                                canvas.el.remove();
                            }
                        }
                        let painted = state.canvases.get_mut(&key.page).is_some_and(|canvas| paint(canvas, payload, pixels, &header));
                        if let Some(canvas) = state.canvases.get(&key.page) {
                            place(canvas, state.sheet.pages.get(key.page as usize), state.sheet.zoom);
                        }
                        painted && !std::mem::replace(&mut state.painted_once, true)
                    }
                }
            }
        }
    };
    if first_paint {
        remove_skeleton(page);
        if let Some(html) = html(page) {
            let _ = html.set_attribute("data-wordcraft-painted", "true");
        }
    }
    // A reply frees a slot: ask for the next page the window wants.
    refresh(page);
}

/// The trailer of a `viewPage` payload, the only bytes of it copied into this module.
fn view_page_header(payload: &js_sys::Uint8Array) -> Option<proto::ViewPageHeader> {
    let len = payload.length();
    let at = len.checked_sub(proto::VIEW_PAGE_TRAILER_BYTES as u32)?;
    proto::decode_view_page_trailer(&payload.subarray(at, len).to_vec(), len as usize).ok()
}

/// Put the page's pixels on its canvas: an `ImageData` over the transferred buffer, no copy
/// through this module, then one `putImageData`. The canvas's bitmap is the page's raster,
/// pixel for pixel.
fn paint(canvas: &mut PageCanvas, payload: &js_sys::Uint8Array, pixels: u32, header: &proto::ViewPageHeader) -> bool {
    let rgba = js_sys::Uint8ClampedArray::new_with_byte_offset_and_length(&payload.buffer(), payload.byte_offset(), pixels);
    let Ok(image) = web_sys::ImageData::new_with_js_u8_clamped_array_and_sh(&rgba, header.width, header.height) else { return false };
    canvas.el.set_width(header.width);
    canvas.el.set_height(header.height);
    let Some(context) = canvas.el.get_context("2d").ok().flatten().and_then(|c| c.dyn_into::<web_sys::CanvasRenderingContext2d>().ok()) else {
        return false;
    };
    if context.put_image_data(&image, 0.0, 0.0).is_err() {
        return false;
    }
    canvas.bitmap = Some((header.width, header.height, header.scale));
    let _ = canvas.el.set_attribute("data-scale", &header.scale.to_string());
    true
}

// ---- the editor ----

/// Add the editor's canvas and its script element. The editor module starts by itself on a
/// canvas with this id and publishes its page API at `window.wordcraft`; the page waits for
/// that, and for the failures the editor reports.
fn load_editor(page: &Rc<Page>) {
    let document = &page.document;
    if document.get_element_by_id(EDITOR_CANVAS_ID).is_none()
        && let Some(canvas) = element(document, "canvas", "position:absolute;inset:0;width:100%;height:100%;display:block;")
    {
        canvas.set_id(EDITOR_CANVAS_ID);
        // Under the viewer, which covers it until the editor has painted.
        if let Some(body) = document.body() {
            let _ = body.insert_before(&canvas, body.first_child().as_ref());
        }
    }

    // The editor rejects its launch promise when it cannot start; nothing else holds that
    // promise, so the rejection surfaces here. It is this page's to handle, not the console's.
    let p = page.clone();
    listen(&page.window, "unhandledrejection", move |event| {
        let Some(rejection) = event.dyn_ref::<web_sys::PromiseRejectionEvent>() else { return };
        let text = js_sys::JsString::from(js_sys::Reflect::get(&rejection.reason(), &JsValue::from_str("message")).unwrap_or(JsValue::UNDEFINED));
        if editor_loading(&p) && String::from(text).contains(EDITOR_START_FAILURE) {
            event.prevent_default();
            dispatch(&p, Event::StartFailed);
        }
    });
    // An error thrown while the editor's own files evaluate (its module could not compile,
    // say) is reported on the window with their name.
    let p = page.clone();
    listen(&page.window, "error", move |event| {
        let Some(error) = event.dyn_ref::<web_sys::ErrorEvent>() else { return };
        if editor_loading(&p) && error.filename().contains("wordcraft-web") {
            event.prevent_default();
            dispatch(&p, Event::StartFailed);
        }
    });

    let Some(script) = document.create_element("script").ok().and_then(|e| e.dyn_into::<web_sys::HtmlScriptElement>().ok()) else {
        return dispatch(page, Event::ScriptFailed);
    };
    script.set_type("module");
    script.set_src(&format!("{}{EDITOR_LOADER_FILE}", page.asset_base));
    let p = page.clone();
    listen(&script, "error", move |_| dispatch(&p, Event::ScriptFailed));
    let appended = document.head().map(|head| head.append_child(&script).is_ok()).unwrap_or(false);
    if !appended {
        return dispatch(page, Event::ScriptFailed);
    }

    watch_for_editor(page);
    // The deadline: one timer, not a poll. An editor that never publishes its API and never
    // reports a failure (a stalled download, a module that hangs) ends in the notice; if it
    // does arrive later, `watch_for_editor` still sees it and it takes over.
    let p = page.clone();
    let deadline = Closure::once_into_js(move || {
        if !p.state.borrow().editor_seen {
            dispatch(&p, Event::EditorTimedOut);
        }
    });
    let wait = i32::try_from(page.editor_wait_ms).unwrap_or(i32::MAX);
    if page.window.set_timeout_with_callback_and_timeout_and_arguments_0(deadline.unchecked_ref(), wait).is_err() {
        web_sys::console::warn_1(&"wordcraft viewer: no timer for the editor's deadline".into());
    }
}

/// Learn the moment the editor publishes `window.wordcraft`, without polling: the property is
/// an accessor until the editor assigns it (`craft-web-api` assigns, it does not define), and
/// the setter turns it back into a plain property holding the API. The editor attaches
/// `loadDocx` to the object in the same task, so the page looks one task later. Costs
/// nothing while the editor is away, however long that is.
fn watch_for_editor(page: &Rc<Page>) {
    if editor_load_fn(page).is_some() {
        return editor_seen(page);
    }
    let key = JsValue::from_str(EDITOR_GLOBAL);
    let p = page.clone();
    let window_obj: js_sys::Object = page.window.clone().into();
    let target = window_obj.clone();
    let set_key = key.clone();
    let setter = Closure::<dyn FnMut(JsValue)>::new(move |api: JsValue| {
        let plain = js_sys::Object::new();
        for (name, value) in [("value", api), ("writable", JsValue::TRUE), ("configurable", JsValue::TRUE), ("enumerable", JsValue::TRUE)] {
            let _ = js_sys::Reflect::set(&plain, &JsValue::from_str(name), &value);
        }
        let _ = js_sys::Reflect::define_property(&target, &set_key, &plain);
        let later = p.clone();
        let next = Closure::once_into_js(move || editor_seen(&later));
        let _ = p.window.set_timeout_with_callback(next.unchecked_ref());
    });
    let descriptor = js_sys::Object::new();
    let getter = Closure::<dyn Fn() -> JsValue>::new(|| JsValue::UNDEFINED);
    let _ = js_sys::Reflect::set(&descriptor, &JsValue::from_str("get"), getter.as_ref());
    let _ = js_sys::Reflect::set(&descriptor, &JsValue::from_str("set"), setter.as_ref());
    let _ = js_sys::Reflect::set(&descriptor, &JsValue::from_str("configurable"), &JsValue::TRUE);
    if js_sys::Reflect::define_property(&window_obj, &key, &descriptor).unwrap_or(false) {
        // The page has one viewer, and the accessor lives until the editor replaces it.
        getter.forget();
        setter.forget();
    } else {
        web_sys::console::warn_1(&"wordcraft viewer: could not watch for the editor".into());
    }
}

/// The editor's page API is up (or published something without `loadDocx`, which the hand-over
/// reports as its failure).
fn editor_seen(page: &Rc<Page>) {
    page.state.borrow_mut().editor_seen = true;
    dispatch(page, Event::EditorReady);
}

/// The editor is still the page's business: loading, being handed the document, or late after
/// the wait (whose failures are then this page's to report, not the console's).
fn editor_loading(page: &Page) -> bool {
    matches!(page.state.borrow().handover.phase(), Phase::LoadingEditor | Phase::HandingOver | Phase::ViewerOnly(Reason::TimedOut))
}

/// The editor's API object and its `loadDocx`, once the editor has published them.
fn editor_load_fn(page: &Page) -> Option<(JsValue, js_sys::Function)> {
    let api = js_sys::Reflect::get(&page.window, &JsValue::from_str(EDITOR_GLOBAL)).ok()?;
    let load = js_sys::Reflect::get(&api, &JsValue::from_str(EDITOR_LOAD_FN)).ok()?.dyn_into::<js_sys::Function>().ok()?;
    Some((api, load))
}

/// Call `api[name](args…)`, a page API method that returns a promise, and wait for it.
async fn call_editor(api: &JsValue, name: &str, args: &js_sys::Array) -> Result<JsValue, String> {
    let method = js_sys::Reflect::get(api, &JsValue::from_str(name))
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok())
        .ok_or_else(|| format!("the editor has no `{name}`"))?;
    let promise: js_sys::Promise =
        method.apply(api, args).map_err(|_| format!("{name} threw"))?.dyn_into().map_err(|_| format!("{name} returned no promise"))?;
    wasm_bindgen_futures::JsFuture::from(promise).await.map_err(|e| {
        let message = js_sys::Reflect::get(&e, &JsValue::from_str("message")).ok().and_then(|m| m.as_string()).unwrap_or_default();
        format!("{name} rejected: {message}")
    })
}

/// Give the editor the document the viewer shows: the same `.docx` bytes, through the
/// editor's existing page API.
///
/// The editor starts on File › Home with no document when it is given none, and refuses to
/// load into that empty window ("No document is open"). So when the first `loadDocx` is
/// refused, the page does what Home's Blank document tile does, through the same page API:
/// makes a blank document (`execute("file.new")`), loads into it (the blank document is
/// replaced in place, so the editor ends with the one document the viewer showed), and leaves
/// File › Home (`execute("ui.backstage", {value: false})`), which a command run through the
/// API does not do by itself.
fn hand_over(page: &Rc<Page>) {
    let page = page.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let outcome = async {
            let Some(bytes) = document_bytes(&page, false).await? else { return Ok(()) };
            let (api, _) = editor_load_fn(&page).ok_or("the editor's page API is gone")?;
            let password = page.state.borrow().password.clone().map_or(JsValue::UNDEFINED, |p| JsValue::from_str(&p));
            let args = js_sys::Array::of2(&bytes, &password);
            if let Err(first) = call_editor(&api, EDITOR_LOAD_FN, &args).await {
                web_sys::console::info_1(&format!("wordcraft viewer: {first}; opening a blank document to load into").into());
                let new_doc = js_sys::Array::of2(&JsValue::from_str("file.new"), &js_sys::Object::new());
                call_editor(&api, "execute", &new_doc).await?;
                call_editor(&api, EDITOR_LOAD_FN, &args).await?;
                let leave_home = js_sys::Object::new();
                let _ = js_sys::Reflect::set(&leave_home, &JsValue::from_str("value"), &JsValue::FALSE);
                call_editor(&api, "execute", &js_sys::Array::of2(&JsValue::from_str("ui.backstage"), &leave_home)).await?;
            }
            Ok::<(), String>(())
        }
        .await;
        match outcome {
            Ok(()) => dispatch(&page, Event::DocumentAccepted),
            Err(why) => {
                web_sys::console::warn_1(&format!("wordcraft viewer: hand-over failed: {why}").into());
                dispatch(&page, Event::HandOverFailed);
            }
        }
    });
}

/// Give the editor two animation frames with the document, then let it take the page.
fn await_paint(page: &Rc<Page>) {
    let (first, second) = (page.clone(), page.clone());
    let painted = Closure::once_into_js(move || {
        let inner = Closure::once_into_js(move || dispatch(&second, Event::EditorPainted));
        if first.window.request_animation_frame(inner.unchecked_ref()).is_err() {
            dispatch(&first, Event::EditorPainted);
        }
    });
    if page.window.request_animation_frame(painted.unchecked_ref()).is_err() {
        dispatch(page, Event::EditorPainted);
    }
}

/// The editor has painted: stop the worker, take the viewer's elements away, and give the
/// editor's canvas the focus.
fn remove_viewer(page: &Rc<Page>) {
    let (worker, canvases) = {
        let mut state = page.state.borrow_mut();
        state.removed = true;
        state.cache.clear();
        (state.worker.take(), state.canvases.drain().collect::<Vec<_>>())
    };
    drop(worker);
    drop(canvases);
    page.root.remove();
    remove_skeleton(page);
    if let Some(canvas) = page.document.get_element_by_id(EDITOR_CANVAS_ID).and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok()) {
        let _ = canvas.focus();
    }
}

// ---- the page API ----

fn set_fn(api: &js_sys::Object, name: &str, function: &JsValue) {
    let _ = js_sys::Reflect::set(api, &JsValue::from_str(name), function);
}

/// The object `start(options)` resolves to: `mode`, `phase()`, `pageCount()`,
/// `missingFonts()`, `open(bytes, password?)`, `installFont(name, bytes)` and `edit()`.
fn build_api(page: &Rc<Page>) -> js_sys::Object {
    let api = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&api, &JsValue::from_str("mode"), &JsValue::from_str(page.mode.name()));

    let p = page.clone();
    let phase = Closure::<dyn Fn() -> JsValue>::new(move || {
        html(&p).and_then(|h| h.get_attribute("data-wordcraft-phase")).map_or(JsValue::NULL, |s| JsValue::from_str(&s))
    });
    set_fn(&api, "phase", phase.as_ref());
    phase.forget();

    let p = page.clone();
    let page_count = Closure::<dyn Fn() -> u32>::new(move || p.state.borrow().page_count);
    set_fn(&api, "pageCount", page_count.as_ref());
    page_count.forget();

    let p = page.clone();
    let missing = Closure::<dyn Fn() -> js_sys::Array>::new(move || p.state.borrow().missing_fonts.iter().map(|s| JsValue::from_str(s)).collect());
    set_fn(&api, "missingFonts", missing.as_ref());
    missing.forget();

    let p = page.clone();
    let edit = Closure::<dyn Fn()>::new(move || dispatch(&p, Event::EditRequested));
    set_fn(&api, "edit", edit.as_ref());
    edit.forget();

    let p = page.clone();
    let open = Closure::<dyn Fn(JsValue, JsValue) -> js_sys::Promise>::new(move |bytes: JsValue, password: JsValue| {
        let Some(bytes) = bytes_of(&bytes) else {
            return js_sys::Promise::reject(&js_error("open(bytes): expected a Uint8Array or an ArrayBuffer"));
        };
        if p.state.borrow().worker.is_none() {
            return js_sys::Promise::reject(&js_error("the viewer is not running"));
        }
        let page = p.clone();
        js_sys::Promise::new(&mut |resolve, reject| {
            page.state.borrow_mut().open_waiters.push((resolve, reject));
            open_document(&page, bytes.clone(), password.as_string());
        })
    });
    set_fn(&api, "open", open.as_ref());
    open.forget();

    let p = page.clone();
    let install = Closure::<dyn Fn(JsValue, JsValue) -> js_sys::Promise>::new(move |name: JsValue, bytes: JsValue| install_font(&p, &name, &bytes));
    set_fn(&api, "installFont", install.as_ref());
    install.forget();

    api
}

/// `installFont(name, bytes)`: make a font file available to the viewer, which lays the open
/// document out again with it.
fn install_font(page: &Rc<Page>, name: &JsValue, bytes: &JsValue) -> js_sys::Promise {
    let (Some(name), Some(bytes)) = (name.as_string(), bytes_of(bytes)) else {
        return js_sys::Promise::reject(&js_error("installFont(name, bytes): expected a string and a Uint8Array or an ArrayBuffer"));
    };
    if bytes.length() as usize > proto::MAX_VIEW_FONT_BYTES {
        return js_sys::Promise::reject(&js_error("installFont: the font file is too large"));
    }
    let payload = match proto::encode_view_install_font_request(&proto::ViewInstallFontRequest { name, bytes: bytes.to_vec() }) {
        Ok(payload) => payload,
        Err(e) => return js_sys::Promise::reject(&js_error(&format!("installFont: {e}"))),
    };
    let page = page.clone();
    js_sys::Promise::new(&mut |resolve, reject| {
        let sink_page = page.clone();
        let failed = reject.clone();
        let sent = page.state.borrow().worker.as_ref().map(|w| {
            w.request_json(
                proto::Kind::ViewInstallFont,
                &payload,
                Box::new(move |env, payload| match env.kind {
                    proto::Kind::ViewOpened => {
                        let opened = worker::small_payload(payload).and_then(|b| proto::decode_payload::<proto::ViewOpenedReply>(&b).ok());
                        if let Some(opened) = opened.filter(|o| proto::view_opened_reply_error(o).is_none()) {
                            // Every painted page was laid out with the old fonts.
                            drop(forget_pages(&sink_page));
                            apply_opened(&sink_page, opened);
                        }
                        let _ = resolve.call0(&JsValue::NULL);
                    }
                    proto::Kind::Ack => {
                        let _ = resolve.call0(&JsValue::NULL);
                    }
                    _ => {
                        let _ = reject.call1(&JsValue::NULL, &js_error(&worker::error_reply(payload).message));
                    }
                }),
            )
        });
        if !matches!(sent, Some(Ok(_))) {
            let _ = failed.call1(&JsValue::NULL, &js_error("the viewer is not running"));
        }
    })
}
