//! The typed client for the PDF export dedicated worker (deferred-items plan §3.4), on the
//! [`wordcraft_worker_proto`] contract: `[headerJson, payloadArrayBuffer]` messages, the
//! `renderPdf` payload a binary frame of `[metaJson, docxBytes, font0Bytes, …]` so a
//! multi-megabyte package never rides inside JSON, and the reply's PDF bytes transferred back.
//!
//! The worker is started lazily on the first export — nothing fetches its scripts on page load
//! (§3.4: "fetched only when `file.export_pdf` is triggered"). A browser that can't start the
//! worker answers `None`, so PDF export is simply unavailable, never a panic (§3.1).
//!
//! Rendering is a whole-file job (the document crosses as a `.docx` interchange package with
//! the non-bundled font faces as copies), so the stale-revision dropping of the proofing client
//! does not apply; cancellation stays cooperative and a long job is stopped with
//! [`PdfWorker::terminate`].

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wasm_bindgen::closure::Closure;
use wordcraft_worker_proto as proto;

/// The Trunk loader shim file (see the `data-type="worker"` + `data-loader-shim` link in
/// `index.html`; Trunk names worker artifacts after the binary without a content hash).
const LOADER_FILE: &str = "wordcraft-worker-pdf_loader.js";

struct Inner {
    worker: web_sys::Worker,
    ids: proto::RequestIds,
    /// The worker's `ready` notice arrived.
    ready: bool,
    /// The worker failed to start or errored: the feature is unavailable.
    failed: bool,

    /// Jobs posted before the worker's `ready` notice: a module worker still evaluating its
    /// top-level `await init()` silently DROPS transferred messages (measured 2026-11-15: a
    /// renderPdf posted pre-ready never arrives; the same bytes posted after `ready` answer),
    /// so early posts wait here and flush when `ready` lands.
    queued: Vec<(u32, proto::Kind, Vec<u8>)>,
    pending: HashMap<u32, Box<dyn FnOnce(&proto::Envelope, &[u8])>>,
}

/// A running PDF worker — one per page ([`ensure`]); it is not `Clone` because its
/// `onmessage`/`onerror` pair is registered exactly once and dies with it.
pub(crate) struct PdfWorker {
    inner: Rc<RefCell<Inner>>,
    /// The `onmessage`/`onerror` closures live as long as the worker is held.
    _onmessage: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _onerror: Closure<dyn FnMut(web_sys::ErrorEvent)>,
}

