//! The page's client for the view-only dedicated worker, on the [`wordcraft_worker_proto`]
//! contract: `[headerJson, payloadArrayBuffer]` messages, payloads transferred.
//!
//! Unlike the editor's worker clients, a reply's payload is handed to its sink as the
//! JavaScript array it arrived in, not copied into this module's memory: a painted page is
//! megabytes of pixels that go straight from the transferred buffer to a canvas.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wasm_bindgen::closure::Closure;
use wordcraft_worker_proto as proto;

/// The Trunk loader shim file (the `data-type="worker"` + `data-loader-shim` link in
/// `apps/wordcraft-web/index.html`; Trunk names worker artifacts after the binary, without a
/// content hash).
pub(crate) const LOADER_FILE: &str = "wordcraft-worker-viewer_loader.js";

/// What a reply is given to: its envelope and its payload, still in JavaScript's memory.
pub(crate) type Sink = Box<dyn FnOnce(&proto::Envelope, &js_sys::Uint8Array)>;

struct Inner {
    worker: web_sys::Worker,
    ids: proto::RequestIds,
    /// The worker's `ready` notice arrived.
    ready: bool,
    /// The worker failed to start or errored.
    failed: bool,
    /// Requests made before `ready`: a module worker still evaluating its top-level
    /// `await init()` drops messages posted to it, so early ones wait here.
    queued: Vec<(u32, proto::Kind, js_sys::Uint8Array)>,
    pending: HashMap<u32, Sink>,
}

/// A running view-only worker. Dropping it stops the worker.
pub(crate) struct ViewerWorker {
    inner: Rc<RefCell<Inner>>,
    _onmessage: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _onerror: Closure<dyn FnMut(web_sys::Event)>,
}

impl ViewerWorker {
    /// `new Worker(<assetBase>wordcraft-worker-viewer_loader.js, { type: "module" })`: the
    /// same-origin loader shim Trunk generates (no `blob:` URL, so `worker-src 'self'` is
    /// enough). `on_failure` is told once if the worker cannot start or stops.
    pub(crate) fn start(asset_base: &str, on_failure: impl Fn(String) + 'static) -> Result<ViewerWorker, String> {
        let url = format!("{asset_base}{LOADER_FILE}");
        let options = web_sys::WorkerOptions::new();
        options.set_type(web_sys::WorkerType::Module);
        let worker = web_sys::Worker::new_with_options(&url, &options).map_err(|e| format!("cannot start {LOADER_FILE}: {e:?}"))?;
        let inner = Rc::new(RefCell::new(Inner {
            worker: worker.clone(),
            ids: proto::RequestIds::new(),
            ready: false,
            failed: false,
            queued: Vec::new(),
            pending: HashMap::new(),
        }));

        let handler = inner.clone();
        let onmessage = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            on_message(&handler, &event.data());
        });
        worker.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

        let handler = inner.clone();
        let onerror = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
            // Handled here: the page shows a notice, the console is not given an uncaught error.
            event.prevent_default();
            let pending = {
                let mut inner = handler.borrow_mut();
                if inner.failed {
                    return;
                }
                inner.failed = true;
                inner.queued.clear();
                inner.pending.drain().collect::<Vec<_>>()
            };
            // A script that cannot be fetched arrives as a plain `Event` with no `message`; only a
            // script that throws sends an `ErrorEvent`. So the text is read as a property that may
            // be missing, never through `ErrorEvent::message`, which fails on `undefined`.
            let message = js_sys::Reflect::get(&event, &"message".into()).ok().and_then(|m| m.as_string()).unwrap_or_default();
            fail_pending(pending, "the viewer worker stopped");
            on_failure(if message.is_empty() { format!("{LOADER_FILE} could not be loaded") } else { message });
        });
        worker.set_onerror(Some(onerror.as_ref().unchecked_ref()));

        Ok(ViewerWorker { inner, _onmessage: onmessage, _onerror: onerror })
    }

    /// Post a request whose payload is `payload`'s whole buffer, which is transferred (the
    /// array is unusable afterwards). `sink` gets the reply, or a typed `error` if the worker
    /// dies first.
    pub(crate) fn request(&self, kind: proto::Kind, payload: js_sys::Uint8Array, sink: Sink) -> Result<u32, String> {
        let mut inner = self.inner.borrow_mut();
        if inner.failed {
            return Err("the viewer worker failed".into());
        }
        let id = inner.ids.next_id();
        if inner.ready {
            post(&inner.worker, id, kind, &payload)?;
        } else {
            inner.queued.push((id, kind, payload));
        }
        inner.pending.insert(id, sink);
        Ok(id)
    }

    /// A request with a small JSON payload.
    pub(crate) fn request_json(&self, kind: proto::Kind, payload: &[u8], sink: Sink) -> Result<u32, String> {
        self.request(kind, js_sys::Uint8Array::from(payload), sink)
    }
}

