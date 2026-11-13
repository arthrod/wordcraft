//! Pure-Rust interactive Demo Bar for WordCraftWeb (`?demo=1` or `<canvas data-demo="true">`).
//!
//! Provides sample switching (`samples/`), live Markdown split-pane editing (`getMarkdown` /
//! `applyMarkdown`), `.docx` import/export, quick command execution (`execute`), and toggling
//! between `100% Browser (Local WASM)` and `Hybrid Cloudflare Edge` mode.

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;

const BAR_ID: &str = "wordcraft_demo_bar";
const PANE_ID: &str = "wordcraft_demo_pane";
const STATUS_ID: &str = "wordcraft_demo_status";

fn create_el<T: wasm_bindgen::JsCast>(document: &web_sys::Document, tag: &str) -> Option<T> {
    document.create_element(tag).ok().and_then(|e| e.dyn_into::<T>().ok())
}

/// Mount the interactive Demo Bar above `canvas` when `?demo=1`/`?demo=true` or
/// `data-demo="true"` is enabled.
pub fn mount_if_enabled(document: &web_sys::Document, canvas: &web_sys::HtmlCanvasElement, asset_base: &str, api: &js_sys::Object) {
    let query = web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default();
    let enabled = canvas.get_attribute("data-demo").as_deref() == Some("true")
        || query.trim_start_matches('?').split('&').any(|p| p == "demo=1" || p == "demo=true");
    if !enabled || document.get_element_by_id(BAR_ID).is_some() {
        return;
    }

    let Some(body) = document.body() else {
        return;
    };

    let Some(bar) = create_el::<web_sys::HtmlElement>(document, "div") else {
        return;
    };
    bar.set_id(BAR_ID);
    bar.style().set_css_text(
        "position:fixed;top:0;left:0;right:0;height:40px;display:flex;align-items:center;gap:8px;padding:0 12px;\
         background:#1e293b;color:#f8fafc;font:12px/1.2 system-ui,-apple-system,sans-serif;z-index:1000;\
         box-sizing:border-box;overflow-x:auto;border-bottom:1px solid #334155;",
    );

    let pane_open = Rc::new(Cell::new(false));
    let edge_mode = Rc::new(Cell::new(false));

    let Some(pane) = create_el::<web_sys::HtmlElement>(document, "div") else {
        return;
    };
    pane.set_id(PANE_ID);
    pane.style().set_css_text(
        "position:fixed;top:40px;right:0;bottom:0;width:min(38%,500px);display:none;flex-direction:column;\
         background:#f8fafc;border-left:1px solid #cbd5e1;padding:10px;gap:8px;z-index:999;box-sizing:border-box;\
         font:12px/1.3 system-ui,-apple-system,sans-serif;color:#0f172a;",
    );

    let _ = canvas.style().set_property("top", "40px");
    let _ = canvas.style().set_property("height", "calc(100% - 40px)");

    add_badge(document, &bar, "WordCraftWeb Demo");

    // A sample's fetch can land after a later click started its own; only the newest
    // selection may touch the document and the status line.
    let sample_generation = Rc::new(Cell::new(0u64));
    for (label, file) in [("Welcome", "samples/welcome.md"), ("Tech Spec", "samples/spec.md"), ("Letter", "samples/letter.md")] {
        let doc_c = document.clone();
        let api_c = api.clone();
        let base_c = asset_base.to_string();
        let generation_c = sample_generation.clone();
        add_button(document, &bar, label, false, move || {
            let doc_c = doc_c.clone();
            let api_c = api_c.clone();
            let url = format!("{base_c}{file}");
            let generation = generation_c.get() + 1;
            generation_c.set(generation);
            let generation_c = generation_c.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let current = || generation_c.get() == generation;
                set_status(&doc_c, &format!("Loading {file}..."));
                match fetch_text(&url).await {
                    Ok(md) => {
                        if !current() {
                            return;
                        }
                        match call_api_str(&api_c, "applyMarkdown", &md).await {
                            Ok(_) if current() => set_status(&doc_c, &format!("Loaded {label} ({file})")),
                            Ok(_) => {}
                            Err(err) if current() => set_status(&doc_c, &format!("Sample failed: {err}")),
                            Err(_) => {}
                        }
                    }
                    Err(err) if current() => set_status(&doc_c, &format!("Fetch {file} failed: {err}")),
                    Err(_) => {}
                }
            });
        });
    }

    // Markdown Split Pane widgets
    let Some(md_area) = create_el::<web_sys::HtmlTextAreaElement>(document, "textarea") else {
        return;
    };
    md_area.set_id("wordcraft_demo_md");
    md_area.style().set_css_text(
        "flex:1;width:100%;resize:none;font:12px/1.45 ui-monospace,SFMono-Regular,monospace;\
         padding:10px;border:1px solid #cbd5e1;border-radius:6px;background:#fff;color:#0f172a;box-sizing:border-box;",
    );

    let pane_hdr = create_el::<web_sys::HtmlElement>(document, "div");
    if let Some(hdr) = &pane_hdr {
        hdr.style().set_css_text("display:flex;align-items:center;justify-content:space-between;gap:8px;");
        hdr.set_inner_html("<strong>Markdown Live Pane</strong>");
        let doc_apply = document.clone();
        let api_apply = api.clone();
        let area_apply = md_area.clone();
        add_button(document, hdr, "Apply to Document", true, move || {
            let doc_c = doc_apply.clone();
            let api_c = api_apply.clone();
            let md = area_apply.value();
            wasm_bindgen_futures::spawn_local(async move {
                match call_api_str(&api_c, "applyMarkdown", &md).await {
                    Ok(_) => set_status(&doc_c, "Applied Markdown to document"),
                    Err(err) => set_status(&doc_c, &format!("applyMarkdown failed: {err}")),
                }
            });
        });
        let _ = pane.append_child(hdr);
    }
    let _ = pane.append_child(&md_area);

    // Toggle Markdown Split-Pane button
    {
        let doc_c = document.clone();
        let api_c = api.clone();
        let canvas_c = canvas.clone();
        let pane_c = pane.clone();
        let pane_open_c = pane_open.clone();
        let area_c = md_area;
        add_button(document, &bar, "Markdown Pane", false, move || {
            let next = !pane_open_c.get();
            pane_open_c.set(next);
            let _ = pane_c.style().set_property("display", if next { "flex" } else { "none" });
            let _ = canvas_c.style().set_property("width", if next { "calc(100% - min(38%, 500px))" } else { "100%" });
            if next {
                let doc_async = doc_c.clone();
                let api_async = api_c.clone();
                let area_async = area_c.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    match call_api0(&api_async, "getMarkdown").await {
                        Ok(val) => {
                            area_async.set_value(&val.as_string().unwrap_or_default());
                            set_status(&doc_async, "Synced Markdown pane");
                        }
                        Err(err) => set_status(&doc_async, &format!("getMarkdown failed: {err}")),
                    }
                });
            }
        });
    }

    // .docx Import input + button
    if let Some(file_input) = create_el::<web_sys::HtmlInputElement>(document, "input") {
        file_input.set_type("file");
        file_input.set_accept(".docx");
        let _ = file_input.style().set_property("display", "none");
        let _ = bar.append_child(&file_input);

        let doc_change = document.clone();
        let api_change = api.clone();
        let input_change = file_input.clone();
        let on_change = Closure::<dyn FnMut()>::new(move || {
            let Some(file) = input_change.files().and_then(|files| files.get(0)) else {
                return;
            };
            let file_name = file.name();
            let doc_c = doc_change.clone();
            let api_c = api_change.clone();
            wasm_bindgen_futures::spawn_local(async move {
                set_status(&doc_c, &format!("Importing {file_name}..."));
                match JsFuture::from(file.array_buffer()).await {
                    Ok(buf) => {
                        let bytes = js_sys::Uint8Array::new(&buf);
                        match call_api_val(&api_c, "loadDocx", bytes.as_ref()).await {
                            Ok(_) => set_status(&doc_c, &format!("Opened {file_name} locally")),
                            Err(err) => set_status(&doc_c, &format!("loadDocx failed: {err}")),
                        }
                    }
                    Err(err) => set_status(&doc_c, &format!("Read failed: {err:?}")),
                }
            });
        });
        file_input.set_onchange(Some(on_change.as_ref().unchecked_ref()));
        on_change.forget();

        let input_click = file_input;
        add_button(document, &bar, "Load .docx", false, move || {
            input_click.click();
        });
    }

    // Export .docx (Local WASM or Hybrid Cloudflare Edge)
    {
        let doc_c = document.clone();
        let api_c = api.clone();
        let edge_c = edge_mode.clone();
        add_button(document, &bar, "Export .docx", false, move || {
            let doc_async = doc_c.clone();
            let api_async = api_c.clone();
            let use_edge = edge_c.get();
            wasm_bindgen_futures::spawn_local(async move {
                if use_edge {
                    set_status(&doc_async, "Uploading Markdown to /api/convert (Hybrid Edge)...");
                    match convert_via_edge(&api_async).await {
                        Ok(bytes) => {
                            if let Some(win) = web_sys::window() {
                                let _ = crate::web::trigger_anchor_download(&win, "wordcraft-edge.docx", &bytes);
                            }
                            set_status(&doc_async, &format!("Downloaded wordcraft-edge.docx ({} B via Edge)", bytes.len()));
                        }
                        Err(err) => set_status(&doc_async, &format!("Edge convert unavailable: {err}")),
                    }
                } else {
                    set_status(&doc_async, "Exporting .docx in browser WASM...");
                    match call_api0(&api_async, "exportDocx").await {
                        Ok(val) => {
                            let bytes = js_sys::Uint8Array::new(&val).to_vec();
                            if let Some(win) = web_sys::window() {
                                let _ = crate::web::trigger_anchor_download(&win, "wordcraft-browser.docx", &bytes);
                            }
                            set_status(&doc_async, &format!("Downloaded wordcraft-browser.docx ({} B locally)", bytes.len()));
                        }
                        Err(err) => set_status(&doc_async, &format!("exportDocx failed: {err}")),
                    }
                }
            });
        });
    }

    // Install Font Packs (downloads every unbundled face from fonts/ on demand)
    {
        let doc_c = document.clone();
        let api_c = api.clone();
        add_button(document, &bar, "Install Fonts", false, move || {
            let doc_async = doc_c.clone();
            let api_async = api_c.clone();
            wasm_bindgen_futures::spawn_local(async move {
                set_status(&doc_async, "Installing font packs...");
                match call_api_cmd(&api_async, "font.installPack", &JsValue::NULL).await {
                    Ok(_) => set_status(&doc_async, "Font packs requested (status follows)"),
                    Err(err) => set_status(&doc_async, &format!("font.installPack failed: {err}")),
                }
            });
        });
    }

    // Install Font Packs (downloads every unbundled face from fonts/ on demand)
    {
        let doc_c = document.clone();
        let api_c = api.clone();
        add_button(document, &bar, "Install Fonts", false, move || {
            let doc_async = doc_c.clone();
            let api_async = api_c.clone();
            wasm_bindgen_futures::spawn_local(async move {
                set_status(&doc_async, "Installing font packs...");
                match call_api_cmd(&api_async, "font.installPack", &JsValue::NULL).await {
                    Ok(_) => set_status(&doc_async, "Font packs requested (status follows)"),
                    Err(err) => set_status(&doc_async, &format!("font.installPack failed: {err}")),
                }
            });
        });
    }

    // Command runner
    if let Some(cmd_input) = create_el::<web_sys::HtmlInputElement>(document, "input") {
        cmd_input.set_type("text");
        cmd_input.set_value("format.bold");
        cmd_input.set_placeholder("command id (e.g. format.bold)");
        cmd_input
            .style()
            .set_css_text("width:130px;padding:4px 6px;border-radius:4px;border:1px solid #475569;background:#0f172a;color:#f8fafc;font:inherit;");
        let _ = bar.append_child(&cmd_input);

        let doc_c = document.clone();
        let api_c = api.clone();
        add_button(document, &bar, "Run Cmd", false, move || {
            let cmd = cmd_input.value().trim().to_string();
            if cmd.is_empty() {
                return;
            }
            let doc_async = doc_c.clone();
            let api_async = api_c.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let empty = js_sys::Object::new();
                match call_api_cmd(&api_async, &cmd, &empty).await {
                    Ok(_) => set_status(&doc_async, &format!("Ran {cmd}")),
                    Err(err) => set_status(&doc_async, &format!("{cmd} failed: {err}")),
                }
            });
        });
    }

    // Local WASM vs Hybrid Cloudflare Edge toggle button
    if let Some(mode_btn) = create_el::<web_sys::HtmlElement>(document, "button") {
        mode_btn.set_inner_text("Mode: 100% Browser (Local WASM)");
        mode_btn.style().set_css_text(
            "padding:4px 8px;border-radius:4px;border:1px solid #38bdf8;background:#0f172a;color:#7dd3fc;\
             cursor:pointer;font:inherit;white-space:nowrap;",
        );
        let doc_c = document.clone();
        let edge_c = edge_mode;
        let btn_c = mode_btn.clone();
        let on_click = Closure::<dyn FnMut()>::new(move || {
            let next = !edge_c.get();
            edge_c.set(next);
            if next {
                btn_c.set_inner_text("Mode: Hybrid Cloudflare Edge");
                let _ = btn_c.style().set_property("border-color", "#f59e0b");
                let _ = btn_c.style().set_property("color", "#fcd34d");
                set_status(&doc_c, "Hybrid Edge enabled: Export uploads document bytes to /api/convert (unlogged).");
            } else {
                btn_c.set_inner_text("Mode: 100% Browser (Local WASM)");
                let _ = btn_c.style().set_property("border-color", "#38bdf8");
                let _ = btn_c.style().set_property("color", "#7dd3fc");
                set_status(&doc_c, "Local WASM active: 100% in-browser execution.");
            }
        });
        mode_btn.set_onclick(Some(on_click.as_ref().unchecked_ref()));
        on_click.forget();
        let _ = bar.append_child(&mode_btn);
    }

    if let Some(status) = create_el::<web_sys::HtmlElement>(document, "span") {
        status.set_id(STATUS_ID);
        status.set_inner_text("Local WASM active: 100% in-browser execution.");
        status.style().set_css_text("margin-left:auto;color:#94a3b8;white-space:nowrap;font-size:11px;");
        let _ = bar.append_child(&status);
    }

    let _ = body.append_child(&bar);
    let _ = body.append_child(&pane);
}

