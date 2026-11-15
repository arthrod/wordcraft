//! The PDF export dedicated worker (deferred-items plan §3.4): lays a document out and writes
//! it as PDF off the main thread, on the [`wordcraft_worker_proto`] contract.
//!
//! Built by Trunk as `data-type="worker"` + `data-bindgen-target="web"` from
//! `apps/wordcraft-web/index.html`. Everything here is message-in/message-out: requests arrive
//! as `[headerJson, payloadArrayBuffer]` and answers leave the same way, the payload buffer
//! transferred. Every failure is a typed `error` reply — this code never panics, so a hostile
//! or malformed message costs one reply, never the worker.
//!
//! The document crosses as a `.docx` interchange package (§3.4), and the client sends the font
//! faces the main thread holds — the four bundled ones ride in the worker binary itself
//! (`minimal-wasm-fonts`), pack faces and user fonts arrive as copies in the request. A face a
//! copy names answers to that name once installed, the same rule a `.docx` file's embedded
//! fonts follow, so the layout here breaks lines where the main thread's layout does.
//!
//! `krilla` (through `craft-pdf`) is linked *only* here: it stays a forbidden crate for the
//! main module, which keeps `wordcraft-pdf` at a stub there.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use wordcraft_worker_proto as proto;
use wordcraft_worker_proto::{Envelope, ErrorCode, Kind};

/// Handle one wire message: `(headerJson, payloadBytes)` in, `(headerJson, payloadBytes)` out.
/// Always answers — a message that can't be decoded gets an `error` reply with id 0.
pub fn handle_message(header: &[u8], payload: &[u8]) -> (Vec<u8>, Vec<u8>) {
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
        Kind::RenderPdf => render_pdf(payload),
        // This worker renders PDFs; it does not proof or convert formats.
        Kind::Check | Kind::Configure | Kind::Convert => error(ErrorCode::UnknownKind, "`renderPdf` is the only job this worker runs".into()),
        // Unreachable: `is_request` was checked above.
        other => error(ErrorCode::UnknownKind, format!("`{other:?}` is a reply, not a request")),
    };
    reply(&env, outcome)
}

/// `id`/`revision` of a header that decoded but failed the version check, so the error reply
/// still matches the request it answers.
fn lenient_envelope(header: &[u8]) -> Envelope {
    let val: serde_json::Value = serde_json::from_slice(header).unwrap_or(serde_json::Value::Null);
    let num = |key: &str| val.get(key).and_then(serde_json::Value::as_u64).unwrap_or_default();
    Envelope { v: proto::PROTOCOL_VERSION, id: u32::try_from(num("id")).unwrap_or_default(), kind: Kind::Error, revision: num("revision") }
}

