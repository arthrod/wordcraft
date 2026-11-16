//! Host modes: a page that runs WordCraft (as an `<iframe>`, or in its own document) opens
//! documents in it and stores what it saves instead of the browser downloading. The messages and
//! the two transports are `craft-web-host`'s (storytold/craft-libs), the same contract as the
//! other Craft apps' bridges, with the `wordcraft:` prefix. This module is WordCraft's part of it.
//!
//! - `?host=parent` (an `<iframe>`, such as a Nextcloud app) talks to the same-origin parent over
//!   `postMessage`; `data-host="page"` on the canvas (the page's own document, no iframe) uses
//!   DOM `CustomEvent`s on the canvas instead. `?host=parent` wins when both are present. With no
//!   parent, or a parent on another origin, there is no bridge: the normal app with downloads.
//! - `wordcraft:ready` once the app is running and listening; send a file now. WordCraft reads
//!   bytes, so it says `file: false` and a host sends `bytes`.
//! - `wordcraft:open` (from the host): `{ name, bytes }`. It opens like a document picked with
//!   File › Open or dropped on the window; a file over [`MAX_OPEN_BYTES`] is refused before it is
//!   copied.
//! - `wordcraft:save` (to the host): `{ name, saveAs, bytes }` for every file WordCraft would
//!   otherwise download. The host stores it and reports failures itself. `saveAs` says whether
//!   the file becomes the document's file:
//!   - Save: `false`. It writes the opened file under its own name and format, as the desktop
//!     app does. A name the parent never sent (an untitled document, an imported .doc saved as
//!     .docx) is a new file: `true`, as for Save As.
//!   - Save As, and File › Export to another document format (which runs Save As): `true` for a
//!     format that keeps everything (.docx and the other Word packages, .odt, .rtf). The document
//!     is now that file; the next Save sends its name with `false`.
//!   - Save As or Export to a format that doesn't (PDF, Markdown, plain text), Export as PDF and
//!     Export Page as PNG: `false`. They are copies; the document's file stays the same.
//!   - A format the browser build writes in an on-demand worker (`.rtf`, `.odt`, `.html`, PDF…;
//!     `web/lazy_io.rs`): `false`, sent when the worker has written it, after the command
//!     returned. Such a save is a copy, whatever the format keeps.
//! - `wordcraft:dirty` whenever unsaved changes appear or go away, `wordcraft:failed` when the
//!   app couldn't start.
//!
//! In both host modes the host sends the document to edit, so no sample opens, saves are handed
//! to the host rather than downloaded, the host guards leaving the page, and `?author=` names the
//! user for comments and tracked changes.

use std::rc::Rc;

use craft_web_host::web::{FileData, Host, Message};
use wordcraft_ui_egui::{Inbox, Services, WordApp};

/// The app id in the host messages (`wordcraft:ready`).
const APP: &str = "wordcraft";

/// The name a document gets when the host sent none (the crate says `document` for a blank name).
const DEFAULT_NAME: &str = "Document.docx";

/// The largest document taken from the host page, refused before it is copied.
const MAX_OPEN_BYTES: u64 = 512 << 20;

/// The longest user name taken from `?author=`.
const MAX_AUTHOR_CHARS: usize = 200;

#[derive(Clone)]
pub struct Bridge {
    host: Rc<Host>,
}

impl Bridge {
    /// The bridge for this page, or `None` when WordCraft isn't hosted: no `?host=parent` and no
    /// `data-host="page"` on the canvas, an unframed `?host=parent` page (it stays the normal app
    /// with downloads), or a parent on another origin.
    pub fn detect(canvas: &web_sys::HtmlCanvasElement) -> Option<Self> {
        Some(Self { host: Rc::new(Host::connect(APP, canvas)?) })
    }

    /// Connect using an explicit `StartOptions.host` override (`"none"`, `"parent"`, or
    /// `"page"`) or fall back to [`Self::detect`] when omitted.
    pub fn detect_with_override(canvas: &web_sys::HtmlCanvasElement, mode: Option<&str>) -> Result<Option<Self>, String> {
        match mode {
            None => Ok(Self::detect(canvas)),
            Some("none") => Ok(None),
            Some("page") => {
                if canvas.get_attribute("data-host").as_deref() != Some("page") {
                    let _ = canvas.set_attribute("data-host", "page");
                }
                // `?host=parent` outranks `data-host="page"` in the crate's detection, so an
                // explicit page mode withdraws that signal from the URL first.
                withdraw_parent_query();
                Ok(Self::detect(canvas))
            }
            Some("parent") => {
                supply_parent_query();
                Ok(Self::detect(canvas))
            }
            Some(other) => Err(format!("invalid host mode `{other}` (expected \"none\", \"parent\", or \"page\")")),
        }
    }

