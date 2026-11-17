//! The view-only dedicated worker (plan 10.3): opens a `.docx` and paints its pages off the
//! main thread, on the [`wordcraft_worker_proto`] contract.
//!
//! Built by Trunk as `data-type="worker"` + `data-bindgen-target="web"` from
//! `apps/wordcraft-web/index.html`. It is the view-only core (`wordcraft-viewer`) with the
//! worker envelope around it and nothing else: requests arrive as
//! `[headerJson, payloadArrayBuffer]` and answers leave the same way, the payload buffer
//! transferred. Every failure is a typed `error` reply. This code never panics, so a hostile
//! or malformed message costs one reply, never the worker.
//!
//! The worker holds one document at a time: `viewOpen` replaces it, `viewRender` paints one
//! page of it, `viewInstallFont` adds a font and lays it out again, `viewMissingFonts` lists
//! what it still wants. `.docx` bytes are the only document interchange; no model crosses.
//!
//! The editor, the engine and `krilla` are never linked here (`cargo xtask wasm-budget`).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use wordcraft_viewer::{Session, ViewerError};
use wordcraft_worker_proto as proto;
use wordcraft_worker_proto::{Envelope, ErrorCode, Kind};

type Outcome = Result<(Kind, Vec<u8>), (ErrorCode, String)>;

/// The worker's state: the open document, if any.
#[derive(Default)]
pub struct ViewerWorker {
    session: Option<Session>,
}

impl ViewerWorker {
    pub fn new() -> ViewerWorker {
        ViewerWorker::default()
    }

    /// Handle one wire message: `(headerJson, payloadBytes)` in, `(headerJson, payloadBytes)`
    /// out. Always answers: a message that can't be decoded gets an `error` reply with id 0.
    pub fn handle_message(&mut self, header: &[u8], payload: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let env = match proto::decode_envelope(header) {
            Ok(env) => env,
            Err(proto::ProtoError::Version { got, .. }) => {
                let env = lenient_envelope(header);
                return reply(&env, error(ErrorCode::UnsupportedVersion, format!("protocol version {got}, expected {}", proto::PROTOCOL_VERSION)));
            }
            Err(e) => {
                return reply(
                    &Envelope { v: proto::PROTOCOL_VERSION, id: 0, kind: Kind::Error, revision: 0 },
                    error(ErrorCode::BadRequest, e.to_string()),
                );
            }
        };

        if !env.kind.is_request() {
            return reply(&env, error(ErrorCode::UnknownKind, format!("`{:?}` is a reply, not a request", env.kind)));
        }

        let outcome = match env.kind {
            Kind::Ping => Ok((Kind::Pong, EMPTY_BODY.to_vec())),
            Kind::Cancel => cancel(payload),
            Kind::ViewOpen => self.open(payload),
            Kind::ViewRender => self.render(payload),
            Kind::ViewInstallFont => self.install_font(payload),
            Kind::ViewMissingFonts => self.missing_fonts(),
            // This worker views documents; it does not proof, convert or export.
            Kind::Check | Kind::Configure | Kind::Convert | Kind::RenderPdf => {
                error(ErrorCode::UnknownKind, "this worker only opens and paints documents".into())
            }
            // Not reached: `is_request` was checked above.
            other => error(ErrorCode::UnknownKind, format!("`{other:?}` is a reply, not a request")),
        };
        reply(&env, outcome)
    }

    /// `viewOpen`: read the package and lay it out. The document held before stays in place
    /// when the new one can't be opened; when it can, the fonts the one before embedded go with
    /// it.
    fn open(&mut self, payload: &[u8]) -> Outcome {
        self.open_answering(payload, opened_reply)
    }

    /// [`ViewerWorker::open`], with the reply made by `answer`. Whatever fails before the new
    /// document takes the old one's place, the open or the reply, leaves the fonts as they were
    /// (the [`wordcraft_viewer::Replacement`] is dropped unkept).
    fn open_answering(&mut self, payload: &[u8], answer: impl FnOnce(&Session) -> Outcome) -> Outcome {
        // The file is read where it lies in the frame: no second copy of up to 64 MiB.
        let (password, docx) = proto::view_open_request_parts(payload).map_err(proto_error)?;
        let mut replacing = wordcraft_viewer::Replacement::begin(self.session.as_ref());
        let session = replacing.open(docx, password.as_deref()).map_err(viewer_error)?;
        let opened = answer(&session)?;
        replacing.keep();
        self.session = Some(session);
        Ok(opened)
    }

    /// `viewRender`: one page of the open document. The reply payload is the page's RGBA bytes
    /// with a trailer ([`proto::finish_view_page`]).
    fn render(&mut self, payload: &[u8]) -> Outcome {
        let req: proto::ViewRenderRequest = proto::decode_payload(payload).map_err(proto_error)?;
        if let Some(code) = proto::view_render_request_error(&req) {
            return error(code, format!("scale {} is not a number above 0 and at most {}", req.scale, proto::MAX_VIEW_SCALE));
        }
        let session = self.session.as_ref().ok_or_else(no_document)?;
        let image = session.render(req.page, req.scale).map_err(viewer_error)?;
        let header = proto::ViewPageHeader { page: req.page, scale: req.scale, width: image.width, height: image.height };
        let bytes = proto::finish_view_page(image.pixels, &header).map_err(|e| (ErrorCode::Internal, e.to_string()))?;
        Ok((Kind::ViewPage, bytes))
    }

