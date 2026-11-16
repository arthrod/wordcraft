//! The codec dedicated worker (deferred-items plan §3.3): conversions between `.docx` and the
//! formats the browser build leaves out — HTML, RTF, ODT, LaTeX, legacy `.doc`, flat OPC XML —
//! off the main thread, on the [`wordcraft_worker_proto`] contract.
//!
//! Built by Trunk as `data-type="worker"` + `data-bindgen-target="web"` from
//! `apps/wordcraft-web/index.html`; the generated `wordcraft-worker-codecs_loader.js` is what
//! `new Worker(..., { type: "module" })` loads. Everything here is message-in/message-out:
//! requests arrive as `[headerJson, payloadArrayBuffer]` on `onmessage` and answers leave the
//! same way, the payload buffer transferred. Every failure is a typed `error` reply — this code
//! never panics, so a hostile or malformed message costs one reply, never the worker.
//!
//! Interchange is `.docx` bytes both ways (§3.3): both sides already read and write them, so a
//! `convert` job is foreign bytes in, a Word package out (`import`), or a Word package in,
//! foreign bytes out (`export`). The document model never crosses. A `password` decrypts an
//! encrypted input and encrypts the output where the output format supports it (Word packages,
//! ODT), the same rule the engine's `save_bytes_with` applies; the missing-password and
//! wrong-password replies carry the `.docx` reader's texts, so a client's `needs_password` /
//! `wrong_password` substring checks keep working.
//!
//! Cancellation is cooperative and only applies *between* jobs (§3.1): a `cancel` message can't
//! preempt a running conversion, it is answered after the job like everything else. To stop a
//! long job the caller terminates the worker. Conversions are also bounded: the protocol caps
//! the input at [`wordcraft_worker_proto::MAX_CONVERT_BYTES`] (64 MiB, well under the 512 MiB
//! host open cap — the bytes are copied into the worker and the result copies back out), and
//! every reader here treats its input as hostile.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use wordcraft_doc::Document;
use wordcraft_worker_proto as proto;
use wordcraft_worker_proto::{ConvertDirection, ConvertRequest, Envelope, ErrorCode, Kind};