    /// Hand saved and exported files to the host instead of downloading them, report unsaved
    /// changes to it, and hear the files it sends.
    pub fn connect(&self, services: &mut Services, inbox: Inbox, ctx: egui::Context) {
        self.listen(inbox, ctx);
        let bridge = self.clone();
        services.host_save = Some(Box::new(move |name: &str, bytes: &[u8], save_as: bool| bridge.post_save(name, bytes, save_as)));
        // Anything else that would download goes to the host too.
        let bridge = self.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| bridge.post_save(name, bytes, false)));
        // The browser's save picker is never used, so no save finishes later.
        services.async_saves = None;
        // The crate does not repeat a state it already reported.
        let host = self.host.clone();
        services.on_dirty = Some(Box::new(move |dirty: bool| host.post_dirty(dirty)));
    }

    /// `?author=`: the host's name for the user signs comments and tracked changes.
    pub fn set_author(app: &mut WordApp, author: &str) {
        let author: String = author.trim().chars().take(MAX_AUTHOR_CHARS).collect();
        if !author.is_empty() {
            app.session.author = author;
        }
    }

    pub fn post_ready(&self) {
        let _ = self.host.post_ready(env!("CARGO_PKG_VERSION"), false);
    }

    /// The app couldn't start (no WebGPU or WebGL2, for example): let the host say so.
    pub fn post_failed(&self, error: &str) {
        let _ = self.host.post_failed(error, None);
    }

    /// A file the host stores that does not become the document's file (`saveAs: false`): an
    /// on-demand worker's export, delivered after the save command returned.
    pub(crate) fn post_copy(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        self.post_save(path, bytes, false)
    }

    fn post_save(&self, path: &str, bytes: &[u8], save_as: bool) -> Result<(), String> {
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string());
        self.host.post_save(&name, bytes, None, save_as)
    }

    /// Hear `wordcraft:open` and queue the file like a dropped one.
    fn listen(&self, inbox: Inbox, ctx: egui::Context) {
        self.host.listen(move |message| {
            if let Message::Open { name, data } = message {
                deliver_open(&inbox, &ctx, &name, &data);
            }
        });
    }
}

/// The `craft_web_host` transport reads the parent mode only from `?host=parent` on the page
/// URL (its `data-host` attribute carries `page` mode), so an explicit `host: "parent"` start
/// option supplies the same signal: add it to the query without reloading. `Host::connect`
/// still refuses an unframed or cross-origin parent itself.
fn supply_parent_query() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let location = window.location();
    let Ok(search) = location.search() else {
        return;
    };
    if search.trim_start_matches('?').split('&').any(|p| p == "host=parent") {
        return;
    }
    let (Ok(pathname), Ok(hash)) = (location.pathname(), location.hash()) else {
        return;
    };
    let query = if search.is_empty() { "?host=parent".to_string() } else { format!("{search}&host=parent") };
    let url = format!("{pathname}{query}{hash}");
    let Ok(history) = window.history() else {
        return;
    };
    // The current entry's state passes through unchanged: the embedding page's router may keep
    // route or restoration data there, and a null state would wipe it.
    let state = history.state().unwrap_or(wasm_bindgen::JsValue::NULL);
    if let Err(e) = history.replace_state_with_url(&state, "", Some(&url)) {
        log::warn!("host=parent: could not update the page URL ({e:?}); not hosted");
    }
}

/// The counterpart of [`supply_parent_query`] for an explicit `host: "page"` start: the crate's
/// `Mode::detect` lets a `?host=parent` query outrank `data-host="page"`, so the query signal
/// is removed (without reloading) before detection runs. Query-based detection is untouched —
/// this only runs when the caller asked for page mode.
fn withdraw_parent_query() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let location = window.location();
    let Ok(search) = location.search() else {
        return;
    };
    let pairs: Vec<&str> = search.trim_start_matches('?').split('&').filter(|p| !p.is_empty()).collect();
    if !pairs.contains(&"host=parent") {
        return;
    }
    let kept: Vec<&str> = pairs.into_iter().filter(|p| *p != "host=parent").collect();
    let query = if kept.is_empty() { String::new() } else { format!("?{}", kept.join("&")) };
    let (Ok(pathname), Ok(hash)) = (location.pathname(), location.hash()) else {
        return;
    };
    let url = format!("{pathname}{query}{hash}");
    let Ok(history) = window.history() else {
        return;
    };
    // The state passes through unchanged, as in `supply_parent_query`.
    let state = history.state().unwrap_or(wasm_bindgen::JsValue::NULL);
    if let Err(e) = history.replace_state_with_url(&state, "", Some(&url)) {
        log::warn!("host=page: could not update the page URL ({e:?}); the parent query still wins");
    }
}

/// The file an open message carries, pushed onto the inbox for the next frame, like a dropped
/// file. A host's file is untrusted input: an oversized one is refused before it is copied into
/// the app's memory (the data's own length, not the host's claim). The crate has cleaned the name
/// (and cut it to 255 characters); a blank one comes as `document` and becomes `Document.docx`.
fn deliver_open(inbox: &Inbox, ctx: &egui::Context, name: &str, data: &FileData) {
    let name = if name == "document" { DEFAULT_NAME } else { name }.to_string();
    let FileData::Bytes(array) = data else {
        log::warn!("host file {name} came as a File; WordCraft reads bytes only; not opened");
        return;
    };
    if data.size() > MAX_OPEN_BYTES as f64 {
        log::warn!("host file is {} bytes, over the {} MiB host open limit; not opened", data.size(), MAX_OPEN_BYTES >> 20);
        return;
    }
    inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, array.to_vec()));
    ctx.request_repaint();
}
