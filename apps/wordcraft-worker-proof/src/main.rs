//! The proofing dedicated worker (deferred-items plan §3.2): spelling and grammar checks off
//! the main thread, on the [`wordcraft_worker_proto`] contract.
//!
//! Built by Trunk as `data-type="worker"` + `data-bindgen-target="web"` from
//! `apps/wordcraft-web/index.html`; the generated `wordcraft-worker-proof_loader.js` is what
//! `new Worker(..., { type: "module" })` loads. Everything here is message-in/message-out:
//! requests arrive as `[headerJson, payloadArrayBuffer]` on `onmessage` and answers leave the
//! same way, the payload buffer transferred. Every failure is a typed `error` reply — this
//! code never panics, so a hostile or malformed message costs one reply, never the worker.
//!
//! Cancellation is cooperative and only applies *between* jobs (§3.1): a `cancel` message can't
//! preempt a running `check`, it is answered after the job like everything else. To stop a long
//! job the caller terminates the worker.
//!
//! Hyphenation is *not* here: it stays in the main module, synchronous inside line breaking
//! (F5). This worker is spelling and grammar only.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::cell::Cell;
use std::ops::Range;

use wordcraft_proof::{IssueKind, ProofLang};
use wordcraft_worker_proto as proto;
use wordcraft_worker_proto::{ByteRange, CheckReply, CheckRequest, Envelope, ErrorCode, ErrorReply, Issue, Kind};

// The options `check` requests without their own `options` run under ([`proto::ConfigureRequest`]).
thread_local! {
    static CONFIGURED_OPTIONS: Cell<Option<proto::ProofOptions>> = const { Cell::new(None) };
}

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
        Kind::Configure => configure(payload),
        Kind::Check => check(payload),
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
        Err((code, message)) => (Kind::Error, proto::encode_payload(&ErrorReply { code, message }).unwrap_or_else(|_| b"{}".to_vec())),
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

/// Apply a `configure` message: default options for later checks plus the user/ignore
/// dictionary deltas the client has accepted since the worker started.
fn configure(payload: &[u8]) -> Result<(Kind, Vec<u8>), (ErrorCode, String)> {
    let cfg = proto::decode_payload::<proto::ConfigureRequest>(payload).map_err(|e| (ErrorCode::BadRequest, e.to_string()))?;
    let words = cfg.add_words.len().saturating_add(cfg.remove_words.len()).saturating_add(cfg.ignore_words.len());
    if words > proto::MAX_DICTIONARY_WORDS
        || cfg.add_words.iter().chain(&cfg.remove_words).chain(&cfg.ignore_words).any(|w| w.len() > proto::MAX_WORD_BYTES)
    {
        return error(ErrorCode::TooLarge, format!("{words} dictionary words in one configure message"));
    }
    // `None` keeps the current options; `Some(ProofOptions::default())` resets to Word's.
    if let Some(opts) = cfg.options {
        CONFIGURED_OPTIONS.with(|o| o.set(Some(opts)));
    }
    for w in &cfg.add_words {
        if let Some(word) = wordcraft_proof::custom_word(w) {
            wordcraft_proof::add_word(&word);
        }
    }
    for w in &cfg.remove_words {
        if let Some(word) = wordcraft_proof::custom_word(w) {
            wordcraft_proof::remove_word(&word);
        }
    }
    for w in &cfg.ignore_words {
        if let Some(word) = wordcraft_proof::custom_word(w) {
            wordcraft_proof::ignore_word(&word);
        }
    }
    Ok((Kind::Ack, EMPTY_BODY.to_vec()))
}

fn check(payload: &[u8]) -> Result<(Kind, Vec<u8>), (ErrorCode, String)> {
    let req = proto::decode_payload::<CheckRequest>(payload).map_err(|e| (ErrorCode::BadRequest, e.to_string()))?;
    if let Some(code) = proto::check_request_error(&req) {
        return error(code, format!("check request failed validation ({code:?})"));
    }
    let rep = run_check(&req);
    proto::encode_payload(&rep).map(|p| (Kind::Issues, p)).map_err(|e| (ErrorCode::Internal, e.to_string()))
}

/// The configured default options (Word's until a `configure` message replaces them).
fn options() -> proto::ProofOptions {
    CONFIGURED_OPTIONS.with(|o| o.get()).unwrap_or_default()
}