/// Handle one wire message: `(headerJson, payloadBytes)` in, `(headerJson, payloadBytes)` out.
/// Always answers — a message that can't be decoded gets an `error` reply with id 0.
pub fn handle_message(header: &[u8], payload: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let env = match proto::decode_envelope(header) {
        Ok(env) => env,
        Err(proto::ProtoError::Version { got, .. }) => {
            // Enough of the envelope parsed to know `v`; recover `id`/`revision` leniently so
            // the client can still match the error to its request.
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
        Kind::Convert => convert(payload),
        // This worker converts; it does not proof or render.
        Kind::Check | Kind::Configure | Kind::RenderPdf => error(ErrorCode::UnknownKind, "`convert` is the only job this worker runs".into()),
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

/// `cancel` is acknowledged, never preemptive: messages run one at a time, so the request it
/// names has already been answered or is next in line (§3.1).
fn cancel(payload: &[u8]) -> Result<(Kind, Vec<u8>), (ErrorCode, String)> {
    proto::decode_payload::<proto::CancelRequest>(payload).map_err(|e| (ErrorCode::BadRequest, e.to_string()))?;
    Ok((Kind::Ack, EMPTY_BODY.to_vec()))
}

/// One conversion: the binary-framed [`ConvertRequest`] in, the converted bytes out as the raw
/// `converted` payload (the envelope's `kind` already says what the bytes are).
fn convert(payload: &[u8]) -> Result<(Kind, Vec<u8>), (ErrorCode, String)> {
    let req = proto::decode_convert_request(payload).map_err(|e| {
        let code = if matches!(e, proto::ProtoError::TooLarge { .. }) { ErrorCode::TooLarge } else { ErrorCode::BadRequest };
        (code, e.to_string())
    })?;
    if let Some(code) = proto::convert_request_error(&req) {
        return error(code, format!("convert request failed validation ({code:?})"));
    }
    let out = run_convert(&req).map_err(|message| (ErrorCode::Internal, message))?;
    Ok((Kind::Converted, out))
}

/// The conversion itself. `import` answers a Word package; `export` takes one. The format name
/// is normalized the way the format crates normalize extensions (no dot, any case).
fn run_convert(req: &ConvertRequest) -> Result<Vec<u8>, String> {
    let format = req.format.trim_start_matches('.').to_ascii_lowercase();
    let password = req.password.as_deref();
    match req.direction {
        ConvertDirection::Import => {
            let mut doc = import_document(&format, &req.input, password)?;
            doc.ensure_nonempty();
            let package = wordcraft_docx::write(&doc).map_err(|e| e.to_string())?;
            encrypt_word_package(package, password)
        }
        ConvertDirection::Export => {
            let doc = wordcraft_docx::read_with_password(&req.input, password).map_err(|e| e.to_string())?;
            export_document(&format, &doc, password)
        }
    }
}

/// The page a file that gives no page size opens on: a new blank document's (the engine opens
/// such files on the session's paper; a worker has no session, so the default it is).
fn page() -> wordcraft_doc::SectionProps {
    wordcraft_doc::SectionProps::default()
}

/// Read `input` in the format `ext` names (import direction). Password errors carry the
/// `.docx` reader's texts so the client's `needs_password` / `wrong_password` checks match.
fn import_document(ext: &str, input: &[u8], password: Option<&str>) -> Result<Document, String> {
    // An encrypted Word package under any Word extension (the extension can lie): [MS-OFFCRYPTO]
    // agile or Standard encryption, decrypted before the package parses.
    if matches!(ext, "docx" | "docm" | "dotx" | "dotm" | "doc" | "dot") && wordcraft_docx::is_encrypted(input) {
        return wordcraft_docx::read_with_password(input, password).map_err(|e| e.to_string());
    }
    match ext {
        "docx" | "docm" | "dotx" | "dotm" => wordcraft_docx::read_with_password(input, password).map_err(|e| e.to_string()),
        // Word XML Document (Flat OPC) or Word 2003 XML, told apart by the root element.
        "xml" => wordcraft_docx::read_xml(input).map_err(|e| e.to_string()),
        // Word 97–2003 binary, decrypting XOR, RC4 and RC4 CryptoAPI when protected.
        "doc" | "dot" => wordcraft_docbin::read_with_password(input, password).map_err(|e| match e {
            wordcraft_docbin::DocbinError::PasswordRequired => wordcraft_docx::PASSWORD_REQUIRED.to_string(),
            wordcraft_docbin::DocbinError::WrongPassword => wordcraft_docx::WRONG_PASSWORD.to_string(),
            other => other.to_string(),
        }),
        // An encrypted ODF package (ODF 1.3 Part 2 §3.4) decrypts before the zip parses.
        "odt" | "ott" if wordcraft_formats::odt::is_encrypted(input) => {
            let pw = password.ok_or_else(|| format!("ODT: {}", wordcraft_docx::PASSWORD_REQUIRED))?;
            let plain = wordcraft_formats::odt::decrypt(input, pw).map_err(|e| e.to_string())?;
            wordcraft_formats::odt::import_on(&plain, &page())
        }
        // HTML, RTF, ODT, LaTeX…: the format crate's readers, on the default page.
        other => {
            wordcraft_formats::import_on(other, input, &page()).unwrap_or_else(|| Err(format!("`.{other}` is not a format this worker converts")))
        }
    }
}

/// Write `doc` in the format `ext` names (export direction), encrypting where the format
/// supports it and a password is given.
fn export_document(ext: &str, doc: &Document, password: Option<&str>) -> Result<Vec<u8>, String> {
    // ODT encrypts as it is packaged; the other formats below write plain bytes.
    if matches!(ext, "odt" | "ott") {
        let plain = if ext == "odt" { wordcraft_formats::odt::export(doc) } else { wordcraft_formats::odt::export_template(doc) }?;
        return match password {
            Some(pw) => wordcraft_formats::odt::encrypt(&plain, pw).map_err(|e| e.to_string()),
            None => Ok(plain),
        };
    }
    if let Some(flavor) = wordcraft_docx::Flavor::from_ext(ext) {
        // .docx/.docm/.dotx/.dotm: the extension picks the package flavour Word expects, and a
        // password encrypts the package ([MS-OFFCRYPTO] agile), like the engine's
        // `save_bytes_with`. Every other format's bytes go out unchanged — encrypting RTF or
        // HTML would hand back bytes no reader of that format can open.
        let package = wordcraft_docx::write_as(doc, flavor).map_err(|e| e.to_string())?;
        return encrypt_word_package(package, password);
    }
    match ext {
        // Flat OPC (the engine's `xml` without the Word 2003 option, which the protocol
        // carries no option for).
        "xml" => wordcraft_docx::write_flat(doc).map_err(|e| e.to_string()),
        other => wordcraft_formats::export_with(other, doc, &wordcraft_formats::ExportOptions::default())
            .unwrap_or_else(|| Err(format!("`.{other}` is not a format this worker converts"))),
    }
}

/// A Word package written with a password is saved encrypted ([MS-OFFCRYPTO] agile), the way the
/// engine's `save_bytes_with` keeps a document protected; without one it is a plain copy.
fn encrypt_word_package(package: Vec<u8>, password: Option<&str>) -> Result<Vec<u8>, String> {
    match password {
        Some(pw) => wordcraft_docx::encrypt(&package, pw).map_err(|e| e.to_string()),
        None => Ok(package),
    }
}

#[cfg(target_arch = "wasm32")]
fn serve() {
    use wasm_bindgen::JsCast as _;

    let scope: web_sys::DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
    let handler_scope = scope.clone();
    let onmessage = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
        let (header, payload, oversized) = unpack(&event.data());
        let (rheader, rpayload) = match oversized {
            Some((env, got, limit)) => reply(&env, error(ErrorCode::TooLarge, format!("message is {got} bytes, over the {limit}-byte limit"))),
            None => handle_message(&header, &payload),
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

/// The `[headerJson, payloadBuffer]` pair a message carries (crate docs). Anything else —
/// not an array, a non-string header, a payload that is not bytes — unpacks to empty parts,
/// which `handle_message` answers with a typed `error`: a malformed message must never throw
/// out of `onmessage` and go unanswered.
///
/// The payload is copied only after its length is checked against the request kind's
/// [`proto::request_payload_limit`]: an oversized one comes back as `Some((env, got, limit))`
/// with no copy, and `serve` answers `TooLarge`. A header or payload the handler never reads
/// (an undecodable header, a reply kind sent as a request) is not copied at all.
#[cfg(target_arch = "wasm32")]
fn unpack(data: &wasm_bindgen::JsValue) -> (Vec<u8>, Vec<u8>, Option<(Envelope, usize, usize)>) {
    use wasm_bindgen::JsCast as _;

    if !js_sys::Array::is_array(data) {
        return (Vec::new(), Vec::new(), None);
    }
    let arr = js_sys::Array::from(data);
    let header_str = arr.get(0).as_string().unwrap_or_default();
    // `decode_envelope` applies `MAX_PAYLOAD_BYTES` itself; an over-limit header gets the same
    // `BadRequest` an empty one does, without paying for the copy first.
    let header = if header_str.len() > proto::MAX_PAYLOAD_BYTES { Vec::new() } else { header_str.into_bytes() };
    let payload = arr.get(1);
    if !payload.is_instance_of::<js_sys::ArrayBuffer>() && !payload.is_instance_of::<js_sys::Uint8Array>() {
        return (header, Vec::new(), None);
    }
    let view = js_sys::Uint8Array::new(&payload);
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
    eprintln!("wordcraft-worker-codecs is a wasm32 dedicated worker; it is built by Trunk from apps/wordcraft-web/index.html");
}

#[cfg(test)]
mod tests {
    use super::*;
    use wordcraft_worker_proto::ConvertRequest;

    fn request(req: &ConvertRequest, id: u32) -> (Vec<u8>, Vec<u8>) {
        let env = Envelope::request(id, Kind::Convert, 77);
        (proto::encode_envelope(&env).unwrap(), proto::encode_convert_request(req).unwrap())
    }

    fn convert_reply(req: &ConvertRequest) -> Result<Vec<u8>, (ErrorCode, String)> {
        let (h, p) = request(req, 1);
        let (rh, rp) = handle_message(&h, &p);
        let env = proto::decode_envelope(&rh).unwrap();
        assert_eq!((env.id, env.revision), (1, 77), "replies echo the request id and revision");
        if env.kind == Kind::Error {
            let body: proto::ErrorReply = serde_json::from_slice(&rp).unwrap();
            return Err((body.code, body.message));
        }
        assert_eq!(env.kind, Kind::Converted);
        Ok(rp)
    }

    fn sample_doc() -> Document {
        let md = b"# Studio\n\nSome **bold** text.";
        wordcraft_formats::import("md", md).expect("md").expect("valid md")
    }

    fn sample_docx() -> Vec<u8> {
        wordcraft_docx::write(&sample_doc()).expect("docx")
    }

    #[test]
    fn import_rtf_answers_a_word_package() {
        let rtf = wordcraft_formats::export("rtf", &sample_doc()).expect("rtf").expect("ok");
        let req = ConvertRequest { format: "rtf".into(), direction: ConvertDirection::Import, password: None, input: rtf };
        let out = convert_reply(&req).expect("conversion");
        assert_eq!(&out[..2], b"PK", "import answers a Word package");
        let doc = wordcraft_docx::read_with_password(&out, None).expect("the package reads back");
        assert!(String::from_utf8_lossy(&wordcraft_formats::txt::export(&doc)).contains("bold"), "the text survived the round trip");
    }

    #[test]
    fn export_docx_answers_the_named_format() {
        for (format, needle) in [("md", "bold"), ("rtf", "\\rtf1"), ("html", "<p>")] {
            let req = ConvertRequest { format: format.into(), direction: ConvertDirection::Export, password: None, input: sample_docx() };
            let out = convert_reply(&req).unwrap_or_else(|e| panic!("{format}: {e:?}"));
            let text = String::from_utf8_lossy(&out);
            assert!(text.contains(needle), "{format}: {text}");
        }
    }

    #[test]
    fn export_to_rtf_is_not_encrypted() {
        // A password decrypts the input and must not wrap the *output* unless the output is a
        // Word package: RTF bytes come back as RTF, not an encrypted Office package.
        let locked = wordcraft_docx::encrypt(&sample_docx(), "open-sesame").expect("encrypts");
        let req = ConvertRequest { format: "rtf".into(), direction: ConvertDirection::Export, password: Some("open-sesame".into()), input: locked };
        let out = convert_reply(&req).expect("rtf export");
        assert!(out.starts_with(b"{\\rtf1"), "rtf export came back encrypted or garbled: {:?}", &out[..out.len().min(16)]);
    }

    #[test]
    fn docx_flavor_change_round_trips() {
        let req = ConvertRequest { format: "dotm".into(), direction: ConvertDirection::Export, password: None, input: sample_docx() };
        let flavored = convert_reply(&req).expect("flavor change");
        let req = ConvertRequest { format: "dotm".into(), direction: ConvertDirection::Import, password: None, input: flavored };
        let out = convert_reply(&req).expect("flavor restore");
        let doc = wordcraft_docx::read_with_password(&out, None).expect("reads back as docx");
        assert!(String::from_utf8_lossy(&wordcraft_formats::txt::export(&doc)).contains("bold"));
    }

    #[test]
    fn encrypted_docx_round_trips_through_the_password() {
        let plain = sample_docx();
        let locked = wordcraft_docx::encrypt(&plain, "open-sesame").expect("encrypts");

        // Without the password: the .docx reader's missing-password text, so a client's
        // `needs_password` substring check matches.
        let req = ConvertRequest { format: "docx".into(), direction: ConvertDirection::Import, password: None, input: locked.clone() };
        let err = convert_reply(&req).expect_err("needs a password");
        assert_eq!(err.0, ErrorCode::Internal);
        assert!(err.1.contains(wordcraft_docx::PASSWORD_REQUIRED), "{:?}", err.1);

        // With it: an output package that is itself encrypted (kept protected, like the engine's
        // `save_bytes_with`), and a wrong password says so.
        let req = ConvertRequest {
            format: "docx".into(),
            direction: ConvertDirection::Import,
            password: Some("open-sesame".into()),
            input: locked.clone(),
        };
        let out = convert_reply(&req).expect("converts");
        assert!(wordcraft_docx::is_encrypted(&out), "the output stays encrypted");
        let doc = wordcraft_docx::read_with_password(&out, Some("open-sesame")).expect("reads back");
        assert!(String::from_utf8_lossy(&wordcraft_formats::txt::export(&doc)).contains("bold"));

        let req = ConvertRequest { format: "docx".into(), direction: ConvertDirection::Import, password: Some("nope".into()), input: locked };
        let err = convert_reply(&req).expect_err("wrong password");
        assert!(err.1.contains(wordcraft_docx::WRONG_PASSWORD), "{:?}", err.1);
    }

    #[test]
    fn unknown_and_broken_requests_answer_typed_errors() {
        // A format no reader handles.
        let req = ConvertRequest { format: "zzz".into(), direction: ConvertDirection::Import, password: None, input: b"x".to_vec() };
        let err = convert_reply(&req).expect_err("unknown format");
        assert_eq!(err.0, ErrorCode::Internal);
        assert!(err.1.contains("not a format"), "{:?}", err.1);

        // Junk bytes where a Word package should be.
        let req = ConvertRequest { format: "docx".into(), direction: ConvertDirection::Export, password: None, input: b"not a package".to_vec() };
        let err = convert_reply(&req).expect_err("junk input");
        assert_eq!(err.0, ErrorCode::Internal);

        // Bounds: a format name and a password past their limits.
        let base = ConvertRequest { format: "rtf".into(), direction: ConvertDirection::Import, password: None, input: b"{\\rtf1}".to_vec() };
        for req in [
            ConvertRequest { format: "x".repeat(proto::MAX_FORMAT_BYTES + 1), ..base.clone() },
            ConvertRequest { password: Some("p".repeat(proto::MAX_PASSWORD_BYTES + 1)), ..base },
        ] {
            let err = convert_reply(&req).expect_err("too large");
            assert_eq!(err.0, ErrorCode::TooLarge);
        }

        // Not a binary frame at all.
        let env = Envelope::request(9, Kind::Convert, 1);
        let (h, p) = (proto::encode_envelope(&env).unwrap(), b"{}".to_vec());
        let (rh, rp) = handle_message(&h, &p);
        let renv = proto::decode_envelope(&rh).unwrap();
        assert_eq!(renv.kind, Kind::Error);
        let body: proto::ErrorReply = serde_json::from_slice(&rp).unwrap();
        assert_eq!(body.code, ErrorCode::BadRequest);
    }

    #[test]
    fn other_kinds_and_malformed_headers_answer_without_panicking() {
        // The proofing and PDF request kinds are not this worker's job.
        for kind in [Kind::Check, Kind::Configure, Kind::RenderPdf] {
            let env = Envelope::request(2, kind, 5);
            let (h, p) = proto::encode_message(&env, &serde_json::json!({})).unwrap();
            let (rh, rp) = handle_message(&h, &p);
            let renv = proto::decode_envelope(&rh).unwrap();
            assert_eq!(renv.kind, Kind::Error, "{kind:?}");
            let body: proto::ErrorReply = serde_json::from_slice(&rp).unwrap();
            assert_eq!(body.code, ErrorCode::UnknownKind, "{kind:?}");
        }

        // Ping, cancel, a reply kind as a request, an undecodable header, a version mismatch.
        let env = Envelope::request(3, Kind::Ping, 0);
        let (h, p) = proto::encode_message(&env, &serde_json::json!({})).unwrap();
        let (rh, _) = handle_message(&h, &p);
        assert_eq!(proto::decode_envelope(&rh).unwrap().kind, Kind::Pong);

        let env = Envelope::request(4, Kind::Cancel, 0);
        let (h, p) = proto::encode_message(&env, &proto::CancelRequest { id: 99 }).unwrap();
        let (rh, _) = handle_message(&h, &p);
        assert_eq!(proto::decode_envelope(&rh).unwrap().kind, Kind::Ack);

        let env = Envelope::request(5, Kind::Pong, 0);
        let (h, p) = proto::encode_message(&env, &serde_json::json!({})).unwrap();
        let (rh, _) = handle_message(&h, &p);
        assert_eq!(proto::decode_envelope(&rh).unwrap().kind, Kind::Error);

        let (rh, rp) = handle_message(b"not json", &[]);
        let renv = proto::decode_envelope(&rh).unwrap();
        assert_eq!((renv.kind, renv.id), (Kind::Error, 0));
        let body: proto::ErrorReply = serde_json::from_slice(&rp).unwrap();
        assert_eq!(body.code, ErrorCode::BadRequest);

        let mut env = Envelope::request(11, Kind::Ping, 3);
        env.v = proto::PROTOCOL_VERSION + 1;
        let h = proto::encode_envelope(&env).unwrap();
        let (rh, rp) = handle_message(&h, &[]);
        let renv = proto::decode_envelope(&rh).unwrap();
        assert_eq!((renv.kind, renv.id, renv.revision), (Kind::Error, 11, 3));
        let body: proto::ErrorReply = serde_json::from_slice(&rp).unwrap();
        assert_eq!(body.code, ErrorCode::UnsupportedVersion);
    }
}
