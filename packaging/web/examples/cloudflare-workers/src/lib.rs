//! Optional Cloudflare Worker entry (`workers-rs`) for WordCraftWeb Hybrid Edge mode.
//!
//! Serves static assets via the `ASSETS` binding and exposes:
//! - `GET /api/health`: runtime capability and limit metadata (`capabilities.preview` lets a
//!   demo button gate the preview affordance on the endpoint existing)
//! - `POST /api/convert`: bounded, unlogged in-memory format conversion (`.md` / `.docx` / `.html` / `.txt`)
//! - `POST /api/preview-page`: bounded PNG raster of page 1 (`.md` or `.docx`) at a fixed low
//!   resolution through `wordcraft-layout` + `wordcraft-render` (vello_cpu) — approximate,
//!   since the Worker carries only the four bundled font faces (plan §4.3, F6/Q2)
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

/// Maximum request payload accepted by `/api/convert` and `/api/preview-page` (4 MiB).
/// Chosen from the Step 5.2a benchmark so isolate peak memory stays well below Cloudflare's 128 MB cap.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Decompression budgets for request-supplied `.docx` input, passed to `wordcraft_docx::read_limited`.
/// The 4 MiB body cap bounds the *compressed* package only; without these a small zip could inflate
/// to the crate defaults (256 MiB per part, 512 MiB total) and blow the isolate's 128 MB. 64 MiB
/// total leaves ample room for the document model, layout and the page-1 raster on top.
const DOCX_MAX_PART_BYTES: u64 = 32 * 1024 * 1024;
const DOCX_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// The 413 message both endpoints share.
const BODY_TOO_LARGE: &str = "Document exceeds 4 MiB Cloudflare edge cap; use Local WASM mode instead.";

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

/// Cache-Control for a wasm served through the Brotli-sidecar path (S4): the content-hashed
/// main module is `immutable`, but Trunk keeps dedicated-worker names un-hashed (they must be
/// named at runtime), so those follow `packaging/web/_headers`' worker rule and revalidate on
/// every load — a worker cached past the hashed main module it talks to would fail the
/// protocol version check for as long as the cache lives.
fn wasm_sidecar_cache_control(path: &str) -> &'static str {
    if path.rsplit('/').next().is_some_and(|name| name.starts_with("wordcraft-worker-")) { "no-cache" } else { "public, max-age=31536000, immutable" }
}

/// Pick the `Accept-Encoding` offer to honour: Cloudflare exposes the client's original offer
/// as `cf.clientAcceptEncoding` because the edge may rewrite the request's `Accept-Encoding`
/// header before the Worker sees it; the header is only a fallback for runtimes without `cf`.
fn accept_encoding_offer<'a>(cf_client_accept_encoding: Option<&'a str>, header: Option<&'a str>) -> Option<&'a str> {
    cf_client_accept_encoding.or(header)
}

/// `cf.clientAcceptEncoding` off the raw incoming request object. The typed `worker::Cf` API
/// does not surface it, so it is read through `Reflect`; anything missing or non-string is
/// `None` and the caller falls back to the header.
#[cfg(target_arch = "wasm32")]
fn cf_client_accept_encoding(req: &Request) -> Option<String> {
    let cf = js_sys::Reflect::get(req.inner(), &wasm_bindgen::JsValue::from_str("cf")).ok()?;
    js_sys::Reflect::get(&cf, &wasm_bindgen::JsValue::from_str("clientAcceptEncoding")).ok()?.as_string()
}

#[cfg(not(target_arch = "wasm32"))]
fn cf_client_accept_encoding(_req: &Request) -> Option<String> {
    None
}

