//! The typed client for the codec dedicated worker (deferred-items plan §3.3), on the
//! [`wordcraft_worker_proto`] contract: `[headerJson, payloadArrayBuffer]` messages, the
//! `convert` payload a binary frame of `[metaJson, inputBytes]` so a multi-megabyte file never
//! rides inside JSON, and the reply's bytes transferred back.
//!
//! The worker is started lazily on the first conversion — nothing fetches its scripts on page
//! load (§3.3: "lazy-loaded in the browser only when a user opens or exports those formats").
//! A browser that can't start the worker answers `None`, so conversion is simply unavailable,
//! never a panic (§3.1).
//!
//! Conversions are revision-less jobs (they carry whole files, not the live document), so the
//! stale-revision dropping of the proofing client does not apply; cancellation stays
//! cooperative and a long job is stopped with [`CodecWorker::terminate`].

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wasm_bindgen::closure::Closure;
use wordcraft_worker_proto as proto;

/// The Trunk loader shim file (see the `data-type="worker"` + `data-loader-shim` link in
/// `index.html`; Trunk names worker artifacts after the binary without a content hash).
const LOADER_FILE: &str = "wordcraft-worker-codecs_loader.js";

/// One in-flight request's continuation.
type Sink = Box<dyn FnOnce(&proto::Envelope, &[u8])>;

struct Pending {
    sink: Sink,
}

struct Inner {
    worker: web_sys::Worker,
    /// The asset base the worker's loader was fetched from: [`ensure`] reuses the worker only
    /// for the same base, so one instance's worker is never borrowed for another's assets.
    asset_base: String,
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
    pending: HashMap<u32, Pending>,
}

/// A running codec worker — one per page ([`ensure`]); it is not `Clone` because its
/// `onmessage`/`onerror` pair is registered exactly once and dies with it.
pub(crate) struct CodecWorker {
    inner: Rc<RefCell<Inner>>,
    /// The `onmessage`/`onerror` closures live as long as the worker is held.
    _onmessage: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _onerror: Closure<dyn FnMut(web_sys::ErrorEvent)>,
}