    /// `viewInstallFont`: add a font file. With a document open it is laid out again and the
    /// reply is its new page list; without one the reply is `ack`.
    fn install_font(&mut self, payload: &[u8]) -> Outcome {
        let req = proto::decode_view_install_font_request(payload).map_err(proto_error)?;
        match self.session.as_mut() {
            Some(session) => {
                session.install_font(&req.name, &req.bytes).map_err(viewer_error)?;
                opened_reply(session)
            }
            None => {
                wordcraft_viewer::install_font(&req.name, &req.bytes).map_err(viewer_error)?;
                Ok((Kind::Ack, EMPTY_BODY.to_vec()))
            }
        }
    }

    /// `viewMissingFonts`: the fonts the open document asks for that aren't available.
    fn missing_fonts(&self) -> Outcome {
        let session = self.session.as_ref().ok_or_else(no_document)?;
        let listed = proto::ViewFontsReply::new(session.missing_fonts());
        proto::encode_payload(&listed).map(|p| (Kind::ViewFonts, p)).map_err(|e| (ErrorCode::Internal, e.to_string()))
    }
}

/// The `viewOpened` reply for `session`: its page count and page sizes.
fn opened_reply(session: &Session) -> Outcome {
    let count = u32::try_from(session.page_count()).unwrap_or(u32::MAX);
    let sizes = (0..count).map_while(|page| session.page_size(page).ok()).map(|s| proto::ViewPageSize { width: s.width, height: s.height });
    let mut opened = proto::ViewOpenedReply::new(sizes.take(proto::MAX_VIEW_PAGES));
    opened.page_count = count;
    proto::encode_payload(&opened).map(|p| (Kind::ViewOpened, p)).map_err(|e| (ErrorCode::Internal, e.to_string()))
}

fn no_document() -> (ErrorCode, String) {
    (ErrorCode::NoDocument, "no document is open: send `viewOpen` first".into())
}

/// A protocol decode failure as the error reply it gets.
fn proto_error(e: proto::ProtoError) -> (ErrorCode, String) {
    let code = if matches!(e, proto::ProtoError::TooLarge { .. }) { ErrorCode::TooLarge } else { ErrorCode::BadRequest };
    (code, e.to_string())
}

/// A refusal of the view-only core as the error reply it gets. The message leads with the
/// core's own stable code (`wrong-password`, `no-such-page`, …).
fn viewer_error(e: ViewerError) -> (ErrorCode, String) {
    let code = match e {
        ViewerError::PasswordRequired => ErrorCode::PasswordRequired,
        ViewerError::WrongPassword => ErrorCode::WrongPassword,
        ViewerError::Unreadable(_) | ViewerError::BadFont(_) => ErrorCode::Unreadable,
        ViewerError::DocumentTooLarge { .. }
        | ViewerError::TooManyPixels { .. }
        | ViewerError::FontTooLarge { .. }
        | ViewerError::TooManyFonts { .. } => ErrorCode::TooLarge,
        ViewerError::EmptyDocument | ViewerError::BadScale { .. } | ViewerError::NoSuchPage { .. } | ViewerError::BadFontName => {
            ErrorCode::BadRequest
        }
    };
    (code, format!("{}: {e}", e.code()))
}

/// `id`/`revision` of a header that decoded but failed the version check, so the error reply
/// still matches the request it answers.
fn lenient_envelope(header: &[u8]) -> Envelope {
    let val: serde_json::Value = serde_json::from_slice(header).unwrap_or(serde_json::Value::Null);
    let num = |key: &str| val.get(key).and_then(serde_json::Value::as_u64).unwrap_or_default();
    Envelope { v: proto::PROTOCOL_VERSION, id: u32::try_from(num("id")).unwrap_or_default(), kind: Kind::Error, revision: num("revision") }
}

/// `(header, payload)` bytes for a reply to `request`. A failed payload encode becomes an
/// `error` reply instead of silence (and a failed error encode falls back to a constant).
fn reply(request: &Envelope, outcome: Outcome) -> (Vec<u8>, Vec<u8>) {
    let (kind, payload) = match outcome {
        Ok((kind, payload)) => (kind, payload),
        Err((code, message)) => (Kind::Error, proto::encode_payload(&proto::ErrorReply { code, message }).unwrap_or_else(|_| b"{}".to_vec())),
    };
    let header =
        proto::encode_envelope(&Envelope::reply(request, kind)).unwrap_or_else(|_| br#"{"v":1,"id":0,"kind":"error","revision":0}"#.to_vec());
    (header, payload)
}

/// The payload of replies that carry none (`pong`, `ack`).
const EMPTY_BODY: &[u8] = b"{}";

fn error(code: ErrorCode, message: String) -> Outcome {
    Err((code, message))
}

/// `cancel` is acknowledged, never preemptive: messages run one at a time (§3.1).
fn cancel(payload: &[u8]) -> Outcome {
    proto::decode_payload::<proto::CancelRequest>(payload).map_err(|e| (ErrorCode::BadRequest, e.to_string()))?;
    Ok((Kind::Ack, EMPTY_BODY.to_vec()))
}

#[cfg(target_arch = "wasm32")]
fn serve() {
    use wasm_bindgen::JsCast as _;

    let scope: web_sys::DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
    let handler_scope = scope.clone();
    let mut worker = ViewerWorker::new();
    let onmessage = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
        let (header, payload, oversized) = unpack(&event.data());
        let (rheader, rpayload) = match oversized {
            Some((env, got, limit)) => reply(&env, error(ErrorCode::TooLarge, format!("message is {got} bytes, over the {limit}-byte limit"))),
            None => worker.handle_message(&header, &payload),
        };
        post(&handler_scope, &rheader, &rpayload);
    });
    scope.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    // Lives as long as the worker does.
    onmessage.forget();
    // Announce that requests can be taken (`id` 0: it answers no request).
    let env = Envelope { v: proto::PROTOCOL_VERSION, id: 0, kind: Kind::Ready, revision: 0 };
    let header = proto::encode_envelope(&env).unwrap_or_else(|_| b"{}".to_vec());
    post(&scope, &header, &[]);
}