impl PdfWorker {
    /// `new Worker(<assetBase>wordcraft-worker-pdf_loader.js, { type: "module" })`, the
    /// same-origin loader shim Trunk generates (§3.1: no `blob:` URL — `worker-src 'self'`).
    fn start(asset_base: &str) -> Result<PdfWorker, String> {
        let url = format!("{asset_base}{LOADER_FILE}");
        let options = web_sys::WorkerOptions::new();
        options.set_type(web_sys::WorkerType::Module);
        let worker = web_sys::Worker::new_with_options(&url, &options).map_err(|e| format!("cannot start {LOADER_FILE}: {e:?}"))?;
        let inner = Rc::new(RefCell::new(Inner {
            worker: worker.clone(),
            ids: proto::RequestIds::new(),
            ready: false,
            failed: false,
            pending: HashMap::new(),
            queued: Vec::new(),
        }));

        let handler = inner.clone();
        let onmessage = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            on_message(&handler, &event.data());
        });
        worker.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

        let handler = inner.clone();
        let onerror = Closure::<dyn FnMut(web_sys::ErrorEvent)>::new(move |event: web_sys::ErrorEvent| {
            let (pending, ready) = {
                let mut inner = handler.borrow_mut();
                inner.failed = true;
                inner.queued.clear();
                (inner.pending.drain().collect::<Vec<_>>(), inner.ready)
            };
            log::warn!("pdf worker failed: {}", super::worker_error_text(&event));
            // As the codec client: not loaded before `ready`, stopped while working after it.
            fail_pending(pending, &super::worker_failure("pdf", ready));
        });
        worker.set_onerror(Some(onerror.as_ref().unchecked_ref()));

        Ok(PdfWorker { inner, _onmessage: onmessage, _onerror: onerror })
    }

    /// Whether the worker answered `ready`.
    #[allow(dead_code)] // For diagnostics; the export resolves through the same channel.
    pub(crate) fn alive(&self) -> bool {
        let inner = self.inner.borrow();
        !inner.failed && inner.ready
    }

    /// Post `(kind, payload)`; the request `id` it went out with.
    fn send(&self, kind: proto::Kind, payload: &[u8]) -> Result<u32, String> {
        let mut inner = self.inner.borrow_mut();
        if inner.failed {
            return Err("pdf worker failed".into());
        }
        let id = inner.ids.next_id();
        if !inner.ready {
            inner.queued.push((id, kind, payload.to_vec()));
            return Ok(id);
        }
        let env = proto::Envelope::request(id, kind, 0);
        let header = proto::encode_envelope(&env).map_err(|e| e.to_string())?;
        let header = String::from_utf8(header).map_err(|_| "envelope was not UTF-8".to_string())?;
        let bytes = js_sys::Uint8Array::from(payload);
        let msg = js_sys::Array::of2(&wasm_bindgen::JsValue::from_str(&header), &bytes);
        let transfer = js_sys::Array::of1(&bytes.buffer());
        inner.worker.post_message_with_transfer(&msg, &transfer).map_err(|e| format!("postMessage failed: {e:?}"))?;
        Ok(id)
    }

    /// `renderPdf` request; `on_reply` gets the PDF bytes, or the typed error the worker
    /// answered with. The pending entry is registered after `postMessage` returns — a reply can
    /// only arrive on a later task turn.
    pub(crate) fn render_pdf(
        &self,
        req: &proto::PdfRenderRequest,
        on_reply: impl FnOnce(Result<Vec<u8>, proto::ErrorReply>) + 'static,
    ) -> Result<u32, String> {
        let payload = proto::encode_pdf_request(req).map_err(|e| e.to_string())?;
        let id = self.send(proto::Kind::RenderPdf, &payload)?;
        self.inner.borrow_mut().pending.insert(
            id,
            Box::new(move |env: &proto::Envelope, payload: &[u8]| match env.kind {
                proto::Kind::Pdf => on_reply(Ok(payload.to_vec())),
                proto::Kind::Error => on_reply(Err(proto::decode_payload::<proto::ErrorReply>(payload)
                    .unwrap_or_else(|e| proto::ErrorReply { code: proto::ErrorCode::Internal, message: format!("bad error payload: {e}") }))),
                other => on_reply(Err(proto::ErrorReply { code: proto::ErrorCode::Internal, message: format!("unexpected reply kind {other:?}") })),
            }),
        );
        Ok(id)
    }

    /// Liveness probe.
    #[allow(dead_code)] // Kept for the wiring that reports worker health.
    pub(crate) fn ping(&self) -> Result<u32, String> {
        self.send(proto::Kind::Ping, &[])
    }

    /// Hard stop — the only way to interrupt a long job (§3.1).
    #[allow(dead_code)] // Kept for a future cancel affordance.
    pub(crate) fn terminate(self) {
        self.inner.borrow().worker.terminate();
    }
}

/// Complete each pending request with a typed `error` reply after the worker died: callers get
/// a failure instead of a promise that never settles. Runs outside the state borrow — the sinks
/// can re-enter the page's callers.
fn fail_pending(pending: Vec<(u32, Box<dyn FnOnce(&proto::Envelope, &[u8])>)>, message: &str) {
    let reply = proto::ErrorReply { code: proto::ErrorCode::Internal, message: message.to_string() };
    let Ok(payload) = proto::encode_payload(&reply) else { return };
    for (id, sink) in pending {
        sink(&proto::Envelope::request(id, proto::Kind::Error, 0), &payload);
    }
}