/// Run [`wordcraft_proof::check_text_with`] the way layout's `paragraph_issues` does, then
/// apply its two overlays: nothing overlapping an unchecked (`skip`) range, and no *spelling*
/// issue overlapping the word being typed (grammar issues stay — `#1552`).
fn run_check(req: &CheckRequest) -> CheckReply {
    let spans: Vec<(Range<usize>, ProofLang)> =
        req.spans.iter().map(|s| (s.start as usize..s.end as usize, ProofLang::from_tag(Some(s.tag.as_str())))).collect();
    let opts = req.options.unwrap_or_else(options);
    let spelling = wordcraft_proof::Options {
        ignore_uppercase: opts.ignore_uppercase,
        ignore_numbers: opts.ignore_numbers,
        ignore_internet: opts.ignore_internet,
    };
    let mut skipped: Vec<ByteRange> = req.skip.clone();
    skipped.sort_unstable_by_key(|r| (r.start, r.end));
    let issues = wordcraft_proof::check_text_with(&req.text, &spans, &spelling, opts.grammar, req.list_item);
    CheckReply {
        para_id: req.para_id,
        issues: issues
            .into_iter()
            .filter(|i| !overlaps(&skipped, i.start, i.end))
            .filter(|i| i.kind == IssueKind::Grammar || req.typing.is_none_or(|t| !(i.start < t.end as usize && (t.start as usize) < i.end)))
            .take(proto::MAX_ISSUES)
            .map(|i| Issue {
                start: u32::try_from(i.start).unwrap_or(u32::MAX),
                end: u32::try_from(i.end).unwrap_or(u32::MAX),
                kind: match i.kind {
                    IssueKind::Spelling => proto::IssueKind::Spelling,
                    IssueKind::Grammar => proto::IssueKind::Grammar,
                },
                message: i.message,
                suggestions: i.suggestions,
                lang: i.lang.tag().into_owned(),
            })
            .collect(),
    }
}