fn add_badge(document: &web_sys::Document, parent: &web_sys::HtmlElement, text: &str) {
    if let Some(span) = create_el::<web_sys::HtmlElement>(document, "span") {
        span.set_inner_text(text);
        span.style().set_css_text("font-weight:700;color:#fff;white-space:nowrap;margin-right:4px;");
        let _ = parent.append_child(&span);
    }
}

fn add_button<F: FnMut() + 'static>(document: &web_sys::Document, parent: &web_sys::HtmlElement, label: &str, primary: bool, on_press: F) {
    let Some(btn) = create_el::<web_sys::HtmlElement>(document, "button") else {
        return;
    };
    btn.set_inner_text(label);
    let bg = if primary { "#2563eb" } else { "#334155" };
    btn.style().set_css_text(&format!(
        "padding:4px 8px;border-radius:4px;border:1px solid #475569;background:{bg};color:#fff;cursor:pointer;font:inherit;white-space:nowrap;"
    ));
    let closure = Closure::<dyn FnMut()>::new(on_press);
    btn.set_onclick(Some(closure.as_ref().unchecked_ref()));
    closure.forget();
    let _ = parent.append_child(&btn);
}

fn set_status(document: &web_sys::Document, msg: &str) {
    if let Some(el) = document.get_element_by_id(STATUS_ID)
        && let Ok(html) = el.dyn_into::<web_sys::HtmlElement>()
    {
        html.set_inner_text(msg);
    }
}

