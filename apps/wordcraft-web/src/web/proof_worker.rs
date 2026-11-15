//! The typed client for the proofing dedicated worker (deferred-items plan §3.2), on the
//! [`wordcraft_worker_proto`] contract: `[headerJson, payloadArrayBuffer]` messages, payload
//! transferred, every failure a typed reply.
//!
//! The worker is started lazily on first ask and only when the `?proof-worker` URL flag opts in
//! — spelling and grammar squiggles are not wired to the UI yet (layout computes each
//! `ParaLayout`'s `issues` inline; injecting overlay spans post-layout is the remaining work),
//! so nothing here may cost a page load until it is asked for. Hyphenation is not involved: it
//! stays in the main module (F5).
//!
//! A browser that can't start the worker gets `false`/`None` — proofing is simply off, never a
//! panic (§3.1: "a worker that fails to start leaves the feature unavailable with a message,
//! never a panic").

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wasm_bindgen::closure::Closure;
use wordcraft_worker_proto as proto;

/// The Trunk loader shim file (`data-type="worker"` + `data-loader-shim` in `index.html`;
/// Trunk names worker artifacts after the binary without a content hash). Kept in lockstep with
/// `data-bin` there and `packaging/web/npm`'s copy of it.
const LOADER_FILE: &str = "wordcraft-worker-proof_loader.js";

/// One in-flight request's continuation.
type Sink = Box<dyn FnOnce(&proto::Envelope, &[u8])>;

/// A request still awaiting its reply, with the document revision it was sent under so a reply
/// stamped with an older one can be dropped (§3.2).
struct Pending {
    revision: u64,
    sink: Sink,
}

struct Inner {
    worker: web_sys::Worker,
    ids: proto::RequestIds,
    /// The document revision `check` calls stamp. Squiggle spans are overlays keyed by
    /// `(revision, para_id)`, so a reply for an older revision is stale and dropped — it never
    /// moves layout, so being late is safe (§3.2).
    revision: u64,
    /// The worker's `ready` notice arrived.
    ready: bool,
    /// The worker failed to start or errored: the feature is unavailable.
    failed: bool,

    /// Jobs posted before the worker's `ready` notice: a module worker still evaluating its
    /// top-level `await init()` silently DROPS transferred messages (measured 2026-11-15: a
    /// renderPdf posted pre-ready never arrives; the same bytes posted after `ready` answer),
    /// so early posts wait here and flush when `ready` lands. The document revision travels
    /// with the job: a `check` flushed with revision 0 would have its reply dropped as stale.
    queued: Vec<(u32, proto::Kind, u64, Vec<u8>)>,
    pending: HashMap<u32, Pending>,
}

/// A running proofing worker — one per page ([`ensure`]); it is not `Clone` because its
/// `onmessage`/`onerror` pair is registered exactly once and dies with it.
pub(crate) struct ProofWorker {
    inner: Rc<RefCell<Inner>>,
    /// The `onmessage`/`onerror` closures live as long as the worker is held.
    _onmessage: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _onerror: Closure<dyn FnMut(web_sys::ErrorEvent)>,
}