/// `(header, payload)` bytes for a reply to `env`'s request. A failed payload encode becomes an
/// `error` reply rather than silence (and a failed error encode falls back to a constant).
fn reply(request: &Envelope, outcome: Result<(Kind, Vec<u8>), (ErrorCode, String)>) -> (Vec<u8>, Vec<u8>) {
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

fn error(code: ErrorCode, message: String) -> Result<(Kind, Vec<u8>), (ErrorCode, String)> {
    Err((code, message))
}

/// `cancel` is acknowledged, never preemptive: messages run one at a time (§3.1).
fn cancel(payload: &[u8]) -> Result<(Kind, Vec<u8>), (ErrorCode, String)> {
    proto::decode_payload::<proto::CancelRequest>(payload).map_err(|e| (ErrorCode::BadRequest, e.to_string()))?;
    Ok((Kind::Ack, EMPTY_BODY.to_vec()))
}

/// One `renderPdf` job: install the font copies, read the `.docx` interchange package, lay it
/// out with the PDF export's own options and write the PDF. The reply payload is the raw file,
/// unframed (the envelope's `kind` already says what the bytes are).
fn render_pdf(payload: &[u8]) -> Result<(Kind, Vec<u8>), (ErrorCode, String)> {
    let req = proto::decode_pdf_request(payload).map_err(|e| {
        let code = if matches!(e, proto::ProtoError::TooLarge { .. }) { ErrorCode::TooLarge } else { ErrorCode::BadRequest };
        (code, e.to_string())
    })?;
    if let Some(code) = proto::pdf_request_error(&req) {
        return error(code, format!("renderPdf request failed validation ({code:?})"));
    }
    install_font_copies(&req.fonts)?;
    let doc = wordcraft_docx::read(&req.docx).map_err(|e| (ErrorCode::Internal, e.to_string()))?;
    let pdf = wordcraft_pdf::PdfOptions::default();
    let lay = wordcraft_layout::layout(&doc, &mut wordcraft_layout::LayoutCache::new(), &pdf.layout_options());
    let bytes = wordcraft_pdf::export_layout(&doc, &lay, &pdf).map_err(|e| (ErrorCode::Internal, e.to_string()))?;
    if bytes.is_empty() {
        return error(ErrorCode::Internal, "the export wrote no bytes".into());
    }
    Ok((Kind::Pdf, bytes))
}

/// Install the client's font copies the way a `.docx` file's embedded fonts are installed: the
/// file decoded (TTF/OTF/TTC/WOFF/WOFF2), then registered under the family the document
/// resolves it under. Faces already available stay as they are.
fn install_font_copies(fonts: &[proto::FontCopy]) -> Result<(), (ErrorCode, String)> {
    let files: Vec<(Vec<u8>, String)> =
        fonts.iter().filter_map(|f| wordcraft_fonts::decode_font_file(&f.bytes).ok().map(|data| (data, f.family.clone()))).collect();
    if !files.is_empty() {
        wordcraft_fonts::FontDb::global().add_document_fonts(files);
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn serve() {
    use wasm_bindgen::JsCast as _;

    let scope: web_sys::DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
    let handler_scope = scope.clone();
    let onmessage = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
        let (header, payload) = unpack(&event.data());
        let (rheader, rpayload) = handle_message(&header, &payload);
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
/// to empty bytes, which `handle_message` answers with a typed `error`. Anything else — not an
/// array, a non-string header, a payload that is not bytes — must never throw out of
/// `onmessage` and go unanswered.
#[cfg(target_arch = "wasm32")]
fn unpack(data: &wasm_bindgen::JsValue) -> (Vec<u8>, Vec<u8>) {
    use wasm_bindgen::JsCast as _;

    if !js_sys::Array::is_array(data) {
        return (Vec::new(), Vec::new());
    }
    let arr = js_sys::Array::from(data);
    let header = arr.get(0).as_string().unwrap_or_default().into_bytes();
    let payload = arr.get(1);
    let payload = if payload.is_instance_of::<js_sys::ArrayBuffer>() || payload.is_instance_of::<js_sys::Uint8Array>() {
        js_sys::Uint8Array::new(&payload).to_vec()
    } else {
        Vec::new()
    };
    (header, payload)
}

/// Post `(headerJson, payloadBuffer)` back to the main thread, transferring the payload buffer.
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
    eprintln!("wordcraft-worker-pdf is a wasm32 dedicated worker; it is built by Trunk from apps/wordcraft-web/index.html");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `.docx` interchange package holding simple text, the way a client sends one.
    fn sample_docx() -> Vec<u8> {
        wordcraft_docx::write(&wordcraft_doc::Document::from_text("# Studio\n\nSome bold text.")).expect("docx write")
    }

    fn request(req: &wordcraft_worker_proto::PdfRenderRequest, id: u32) -> (Vec<u8>, Vec<u8>) {
        let env = Envelope::request(id, Kind::RenderPdf, 5);
        (proto::encode_envelope(&env).unwrap(), proto::encode_pdf_request(req).unwrap())
    }

    #[test]
    fn render_pdf_answers_a_pdf_for_a_document_package() {
        let req = wordcraft_worker_proto::PdfRenderRequest { docx: sample_docx(), fonts: Vec::new() };
        let (h, p) = request(&req, 1);
        let (rh, rp) = handle_message(&h, &p);
        let env = proto::decode_envelope(&rh).unwrap();
        assert_eq!((env.id, env.revision), (1, 5));
        assert_eq!(env.kind, Kind::Pdf, "body starts with {:?} instead of %PDF", &rp[..rp.len().min(8)]);
        assert!(rp.starts_with(b"%PDF"), "not a PDF");
        assert!(rp.len() > 1_000, "suspiciously small PDF: {} bytes", rp.len());
    }

    #[test]
    fn bounds_and_junk_answer_typed_errors_without_panicking() {
        // Not a binary frame.
        let env = Envelope::request(9, Kind::RenderPdf, 1);
        let (h, p) = (proto::encode_envelope(&env).unwrap(), b"{}".to_vec());
        let (rh, rp) = handle_message(&h, &p);
        assert_eq!(proto::decode_envelope(&rh).unwrap().kind, Kind::Error);
        let body: proto::ErrorReply = serde_json::from_slice(&rp).unwrap();
        assert_eq!(body.code, ErrorCode::BadRequest);

        // An empty package is a bad request; a blob that is not a package is an internal
        // error — both typed, never a panic.
        let req = wordcraft_worker_proto::PdfRenderRequest { docx: Vec::new(), fonts: Vec::new() };
        let (h, p) = request(&req, 2);
        let (_rh, rp) = handle_message(&h, &p);
        let body: proto::ErrorReply = serde_json::from_slice(&rp).unwrap();
        assert_eq!(body.code, ErrorCode::BadRequest);

        let req = wordcraft_worker_proto::PdfRenderRequest { docx: b"not a package".to_vec(), fonts: Vec::new() };
        let (h, p) = request(&req, 3);
        let (rh, rp) = handle_message(&h, &p);
        assert_eq!(proto::decode_envelope(&rh).unwrap().kind, Kind::Error);
        let body: proto::ErrorReply = serde_json::from_slice(&rp).unwrap();
        assert_eq!(body.code, ErrorCode::Internal);

        // The proofing and codec request kinds are not this worker's job; ping and cancel are.
        for kind in [Kind::Check, Kind::Configure, Kind::Convert] {
            let env = Envelope::request(4, kind, 0);
            let (h, p) = proto::encode_message(&env, &serde_json::json!({})).unwrap();
            let (rh, rp) = handle_message(&h, &p);
            assert_eq!(proto::decode_envelope(&rh).unwrap().kind, Kind::Error, "{kind:?}");
            let body: proto::ErrorReply = serde_json::from_slice(&rp).unwrap();
            assert_eq!(body.code, ErrorCode::UnknownKind, "{kind:?}");
        }
        let env = Envelope::request(6, Kind::Ping, 0);
        let (h, p) = proto::encode_message(&env, &serde_json::json!({})).unwrap();
        assert_eq!(proto::decode_envelope(&handle_message(&h, &p).0).unwrap().kind, Kind::Pong);
        let env = Envelope::request(7, Kind::Cancel, 0);
        let (h, p) = proto::encode_message(&env, &proto::CancelRequest { id: 99 }).unwrap();
        assert_eq!(proto::decode_envelope(&handle_message(&h, &p).0).unwrap().kind, Kind::Ack);

        // An undecodable header and a version mismatch keep the request's identity.
        let (rh, _) = handle_message(b"not json", &[]);
        assert_eq!(proto::decode_envelope(&rh).unwrap().id, 0);
        let mut env = Envelope::request(11, Kind::Ping, 3);
        env.v = proto::PROTOCOL_VERSION + 1;
        let h = proto::encode_envelope(&env).unwrap();
        let (rh, rp) = handle_message(&h, &[]);
        let renv = proto::decode_envelope(&rh).unwrap();
        assert_eq!((renv.id, renv.revision), (11, 3));
        let body: proto::ErrorReply = serde_json::from_slice(&rp).unwrap();
        assert_eq!(body.code, ErrorCode::UnsupportedVersion);
    }

    /// The browser hangs reproducibly on the real sample document (a native render of the same
    /// bytes answers in milliseconds): pin that exact input.
    #[test]
    fn render_pdf_answers_for_the_real_sample_document() {
        let Ok(docx) = std::fs::read("/tmp/sample.docx") else {
            eprintln!("skipped: /tmp/sample.docx not present (wordcraft-cli run --template sample --save /tmp/sample.docx)");
            return;
        };
        let req = wordcraft_worker_proto::PdfRenderRequest { docx, fonts: Vec::new() };
        let (h, p) = request(&req, 21);
        let t0 = std::time::Instant::now();
        let (rh, rp) = handle_message(&h, &p);
        let env = proto::decode_envelope(&rh).unwrap();
        assert_eq!(env.kind, Kind::Pdf, "body starts with {:?}", &rp[..rp.len().min(8)]);
        assert!(rp.starts_with(b"%PDF"));
        eprintln!("sample render: {} bytes in {:?}", rp.len(), t0.elapsed());
    }

    #[test]
    fn font_copies_install_without_panicking() {
        // Junk bytes are dropped by the decoder, never a panic; a real face is not available in
        // the test environment, so only the hostile path is exercised here.
        let fonts = vec![proto::FontCopy { family: "NotAFont".into(), style: "Regular".into(), bytes: vec![0, 1, 2, 3, 4] }];
        assert!(install_font_copies(&fonts).is_ok());
        assert!(install_font_copies(&[]).is_ok());
    }
}
