//! `window.wordcraft`: the page command API (`craft-web-api`, storytold/craft-libs#413) over the
//! app's own control channel, the one the desktop control server drives (`docs/control-protocol.md`).
//!
//! The crate owns the JavaScript side (`request`, `execute`, `commands`, `docInspect`, `uiInspect`,
//! `screenshot`, as promises with a timeout). This module is WordCraft's part: a [`Request`] that
//! queues a [`ControlRequest`] for the app, which answers it on its next frame, and resolves its
//! future immediately when `WebShell::logic` / `WebShell::ui` finishes draining replies via
//! [`wake_waiters`].

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::task::{Context, Poll, Waker};

use craft_web_api::methods::Methods;
use craft_web_api::page::{PageApi, Request};
use serde_json::{Value, json};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::future_to_promise;
use wordcraft_engine::cmd::insert::{base64_decode, base64_encode};
use wordcraft_ui_egui::ControlRequest;

/// The app id in `window.<app>`.
const APP: &str = "wordcraft";

thread_local! {
    static PENDING_WAKERS: RefCell<Vec<Rc<RefCell<Option<Waker>>>>> = const { RefCell::new(Vec::new()) };
}

/// Wakes any pending `window.wordcraft` call futures after the app drains control requests/replies.
pub fn wake_waiters() {
    let slots: Vec<_> = PENDING_WAKERS.with(|p| p.borrow_mut().drain(..).collect());
    for slot in slots {
        if let Some(w) = slot.borrow_mut().take() {
            w.wake();
        }
    }
}

fn js_err(msg: &str) -> JsValue {
    js_sys::Error::new(msg).into()
}

/// Put `window.wordcraft` on the page and return the queue the app drains each frame (hand it to
/// `WordApp::with_control`). An existing `window.wordcraft` keeps the functions it already has.
#[allow(dead_code)]
pub fn install(ctx: egui::Context) -> Receiver<ControlRequest> {
    install_with_global(ctx, true, "./").0
}

/// Build the `WordCraftApi` object over the app control channel, optionally publishing it at
/// `window.wordcraft` when `install_global` is true. `asset_base` is the launching instance's
/// asset base: the codec/pdf helpers resolve their lazy workers against it for this instance's
/// lifetime, whatever later `start` calls do.
pub fn install_with_global(ctx: egui::Context, install_global: bool, asset_base: &str) -> (Receiver<ControlRequest>, js_sys::Object) {
    let (queue, requests) = channel::<ControlRequest>();
    let req_queue = queue.clone();
    let req_ctx = ctx.clone();
    let request: Request = Rc::new(move |method, params| dispatch_raw(&req_queue, &req_ctx, method, params));

    let global = js_sys::global();
    let app_key = JsValue::from_str(APP);
    let saved_global = if install_global {
        None
    } else {
        let had = js_sys::Reflect::has(&global, &app_key).unwrap_or(false);
        let prev = js_sys::Reflect::get(&global, &app_key).ok();
        let _ = js_sys::Reflect::delete_property(&global, &app_key);
        Some((had, prev))
    };

    let install_res = Rc::new(PageApi::new(APP, Methods::default(), request)).install();
    let api_obj: js_sys::Object = if install_res.is_ok() {
        js_sys::Reflect::get(&global, &app_key).ok().and_then(|v| v.dyn_into::<js_sys::Object>().ok()).unwrap_or_default()
    } else {
        if let Err(e) = install_res {
            log::warn!("couldn't install {APP} API: {e:?}");
        }
        js_sys::Object::new()
    };

    if let Some((had, prev)) = saved_global {
        if had {
            if let Some(prev_val) = prev {
                let _ = js_sys::Reflect::set(&global, &app_key, &prev_val);
            }
        } else {
            let _ = js_sys::Reflect::delete_property(&global, &app_key);
        }
    }

    attach_doc_helpers(&api_obj, queue, ctx, asset_base);
    (requests, api_obj)
}

fn dispatch_raw(queue: &Sender<ControlRequest>, ctx: &egui::Context, method: String, params: Value) -> Pin<Box<dyn Future<Output = Value>>> {
    let (control, reply) = ControlRequest::new(method, params);
    if queue.send(control).is_err() {
        return Box::pin(async { json!({"ok": false, "error": "the app is not running"}) });
    }
    ctx.request_repaint();
    Box::pin(ReplyFuture { reply, waker: Rc::new(RefCell::new(None)) })
}

async fn execute_command(queue: Sender<ControlRequest>, ctx: egui::Context, command: &str, params: Value) -> Result<Value, JsValue> {
    let resp = dispatch_raw(&queue, &ctx, "engine.execute".into(), json!({"command": command, "params": params})).await;
    if resp.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    } else {
        let msg = resp.get("error").and_then(Value::as_str).unwrap_or("command failed");
        Err(js_err(msg))
    }
}