/// Route one `[headerJson, payloadBuffer]` message from the worker.
fn on_message(inner: &Rc<RefCell<Inner>>, data: &wasm_bindgen::JsValue) {
    let arr = js_sys::Array::from(data);
    let Some(header) = arr.get(0).as_string() else {
        log::warn!("pdf worker: message without a JSON header");
        return;
    };
    let payload = js_sys::Uint8Array::new(&arr.get(1)).to_vec();
    let env = match proto::decode_envelope(header.as_bytes()) {
        Ok(env) => env,
        Err(e) => {
            log::warn!("pdf worker: undecodable header ({e})");
            return;
        }
    };
    let pending = {
        let mut inner = inner.borrow_mut();
        if env.kind == proto::Kind::Ready {
            inner.ready = true;
            let queued = std::mem::take(&mut inner.queued);
            let worker = inner.worker.clone();
            drop(inner);
            for (id, kind, payload) in queued {
                let env = proto::Envelope::request(id, kind, 0);
                let Ok(header) = proto::encode_envelope(&env)
                    .map_err(|e| e.to_string())
                    .and_then(|h| String::from_utf8(h).map_err(|_| "envelope was not UTF-8".to_string()))
                else {
                    log::warn!("worker: cannot encode a queued message");
                    continue;
                };
                let bytes = js_sys::Uint8Array::from(payload.as_slice());
                let msg = js_sys::Array::of2(&wasm_bindgen::JsValue::from_str(&header), &bytes);
                let transfer = js_sys::Array::of1(&bytes.buffer());
                if let Err(e) = worker.post_message_with_transfer(&msg, &transfer) {
                    log::warn!("worker: a queued message failed to send: {e:?}");
                }
            }
            return;
        }
        match inner.pending.remove(&env.id) {
            Some(p) => p,
            None => return,
        }
    };
    (pending)(&env, &payload);
}

impl Drop for PdfWorker {
    fn drop(&mut self) {
        // Complete the outstanding requests (a worker stopped for a deadline must not leave the
        // jobs behind it waiting forever), detach so a message that races the drop can't touch
        // freed state, then stop the worker itself.
        let (worker, pending) = {
            let mut inner = self.inner.borrow_mut();
            inner.queued.clear();
            (inner.worker.clone(), inner.pending.drain().collect::<Vec<_>>())
        };
        worker.set_onmessage(None);
        worker.set_onerror(None);
        worker.terminate();
        fail_pending(pending, "the pdf worker was stopped");
    }
}

thread_local! {
    /// The one PDF worker per page, started on first ask.
    static WORKER: RefCell<Option<PdfWorker>> = const { RefCell::new(None) };
    /// Construction already failed once: don't retry every ask.
    static WORKER_FAILED: Cell<bool> = const { Cell::new(false) };
}

/// Start the PDF worker if it isn't already; `true` when it is running (or already was).
pub(crate) fn ensure(asset_base: &str) -> bool {
    // A worker that errored at runtime is dead weight: drop it (its `Drop` terminates the JS
    // worker) and start fresh below. `WORKER_FAILED` stays reserved for `start` failures.
    let (alive, dropped) = WORKER.with(|w| {
        let mut slot = w.borrow_mut();
        let dropped = if slot.as_ref().is_some_and(|worker| worker.inner.borrow().failed) { slot.take() } else { None };
        (slot.is_some(), dropped)
    });
    // Dropped outside the slot's borrow: its `Drop` completes waiting requests, whose
    // continuations may ask for the worker again.
    drop(dropped);
    if alive {
        return true;
    }
    if WORKER_FAILED.with(|f| f.get()) {
        return false;
    }
    match PdfWorker::start(asset_base) {
        Ok(worker) => {
            WORKER.with(|w| *w.borrow_mut() = Some(worker));
            true
        }
        Err(e) => {
            log::warn!("{e}");
            WORKER_FAILED.with(|f| f.set(true));
            false
        }
    }
}

/// Stop the shared worker (one stuck past a job's deadline): its waiting requests fail, and the
/// next [`ensure`] starts a fresh one. Dropped outside the slot's borrow, so a request's
/// continuation may start the next job.
pub(crate) fn stop() {
    let stopped = WORKER.with(|w| w.borrow_mut().take());
    drop(stopped);
}

/// Run `f` on the shared worker when it is up (after [`ensure`]).
pub(crate) fn with<R>(f: impl FnOnce(&PdfWorker) -> R) -> Option<R> {
    WORKER.with(|w| w.borrow().as_ref().map(f))
}