impl ProofWorker {
    /// `new Worker(<assetBase>wordcraft-worker-proof_loader.js, { type: "module" })`, the
    /// same-origin loader shim Trunk generates (§3.1: no `blob:` URL — `worker-src 'self'`).
    fn start(asset_base: &str) -> Result<ProofWorker, String> {
        let url = format!("{asset_base}{LOADER_FILE}");
        let options = web_sys::WorkerOptions::new();
        options.set_type(web_sys::WorkerType::Module);
        let worker = web_sys::Worker::new_with_options(&url, &options).map_err(|e| format!("cannot start {LOADER_FILE}: {e:?}"))?;
        let inner = Rc::new(RefCell::new(Inner {
            worker: worker.clone(),
            ids: proto::RequestIds::new(),
            revision: 0,
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
            log::warn!("proofing worker failed ({}:{}): {}", event.filename(), event.lineno(), event.message());
            fail_pending(pending, "the proofing worker stopped");
        });
        worker.set_onerror(Some(onerror.as_ref().unchecked_ref()));

        Ok(ProofWorker { inner, _onmessage: onmessage, _onerror: onerror })
    }

    /// Whether the worker answered `ready`.
    #[allow(dead_code)] // For the UI wiring once squiggles reach it (§3.2).
    pub(crate) fn alive(&self) -> bool {
        let inner = self.inner.borrow();
        !inner.failed && inner.ready
    }

    /// The document revision later `check` calls stamp; replies stamped with an older one are
    /// dropped on arrival, and requests still pending from an older revision are dropped now.
    #[allow(dead_code)] // For the UI wiring once squiggles reach it (§3.2).
    pub(crate) fn set_revision(&self, revision: u64) {
        let mut inner = self.inner.borrow_mut();
        inner.revision = revision;
        inner.pending.retain(|_, p| p.revision == revision || p.revision == 0);
    }

    /// Post `(kind, revision, payload)`; the request `id` it went out with.
    fn send(&self, kind: proto::Kind, revision: u64, payload: &[u8]) -> Result<u32, String> {
        let mut inner = self.inner.borrow_mut();
        if inner.failed {
            return Err("proofing worker failed".into());
        }
        let id = inner.ids.next_id();
        if !inner.ready {
            inner.queued.push((id, kind, revision, payload.to_vec()));
            return Ok(id);
        }
        let env = proto::Envelope::request(id, kind, revision);
        let header = proto::encode_envelope(&env).map_err(|e| e.to_string())?;
        let header = String::from_utf8(header).map_err(|_| "envelope was not UTF-8".to_string())?;
        let bytes = js_sys::Uint8Array::from(payload);
        let msg = js_sys::Array::of2(&wasm_bindgen::JsValue::from_str(&header), &bytes);
        let transfer = js_sys::Array::of1(&bytes.buffer());
        inner.worker.post_message_with_transfer(&msg, &transfer).map_err(|e| format!("postMessage failed: {e:?}"))?;
        Ok(id)
    }

    /// `check` request for the current revision; `on_reply` gets the [`proto::CheckReply`], or
    /// the [`proto::ErrorReply`] when the worker answered `error`. The pending entry is
    /// registered after `postMessage` returns — a reply can only arrive on a later task turn.
    pub(crate) fn check(
        &self,
        req: &proto::CheckRequest,
        on_reply: impl FnOnce(Result<proto::CheckReply, proto::ErrorReply>) + 'static,
    ) -> Result<u32, String> {
        let payload = proto::encode_payload(req).map_err(|e| e.to_string())?;
        let revision = self.inner.borrow().revision;
        let id = self.send(proto::Kind::Check, revision, &payload)?;
        self.inner.borrow_mut().pending.insert(
            id,
            Pending {
                revision,
                sink: Box::new(move |env: &proto::Envelope, payload: &[u8]| match env.kind {
                    proto::Kind::Issues => on_reply(
                        proto::decode_payload::<proto::CheckReply>(payload)
                            .map_err(|e| proto::ErrorReply { code: proto::ErrorCode::BadRequest, message: e.to_string() }),
                    ),
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

    /// `configure` request: default options and the user/ignore dictionary deltas.
    #[allow(dead_code)] // For the UI wiring once squiggles reach it (§3.2).
    pub(crate) fn configure(&self, cfg: &proto::ConfigureRequest) -> Result<u32, String> {
        let payload = proto::encode_payload(cfg).map_err(|e| e.to_string())?;
        self.send(proto::Kind::Configure, 0, &payload)
    }

    /// `cancel` request. Cooperative only — it cannot preempt a running job (§3.1); to stop a
    /// long job use [`Self::terminate`].
    #[allow(dead_code)] // For the UI wiring once squiggles reach it (§3.2).
    pub(crate) fn cancel(&self, request_id: u32) -> Result<u32, String> {
        self.inner.borrow_mut().pending.remove(&request_id);
        let payload = proto::encode_payload(&proto::CancelRequest { id: request_id }).map_err(|e| e.to_string())?;
        self.send(proto::Kind::Cancel, 0, &payload)
    }

    /// Liveness probe (`?proof-worker`'s smoke path uses it; the UI wiring will too).
    pub(crate) fn ping(&self) -> Result<u32, String> {
        self.send(proto::Kind::Ping, 0, &[])
    }

    /// Hard stop — the only way to interrupt a long job (§3.1).
    #[allow(dead_code)] // For the UI wiring once squiggles reach it (§3.2).
    pub(crate) fn terminate(self) {
        self.inner.borrow().worker.terminate();
    }
}

/// Complete each pending request with a typed `error` reply after the worker died: callers get
/// a failure instead of a promise that never settles (and their scheduler slots free up). Runs
/// outside the state borrow — the sinks can re-enter the page's callers.
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
        log::warn!("proofing worker: message without a JSON header");
        return;
    };
    let payload = js_sys::Uint8Array::new(&arr.get(1)).to_vec();
    let env = match proto::decode_envelope(header.as_bytes()) {
        Ok(env) => env,
        Err(e) => {
            log::warn!("proofing worker: undecodable header ({e})");
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
            for (id, kind, revision, payload) in queued {
                let env = proto::Envelope::request(id, kind, revision);
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
    // A revision-less exchange (`pong`, `ack`) is never stale; a reply stamped with an older
    // document revision than the request went out under — or than the document is at now — is
    // (§3.2). Dropped silently, as the plan allows.
    if pending.revision != 0 && env.revision != pending.revision {
        return;
    }
    let current = inner.borrow().revision;
    if pending.revision != 0 && pending.revision != current {
        return;
    }
    (pending.sink)(&env, &payload);
}

impl Drop for ProofWorker {
    fn drop(&mut self) {
        // Detach first so a message that races the drop can't touch freed state, then stop the
        // worker itself — its closure registrations would otherwise outlive us.
        let inner = self.inner.borrow();
        inner.worker.set_onmessage(None);
        inner.worker.set_onerror(None);
        inner.worker.terminate();
    }
}

thread_local! {
    /// The one proofing worker per page, started on first ask.
    static WORKER: RefCell<Option<ProofWorker>> = const { RefCell::new(None) };
    /// Construction already failed once: don't retry every ask.
    static WORKER_FAILED: Cell<bool> = const { Cell::new(false) };
}

/// Start the proofing worker if it isn't already; `true` when it is running (or already was).
/// Called lazily — nothing fetches the worker scripts until the first ask (§3.2's opt-in
/// `?proof-worker` flag until squiggles reach the UI).
pub(crate) fn ensure(asset_base: &str) -> bool {
    let running = WORKER.with(|w| w.borrow().is_some());
    if running {
        return true;
    }
    if WORKER_FAILED.with(|f| f.get()) {
        return false;
    }
    match ProofWorker::start(asset_base) {
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

/// Run `f` on the shared worker when it is up (after [`ensure`]).
pub(crate) fn with<R>(f: impl FnOnce(&ProofWorker) -> R) -> Option<R> {
    WORKER.with(|w| w.borrow().as_ref().map(f))
}