/// The `[headerJson, payloadBuffer]` pair a message carries; a missing or wrong shape unpacks
/// to empty bytes, which `handle_message` answers with a typed `error`. Anything else (not an
/// array, a non-string header, a payload that is not bytes) must never throw out of
/// `onmessage` and go unanswered.
///
/// The payload is copied, once, into this module only after its length is checked against the
/// request kind's [`proto::request_payload_limit`]: an oversized one comes back as
/// `Some((env, got, limit))` with no copy, and `serve` answers `TooLarge`.
#[cfg(target_arch = "wasm32")]
fn unpack(data: &wasm_bindgen::JsValue) -> (Vec<u8>, Vec<u8>, Option<(Envelope, usize, usize)>) {
    use wasm_bindgen::JsCast as _;

    if !js_sys::Array::is_array(data) {
        return (Vec::new(), Vec::new(), None);
    }
    let arr = js_sys::Array::from(data);
    // The header's length is read from the JS string before it is copied: a string of hundreds
    // of megabytes must cost one reply, not an allocation this module may not survive.
    let first = arr.get(0);
    let header_str = match first.dyn_ref::<js_sys::JsString>() {
        Some(s) if s.length() as usize <= proto::MAX_PAYLOAD_BYTES => String::from(s),
        _ => String::new(),
    };
    let header = if header_str.len() > proto::MAX_PAYLOAD_BYTES { Vec::new() } else { header_str.into_bytes() };
    // A `Uint8Array` (what the page posts) is used as it is: `new Uint8Array(typedArray)` would
    // copy every byte (ECMA-262 InitializeTypedArrayFromTypedArray clones the buffer), before the
    // length check. An `ArrayBuffer` gets a view, which copies nothing.
    let view = match arr.get(1).dyn_into::<js_sys::Uint8Array>() {
        Ok(view) => view,
        Err(other) if other.is_instance_of::<js_sys::ArrayBuffer>() => js_sys::Uint8Array::new(&other),
        Err(_) => return (header, Vec::new(), None),
    };
    let got = view.length() as usize;
    let Some(env) = proto::decode_envelope(&header).ok().filter(|e| e.kind.is_request()) else {
        return (header, Vec::new(), None);
    };
    let limit = proto::request_payload_limit(env.kind);
    if got > limit {
        return (header, Vec::new(), Some((env, got, limit)));
    }
    (header, view.to_vec(), None)
}

/// Post `(headerJson, payloadBuffer)` back to the page, transferring the payload buffer: a
/// page's pixels are copied once, out of this module's memory, and never again.
#[cfg(target_arch = "wasm32")]
fn post(scope: &web_sys::DedicatedWorkerGlobalScope, header: &[u8], payload: &[u8]) {
    let Ok(header_str) = std::str::from_utf8(header) else {
        return;
    };
    let bytes = js_sys::Uint8Array::from(payload);
    let msg = js_sys::Array::of2(&wasm_bindgen::JsValue::from_str(header_str), &bytes);
    let transfer = js_sys::Array::of1(&bytes.buffer());
    if let Err(e) = scope.post_message_with_transfer(&msg, &transfer) {
        // Nowhere to report to: the channel itself is broken. The wasm-bindgen logger shows it.
        web_sys::console::error_1(&e);
    }
}