/// Try to serve the precompressed Brotli sidecar `<path>.br` for a `.wasm` request from the
/// `ASSETS` binding. `None` on any failure (missing sidecar, non-200, fetch or header error) so
/// the caller falls back to the plain asset path; this never yields an error response.
///
/// The binding's response carries *immutable* headers (workerd guards responses fetched through
/// a binding: mutating them throws "Can't modify immutable headers"), so the sidecar cannot be
/// tagged in place — verified live 2026-11-14, and the reason this path silently fell through to
/// the plain asset before. Instead the body is taken out (`bytes()` buffers it, ≈5 MB, far
/// under the isolate's 128 MB) and a fresh response carries the tags, with `EncodeBody::Manual`
/// so the runtime does not compress the already-Brotli body again.
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
    let body = resp.bytes().await.ok()?;
    let headers = Headers::new();
    let tagged = [
        ("Content-Type", "application/wasm"),
        ("Content-Encoding", "br"),
        ("Vary", "Accept-Encoding"),
        ("Cache-Control", wasm_sidecar_cache_control(url.path())),
        ("X-Content-Type-Options", "nosniff"),
    ];
    if tagged.iter().any(|(k, v)| headers.set(k, v).is_err()) {
        return None;
    }
    Some(Response::from_bytes(body).ok()?.with_headers(headers).with_encode_body(EncodeBody::Manual))
}

#[derive(Deserialize)]
struct ConvertRequest {
    from: String,
    to: String,
    content: String,
}

#[derive(Deserialize)]
struct PreviewRequest {
    from: String,
    content: String,
}

/// Source formats `/api/preview-page` reads, shaped like `/api/convert`'s: `docxBase64` is a
/// base64-wrapped `.docx`, `md` is Markdown text. Page 1 only, always.
const PREVIEW_SOURCES: [&str; 2] = ["md", "docxBase64"];

/// Preview raster resolution: 96 CSS pixels per inch against layout's page points (72/inch),
/// fixed so a preview stays cheap (`render_page` also clamps to `MAX_SIDE`/`MAX_RASTER_PIXELS`).
const PREVIEW_SCALE: f32 = 96.0 / 72.0;

/// Read a request body through `req`'s stream, bounded by [`MAX_BODY_BYTES`] — first by the
/// `Content-Length` header when the runtime sent one, then chunk by chunk so a lying header
/// cannot grow the allocation. `Err((status, msg))` is what the caller turns into a JSON reply.
async fn read_body_capped(req: &mut Request) -> std::result::Result<Vec<u8>, (u16, &'static str)> {
    if let Ok(Some(len_hdr)) = req.headers().get("Content-Length")
        && let Ok(len) = len_hdr.parse::<usize>()
        && len > MAX_BODY_BYTES
    {
        return Err((413, BODY_TOO_LARGE));
    }
    let mut stream = req.stream().map_err(|_| (400, "Invalid request body"))?;
    let mut body_bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| (400, "Invalid request body"))?;
        if body_bytes.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            return Err((413, BODY_TOO_LARGE));
        }
        body_bytes.extend_from_slice(&chunk);
    }
    Ok(body_bytes)
}

/// Render page 1 of `doc` at the fixed preview resolution as PNG bytes. The whole pipeline is
/// the one the app uses for page images (`layout` → `render_page` → `to_png`), with the
/// defaults' Print view and no proofing markup; `None` when the document produces no page or
/// the PNG encode fails.
fn preview_png(doc: &wordcraft_doc::Document) -> Option<Vec<u8>> {
    let layout = wordcraft_layout::layout(doc, &mut wordcraft_layout::LayoutCache::new(), &wordcraft_layout::LayoutOptions::default());
    let page = layout.pages.first()?;
    let img = wordcraft_render::render_page(doc, page, PREVIEW_SCALE, &wordcraft_render::RenderOptions::default());
    let png = img.to_png();
    (!png.is_empty()).then_some(png)
}

