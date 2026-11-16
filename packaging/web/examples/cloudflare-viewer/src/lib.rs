//! A tiny server-rendered WordCraft viewer: a Cloudflare Worker (`workers-rs`) that opens a
//! `.docx` with the view-only core (`wordcraft-viewer`) and sends the reader a small HTML page
//! and PNG images of the pages. No wasm and no script reach the browser.
//!
//! Routes (the Worker runs first only for these; everything else is the static site):
//! - `GET /view?doc=samples/welcome.docx&page=1[&scale=1]`: the viewer page for a document of
//!   this deployment, read through the `ASSETS` binding.
//! - `GET /view/page.png?doc=...&page=N[&scale=S]`: one page as `image/png`.
//! - `GET /view`: the upload form. `POST /view` (multipart): the uploaded document's pages,
//!   inline as `data:` PNGs, never stored or logged.
//!
//! The pure logic (parameters, HTML, form data, opening and drawing, the font manifest) lives in
//! the modules below and is what the host tests call; this file is the thin layer over `worker`.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod document;
mod fonts;
mod html;
mod limits;
mod multipart;
mod params;
mod png_out;

use std::cell::RefCell;
use std::rc::Rc;

use futures_util::StreamExt;
use worker::{ByteStream, Context, Env, Headers, Method, Request, Response, Url, event};

use crate::document::OpenError;
use crate::limits::{ASSET_CACHE_CONTROL, DEFAULT_SCALE, MAX_BODY_BYTES, MAX_INLINE_PAGES, MAX_MANIFEST_BYTES, MAX_RESPONSE_BYTES};
use crate::params::{DocPath, ParamError, ViewQuery};

const HTML_TYPE: &str = "text/html; charset=utf-8";

/// How long a response may be kept.
#[derive(Clone, Copy)]
enum Caching {
    /// Made from a deployed document, or the same for everyone: [`ASSET_CACHE_CONTROL`].
    Asset,
    /// Made from an upload, or an error: `no-store`.
    NoStore,
}

fn response(status: u16, content_type: &str, caching: Caching, body: Vec<u8>) -> worker::Result<Response> {
    let headers = Headers::new();
    headers.set("Content-Type", content_type)?;
    headers.set(
        "Cache-Control",
        match caching {
            Caching::Asset => ASSET_CACHE_CONTROL,
            Caching::NoStore => "no-store",
        },
    )?;
    headers.set("X-Content-Type-Options", "nosniff")?;
    headers.set("Referrer-Policy", "no-referrer")?;
    if content_type == HTML_TYPE {
        headers.set("Content-Security-Policy", html::CONTENT_SECURITY_POLICY)?;
    }
    Ok(Response::from_bytes(body)?.with_status(status).with_headers(headers))
}

fn html_response(status: u16, caching: Caching, html: String) -> worker::Result<Response> {
    response(status, HTML_TYPE, caching, html.into_bytes())
}

fn error_response(status: u16, title: &str, detail: &str) -> worker::Result<Response> {
    html_response(status, Caching::NoStore, html::error_page(status, title, detail))
}

