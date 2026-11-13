//! Optional Cloudflare Worker entry (`workers-rs`) for WordCraftWeb Hybrid Edge mode.
//!
//! Serves static assets via the `ASSETS` binding and exposes:
//! - `GET /api/health`: runtime capability and limit metadata
//! - `POST /api/convert`: bounded, unlogged in-memory format conversion (`.md` / `.docx` / `.html` / `.txt`)
//! - `GET`/`HEAD *.wasm` with `Accept-Encoding: br`: precompressed Brotli sidecars (`<name>.wasm.br`,
//!   plan §2.2 F8) served straight through with `EncodeBody::Manual` so the runtime does not
//!   re-compress them. Any sidecar miss falls back to the plain asset, never an error page.
//!
//! Security and privacy guarantees:
//! - Enforces a strict `MAX_BODY_BYTES` (4 MiB) limit so isolate peak memory stays well under 128 MB.
//! - Sets `Cache-Control: no-store` and `X-Content-Type-Options: nosniff` on API replies.
//! - Never logs or persists user document bytes.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures_util::StreamExt;
use serde::Deserialize;
use worker::*;

/// Maximum request payload accepted by `/api/convert` (4 MiB).
/// Chosen from the Step 5.2a benchmark so isolate memory stays well below Cloudflare's 128 MB cap.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Formats advertised by `GET /api/health` (`supportedFormats`). Kept as a `const` so the F13
/// test proves every advertised format really converts with the exact crate builds this Worker
/// uses (`wordcraft-formats` with `all-formats`, plus `wordcraft-docx`).
const ADVERTISED_FORMATS: [&str; 4] = ["md", "txt", "html", "docx"];

/// Text source formats `/api/convert` reads as raw bytes; `docxBase64` arrives base64-wrapped
/// and goes through `wordcraft-docx` instead.
const CONVERT_TEXT_SOURCES: [&str; 3] = ["md", "html", "txt"];

/// Text target formats `/api/convert` writes via `wordcraft-formats` (`docx` goes through
/// `wordcraft-docx`). Every entry must also appear in `ADVERTISED_FORMATS` (checked by test).
const CONVERT_TEXT_TARGETS: [&str; 3] = ["md", "html", "txt"];

/// True when `Accept-Encoding` offers the `br` token (RFC 9110 §12.5.3), matching the token —
/// not a substring of another encoding like `zbr` — and without an explicit `q=0` refusal.
/// `*` alone does not opt in: we only send Brotli when the client names it.
fn accepts_brotli(header: &str) -> bool {
    header.split(',').any(|entry| {
        let mut params = entry.split(';');
        let Some(token) = params.next() else { return false };
        if !token.trim().eq_ignore_ascii_case("br") {
            return false;
        }
        // `br;q=0` explicitly refuses the coding; any other q (or none) accepts it.
        !params.any(|p| {
            let p = p.trim();
            let Some(q) = p.strip_prefix('q').or_else(|| p.strip_prefix('Q')) else {
                return false;
            };
            let q = q.trim().strip_prefix('=').map(str::trim).unwrap_or("");
            matches!(q.parse::<f32>(), Ok(v) if v == 0.0)
        })
    })
}