fn main() {
    #[cfg(target_arch = "wasm32")]
    serve();
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("wordcraft-worker-viewer is a wasm32 dedicated worker; it is built by Trunk from apps/wordcraft-web/index.html");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `.docx` long enough for several pages, the way a host sends one.
    fn sample_docx() -> Vec<u8> {
        let text: String = (1..=140).map(|n| format!("Paragraph {n} of the Alder Street studio handbook.\n")).collect();
        wordcraft_docx::write(&wordcraft_doc::Document::from_text(&text)).expect("docx write")
    }

    fn header(kind: Kind, id: u32) -> Vec<u8> {
        proto::encode_envelope(&Envelope::request(id, kind, 0)).unwrap()
    }

    fn open(worker: &mut ViewerWorker, docx: &[u8], password: Option<&str>, id: u32) -> (Envelope, Vec<u8>) {
        let payload =
            proto::encode_view_open_request(&proto::ViewOpenRequest { password: password.map(str::to_string), docx: docx.to_vec() }).unwrap();
        let (rh, rp) = worker.handle_message(&header(Kind::ViewOpen, id), &payload);
        (proto::decode_envelope(&rh).unwrap(), rp)
    }

    fn render(worker: &mut ViewerWorker, page: u32, scale: f32, id: u32) -> (Envelope, Vec<u8>) {
        let payload = proto::encode_payload(&proto::ViewRenderRequest { page, scale }).unwrap();
        let (rh, rp) = worker.handle_message(&header(Kind::ViewRender, id), &payload);
        (proto::decode_envelope(&rh).unwrap(), rp)
    }

    fn error_code(env: &Envelope, payload: &[u8]) -> ErrorCode {
        assert_eq!(env.kind, Kind::Error, "expected an error reply");
        serde_json::from_slice::<proto::ErrorReply>(payload).unwrap().code
    }

    /// The acceptance test of this crate: what the worker answers for open and render is what
    /// the view-only core returns when called directly, byte for byte.
    #[test]
    fn open_and_render_through_the_worker_equal_the_core() {
        let docx = sample_docx();
        let direct = wordcraft_viewer::open(&docx, None).unwrap();
        assert!(direct.page_count() >= 3, "the sample should run to several pages, has {}", direct.page_count());

        let mut worker = ViewerWorker::new();
        let (env, payload) = open(&mut worker, &docx, None, 1);
        assert_eq!((env.kind, env.id), (Kind::ViewOpened, 1));
        let opened: proto::ViewOpenedReply = proto::decode_payload(&payload).unwrap();
        assert_eq!(proto::view_opened_reply_error(&opened), None);
        assert_eq!(opened.page_count as usize, direct.page_count());
        assert_eq!(opened.sizes.len(), direct.page_count());
        for (i, size) in opened.sizes.iter().enumerate() {
            let want = direct.page_size(i as u32).unwrap();
            assert_eq!((size.width, size.height), (want.width, want.height), "page {i}");
        }

        for (id, (page, scale)) in [(0u32, 1.0f32), (0, 1.5), (2, 0.75), (opened.page_count - 1, 2.0)].into_iter().enumerate() {
            let want = direct.render(page, scale).unwrap();
            let (env, payload) = render(&mut worker, page, scale, 10 + id as u32);
            assert_eq!((env.kind, env.id), (Kind::ViewPage, 10 + id as u32));
            let (head, pixels) = proto::split_view_page(&payload).unwrap();
            assert_eq!(head, proto::ViewPageHeader { page, scale, width: want.width, height: want.height });
            assert!(pixels == want.pixels.as_slice(), "page {page} at scale {scale}: the worker's pixels differ from the core's");
            assert!(pixels.as_chunks::<4>().0.iter().any(|px| px[..3] != [255, 255, 255]), "page {page} is blank");
        }
    }

    #[test]
    fn a_new_open_replaces_the_document() {
        let mut worker = ViewerWorker::new();
        let (env, payload) = open(&mut worker, &sample_docx(), None, 1);
        assert_eq!(env.kind, Kind::ViewOpened);
        let long: proto::ViewOpenedReply = proto::decode_payload(&payload).unwrap();
        let short_docx = wordcraft_docx::write(&wordcraft_doc::Document::from_text("One line.")).unwrap();
        let (env, payload) = open(&mut worker, &short_docx, None, 2);
        assert_eq!(env.kind, Kind::ViewOpened);
        let short: proto::ViewOpenedReply = proto::decode_payload(&payload).unwrap();
        assert_eq!(short.page_count, 1);
        assert!(long.page_count > 1);
        // The old document's later pages are gone with it.
        let (env, payload) = render(&mut worker, 1, 1.0, 3);
        assert_eq!(error_code(&env, &payload), ErrorCode::BadRequest);
        // A failed open leaves the document that was open in place.
        let (env, payload) = open(&mut worker, b"not a package", None, 4);
        assert_eq!(error_code(&env, &payload), ErrorCode::Unreadable);
        assert_eq!(render(&mut worker, 0, 1.0, 5).0.kind, Kind::ViewPage);
    }

    /// The repository's JetBrains Mono under `family` (the same length as its own name), so the
    /// family is one no system and no other test has.
    fn font_named(family: &str) -> Vec<u8> {
        const ORIGINAL: &str = "JetBrains Mono";
        assert_eq!(family.len(), ORIGINAL.len());
        let mut data = std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts/JetBrainsMono-Regular.ttf")).unwrap();
        let utf16 = |s: &str| s.encode_utf16().flat_map(u16::to_be_bytes).collect::<Vec<u8>>();
        for (from, to) in [(utf16(ORIGINAL), utf16(family)), (ORIGINAL.as_bytes().to_vec(), family.as_bytes().to_vec())] {
            let mut i = 0;
            while let Some(at) = data[i..].windows(from.len()).position(|w| w == from) {
                data[i + at..i + at + to.len()].copy_from_slice(&to);
                i += at + to.len();
            }
        }
        data
    }

    /// A `.docx` whose text is set in `family`, embedding `font` for it (a plain `.ttf`, the
    /// all-zero key) or not.
    fn docx_in(family: &str, font: Option<&[u8]>) -> Vec<u8> {
        const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
        const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
        const PR: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
        let rels = |items: &[(&str, &str)]| {
            let list: String = items.iter().map(|(t, g)| format!(r#"<Relationship Id="rId{t}" Type="{R}/{t}" Target="{g}"/>"#)).collect();
            format!(r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="{PR}">{list}</Relationships>"#)
        };
        let mut files: Vec<(String, Vec<u8>)> = vec![
            (
                "[Content_Types].xml".into(),
                br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="ttf" ContentType="application/x-font-ttf"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/fontTable.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.fontTable+xml"/></Types>"#.to_vec(),
            ),
            ("_rels/.rels".into(), rels(&[("officeDocument", "word/document.xml")]).into_bytes()),
            ("word/_rels/document.xml.rels".into(), rels(&[("fontTable", "fontTable.xml")]).into_bytes()),
            (
                "word/document.xml".into(),
                format!(r#"<w:document xmlns:w="{W}"><w:body><w:p><w:r><w:rPr><w:rFonts w:ascii="{family}" w:hAnsi="{family}"/></w:rPr><w:t>Set in {family}</w:t></w:r></w:p></w:body></w:document>"#).into_bytes(),
            ),
        ];
        let mut table = format!(r#"<w:fonts xmlns:w="{W}" xmlns:r="{R}"><w:font w:name="{family}">"#);
        if let Some(font) = font {
            table.push_str(r#"<w:embedRegular r:id="rIdfont" w:fontKey="{00000000-0000-0000-0000-000000000000}" w:subsetted="0"/>"#);
            files.push(("word/_rels/fontTable.xml.rels".into(), rels(&[("font", "fonts/font1.ttf")]).into_bytes()));
            files.push(("word/fonts/font1.ttf".into(), font.to_vec()));
        }
        table.push_str("</w:font></w:fonts>");
        files.push(("word/fontTable.xml".into(), table.into_bytes()));
        let mut zip = craft_zip::Writer::new();
        for (name, bytes) in files {
            zip.add(&name, &bytes, craft_zip::Compression::Deflated).unwrap();
        }
        zip.finish().unwrap()
    }

    fn missing(worker: &mut ViewerWorker, id: u32) -> Vec<String> {
        let (rh, rp) = worker.handle_message(&header(Kind::ViewMissingFonts, id), b"{}");
        assert_eq!(proto::decode_envelope(&rh).unwrap().kind, Kind::ViewFonts, "{}", String::from_utf8_lossy(&rp));
        serde_json::from_slice::<proto::ViewFontsReply>(&rp).unwrap().missing
    }

    #[test]
    fn a_documents_embedded_fonts_do_not_outlive_it() {
        const FAMILY: &str = "JetBrains Wkr1";
        let embedding = docx_in(FAMILY, Some(&font_named(FAMILY)));
        let naming = docx_in(FAMILY, None);
        let mut worker = ViewerWorker::new();
        assert_eq!(open(&mut worker, &embedding, None, 1).0.kind, Kind::ViewOpened);
        assert_eq!(missing(&mut worker, 2), Vec::<String>::new(), "it embeds the font it uses");
        // A replacement that cannot be opened leaves the document shown with its fonts.
        assert_eq!(open(&mut worker, b"not a package", None, 3).0.kind, Kind::Error);
        assert_eq!(missing(&mut worker, 4), Vec::<String>::new(), "the shown document keeps its font");
        // The next document only names the family: the first one's font is gone with it.
        assert_eq!(open(&mut worker, &naming, None, 5).0.kind, Kind::ViewOpened);
        assert_eq!(missing(&mut worker, 6), vec![FAMILY.to_string()], "another document's embedded font does not answer for this one");
        // Opened again, the first document brings its font again.
        assert_eq!(open(&mut worker, &embedding, None, 7).0.kind, Kind::ViewOpened);
        assert_eq!(missing(&mut worker, 8), Vec::<String>::new());
    }

    #[test]
    fn a_font_the_host_installs_stays_when_the_document_that_embedded_it_goes() {
        const FAMILY: &str = "JetBrains Wkr2";
        let font = font_named(FAMILY);
        let mut worker = ViewerWorker::new();
        assert_eq!(open(&mut worker, &docx_in(FAMILY, Some(&font)), None, 1).0.kind, Kind::ViewOpened);
        // The host installs the same font by that name: already there, so nothing new is added.
        let payload = proto::encode_view_install_font_request(&proto::ViewInstallFontRequest { name: FAMILY.into(), bytes: font.clone() }).unwrap();
        let (rh, rp) = worker.handle_message(&header(Kind::ViewInstallFont, 2), &payload);
        assert_eq!(proto::decode_envelope(&rh).unwrap().kind, Kind::ViewOpened, "{}", String::from_utf8_lossy(&rp));
        assert_eq!(open(&mut worker, &docx_in(FAMILY, None), None, 3).0.kind, Kind::ViewOpened);
        assert_eq!(missing(&mut worker, 4), Vec::<String>::new(), "the host's font is still installed");
    }

    #[test]
    fn a_document_kept_because_its_replacement_could_not_be_answered_keeps_its_fonts() {
        const FAMILY: &str = "JetBrains Wkr4";
        const OTHER: &str = "JetBrains Wkr5";
        let mut worker = ViewerWorker::new();
        assert_eq!(open(&mut worker, &docx_in(FAMILY, Some(&font_named(FAMILY))), None, 1).0.kind, Kind::ViewOpened);
        // The replacement opens, but its reply cannot be made: the worker keeps the first.
        let payload =
            proto::encode_view_open_request(&proto::ViewOpenRequest { password: None, docx: docx_in(OTHER, Some(&font_named(OTHER))) }).unwrap();
        let refused = worker.open_answering(&payload, |_| Err((ErrorCode::Internal, "refused by the test".into())));
        assert!(refused.is_err());
        // Laid out again (a font arrives), it is still set in its own font.
        let payload = proto::encode_view_install_font_request(&proto::ViewInstallFontRequest {
            name: "JetBrains Wkr6".into(),
            bytes: font_named("JetBrains Wkr6"),
        })
        .unwrap();
        let (rh, rp) = worker.handle_message(&header(Kind::ViewInstallFont, 2), &payload);
        assert_eq!(proto::decode_envelope(&rh).unwrap().kind, Kind::ViewOpened, "{}", String::from_utf8_lossy(&rp));
        assert_eq!(missing(&mut worker, 2), Vec::<String>::new(), "the document kept still has its embedded font");
        // And the replacement's font did not stay behind it.
        assert_eq!(open(&mut worker, &docx_in(OTHER, None), None, 3).0.kind, Kind::ViewOpened);
        assert_eq!(missing(&mut worker, 4), vec![OTHER.to_string()], "the refused replacement's font is gone with it");
    }

    #[test]
    fn a_font_the_host_installs_does_not_keep_the_documents_own_faces_of_that_family() {
        // The document embeds the family's Regular; the host installs its Bold under that name.
        // Once another document takes its place, the Regular it embedded goes with it: only
        // the host's Bold is left of the family.
        const FAMILY: &str = "JetBrains Wkr7";
        let mut worker = ViewerWorker::new();
        assert_eq!(open(&mut worker, &docx_in(FAMILY, Some(&font_named(FAMILY))), None, 1).0.kind, Kind::ViewOpened);
        let bold = std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts/SourceSans3-Bold.ttf")).unwrap();
        let payload = proto::encode_view_install_font_request(&proto::ViewInstallFontRequest { name: FAMILY.into(), bytes: bold }).unwrap();
        let (rh, rp) = worker.handle_message(&header(Kind::ViewInstallFont, 2), &payload);
        assert_eq!(proto::decode_envelope(&rh).unwrap().kind, Kind::ViewOpened, "{}", String::from_utf8_lossy(&rp));
        assert_eq!(wordcraft_fonts::FontDb::global().styles(FAMILY), ["Regular", "Bold"]);
        assert_eq!(open(&mut worker, &docx_in(FAMILY, None), None, 3).0.kind, Kind::ViewOpened);
        assert_eq!(wordcraft_fonts::FontDb::global().styles(FAMILY), ["Bold"], "the document's own face left with it");
    }

    #[test]
    fn opening_documents_again_does_not_spend_the_embedded_font_budget() {
        const FAMILY: &str = "JetBrains Wkr8";
        const HOSTED: &str = "JetBrains Wkr9";
        const LAST: &str = "JetBrains WkrA";
        let font = font_named(FAMILY);
        // More opens than the budget would take if each paid for the font again.
        let opens = u32::try_from(wordcraft_viewer::MAX_EMBEDDED_FONT_BYTES / font.len() + 2).unwrap();
        let mut worker = ViewerWorker::new();
        let embedding = docx_in(FAMILY, Some(&font));
        for id in 0..opens {
            assert_eq!(open(&mut worker, &embedding, None, id).0.kind, Kind::ViewOpened);
        }
        assert_eq!(missing(&mut worker, 0), Vec::<String>::new(), "the same document keeps its font however often it is opened");
        // Documents embedding a font the host installed first: the embedded copy is never
        // added, so it costs nothing either.
        let payload =
            proto::encode_view_install_font_request(&proto::ViewInstallFontRequest { name: HOSTED.into(), bytes: font_named(HOSTED) }).unwrap();
        let (rh, rp) = worker.handle_message(&header(Kind::ViewInstallFont, 0), &payload);
        assert_eq!(proto::decode_envelope(&rh).unwrap().kind, Kind::ViewOpened, "{}", String::from_utf8_lossy(&rp));
        let hosted = docx_in(HOSTED, Some(&font_named(HOSTED)));
        for id in 0..opens {
            assert_eq!(open(&mut worker, &hosted, None, id).0.kind, Kind::ViewOpened);
        }
        // So a document with a font of its own still gets it.
        assert_eq!(open(&mut worker, &docx_in(LAST, Some(&font_named(LAST))), None, 0).0.kind, Kind::ViewOpened);
        assert_eq!(missing(&mut worker, 0), Vec::<String>::new(), "a new document's own font is still loaded");
    }

    /// `font`'s bytes with four more after them: another file of the same family and style.
    fn other_bytes(font: &[u8]) -> Vec<u8> {
        let mut data = font.to_vec();
        data.extend_from_slice(&[0; 4]);
        data
    }

    #[test]
    fn a_replacement_kept_before_anything_opened_keeps_the_shown_documents_fonts() {
        const FAMILY: &str = "JetBrains WkrB";
        let shown = wordcraft_viewer::open(&docx_in(FAMILY, Some(&font_named(FAMILY))), None).unwrap();
        assert_eq!(wordcraft_fonts::FontDb::global().styles(FAMILY), ["Regular"]);
        wordcraft_viewer::Replacement::begin(Some(&shown)).keep();
        assert_eq!(wordcraft_fonts::FontDb::global().styles(FAMILY), ["Regular"], "nothing took its place: its font stays");
        // Kept after an open that failed: the same.
        let mut replacing = wordcraft_viewer::Replacement::begin(Some(&shown));
        assert!(replacing.open(b"not a package", None).is_err());
        replacing.keep();
        assert_eq!(wordcraft_fonts::FontDb::global().styles(FAMILY), ["Regular"]);
        assert_eq!(shown.missing_fonts(), Vec::<String>::new());
    }

    #[test]
    fn a_second_replacement_is_laid_out_with_its_own_fonts_not_the_firsts() {
        const FAMILY: &str = "JetBrains WkrC";
        let font = font_named(FAMILY);
        let mut replacing = wordcraft_viewer::Replacement::begin(None);
        let first = replacing.open(&docx_in(FAMILY, Some(&font)), None).unwrap();
        let second = replacing.open(&docx_in(FAMILY, Some(&other_bytes(&font))), None).unwrap();
        // The second embeds the same family and style in other bytes: its own face is added,
        // not skipped as the first's, so it is the second's still once the first has gone.
        assert_eq!(wordcraft_fonts::FontDb::global().styles(FAMILY), ["Regular"]);
        replacing.keep();
        drop(first);
        assert_eq!(wordcraft_fonts::FontDb::global().styles(FAMILY), ["Regular"], "the shown document's own face");
        assert_eq!(second.missing_fonts(), Vec::<String>::new());
        // One that cannot be opened after another did leaves the one opened in place.
        let mut replacing = wordcraft_viewer::Replacement::begin(Some(&second));
        let third = replacing.open(&docx_in(FAMILY, Some(&font)), None).unwrap();
        assert!(replacing.open(b"not a package", None).is_err());
        replacing.keep();
        drop(second);
        assert_eq!(wordcraft_fonts::FontDb::global().styles(FAMILY), ["Regular"], "the replacement opened before the failure is kept");
        assert_eq!(third.missing_fonts(), Vec::<String>::new());
    }

    #[test]
    fn a_documents_fonts_leave_with_the_last_session_showing_it() {
        // A server opens a document for each request and drops it after the answer, with no
        // replacement; two requests for the same document may be open at once.
        const FAMILY: &str = "JetBrains WkrD";
        let docx = docx_in(FAMILY, Some(&font_named(FAMILY)));
        let first = wordcraft_viewer::open(&docx, None).unwrap();
        let second = wordcraft_viewer::open(&docx, None).unwrap();
        assert_eq!(wordcraft_fonts::FontDb::global().styles(FAMILY), ["Regular"]);
        drop(first);
        assert_eq!(wordcraft_fonts::FontDb::global().styles(FAMILY), ["Regular"], "the other session still shows it");
        drop(second);
        assert!(wordcraft_fonts::FontDb::global().styles(FAMILY).is_empty(), "gone with the last session showing it");
        // A worker dropped with its document: the same.
        let mut worker = ViewerWorker::new();
        assert_eq!(open(&mut worker, &docx, None, 1).0.kind, Kind::ViewOpened);
        assert_eq!(wordcraft_fonts::FontDb::global().styles(FAMILY), ["Regular"]);
        drop(worker);
        assert!(wordcraft_fonts::FontDb::global().styles(FAMILY).is_empty());
    }

    #[test]
    fn the_protocol_bounds_are_the_cores() {
        // The page module sizes its requests by the protocol's copies, without linking the core.
        assert_eq!(proto::MAX_VIEW_SIDE, wordcraft_viewer::MAX_RASTER_SIDE);
        assert_eq!(proto::MAX_VIEW_SCALE, wordcraft_viewer::MAX_SCALE);
        assert_eq!(proto::MAX_VIEW_PIXELS, wordcraft_viewer::MAX_RENDER_PIXELS);
        assert_eq!(proto::MAX_VIEW_DOC_BYTES, wordcraft_viewer::MAX_DOCUMENT_BYTES);
    }

    #[test]
    fn requests_before_a_document_is_open_are_typed_errors() {
        let mut worker = ViewerWorker::new();
        let (env, payload) = render(&mut worker, 0, 1.0, 1);
        assert_eq!(error_code(&env, &payload), ErrorCode::NoDocument);
        let (rh, rp) = worker.handle_message(&header(Kind::ViewMissingFonts, 2), b"{}");
        assert_eq!(error_code(&proto::decode_envelope(&rh).unwrap(), &rp), ErrorCode::NoDocument);
    }

    #[test]
    fn bounds_and_junk_answer_typed_errors_without_panicking() {
        let mut worker = ViewerWorker::new();
        let docx = sample_docx();

        // Open: not a frame, an empty package, bytes that are not a package, a cut package.
        for (payload, want) in [
            (b"{}".to_vec(), ErrorCode::BadRequest),
            (proto::pack_binary(&[b"{}", b""]).unwrap(), ErrorCode::BadRequest),
            (proto::pack_binary(&[b"{}", b"not a package"]).unwrap(), ErrorCode::Unreadable),
            (proto::pack_binary(&[b"{}", &docx[..docx.len() / 2]]).unwrap(), ErrorCode::Unreadable),
            (proto::pack_binary(&[b"not json", &docx]).unwrap(), ErrorCode::BadRequest),
        ] {
            let (rh, rp) = worker.handle_message(&header(Kind::ViewOpen, 1), &payload);
            assert_eq!(error_code(&proto::decode_envelope(&rh).unwrap(), &rp), want);
        }
        let over = proto::pack_binary(&[b"{}", &vec![0u8; proto::MAX_VIEW_DOC_BYTES + 1]]).unwrap();
        let (rh, rp) = worker.handle_message(&header(Kind::ViewOpen, 2), &over);
        assert_eq!(error_code(&proto::decode_envelope(&rh).unwrap(), &rp), ErrorCode::TooLarge);

        assert_eq!(open(&mut worker, &docx, None, 3).0.kind, Kind::ViewOpened);

        // Render: junk JSON, junk scales, a page that does not exist, more pixels than the cap.
        for payload in [&b"not json"[..], br#"{"page":0,"scale":null}"#, br#"{"page":-1,"scale":1}"#, br#"{"scale":1}"#, b""] {
            let (rh, rp) = worker.handle_message(&header(Kind::ViewRender, 4), payload);
            assert_eq!(error_code(&proto::decode_envelope(&rh).unwrap(), &rp), ErrorCode::BadRequest, "{}", String::from_utf8_lossy(payload));
        }
        for scale in [0.0, -2.0, proto::MAX_VIEW_SCALE * 2.0, f32::MIN_POSITIVE] {
            let (env, payload) = render(&mut worker, 0, scale, 5);
            if scale == f32::MIN_POSITIVE {
                // A valid number: one pixel by one pixel, never a zero-sized page.
                assert_eq!(env.kind, Kind::ViewPage);
                assert_eq!(proto::split_view_page(&payload).unwrap().0.width, 1);
            } else {
                assert_eq!(error_code(&env, &payload), ErrorCode::BadRequest, "{scale}");
            }
        }
        let (env, payload) = render(&mut worker, u32::MAX, 1.0, 6);
        assert_eq!(error_code(&env, &payload), ErrorCode::BadRequest);
        let (env, payload) = render(&mut worker, 0, proto::MAX_VIEW_SCALE, 7);
        assert_eq!(error_code(&env, &payload), ErrorCode::TooLarge, "a Letter page at 16 pixels a point is 122 million pixels");

        // Fonts: not a frame, an empty name, bytes that are not a font.
        for (payload, want) in [
            (b"{}".to_vec(), ErrorCode::BadRequest),
            (proto::pack_binary(&[br#"{"name":""}"#, b"x"]).unwrap(), ErrorCode::BadRequest),
            (proto::pack_binary(&[br#"{"name":"Alder"}"#, b"not a font"]).unwrap(), ErrorCode::Unreadable),
        ] {
            let (rh, rp) = worker.handle_message(&header(Kind::ViewInstallFont, 8), &payload);
            assert_eq!(error_code(&proto::decode_envelope(&rh).unwrap(), &rp), want);
        }
        // The document survives all of it.
        assert_eq!(render(&mut worker, 0, 1.0, 9).0.kind, Kind::ViewPage);
    }

    #[test]
    fn an_encrypted_package_needs_its_password() {
        let locked = wordcraft_docx::encrypt_with_spin_count(&sample_docx(), "open sesame", 1000).unwrap();
        let mut worker = ViewerWorker::new();
        let (env, payload) = open(&mut worker, &locked, None, 1);
        assert_eq!(error_code(&env, &payload), ErrorCode::PasswordRequired);
        let (env, payload) = open(&mut worker, &locked, Some("guess"), 2);
        assert_eq!(error_code(&env, &payload), ErrorCode::WrongPassword);
        let (env, _) = open(&mut worker, &locked, Some("open sesame"), 3);
        assert_eq!(env.kind, Kind::ViewOpened);
    }

    #[test]
    fn missing_fonts_are_listed_for_the_open_document() {
        let mut worker = ViewerWorker::new();
        assert_eq!(open(&mut worker, &sample_docx(), None, 1).0.kind, Kind::ViewOpened);
        let (rh, rp) = worker.handle_message(&header(Kind::ViewMissingFonts, 2), b"{}");
        assert_eq!(proto::decode_envelope(&rh).unwrap().kind, Kind::ViewFonts);
        let listed: proto::ViewFontsReply = proto::decode_payload(&rp).unwrap();
        let direct = wordcraft_viewer::open(&sample_docx(), None).unwrap().missing_fonts();
        assert_eq!(listed.missing, direct);
    }

    #[test]
    fn other_workers_kinds_and_the_envelope_rules() {
        let mut worker = ViewerWorker::new();
        for kind in [Kind::Check, Kind::Configure, Kind::Convert, Kind::RenderPdf] {
            let (rh, rp) = worker.handle_message(&header(kind, 4), b"{}");
            assert_eq!(error_code(&proto::decode_envelope(&rh).unwrap(), &rp), ErrorCode::UnknownKind, "{kind:?}");
        }
        // A reply kind sent as a request.
        let (rh, rp) = worker.handle_message(&header(Kind::ViewPage, 5), b"");
        assert_eq!(error_code(&proto::decode_envelope(&rh).unwrap(), &rp), ErrorCode::UnknownKind);
        assert_eq!(proto::decode_envelope(&worker.handle_message(&header(Kind::Ping, 6), b"{}").0).unwrap().kind, Kind::Pong);
        let cancel = proto::encode_payload(&proto::CancelRequest { id: 99 }).unwrap();
        assert_eq!(proto::decode_envelope(&worker.handle_message(&header(Kind::Cancel, 7), &cancel).0).unwrap().kind, Kind::Ack);

        // An undecodable header answers with id 0; a version mismatch keeps the request's id.
        let (rh, _) = worker.handle_message(b"not json", &[]);
        assert_eq!(proto::decode_envelope(&rh).unwrap().id, 0);
        let mut env = Envelope::request(11, Kind::ViewRender, 3);
        env.v = proto::PROTOCOL_VERSION + 1;
        let (rh, rp) = worker.handle_message(&proto::encode_envelope(&env).unwrap(), &[]);
        let renv = proto::decode_envelope(&rh).unwrap();
        assert_eq!((renv.id, renv.revision), (11, 3));
        assert_eq!(error_code(&renv, &rp), ErrorCode::UnsupportedVersion);
    }
}