fn method_not_allowed(allow: &str) -> worker::Result<Response> {
    let mut resp = error_response(405, "Method not allowed", &format!("This address answers {allow}."))?;
    resp.headers_mut().set("Allow", allow)?;
    Ok(resp)
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> worker::Result<Response> {
    let url = req.url()?;
    match url.path() {
        "/view" => match req.method() {
            Method::Get | Method::Head => view_page(&env, &url).await,
            Method::Post => upload(&mut req, &env, &url).await,
            _ => method_not_allowed("GET, HEAD, POST"),
        },
        "/view/page.png" => match req.method() {
            Method::Get | Method::Head => page_image(&env, &url).await,
            _ => method_not_allowed("GET, HEAD"),
        },
        path if path.starts_with("/view/") => error_response(404, "Not found", "There is nothing at this address."),
        // Only the routes above run the Worker first (`wrangler.toml`); anything else that
        // reaches it is the static site's.
        _ => env.assets("ASSETS")?.fetch_request(req).await,
    }
}

/// A query refused: 400, saying which parameter and why.
fn bad_query(e: ParamError) -> worker::Result<Response> {
    error_response(400, "Bad request", &e.to_string())
}

/// `GET /view`: the viewer page for `doc`, or the upload form when there is no `doc`.
async fn view_page(env: &Env, url: &Url) -> worker::Result<Response> {
    let query = match params::parse_view_query(url.query(), DEFAULT_SCALE) {
        Ok(q) => q,
        Err(e) => return bad_query(e),
    };
    let Some(doc) = &query.doc else {
        return html_response(200, Caching::Asset, html::upload_form(None, false));
    };
    let (session, caching) = match asset_session(env, url, doc).await? {
        Ok(s) => s,
        Err(resp) => return Ok(resp),
    };
    let (index, _) = match page_of(&session, &query) {
        Ok(found) => found,
        Err(resp) => return resp,
    };
    // The core's own size, or its refusal: the status the image would answer with.
    let (width, height) = match document::page_raster(&session, index, query.scale) {
        Ok(px) => px,
        Err(e) => return error_response(e.status(), "The page can't be drawn", &e.to_string()),
    };
    html_response(200, caching, html::viewer_page(doc, query.page, session.page_count(), width, height, query.scale))
}

/// `GET /view/page.png`: one page of `doc` as a PNG.
async fn page_image(env: &Env, url: &Url) -> worker::Result<Response> {
    let query = match params::parse_view_query(url.query(), DEFAULT_SCALE) {
        Ok(q) => q,
        Err(e) => return bad_query(e),
    };
    let Some(doc) = &query.doc else {
        return error_response(400, "Bad request", "The doc parameter is required.");
    };
    let (session, caching) = match asset_session(env, url, doc).await? {
        Ok(s) => s,
        Err(resp) => return Ok(resp),
    };
    let (index, _) = match page_of(&session, &query) {
        Ok(found) => found,
        Err(resp) => return resp,
    };
    match document::page_png(&session, index, query.scale) {
        Ok((png, _, _)) => response(200, "image/png", caching, png),
        Err(e) => error_response(e.status(), "The page can't be drawn", &e.to_string()),
    }
}

/// The 0-based index and size of the query's page, or a 404 past the end.
fn page_of(session: &wordcraft_viewer::Session, query: &ViewQuery) -> Result<(u32, wordcraft_viewer::PageSize), worker::Result<Response>> {
    let pages = session.page_count();
    let index = query.page.saturating_sub(1);
    match session.page_size(index) {
        Ok(size) => Ok((index, size)),
        Err(_) => Err(error_response(404, "No such page", &format!("The document has {pages} page{}.", if pages == 1 { "" } else { "s" }))),
    }
}

/// Open the deployed document `doc`, with any font packs it asks for, and how its answer may be
/// kept: publicly only when its layout is final ([`fonts::settle`]), so a page drawn with a
/// substitute because a pack couldn't be read yet is never cached. The inner `Err` is the
/// response to send instead (404 for a missing document, 4xx for one that can't be opened).
async fn asset_session(env: &Env, url: &Url, doc: &DocPath) -> worker::Result<Result<(wordcraft_viewer::Session, Caching), Response>> {
    let bytes = match fetch_asset(env, url, doc.as_str(), MAX_BODY_BYTES).await? {
        Fetched::Bytes(b) => b,
        other => {
            let (status, title, detail) = unread_document(&other);
            return Ok(Err(error_response(status, title, &detail)?));
        }
    };
    match document::open_document(&bytes, None) {
        Ok(mut session) => {
            // Read before any await: the generation the document was just laid out at.
            let laid_out_at = wordcraft_fonts::FontDb::global().generation();
            let caching = if install_wanted_packs(env, url, &mut session, laid_out_at).await { Caching::Asset } else { Caching::NoStore };
            Ok(Ok((session, caching)))
        }
        Err(OpenError::PasswordRequired) => Ok(Err(error_response(
            422,
            "Protected document",
            "This document is protected with a password. Upload it at /view to open it with its password.",
        )?)),
        Err(e) => Ok(Err(error_response(e.status(), "The document can't be opened", &e.to_string())?)),
    }
}

/// A deployed file read through `ASSETS`.
#[derive(Debug, PartialEq)]
enum Fetched {
    Bytes(Vec<u8>),
    /// The asset store answered 404: the deployment has no such file.
    Missing,
    /// More than the reader's cap.
    TooLarge,
    /// Any other answer (403, 500…), no answer, or a body that broke off: the store failed, not
    /// the request, and another try may succeed.
    Failed,
}

impl Fetched {
    /// What an `ASSETS` answer with `status` is, before its body is read: `None` for a 200, whose
    /// body is to be read.
    fn from_status(status: u16) -> Option<Fetched> {
        match status {
            200 => None,
            404 => Some(Fetched::Missing),
            _ => Some(Fetched::Failed),
        }
    }
}

/// The answer for a deployed document that wasn't read, as (status, title, detail): 404 only
/// when the store says there is no such file; a store that failed is a 502, so an operator
/// isn't told a deployed document is missing.
fn unread_document(fetched: &Fetched) -> (u16, &'static str, String) {
    match fetched {
        Fetched::Missing => (404, "No such document", "This site has no document by that name.".into()),
        Fetched::TooLarge => (413, "Document too large", OpenError::TooLarge { bytes: MAX_BODY_BYTES + 1 }.to_string()),
        Fetched::Bytes(_) | Fetched::Failed => (502, "Storage error", "The site's storage did not return the document. Try again later.".into()),
    }
}

/// Read the deployed file at `path` (relative to the site root) through `ASSETS`, at most `cap`
/// bytes. Only paths the Worker built (a validated `doc`, a manifest entry) reach here.
async fn fetch_asset(env: &Env, base: &Url, path: &str, cap: usize) -> worker::Result<Fetched> {
    let target = base.join(&format!("/{path}"))?;
    let assets = env.assets("ASSETS")?;
    let Ok(mut resp) = assets.fetch_request(Request::new(target.as_str(), Method::Get)?).await else {
        return Ok(Fetched::Failed);
    };
    if let Some(answer) = Fetched::from_status(resp.status_code()) {
        return Ok(answer);
    }
    match read_capped(resp.stream()?, cap).await {
        Ok(bytes) => Ok(Fetched::Bytes(bytes)),
        Err(Capped::TooLarge) => Ok(Fetched::TooLarge),
        Err(Capped::Broken) => Ok(Fetched::Failed),
    }
}

enum Capped {
    TooLarge,
    Broken,
}

/// A body read chunk by chunk, refused as soon as it passes `cap`: a lying or missing
/// Content-Length can't grow the buffer past it.
async fn read_capped(mut stream: ByteStream, cap: usize) -> Result<Vec<u8>, Capped> {
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| Capped::Broken)?;
        if out.len().saturating_add(chunk.len()) > cap {
            return Err(Capped::TooLarge);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// `POST /view`: render an uploaded document's pages inline. Nothing about the upload is
/// stored, logged or echoed (its file name is shown back, escaped; the password never).
async fn upload(req: &mut Request, env: &Env, url: &Url) -> worker::Result<Response> {
    let form_error = |status: u16, notice: &str, ask_password: bool| -> worker::Result<Response> {
        html_response(status, Caching::NoStore, html::upload_form(Some(notice), ask_password))
    };
    let too_large = OpenError::TooLarge { bytes: MAX_BODY_BYTES + 1 }.to_string();
    if let Some(len) = req.headers().get("Content-Length")?
        && len.trim().parse::<u64>().is_ok_and(|n| n > MAX_BODY_BYTES as u64)
    {
        return form_error(413, &too_large, false);
    }
    let content_type = req.headers().get("Content-Type")?.unwrap_or_default();
    let boundary = match multipart::boundary(&content_type) {
        Ok(b) => b,
        Err(e) => return form_error(if e == multipart::FormError::NotMultipart { 415 } else { 400 }, &e.to_string(), false),
    };
    let body = match read_capped(req.stream()?, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(Capped::TooLarge) => return form_error(413, &too_large, false),
        Err(Capped::Broken) => return form_error(400, "The upload was interrupted.", false),
    };
    let form = match multipart::parse(&body, &boundary) {
        Ok(f) => f,
        Err(e) => return form_error(400, &e.to_string(), false),
    };
    let mut session = match document::open_upload(form.file, form.password.as_deref()) {
        Ok(s) => s,
        Err(e @ (OpenError::PasswordRequired | OpenError::WrongPassword)) => {
            return form_error(e.status(), &format!("{e} Choose the file again and enter its password."), true);
        }
        Err(e) => return form_error(e.status(), &e.to_string(), false),
    };
    // Read before any await: the generation the document was just laid out at. The answer is
    // `no-store` whatever the packs do.
    let laid_out_at = wordcraft_fonts::FontDb::global().generation();
    install_wanted_packs(env, url, &mut session, laid_out_at).await;

    let total = session.page_count();
    let mut images: Vec<(usize, Vec<u8>, u32, u32)> = Vec::new();
    // Room for the page around the images, with the longest note it can carry.
    let frame = html::upload_result(form.file_name.as_deref(), &[], total, Some(&html::left_out_note(total, 0, true))).len();
    let mut budget = MAX_RESPONSE_BYTES.saturating_sub(frame);
    let mut stopped_for_size = false;
    for index in 0..total.min(MAX_INLINE_PAGES) {
        let Ok(page) = u32::try_from(index) else { break };
        let (png, w, h) = match document::page_png(&session, page, DEFAULT_SCALE) {
            Ok(drawn) => drawn,
            Err(e) => return form_error(e.status(), &e.to_string(), false),
        };
        let cost = html::inline_page_cost(png.len());
        if cost > budget {
            stopped_for_size = true;
            break;
        }
        budget -= cost;
        images.push((index.saturating_add(1), png, w, h));
    }
    let shown = images.len();
    let left_out = (shown < total).then(|| html::left_out_note(total, shown, stopped_for_size));
    let pages: Vec<html::InlinePage<'_>> =
        images.iter().map(|(number, png, width, height)| html::InlinePage { number: *number, width: *width, height: *height, png }).collect();
    html_response(200, Caching::NoStore, html::upload_result(form.file_name.as_deref(), &pages, total, left_out.as_deref()))
}

// A Workers isolate runs its wasm on one thread, so these are one per isolate (on the host, where
// the tests run, one per thread).
thread_local! {
    /// `fonts/manifest.json` once read: its faces, or none when the deployment has no usable one
    /// (missing, too large or not valid). `None` until read; a store that failed is asked again.
    static MANIFEST: RefCell<Option<Rc<Vec<fonts::Face>>>> = const { RefCell::new(None) };
    /// The packs wanted, installed and unusable, for every request ([`fonts::settle`]).
    static PACKS: Rc<RefCell<fonts::PackState>> = Rc::new(RefCell::new(fonts::PackState::default()));
}

/// Install the font packs the isolate's documents want and lay `session` out again when a font
/// arrived since `laid_out_at`. Returns whether its layout is final (see [`fonts::settle`]): a
/// pack that couldn't be read leaves the text in a substitute face, and the answer uncached.
async fn install_wanted_packs(env: &Env, base: &Url, session: &mut wordcraft_viewer::Session, laid_out_at: u64) -> bool {
    let state = PACKS.with(Rc::clone);
    let Some(faces) = manifest(env, base).await else {
        // The store failed: the wants stay for a later request.
        state.borrow_mut().note(wordcraft_fonts::FontDb::global().take_wanted_packs());
        return !state.borrow().has_pending() && wordcraft_fonts::FontDb::global().generation() == laid_out_at;
    };
    let mut host = WorkerPacks { env, base, session };
    fonts::settle(&state, &faces, &mut host, laid_out_at).await
}

/// [`fonts::PackHost`] over the font database, `ASSETS` and one request's document.
struct WorkerPacks<'a> {
    env: &'a Env,
    base: &'a Url,
    session: &'a mut wordcraft_viewer::Session,
}

impl fonts::PackHost for WorkerPacks<'_> {
    fn take_wants(&mut self) -> Vec<wordcraft_fonts::WantedPackFace> {
        wordcraft_fonts::FontDb::global().take_wanted_packs()
    }

    async fn read(&mut self, file: &str) -> fonts::PackRead {
        match fetch_asset(self.env, self.base, &format!("fonts/{file}"), wordcraft_fonts::MAX_FONT_FILE_BYTES).await {
            Ok(Fetched::Bytes(b)) => fonts::PackRead::Bytes(b),
            Ok(Fetched::Missing | Fetched::TooLarge) => fonts::PackRead::Unusable,
            Ok(Fetched::Failed) | Err(_) => fonts::PackRead::Failed,
        }
    }

    fn install(&mut self, face: &fonts::Face, packed: &[u8]) -> bool {
        let Ok(ttf) = wordcraft_fonts::decode_pack_bytes(packed, face.bytes) else { return false };
        matches!(
            wordcraft_fonts::FontDb::global().install_pack_font(&face.file, &ttf),
            Ok(_) | Err(wordcraft_fonts::UserFontError::AlreadyAvailable(_))
        )
    }

    fn generation(&self) -> u64 {
        wordcraft_fonts::FontDb::global().generation()
    }

    fn relayout(&mut self) {
        self.session.relayout();
    }
}