impl Drop for ViewerWorker {
    /// Stop the worker. Every request still waiting is answered with a typed `error`, as when
    /// the worker dies: its sink may hold a host promise (`installFont`), which would otherwise
    /// wait for ever. The sinks run after the state borrow is released, so the owner must drop
    /// this outside its own borrows (the page does: `remove_viewer`).
    fn drop(&mut self) {
        let pending = {
            let mut inner = self.inner.borrow_mut();
            // Detach first so a message that races the drop can't touch freed state, then stop
            // the worker itself.
            inner.worker.set_onmessage(None);
            inner.worker.set_onerror(None);
            inner.worker.terminate();
            inner.failed = true;
            inner.queued.clear();
            inner.pending.drain().collect::<Vec<_>>()
        };
        fail_pending(pending, "the viewer stopped: the editor took over the page");
    }
}

fn post(worker: &web_sys::Worker, id: u32, kind: proto::Kind, payload: &js_sys::Uint8Array) -> Result<(), String> {
    let header = proto::encode_envelope(&proto::Envelope::request(id, kind, 0)).map_err(|e| e.to_string())?;
    let header = String::from_utf8(header).map_err(|_| "envelope was not UTF-8".to_string())?;
    let msg = js_sys::Array::of2(&wasm_bindgen::JsValue::from_str(&header), payload);
    let transfer = js_sys::Array::of1(&payload.buffer());
    worker.post_message_with_transfer(&msg, &transfer).map_err(|e| format!("postMessage failed: {e:?}"))
}

/// Complete each pending request with a typed `error` reply after the worker died (or the
/// request could not be sent to it), so no caller waits forever. Runs outside the state borrow: the sinks re-enter the page.
fn fail_pending(pending: Vec<(u32, Sink)>, message: &str) {
    let reply = proto::ErrorReply { code: proto::ErrorCode::Internal, message: message.to_string() };
    crate::settle::settle_all(pending, proto::encode_payload(&reply), |id, sink, payload| {
        sink(&proto::Envelope::request(id, proto::Kind::Error, 0), &js_sys::Uint8Array::from(payload));
    });
}

/// Route one `[headerJson, payload]` message from the worker.
fn on_message(inner: &Rc<RefCell<Inner>>, data: &wasm_bindgen::JsValue) {
    if !js_sys::Array::is_array(data) {
        return;
    }
    let arr = js_sys::Array::from(data);
    let Some(header) = arr.get(0).as_string() else { return };
    let Ok(env) = proto::decode_envelope(header.as_bytes()) else { return };
    // The worker posts a `Uint8Array` over the transferred buffer: use it as it is. An
    // `ArrayBuffer` gets a view; neither is a copy.
    let payload = arr.get(1);
    let payload = match payload.dyn_into::<js_sys::Uint8Array>() {
        Ok(view) => view,
        Err(other) if other.is_instance_of::<js_sys::ArrayBuffer>() => js_sys::Uint8Array::new(&other),
        Err(_) => js_sys::Uint8Array::new_with_length(0),
    };
    let sink = {
        let mut state = inner.borrow_mut();
        if env.kind == proto::Kind::Ready {
            state.ready = true;
            let queued = std::mem::take(&mut state.queued);
            let worker = state.worker.clone();
            // A request that cannot be posted is answered with an error, as one posted at once
            // is refused to its caller: never left waiting for a reply that cannot come.
            let unsent: Vec<(u32, Sink)> = queued
                .into_iter()
                .filter(|(id, kind, payload)| post(&worker, *id, *kind, payload).is_err())
                .filter_map(|(id, ..)| state.pending.remove(&id).map(|sink| (id, sink)))
                .collect();
            drop(state);
            fail_pending(unsent, "a request could not be sent to the viewer worker");
            return;
        }
        match state.pending.remove(&env.id) {
            Some(sink) => sink,
            None => return,
        }
    };
    sink(&env, &payload);
}

/// The bytes of a small reply payload (JSON), or `None` if it is larger than any JSON payload
/// the protocol allows.
pub(crate) fn small_payload(payload: &js_sys::Uint8Array) -> Option<Vec<u8>> {
    (payload.length() as usize <= proto::MAX_PAYLOAD_BYTES).then(|| payload.to_vec())
}

/// The `error` reply in `payload`, or a stand-in if it does not decode.
pub(crate) fn error_reply(payload: &js_sys::Uint8Array) -> proto::ErrorReply {
    small_payload(payload)
        .and_then(|bytes| proto::decode_payload::<proto::ErrorReply>(&bytes).ok())
        .unwrap_or_else(|| proto::ErrorReply { code: proto::ErrorCode::Internal, message: "the viewer worker sent an unreadable error".into() })
}