fn api_fn(api: &js_sys::Object, name: &str) -> Result<js_sys::Function, String> {
    js_sys::Reflect::get(api, &JsValue::from_str(name))
        .ok()
        .and_then(|v| v.dyn_into::<js_sys::Function>().ok())
        .ok_or_else(|| format!("missing `{name}` on WordCraftApi"))
}

async fn call_api0(api: &js_sys::Object, name: &str) -> Result<JsValue, String> {
    let func = api_fn(api, name)?;
    let prom =
        func.call0(api).map_err(|e| format!("{e:?}"))?.dyn_into::<js_sys::Promise>().map_err(|_| format!("{name} did not return a Promise"))?;
    JsFuture::from(prom).await.map_err(|e| format!("{e:?}"))
}

async fn call_api_str(api: &js_sys::Object, name: &str, arg: &str) -> Result<JsValue, String> {
    call_api_val(api, name, &JsValue::from_str(arg)).await
}

async fn call_api_val(api: &js_sys::Object, name: &str, arg: &JsValue) -> Result<JsValue, String> {
    let func = api_fn(api, name)?;
    let prom =
        func.call1(api, arg).map_err(|e| format!("{e:?}"))?.dyn_into::<js_sys::Promise>().map_err(|_| format!("{name} did not return a Promise"))?;
    JsFuture::from(prom).await.map_err(|e| format!("{e:?}"))
}