/// The parsed manifest, fetched on first use. A deployment without a usable one has no packs;
/// a store that failed is asked again by a later request.
async fn manifest(env: &Env, base: &Url) -> Option<Rc<Vec<fonts::Face>>> {
    if let Some(m) = MANIFEST.with(|m| m.borrow().clone()) {
        return Some(m);
    }
    let faces = match fetch_asset(env, base, "fonts/manifest.json", MAX_MANIFEST_BYTES).await {
        Ok(Fetched::Bytes(json)) => fonts::parse_manifest(&json).unwrap_or_default(),
        Ok(Fetched::Missing | Fetched::TooLarge) => Vec::new(),
        Ok(Fetched::Failed) | Err(_) => return None,
    };
    let faces = Rc::new(faces);
    MANIFEST.with(|m| *m.borrow_mut() = Some(faces.clone()));
    Some(faces)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_404_from_the_store_is_a_missing_document() {
        assert_eq!(Fetched::from_status(200), None);
        assert_eq!(Fetched::from_status(404), Some(Fetched::Missing));
        for status in [301, 403, 410, 429, 500, 503] {
            assert_eq!(Fetched::from_status(status), Some(Fetched::Failed), "{status}");
        }
        assert_eq!(unread_document(&Fetched::Missing).0, 404);
        assert_eq!(unread_document(&Fetched::TooLarge).0, 413);
        let (status, _, detail) = unread_document(&Fetched::Failed);
        assert_eq!(status, 502);
        assert!(!detail.contains("no document"), "{detail}");
    }
}