/// The bytes a page passed as `val`, without copying them: a `Uint8Array` as it is (`new
/// Uint8Array(typedArray)` would clone its buffer, ECMA-262 InitializeTypedArrayFromTypedArray),
/// a view on an `ArrayBuffer`; `None` for anything else. The caller bounds the length, then
/// copies once.
fn byte_view(val: &JsValue) -> Option<js_sys::Uint8Array> {
    match val.dyn_ref::<js_sys::Uint8Array>() {
        Some(view) => Some(view.clone()),
        None => val.is_instance_of::<js_sys::ArrayBuffer>().then(|| js_sys::Uint8Array::new(val)),
    }
}

fn json_to_js(v: &Value) -> JsValue {
    js_sys::JSON::parse(&v.to_string()).unwrap_or(JsValue::NULL)
}

fn attach_doc_helpers(api: &js_sys::Object, queue: Sender<ControlRequest>, ctx: egui::Context, asset_base: &str) {
    let asset_base = asset_base.to_string();
    let q_get = queue.clone();
    let c_get = ctx.clone();
    let get_md = wasm_bindgen::closure::Closure::<dyn Fn() -> js_sys::Promise>::new(move || {
        let q = q_get.clone();
        let c = c_get.clone();
        future_to_promise(async move {
            let res = execute_command(q, c, "doc.getMarkdown", json!({})).await?;
            let md = res.get("markdown").and_then(Value::as_str).unwrap_or_default();
            Ok(JsValue::from_str(md))
        })
    });
    let _ = js_sys::Reflect::set(api, &JsValue::from_str("getMarkdown"), get_md.as_ref().unchecked_ref());
    get_md.forget();

    let q_apply = queue.clone();
    let c_apply = ctx.clone();
    let apply_md = wasm_bindgen::closure::Closure::<dyn Fn(JsValue) -> js_sys::Promise>::new(move |val: JsValue| {
        let q = q_apply.clone();
        let c = c_apply.clone();
        future_to_promise(async move {
            let md = val.as_string().ok_or_else(|| js_err("applyMarkdown expects a string"))?;
            let res = execute_command(q, c, "doc.setMarkdown", json!({"markdown": md})).await?;
            Ok(json_to_js(&res))
        })
    });
    let _ = js_sys::Reflect::set(api, &JsValue::from_str("applyMarkdown"), apply_md.as_ref().unchecked_ref());
    apply_md.forget();

    let q_exp = queue.clone();
    let c_exp = ctx.clone();
    // Cloned ahead of the later closures, which move `queue`/`ctx` into themselves.
    let queue_for_pdf = queue.clone();
    let ctx_for_pdf = ctx.clone();
    let export_docx = wasm_bindgen::closure::Closure::<dyn Fn() -> js_sys::Promise>::new(move || {
        let q = q_exp.clone();
        let c = c_exp.clone();
        future_to_promise(async move {
            let res = execute_command(q, c, "doc.exportDocx", json!({})).await?;
            let b64 = res.get("docxBase64").and_then(Value::as_str).ok_or_else(|| js_err("missing docxBase64"))?;
            let bytes = base64_decode(b64).ok_or_else(|| js_err("invalid docxBase64"))?;
            Ok(JsValue::from(js_sys::Uint8Array::from(bytes.as_slice())))
        })
    });
    let _ = js_sys::Reflect::set(api, &JsValue::from_str("exportDocx"), export_docx.as_ref().unchecked_ref());
    export_docx.forget();

    let c_fonts = ctx.clone();
    let load_docx = wasm_bindgen::closure::Closure::<dyn Fn(JsValue, JsValue) -> js_sys::Promise>::new(move |val: JsValue, password: JsValue| {
        let q = queue.clone();
        let c = ctx.clone();
        future_to_promise(async move {
            // Bounded before any copy, with `doc.loadDocx`'s own refusal.
            let b64 = if let Some(s) = val.dyn_ref::<js_sys::JsString>() {
                if let Some(refused) = crate::lazy_formats::load_docx_error(s.length() as usize) {
                    return Err(js_err(&refused));
                }
                String::from(s)
            } else if let Some(view) = byte_view(&val) {
                if let Some(refused) = crate::lazy_formats::load_docx_error(crate::lazy_formats::base64_len(view.length() as usize)) {
                    return Err(js_err(&refused));
                }
                base64_encode(&view.to_vec())
            } else {
                return Err(js_err("loadDocx expects Uint8Array, ArrayBuffer, or base64 string"));
            };
            let mut params = json!({"docxBase64": b64});
            if let Some(pw) = password.as_string() {
                params["password"] = json!(pw);
            }
            let res = execute_command(q, c, "doc.loadDocx", params).await?;
            Ok(json_to_js(&res))
        })
    });
    let _ = js_sys::Reflect::set(api, &JsValue::from_str("loadDocx"), load_docx.as_ref().unchecked_ref());
    load_docx.forget();

    let install_fonts = wasm_bindgen::closure::Closure::<dyn Fn(JsValue) -> js_sys::Promise>::new(move |val: JsValue| {
        let families: Option<Vec<String>> = if val.is_undefined() || val.is_null() {
            None
        } else {
            js_sys::Array::from(&val).iter().filter_map(|v| v.as_string()).take(64).collect::<Vec<String>>().into()
        };
        crate::web::font_packs::install_fonts_promise(families, c_fonts.clone())
    });
    let _ = js_sys::Reflect::set(api, &JsValue::from_str("installFonts"), install_fonts.as_ref().unchecked_ref());
    install_fonts.forget();

    // The codec dedicated worker (plan §3.3): `convertDocument({ from, to, data, password? })`
    // converts between `.docx` and the formats the browser build leaves out (RTF, ODT, HTML,
    // LaTeX, legacy `.doc`, flat OPC), off the main thread. Exactly one of `from`/`to` is
    // `docx`; the promise resolves to the converted bytes as a `Uint8Array`, or rejects with an
    // `Error` carrying the worker's message (password problems say so in the `.docx` reader's
    // words, so a caller's prompts can match). The worker starts lazily here — its scripts are
    // never fetched on page load.
    let asset_base_convert = asset_base.clone();
    let convert_document = wasm_bindgen::closure::Closure::<dyn Fn(JsValue) -> js_sys::Promise>::new(move |val: JsValue| {
        js_sys::Promise::new(&mut |resolve, reject| {
            let fail = |reject: &js_sys::Function, msg: String| {
                let _ = reject.call1(&wasm_bindgen::JsValue::NULL, &js_sys::Error::new(&msg));
            };
            let str_prop = |name: &str| -> Option<String> {
                js_sys::Reflect::get(&val, &JsValue::from_str(name)).ok().and_then(|v| v.as_string()).filter(|s| !s.is_empty())
            };
            let (Some(from), Some(to)) = (str_prop("from"), str_prop("to")) else {
                return fail(&reject, "convertDocument expects non-empty `from` and `to` format names".into());
            };
            let direction = match (from.eq_ignore_ascii_case("docx"), to.eq_ignore_ascii_case("docx")) {
                (false, true) => wordcraft_worker_proto::ConvertDirection::Import,
                (true, false) => wordcraft_worker_proto::ConvertDirection::Export,
                _ => {
                    return fail(
                        &reject,
                        "convertDocument converts between docx and one other format: exactly one of `from`/`to` must be \"docx\"".into(),
                    );
                }
            };
            // The foreign side's name: the non-docx of the two (flavor changes keep `docx`
            // flavors too — `from: "docx", to: "docm"` rewraps the package).
            let format = if direction == wordcraft_worker_proto::ConvertDirection::Import { from } else { to };
            let data = js_sys::Reflect::get(&val, &JsValue::from_str("data")).unwrap_or(JsValue::UNDEFINED);
            let input = if let Some(s) = data.dyn_ref::<js_sys::JsString>() {
                // Bounded by its length before it is copied here or decoded, with the
                // conversion's own refusal; then the exact cap on the decoded bytes.
                if let Some(refused) = crate::lazy_formats::convert_base64_error(s.length() as usize) {
                    return fail(&reject, refused);
                }
                match crate::lazy_formats::decode_convert_base64(&String::from(s)) {
                    Ok(bytes) => bytes,
                    Err(e) => return fail(&reject, e),
                }
            } else if let Some(view) = byte_view(&data) {
                // Bounded before the copy, with the conversion's own refusal.
                if let Some(refused) = crate::lazy_formats::convert_input_error(view.length() as usize) {
                    return fail(&reject, refused);
                }
                view.to_vec()
            } else {
                return fail(&reject, "convertDocument expects `data` as Uint8Array, ArrayBuffer, or base64 string".into());
            };
            if input.is_empty() {
                return fail(&reject, "convertDocument expects a non-empty `data`".into());
            }
            let password = str_prop("password");
            // A page-level conversion has no session, so no region: a file without a page size
            // converts on a new blank document's page.
            let req = wordcraft_worker_proto::ConvertRequest { format, direction, password, input, page: None };
            // The same job the app's own Open and Save As run (`lazy_io`): starts the worker on
            // first use and answers once, with the bytes or the reason there are none.
            crate::web::lazy_io::convert(&asset_base_convert, &req, move |reply| match reply {
                Ok(bytes) => {
                    let _ = resolve.call1(&wasm_bindgen::JsValue::NULL, &js_sys::Uint8Array::from(bytes.as_slice()));
                }
                Err(e) => fail(&reject, e),
            });
        })
    });
    let _ = js_sys::Reflect::set(api, &JsValue::from_str("convertDocument"), convert_document.as_ref().unchecked_ref());
    convert_document.forget();

    // PDF export through the dedicated worker (plan §3.4), as the app's own File › Export runs
    // it: the API runs `file.exportPdf` with a tag it claims (`lazy_io::claim_api_export`), so
    // the document as printed, the export options (Print Markup, comments, digits), the strict
    // check and the font copies are the command's, and the finished bytes come back here
    // instead of a download. `markup` overrides Print Markup, as the command's own parameter.
    // The worker starts lazily, never on page load. Resolves to the PDF bytes as a
    // `Uint8Array`; rejects with an `Error` carrying the reason there are none.
    let export_pdf = wasm_bindgen::closure::Closure::<dyn Fn(JsValue) -> js_sys::Promise>::new(move |val: JsValue| {
        let q = queue_for_pdf.clone();
        let c = ctx_for_pdf.clone();
        future_to_promise(async move {
            let markup = js_sys::Reflect::get(&val, &JsValue::from_str("markup")).ok().and_then(|v| v.as_bool());
            // The reply lands on a later task turn; the shared waker set (drained every shell
            // frame by `wake_waiters`, and woken here from the reply closure) completes it.
            let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<u8>, String>>();
            let (tag, name) = crate::web::lazy_io::claim_api_export(Box::new(move |reply| {
                let _ = tx.send(reply);
                wake_waiters();
            }))
            .map_err(|e| js_err(&e))?;
            let params = crate::lazy_formats::api_pdf_params(&name, tag, markup);
            match execute_command(q, c, "file.exportPdf", params).await {
                // The worker has the job (or already answered it).
                Ok(res) if res.get("pending").and_then(Value::as_bool) == Some(true) => {}
                Ok(_) => {
                    crate::web::lazy_io::release_api_export(tag);
                    return Err(js_err("the PDF export did not run in the PDF worker"));
                }
                Err(e) => {
                    crate::web::lazy_io::release_api_export(tag);
                    return Err(e);
                }
            }
            let bytes = ChannelFuture(rx).await.map_err(|e| js_err(&e))?;
            Ok(wasm_bindgen::JsValue::from(js_sys::Uint8Array::from(bytes.as_slice())))
        })
    });
    let _ = js_sys::Reflect::set(api, &JsValue::from_str("exportPdf"), export_pdf.as_ref().unchecked_ref());
    export_pdf.forget();
}