async fn call_api_cmd(api: &js_sys::Object, command: &str, params: &JsValue) -> Result<JsValue, String> {
    let func = api_fn(api, "execute")?;
    let prom = func
        .call2(api, &JsValue::from_str(command), params)
        .map_err(|e| format!("{e:?}"))?
        .dyn_into::<js_sys::Promise>()
        .map_err(|_| "execute did not return a Promise".to_string())?;
    JsFuture::from(prom).await.map_err(|e| format!("{e:?}"))
}

async fn fetch_text(url: &str) -> Result<String, String> {
    let window = web_sys::window().ok_or_else(|| "no window".to_string())?;
    let resp_val = JsFuture::from(window.fetch_with_str(url)).await.map_err(|e| format!("{e:?}"))?;
    let resp: web_sys::Response = resp_val.dyn_into().map_err(|_| "invalid Response".to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let text_prom = resp.text().map_err(|e| format!("{e:?}"))?;
    let text_val = JsFuture::from(text_prom).await.map_err(|e| format!("{e:?}"))?;
    text_val.as_string().ok_or_else(|| "response was not text".to_string())
}

async fn convert_via_edge(api: &js_sys::Object) -> Result<Vec<u8>, String> {
    let md_val = call_api0(api, "getMarkdown").await?;
    let md = md_val.as_string().unwrap_or_default();
    let payload = serde_json::json!({"from": "md", "to": "docx", "content": md}).to_string();

    let window = web_sys::window().ok_or_else(|| "no window".to_string())?;
    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    init.set_body(&JsValue::from_str(&payload));
    let headers = web_sys::Headers::new().map_err(|e| format!("{e:?}"))?;
    headers.set("Content-Type", "application/json").map_err(|e| format!("{e:?}"))?;
    init.set_headers(headers.as_ref());

    let req = web_sys::Request::new_with_str_and_init("/api/convert", &init).map_err(|e| format!("{e:?}"))?;
    let resp_val = JsFuture::from(window.fetch_with_request(&req)).await.map_err(|e| format!("{e:?}"))?;
    let resp: web_sys::Response = resp_val.dyn_into().map_err(|_| "invalid Response".to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {} (requires Workers deployment)", resp.status()));
    }
    let buf_prom = resp.array_buffer().map_err(|e| format!("{e:?}"))?;
    let buf_val = JsFuture::from(buf_prom).await.map_err(|e| format!("{e:?}"))?;
    Ok(js_sys::Uint8Array::new(&buf_val).to_vec())
}
