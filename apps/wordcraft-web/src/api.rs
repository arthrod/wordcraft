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
    install_with_global(ctx, true).0
}

/// Build the `WordCraftApi` object over the app control channel, optionally publishing it at
/// `window.wordcraft` when `install_global` is true.
pub fn install_with_global(ctx: egui::Context, install_global: bool) -> (Receiver<ControlRequest>, js_sys::Object) {
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

    attach_doc_helpers(&api_obj, queue, ctx);
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

fn json_to_js(v: &Value) -> JsValue {
    js_sys::JSON::parse(&v.to_string()).unwrap_or(JsValue::NULL)
}

fn attach_doc_helpers(api: &js_sys::Object, queue: Sender<ControlRequest>, ctx: egui::Context) {
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
            let b64 = if let Some(s) = val.as_string() {
                s
            } else if val.is_instance_of::<js_sys::Uint8Array>() || val.is_instance_of::<js_sys::ArrayBuffer>() {
                let bytes = js_sys::Uint8Array::new(&val).to_vec();
                base64_encode(&bytes)
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
