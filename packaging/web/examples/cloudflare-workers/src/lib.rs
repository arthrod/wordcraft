//! Optional Cloudflare Worker entry (`workers-rs`) for WordCraftWeb Hybrid Edge mode.
//!
//! Serves static assets via the `ASSETS` binding and exposes:
//! - `GET /api/health`: runtime capability and limit metadata
//! - `POST /api/convert`: bounded, unlogged in-memory format conversion (`.md` / `.docx` / `.html` / `.txt`)
//!
//! Security and privacy guarantees:
//! - Enforces a strict `MAX_BODY_BYTES` (4 MiB) limit so isolate peak memory stays well under 128 MB.
//! - Sets `Cache-Control: no-store` and `X-Content-Type-Options: nosniff` on API replies.
//! - Never logs or persists user document bytes.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use worker::*;

/// Maximum request payload accepted by `/api/convert` (4 MiB).
/// Chosen from the Step 5.2a benchmark so isolate memory stays well below Cloudflare's 128 MB cap.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
struct ConvertRequest {
    from: String,
    to: String,
    content: String,
}

fn no_store_headers(content_type: &str) -> Result<Headers> {
    let headers = Headers::new();
    headers.set("Content-Type", content_type)?;
    headers.set("Cache-Control", "no-store")?;
    headers.set("X-Content-Type-Options", "nosniff")?;
    Ok(headers)
}

fn json_error(status: u16, msg: &str) -> Result<Response> {
    let body = serde_json::json!({ "ok": false, "error": msg }).to_string();
    let resp = Response::from_body(ResponseBody::Body(body.into_bytes()))?
        .with_status(status)
        .with_headers(no_store_headers("application/json; charset=utf-8")?);
    Ok(resp)
}

#[event(fetch)]
async fn main(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let url = req.url()?;
    let path = url.path();

    if path == "/api/health" && req.method() == Method::Get {
        let body = serde_json::json!({
            "ok": true,
            "mode": "hybrid-cloudflare-worker",
            "maxBodyBytes": MAX_BODY_BYTES,
            "supportedFormats": ["md", "txt", "html", "docx"],
            "privacy": "zero-retention; document bytes are never logged or stored"
        })
        .to_string();
        return Ok(Response::from_body(ResponseBody::Body(body.into_bytes()))?
            .with_status(200)
            .with_headers(no_store_headers("application/json; charset=utf-8")?));
    }

    if path == "/api/convert" {
        if req.method() != Method::Post {
            return json_error(405, "POST required");
        }
        if let Some(len_hdr) = req.headers().get("Content-Length")?
            && let Ok(len) = len_hdr.parse::<usize>()
            && len > MAX_BODY_BYTES
        {
            return json_error(413, "Document exceeds 4 MiB Cloudflare edge conversion cap; use Local WASM mode instead.");
        }

        let body_bytes = req.bytes().await?;
        if body_bytes.len() > MAX_BODY_BYTES {
            return json_error(413, "Document exceeds 4 MiB Cloudflare edge conversion cap; use Local WASM mode instead.");
        }

        let parsed: ConvertRequest = match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(_) => return json_error(400, "Invalid JSON conversion request"),
        };

        let input_bytes = match parsed.from.as_str() {
            "docxBase64" => match STANDARD.decode(parsed.content.as_bytes()) {
                Ok(b) => b,
                Err(_) => return json_error(400, "Invalid base64 payload for docxBase64"),
            },
            "md" | "html" | "txt" => parsed.content.into_bytes(),
            _ => return json_error(400, "Unsupported source format (expected md, html, txt, or docxBase64)"),
        };

        let doc = match parsed.from.as_str() {
            "docxBase64" => match wordcraft_docx::read(&input_bytes) {
                Ok(d) => d,
                Err(e) => return json_error(422, &format!("DOCX import error: {e}")),
            },
            ext => match wordcraft_formats::import(ext, &input_bytes) {
                Some(Ok(d)) => d,
                Some(Err(e)) => return json_error(422, &format!("Import error: {e}")),
                None => return json_error(400, "Unsupported source format"),
            },
        };

        return match parsed.to.as_str() {
            "docx" => match wordcraft_docx::write(&doc) {
                Ok(out) => Ok(Response::from_body(ResponseBody::Body(out))?
                    .with_status(200)
                    .with_headers(no_store_headers("application/vnd.openxmlformats-officedocument.wordprocessingml.document")?)),
                Err(e) => json_error(500, &format!("DOCX export error: {e}")),
            },
            "md" | "html" | "txt" => match wordcraft_formats::export(parsed.to.as_str(), &doc) {
                Some(Ok(out)) => {
                    let mime = match parsed.to.as_str() {
                        "html" => "text/html; charset=utf-8",
                        "md" => "text/markdown; charset=utf-8",
                        _ => "text/plain; charset=utf-8",
                    };
                    Ok(Response::from_body(ResponseBody::Body(out))?.with_status(200).with_headers(no_store_headers(mime)?))
                }
                Some(Err(e)) => json_error(500, &format!("Export error: {e}")),
                None => json_error(400, "Unsupported target format"),
            },
            _ => json_error(400, "Unsupported target format (expected docx, md, html, or txt)"),
        };
    }

    if path.starts_with("/api/") {
        return json_error(404, "Unknown API route");
    }

    // Fall through to Cloudflare Workers Static Assets (`dist/web` + `_headers`).
    let assets = env.assets("ASSETS")?;
    assets.fetch_request(req).await
}