/// Whether `a..b` overlaps one of `skipped` (sorted): the first range ending after `a` is the
/// only one that can — the same binary-search rule layout's `overlaps` uses.
fn overlaps(skipped: &[ByteRange], a: usize, b: usize) -> bool {
    let k = skipped.partition_point(|r| r.end as usize <= a);
    skipped.get(k).is_some_and(|r| (r.start as usize) < b)
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

/// The `[headerJson, payloadBuffer]` pair a message carries (crate docs); a missing or wrong
/// shape unpacks to empty bytes, which `handle_message` answers with a typed `error`.
#[cfg(target_arch = "wasm32")]
fn unpack(data: &wasm_bindgen::JsValue) -> (Vec<u8>, Vec<u8>) {
    let arr = js_sys::Array::from(data);
    let header = arr.get(0).as_string().unwrap_or_default().into_bytes();
    let payload = js_sys::Uint8Array::new(&arr.get(1)).to_vec();
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
    eprintln!("wordcraft-worker-proof is a wasm32 dedicated worker; it is built by Trunk from apps/wordcraft-web/index.html");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(kind: Kind, payload: &impl serde::Serialize, id: u32) -> (Vec<u8>, Vec<u8>) {
        proto::encode_message(&Envelope::request(id, kind, 77), payload).unwrap()
    }

    fn decode_reply(h: &[u8], p: &[u8]) -> (Envelope, serde_json::Value) {
        (proto::decode_envelope(h).unwrap(), serde_json::from_slice(p).unwrap_or(serde_json::Value::Null))
    }

    #[test]
    fn check_returns_issues_for_the_request_revision_and_para() {
        let req = CheckRequest {
            para_id: 9,
            text: "Ths is a tst. This this is fine.".into(),
            spans: vec![proto::LangSpan { start: 0, end: 31, tag: "en-US".into() }],
            skip: Vec::new(),
            options: None,
            typing: None,
            list_item: false,
        };
        let (h, p) = request(Kind::Check, &req, 5);
        let (rh, rp) = handle_message(&h, &p);
        let (env, body) = decode_reply(&rh, &rp);
        assert_eq!(env.kind, Kind::Issues);
        assert_eq!(env.id, 5);
        assert_eq!(env.revision, 77, "replies echo the request revision so stale ones drop");
        let reply: CheckReply = serde_json::from_value(body).unwrap();
        assert_eq!(reply.para_id, 9);
        assert!(!reply.issues.is_empty(), "misspellings should be flagged");
        assert!(reply.issues.iter().all(|i| i.start < i.end));
    }

    #[test]
    fn skip_ranges_and_typing_are_filtered_like_inline_proofing() {
        // "recieve" is misspelled; inside a skip range it produces no issue.
        let text = "recieve and an apple";
        let req = CheckRequest {
            para_id: 1,
            text: text.into(),
            spans: Vec::new(),
            skip: vec![ByteRange { start: 0, end: 7 }],
            options: None,
            typing: None,
            list_item: false,
        };
        let (h, p) = request(Kind::Check, &req, 1);
        let (rh, rp) = handle_message(&h, &p);
        let (_, body) = decode_reply(&rh, &rp);
        let reply: CheckReply = serde_json::from_value(body).unwrap();
        assert!(reply.issues.iter().all(|i| i.start >= 7), "{:?}", reply.issues);

        // A spelling issue under the typing range is dropped; a grammar issue is not.
        let text = "recieve had had";
        let base = CheckRequest { para_id: 1, text: text.into(), spans: Vec::new(), skip: Vec::new(), options: None, typing: None, list_item: false };
        let (h, p) = request(Kind::Check, &base, 1);
        let (rh, rp) = handle_message(&h, &p);
        let before: CheckReply = serde_json::from_value(decode_reply(&rh, &rp).1).unwrap();
        let (h, p) = request(Kind::Check, &CheckRequest { typing: Some(ByteRange { start: 0, end: 7 }), ..base.clone() }, 1);
        let (rh, rp) = handle_message(&h, &p);
        let after: CheckReply = serde_json::from_value(decode_reply(&rh, &rp).1).unwrap();
        assert!(after.issues.iter().all(|i| i.kind != proto::IssueKind::Spelling || i.start >= 7));
        assert_eq!(
            after.issues.iter().filter(|i| i.kind == proto::IssueKind::Grammar).count(),
            before.issues.iter().filter(|i| i.kind == proto::IssueKind::Grammar).count()
        );
    }

    #[test]
    fn malformed_messages_get_typed_error_replies() {
        // Undecodable header.
        let (rh, rp) = handle_message(b"not json", &[]);
        let (env, body) = decode_reply(&rh, &rp);
        assert_eq!(env.kind, Kind::Error);
        assert_eq!(body["code"], "badRequest");

        // Valid envelope, undecodable payload.
        let (h, _) = proto::encode_message(&Envelope::request(3, Kind::Check, 8), &serde_json::json!({})).unwrap();
        let (rh, rp) = handle_message(&h, b"{broken");
        let (env, body) = decode_reply(&rh, &rp);
        assert_eq!((env.kind, env.id, env.revision), (Kind::Error, 3, 8));
        assert_eq!(body["code"], "badRequest");

        // A reply kind sent as a request.
        let (h, p) = proto::encode_message(&Envelope::request(4, Kind::Issues, 0), &serde_json::json!({})).unwrap();
        let (rh, rp) = handle_message(&h, &p);
        let (env, body) = decode_reply(&rh, &rp);
        assert_eq!(env.kind, Kind::Error);
        assert_eq!(body["code"], "unknownKind");

        // Oversized text.
        let big = CheckRequest {
            para_id: 0,
            text: "x".repeat(proto::MAX_TEXT_BYTES + 1),
            spans: Vec::new(),
            skip: Vec::new(),
            options: None,
            typing: None,
            list_item: false,
        };
        let (h, p) = request(Kind::Check, &big, 6);
        let (rh, rp) = handle_message(&h, &p);
        let (env, body) = decode_reply(&rh, &rp);
        assert_eq!(env.kind, Kind::Error);
        assert_eq!(body["code"], "tooLarge");
    }

    #[test]
    fn version_mismatch_gets_a_typed_reply_with_the_requests_id() {
        let mut env = Envelope::request(11, Kind::Ping, 3);
        env.v = proto::PROTOCOL_VERSION + 1;
        let h = proto::encode_envelope(&env).unwrap();
        let (rh, rp) = handle_message(&h, &[]);
        let (renv, body) = decode_reply(&rh, &rp);
        assert_eq!(renv.kind, Kind::Error);
        assert_eq!((renv.id, renv.revision), (11, 3));
        assert_eq!(body["code"], "unsupportedVersion");
    }

    #[test]
    fn ping_cancel_configure_answer_without_panicking() {
        let (h, p) = request(Kind::Ping, &serde_json::json!({}), 1);
        let (rh, rp) = handle_message(&h, &p);
        assert_eq!(decode_reply(&rh, &rp).0.kind, Kind::Pong);

        let (h, p) = request(Kind::Cancel, &proto::CancelRequest { id: 999 }, 2);
        let (rh, rp) = handle_message(&h, &p);
        assert_eq!(decode_reply(&rh, &rp).0.kind, Kind::Ack);

        let cfg = proto::ConfigureRequest {
            options: Some(proto::ProofOptions { grammar: false, ..Default::default() }),
            add_words: vec!["qwzxy".into()],
            remove_words: Vec::new(),
            ignore_words: Vec::new(),
        };
        let (h, p) = request(Kind::Configure, &cfg, 3);
        let (rh, rp) = handle_message(&h, &p);
        assert_eq!(decode_reply(&rh, &rp).0.kind, Kind::Ack);
        assert!(wordcraft_proof::is_correct("qwzxy"), "the configure added the user word");
    }
}