/// The `GET /api/health` body. A `fn` (not an inline literal) so tests can assert every
/// advertised capability is real.
fn health_body() -> String {
    serde_json::json!({
        "ok": true,
        "mode": "hybrid-cloudflare-worker",
        "maxBodyBytes": MAX_BODY_BYTES,
        "supportedFormats": ADVERTISED_FORMATS,
        "capabilities": { "convert": true, "preview": true },
        "preview": { "route": "/api/preview-page", "formats": PREVIEW_SOURCES, "approximate": true, "page": 1, "contentType": "image/png" },
        "privacy": "zero-retention; document bytes are never logged or stored"
    })
    .to_string()
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
        let body = health_body();
        return Ok(Response::from_body(ResponseBody::Body(body.into_bytes()))?
            .with_status(200)
            .with_headers(no_store_headers("application/json; charset=utf-8")?));
    }

    if path == "/api/preview-page" {
        if req.method() != Method::Post {
            return json_error(405, "POST required");
        }
        let body_bytes = match read_body_capped(&mut req).await {
            Ok(b) => b,
            Err((status, msg)) => return json_error(status, msg),
        };
        let parsed: PreviewRequest = match serde_json::from_slice(&body_bytes) {
            Ok(v) => v,
            Err(_) => return json_error(400, "Invalid JSON preview request"),
        };
        let input_bytes = match parsed.from.as_str() {
            "docxBase64" => match STANDARD.decode(parsed.content.as_bytes()) {
                Ok(b) => b,
                Err(_) => return json_error(400, "Invalid base64 payload for docxBase64"),
            },
            "md" => parsed.content.into_bytes(),
            _ => return json_error(400, "Unsupported preview format (expected md or docxBase64)"),
        };
        let doc = match parsed.from.as_str() {
            "docxBase64" => match wordcraft_docx::read_limited(&input_bytes, DOCX_MAX_PART_BYTES, DOCX_MAX_TOTAL_BYTES) {
                Ok(d) => d,
                Err(e) => return json_error(422, &format!("DOCX import error: {e}")),
            },
            _ => match wordcraft_formats::import("md", &input_bytes) {
                Some(Ok(d)) => d,
                Some(Err(e)) => return json_error(422, &format!("Import error: {e}")),
                None => return json_error(400, "Unsupported source format"),
            },
        };
        let Some(png) = preview_png(&doc) else {
            return json_error(422, "Document produced no previewable page");
        };
        let mut resp = Response::from_body(ResponseBody::Body(png))?
            .with_status(200)
            .with_headers(no_store_headers("image/png")?);
        // The Worker has only the four bundled faces, so fonts the document asks for may be
        // substituted: say so on the wire, next to the `approximate` flag in /api/health.
        if let Err(e) = resp.headers_mut().set("X-Wordcraft-Preview", "approximate; fonts=bundled-4") {
            console_error!("preview: cannot set header: {e}");
        }
        return Ok(resp);
    }

    if path == "/api/convert" {
        if req.method() != Method::Post {
            return json_error(405, "POST required");
        }
        let body_bytes = match read_body_capped(&mut req).await {
            Ok(b) => b,
            Err((status, msg)) => return json_error(status, msg),
        };

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
            "docxBase64" => match wordcraft_docx::read_limited(&input_bytes, DOCX_MAX_PART_BYTES, DOCX_MAX_TOTAL_BYTES) {
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
        && matches!(accept_encoding_offer(cf_client_accept_encoding(&req).as_deref(), req.headers().get("Accept-Encoding").ok().flatten().as_deref()), Some(v) if accepts_brotli(v))
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

    /// S4: sidecar responses must not pin the un-hashed dedicated-worker modules for a year;
    /// only the content-hashed main module is `immutable`. Pure function, host-testable.
    #[test]
    fn sidecar_cache_control_follows_hashed_vs_worker_names() {
        assert_eq!(wasm_sidecar_cache_control("/wordcraft-web-5192d64fd8466f72_bg.wasm"), "public, max-age=31536000, immutable");
        assert_eq!(wasm_sidecar_cache_control("/wordcraft-worker-proof_bg.wasm"), "no-cache");
        assert_eq!(wasm_sidecar_cache_control("/wordcraft-worker-codecs_bg.wasm"), "no-cache");
        assert_eq!(wasm_sidecar_cache_control("/wordcraft-worker-proof_bg.wasm.br"), "no-cache");
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

    /// The edge may rewrite `Accept-Encoding`; `cf.clientAcceptEncoding` carries the client's
    /// real offer and wins when present, so a rewritten `br` header cannot force Brotli on a
    /// client that never asked for it — the decision then falls through to the plain asset.
    #[test]
    fn client_offer_prefers_cf_over_rewritten_header() {
        // Original offer and rewritten header differ: cf wins in both directions.
        assert_eq!(accept_encoding_offer(Some("gzip"), Some("br")), Some("gzip"));
        assert!(!accepts_brotli(accept_encoding_offer(Some("gzip"), Some("br")).unwrap()));
        assert_eq!(accept_encoding_offer(Some("br"), Some("gzip")), Some("br"));
        assert!(accepts_brotli(accept_encoding_offer(Some("br"), Some("gzip")).unwrap()));
        // A `q=0` refusal in the original offer still wins over a rewritten `br` header.
        assert!(!accepts_brotli(accept_encoding_offer(Some("br;q=0"), Some("br")).unwrap()));
        // No cf property (local tooling, other runtimes): the request header decides.
        assert_eq!(accept_encoding_offer(None, Some("br")), Some("br"));
        assert!(accepts_brotli(accept_encoding_offer(None, Some("br")).unwrap()));
        // Neither source: no offer, so the plain asset path is preserved.
        assert_eq!(accept_encoding_offer(None, None), None);
        assert_eq!(accept_encoding_offer(Some(""), None), Some(""));
        assert!(!accepts_brotli(accept_encoding_offer(Some(""), None).unwrap()));
    }

    /// 4.3 (F6/Q2): `/api/preview-page` renders page 1 of a known sample through the same
    /// calls the handler makes (`import`/`read` → `layout` → `render_page` → `to_png`), which
    /// are target-agnostic, so the test runs natively. The four bundled faces are initialised
    /// the way the wasm Worker's font set is built (no system scan).
    #[test]
    fn preview_page_renders_a_nonempty_png() {
        assert!(wordcraft_fonts::FontDb::init_test_fonts(), "preview tests must initialize fixed fonts before any font lookup");
        assert_eq!(PREVIEW_SOURCES, ["md", "docxBase64"]);

        const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
        let md = b"# Preview\n\nHello **world**, this is page one.\n\nMore text to fill the page.\n";
        let doc = wordcraft_formats::import("md", md)
            .unwrap_or_else(|| panic!("import(md) unsupported"))
            .unwrap_or_else(|e| panic!("import(md) failed: {e}"));
        let t0 = std::time::Instant::now();
        let png = preview_png(&doc).expect("preview_png returned None for a valid document");
        // Rough CPU hint for the plan's Free-vs-Paid gate (native build shown; re-measure on the
        // deployment): `cargo test --release -- --nocapture preview_page`.
        eprintln!("preview_png(md): {:?} -> {} bytes", t0.elapsed(), png.len());
        assert_eq!(png.get(..8), Some(PNG_MAGIC.as_slice()), "preview is not a PNG");
        assert!(png.len() > 1_000, "page-1 PNG suspiciously small: {} bytes", png.len());

        // The docxBase64 arm: the same document round-tripped through docx also renders.
        let docx = wordcraft_docx::write(&doc).expect("docx write");
        let reread = wordcraft_docx::read(&docx).expect("docx read");
        let t0 = std::time::Instant::now();
        let png2 = preview_png(&reread).expect("preview_png returned None for a docx document");
        eprintln!("preview_png(docx): {:?} -> {} bytes", t0.elapsed(), png2.len());
        assert_eq!(png2.get(..8), Some(PNG_MAGIC.as_slice()), "docx preview is not a PNG");
        assert!(png2.len() > 1_000, "docx page-1 PNG suspiciously small: {} bytes", png2.len());
    }

    /// `/api/health` must only advertise what the crate builds and the routes serve: the
    /// `preview` capability flag a demo button gates on, and the preview's real contract.
    #[test]
    fn health_advertises_only_real_capabilities() {
        let v: serde_json::Value = serde_json::from_str(&health_body()).expect("health body is JSON");
        assert_eq!(v["capabilities"]["preview"], true, "demo buttons gate on this flag");
        assert_eq!(v["capabilities"]["convert"], true);
        assert_eq!(v["preview"]["route"], "/api/preview-page");
        assert_eq!(v["preview"]["formats"], serde_json::json!(PREVIEW_SOURCES));
        assert_eq!(v["preview"]["approximate"], true, "the four bundled faces make previews approximate");
        assert_eq!(v["maxBodyBytes"], MAX_BODY_BYTES as u64);
        let advertised: Vec<&str> = v["supportedFormats"].as_array().expect("formats array").iter().filter_map(|f| f.as_str()).collect();
        assert_eq!(advertised, ADVERTISED_FORMATS);
    }
}