impl CodecWorker {
    /// `new Worker(<assetBase>wordcraft-worker-codecs_loader.js, { type: "module" })`, the
    /// same-origin loader shim Trunk generates (§3.1: no `blob:` URL — `worker-src 'self'`).
    fn start(asset_base: &str) -> Result<CodecWorker, String> {
        let url = format!("{asset_base}{LOADER_FILE}");
        let options = web_sys::WorkerOptions::new();
        options.set_type(web_sys::WorkerType::Module);
        let worker = web_sys::Worker::new_with_options(&url, &options).map_err(|e| format!("cannot start {LOADER_FILE}: {e:?}"))?;
        let inner = Rc::new(RefCell::new(Inner {
            worker: worker.clone(),
            asset_base: asset_base.to_string(),
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
            let pending = {
                let mut inner = handler.borrow_mut();
                inner.failed = true;
                inner.queued.clear();
                inner.pending.drain().map(|(id, p)| (id, p.sink)).collect::<Vec<_>>()
            };
            log::warn!("codec worker failed ({}:{}): {}", event.filename(), event.lineno(), event.message());
            fail_pending(pending, "the codec worker stopped");
        });
        worker.set_onerror(Some(onerror.as_ref().unchecked_ref()));

        Ok(CodecWorker { inner, _onmessage: onmessage, _onerror: onerror })
    }

    /// Whether the worker answered `ready`.
    #[allow(dead_code)] // For the open/save flow's "converting…" state and diagnostics.
    pub(crate) fn alive(&self) -> bool {
        let inner = self.inner.borrow();
        !inner.failed && inner.ready
    }

    /// Post `(kind, payload)`; the request `id` it went out with.
    fn send(&self, kind: proto::Kind, payload: &[u8]) -> Result<u32, String> {
        let mut inner = self.inner.borrow_mut();
        if inner.failed {
            return Err("codec worker failed".into());
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

    /// `convert` request; `on_reply` gets the converted bytes, or the typed error the worker
    /// answered with (password problems carry the `.docx` reader's texts, so the shell's
    /// password prompts keep matching). The pending entry is registered after `postMessage`
    /// returns — a reply can only arrive on a later task turn.
    pub(crate) fn convert(
        &self,
        req: &proto::ConvertRequest,
        on_reply: impl FnOnce(Result<Vec<u8>, proto::ErrorReply>) + 'static,
    ) -> Result<u32, String> {
        let payload = proto::encode_convert_request(req).map_err(|e| e.to_string())?;
        let id = self.send(proto::Kind::Convert, &payload)?;
        self.inner.borrow_mut().pending.insert(
            id,
            Pending {
                sink: Box::new(move |env: &proto::Envelope, payload: &[u8]| match env.kind {
                    proto::Kind::Converted => on_reply(Ok(payload.to_vec())),
                    proto::Kind::Error => on_reply(Err(proto::decode_payload::<proto::ErrorReply>(payload)
                        .unwrap_or_else(|e| proto::ErrorReply { code: proto::ErrorCode::Internal, message: format!("bad error payload: {e}") }))),
                    other => {
                        on_reply(Err(proto::ErrorReply { code: proto::ErrorCode::Internal, message: format!("unexpected reply kind {other:?}") }))
                    }
                }),
            },
        );
        Ok(id)
    }

    /// Liveness probe.
    #[allow(dead_code)] // The smoke path and diagnostics use it; conversion itself does not.
    pub(crate) fn ping(&self) -> Result<u32, String> {
        self.send(proto::Kind::Ping, &[])
    }

    /// Hard stop — the only way to interrupt a long job (§3.1).
    #[allow(dead_code)] // Kept for the wiring that queues jobs behind a cancellable UI state.
    pub(crate) fn terminate(self) {
        self.inner.borrow().worker.terminate();
    }
}

/// Complete each pending request with a typed `error` reply after the worker died: callers get
/// a failure instead of a promise that never settles. Runs outside the state borrow — the sinks
/// can re-enter the page's callers.
fn fail_pending(pending: Vec<(u32, Sink)>, message: &str) {
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
        log::warn!("codec worker: message without a JSON header");
        return;
    };
    let payload = js_sys::Uint8Array::new(&arr.get(1)).to_vec();
    let env = match proto::decode_envelope(header.as_bytes()) {
        Ok(env) => env,
        Err(e) => {
            log::warn!("codec worker: undecodable header ({e})");
            return;
        }
    };
    let pending = {
        let mut guard = inner.borrow_mut();
        if env.kind == proto::Kind::Ready {
            guard.ready = true;
            let queued = std::mem::take(&mut guard.queued);
            let worker = guard.worker.clone();
            drop(guard);
            let mut unsent = Vec::new();
            for (id, kind, payload) in queued {
                let env = proto::Envelope::request(id, kind, 0);
                let Ok(header) = proto::encode_envelope(&env)
                    .map_err(|e| e.to_string())
                    .and_then(|h| String::from_utf8(h).map_err(|_| "envelope was not UTF-8".to_string()))
                else {
                    log::warn!("worker: cannot encode a queued message");
                    unsent.push(id);
                    continue;
                };
                let bytes = js_sys::Uint8Array::from(payload.as_slice());
                let msg = js_sys::Array::of2(&wasm_bindgen::JsValue::from_str(&header), &bytes);
                let transfer = js_sys::Array::of1(&bytes.buffer());
                if let Err(e) = worker.post_message_with_transfer(&msg, &transfer) {
                    log::warn!("worker: a queued message failed to send: {e:?}");
                    unsent.push(id);
                }
            }
            if !unsent.is_empty() {
                // A request that never went out must not leave its pending entry — and the
                // caller's promise — hanging: complete it with an error instead.
                let sinks = {
                    let mut guard = inner.borrow_mut();
                    unsent.into_iter().filter_map(|id| guard.pending.remove(&id).map(|p| (id, p.sink))).collect::<Vec<_>>()
                };
                fail_pending(sinks, "the codec worker could not send the queued request");
            }
            return;
        }
        match guard.pending.remove(&env.id) {
            Some(p) => p,
            None => return,
        }
    };
    (pending.sink)(&env, &payload);
}

impl Drop for CodecWorker {
    fn drop(&mut self) {
        // Complete the outstanding requests first — a dropped worker (replaced by [`ensure`]
        // for another asset base, or `terminate`d) must not leave their promises unsettled —
        // then detach so a message that races the drop can't touch freed state, then stop the
        // worker itself.
        let (worker, pending) = {
            let mut inner = self.inner.borrow_mut();
            inner.queued.clear();
            let pending = inner.pending.drain().map(|(id, p)| (id, p.sink)).collect::<Vec<_>>();
            (inner.worker.clone(), pending)
        };
        worker.set_onmessage(None);
        worker.set_onerror(None);
        worker.terminate();
        fail_pending(pending, "the codec worker was stopped");
    }
}

thread_local! {
    /// The one codec worker per page, started on first ask.
    static WORKER: RefCell<Option<CodecWorker>> = const { RefCell::new(None) };
    /// Asset bases whose worker construction already failed: don't retry every ask, and don't
    /// let one base's failure block another's.
    static WORKER_FAILED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Start the codec worker if it isn't already; `true` when it is running (or already was).
/// A running worker is reused only for the asset base it was started with.
pub(crate) fn ensure(asset_base: &str) -> bool {
    // A worker that errored at runtime — or one started for a different asset base — is dead
    // weight here: drop it (its `Drop` terminates the JS worker and fails its pending
    // requests) and start fresh below. `WORKER_FAILED` stays reserved for `start` failures,
    // remembered per asset base.
    let alive = WORKER.with(|w| {
        let mut slot = w.borrow_mut();
        let reusable = slot.as_ref().is_some_and(|worker| {
            let inner = worker.inner.borrow();
            !inner.failed && inner.asset_base == asset_base
        });
        if !reusable && slot.is_some() {
            *slot = None;
        }
        reusable
    });
    if alive {
        return true;
    }
    if WORKER_FAILED.with(|f| f.borrow().iter().any(|b| b == asset_base)) {
        return false;
    }
    match CodecWorker::start(asset_base) {
        Ok(worker) => {
            WORKER.with(|w| *w.borrow_mut() = Some(worker));
            true
        }
        Err(e) => {
            log::warn!("{e}");
            WORKER_FAILED.with(|f| f.borrow_mut().push(asset_base.to_string()));
            false
        }
    }
}

/// Run `f` on the shared worker when it is up (after [`ensure`]).
pub(crate) fn with<R>(f: impl FnOnce(&CodecWorker) -> R) -> Option<R> {
    WORKER.with(|w| w.borrow().as_ref().map(f))
}