/// Try to serve the precompressed Brotli sidecar `<path>.br` for a `.wasm` request from the
/// `ASSETS` binding. `None` on any failure (missing sidecar, non-200, fetch or header error) so
/// the caller falls back to the plain asset path; this never yields an error response.
async fn brotli_wasm_sidecar(env: &Env, url: &Url, method: Method) -> Option<Response> {
    let mut br_url = url.clone();
    br_url.set_path(&format!("{}.br", url.path()));
    br_url.set_query(None);
    let br_req = Request::new(br_url.as_str(), method).ok()?;
    let assets = env.assets("ASSETS").ok()?;
    let mut resp = assets.fetch_request(br_req).await.ok()?;
    if resp.status_code() != 200 {
        return None;
    }
    let headers = resp.headers_mut();
    let tagged = [
        ("Content-Type", "application/wasm"),
        ("Content-Encoding", "br"),
        ("Vary", "Accept-Encoding"),
        ("Cache-Control", "public, max-age=31536000, immutable"),
        ("X-Content-Type-Options", "nosniff"),
    ];
    if tagged.iter().any(|(k, v)| headers.set(k, v).is_err()) {
        return None;
    }
    // The body is already Brotli: stop the runtime compressing it again.
    Some(resp.with_encode_body(EncodeBody::Manual))
}

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
            "supportedFormats": ADVERTISED_FORMATS,
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

        let mut stream = match req.stream() {
            Ok(s) => s,
            Err(_) => return json_error(400, "Invalid JSON conversion request"),
        };
        let mut body_bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if body_bytes.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
                return json_error(413, "Document exceeds 4 MiB Cloudflare edge conversion cap; use Local WASM mode instead.");
            }
            body_bytes.extend_from_slice(&chunk);
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
            ext if CONVERT_TEXT_SOURCES.contains(&ext) => parsed.content.into_bytes(),
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
            target if CONVERT_TEXT_TARGETS.contains(&target) => match wordcraft_formats::export(target, &doc) {
                Some(Ok(out)) => {
                    let mime = match target {
                        "html" => "text/html; charset=utf-8",
                        "md" => "text/markdown; charset=utf-8",
                        _ => "text/plain; charset=utf-8",
                    };
                    Ok(Response::from_body(ResponseBody::Body(out))?.with_status(200).with_headers(no_store_headers(mime)?))
                }
                Some(Err(e)) => json_error(500, &format!("Export error: {e}")),
                None => return json_error(400, "Unsupported target format"),
            },
            _ => json_error(400, "Unsupported target format (expected docx, md, html, or txt)"),
        };
    }

    if path.starts_with("/api/") {
        return json_error(404, "Unknown API route");
    }

    // Precompressed Brotli wasm sidecars (plan §2.2, F8): serve `<name>.wasm.br` when the client
    // names `br` and the build emitted the sidecar. Range requests (partial content the asset
    // tier must handle) and clients without `br` fall through to the plain asset below.
    if matches!(req.method(), Method::Get | Method::Head)
        && path.ends_with(".wasm")
        && matches!(req.headers().get("Range"), Ok(None))
        && matches!(&req.headers().get("Accept-Encoding"), Ok(Some(v)) if accepts_brotli(v))
        && let Some(resp) = brotli_wasm_sidecar(&env, &url, req.method()).await
    {
        return Ok(resp);
    }

    // Fall through to Cloudflare Workers Static Assets (`dist/web` + `_headers`).
    let assets = env.assets("ASSETS")?;
    assets.fetch_request(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F13: every format `/api/health` advertises converts end to end with the exact crate
    /// builds the Worker links against (`wordcraft-formats` `all-formats` + `wordcraft-docx`),
    /// through the same `import`/`export`/`read`/`write` calls `/api/convert` dispatches to.
    #[test]
    fn every_advertised_format_converts() {
        // The lists the API dispatch uses and the health advertisement agree.
        assert_eq!(ADVERTISED_FORMATS, ["md", "txt", "html", "docx"]);
        assert_eq!(CONVERT_TEXT_SOURCES, ["md", "html", "txt"]);
        assert_eq!(CONVERT_TEXT_TARGETS, ["md", "html", "txt"]);
        assert!(CONVERT_TEXT_TARGETS.iter().all(|t| ADVERTISED_FORMATS.contains(t)));

        let sources: [(&str, &[u8]); 3] = [
            ("md", b"# Worker test\n\nHello **world**.\n"),
            ("html", b"<h1>Worker test</h1><p>Hello <b>world</b>.</p>"),
            ("txt", b"Worker test\n\nHello world.\n"),
        ];
        for (ext, src) in sources {
            let doc = wordcraft_formats::import(ext, src)
                .unwrap_or_else(|| panic!("import({ext}) unsupported"))
                .unwrap_or_else(|e| panic!("import({ext}) failed: {e}"));
            for target in CONVERT_TEXT_TARGETS {
                let out = wordcraft_formats::export(target, &doc)
                    .unwrap_or_else(|| panic!("export({target}) unsupported"))
                    .unwrap_or_else(|e| panic!("export({target}) failed: {e}"));
                assert!(!out.is_empty(), "export({target}) produced empty output");
            }
        }

        // docx (the fourth advertised format): write with the same call `/api/convert` uses,
        // then read it back and convert it out to every advertised text target.
        let doc = wordcraft_formats::import("md", sources[0].1)
            .expect("md import")
            .expect("md import ok");
        let docx = wordcraft_docx::write(&doc).expect("docx write");
        assert!(docx.len() > 100, "docx output suspiciously small: {}", docx.len());
        let reread = wordcraft_docx::read(&docx).expect("docx read");
        for target in CONVERT_TEXT_TARGETS {
            let out = wordcraft_formats::export(target, &reread)
                .unwrap_or_else(|| panic!("export({target}) of round-tripped docx unsupported"))
                .unwrap_or_else(|e| panic!("export({target}) of round-tripped docx failed: {e}"));
            assert!(!out.is_empty(), "export({target}) of docx produced empty output");
        }
    }

    /// F8: the `br` decision must match the Accept-Encoding token, not a substring of another
    /// coding, and must honour an explicit `q=0` refusal. Pure function, host-testable.
    #[test]
    fn accept_encoding_matches_the_br_token() {
        let cases: [(&str, bool); 13] = [
            ("br", true),
            ("gzip, br", true),
            ("gzip, br, zstd", true),
            ("BR", true),
            ("br;q=1, gzip", true),
            (" br ; q=1 ", true),
            ("", false),
            ("gzip", false),
            ("zbr", false),          // substring trap: token is not `br`
            ("brotli", false),       // likewise a different token
            ("br;q=0", false),        // explicit refusal
            ("gzip, br;q=0.0", false),
            ("*", false),             // only an explicit `br` token opts in
        ];
        for (header, expected) in cases {
            assert_eq!(accepts_brotli(header), expected, "Accept-Encoding {header:?}");
        }
    }
}