/// A one-shot answer that arrives outside the frame loop (a dedicated worker's reply): polled
/// like [`ReplyFuture`], woken by [`wake_waiters`] — which the reply closure itself calls, so an
/// answer between frames still completes the awaiting promise without waiting for the next one.
struct ChannelFuture(std::sync::mpsc::Receiver<Result<Vec<u8>, String>>);

impl Future for ChannelFuture {
    type Output = Result<Vec<u8>, String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.0.try_recv() {
            Ok(reply) => Poll::Ready(reply),
            Err(TryRecvError::Disconnected) => Poll::Ready(Err("the pdf worker went away before answering".into())),
            Err(TryRecvError::Empty) => {
                PENDING_WAKERS.with(|p| p.borrow_mut().push(Rc::new(RefCell::new(Some(cx.waker().clone())))));
                Poll::Pending
            }
        }
    }
}

struct ReplyFuture {
    reply: Receiver<Value>,
    waker: Rc<RefCell<Option<Waker>>>,
}

impl Future for ReplyFuture {
    type Output = Value;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.reply.try_recv() {
            Ok(reply) => Poll::Ready(reply),
            Err(TryRecvError::Disconnected) => Poll::Ready(json!({"ok": false, "error": "the app dropped the request"})),
            Err(TryRecvError::Empty) => {
                let mut slot = self.waker.borrow_mut();
                let needs_push = slot.is_none();
                *slot = Some(cx.waker().clone());
                drop(slot);
                if needs_push {
                    PENDING_WAKERS.with(|p| p.borrow_mut().push(Rc::clone(&self.waker)));
                }
                Poll::Pending
            }
        }
    }
}

impl Drop for ReplyFuture {
    fn drop(&mut self) {
        *self.waker.borrow_mut() = None;
    }
}
