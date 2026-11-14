//! The WordCraft web worker contract (deferred-items plan §3.1): one wire protocol for the
//! dedicated workers (`apps/wordcraft-worker-proof`, `-codecs`, `-pdf`) and the clients that
//! drive them.
//!
//! A message on the wire is a two-element JavaScript array `[header, payload]`:
//!
//! - `header` is a JSON-encoded [`Envelope`] (`{v, id, kind, revision}`). It stays small: every
//!   other field a request or reply needs travels in the payload.
//! - `payload` is an `ArrayBuffer`, transferred (never copied, never embedded in the JSON as
//!   base64). Its contents depend on `kind`: request and reply payloads are the JSON-encoded
//!   types below; binary kinds (codec input, rendered output) carry raw bytes whose framing is
//!   the kind's own.
//!
//! `id` is a `u32` counter per worker ([`RequestIds`]), always below 2^53 so it crosses to
//! JavaScript and back as an exact number. `revision` is the document revision the request was
//! made for; replies echo it so a caller can drop results computed on text the user has already
//! edited. Squiggle spans are overlays keyed by `(revision, para_id)`, so a late or stale reply
//! is safe to ignore — it never moves layout.
//!
//! Cancellation is cooperative and only applies *between* jobs: a worker inside a synchronous
//! job cannot read a `cancel` message until the job returns, so cancelling a long job means
//! `worker.terminate()` and a fresh worker (§3.1). There is no shared memory: no
//! `SharedArrayBuffer`, no wasm threads, no COOP/COEP requirement.
//!
//! Every failure is a typed reply (`kind: "error"` with an [`ErrorReply`] payload), never a
//! panic and never a dropped message.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// The protocol version in every envelope's `v` field. A peer on another version gets an
/// [`ErrorCode::UnsupportedVersion`] reply.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest payload a worker or client accepts, in bytes. Paragraph text is capped at
/// [`MAX_TEXT_BYTES`] and every other field is small, so 4 MiB is generous headroom for the
/// JSON kinds; binary kinds apply their own caps.
pub const MAX_PAYLOAD_BYTES: usize = 4 << 20;

/// Largest paragraph a single `check` job proofs, in bytes — the same bound layout applies
/// inline (`crates/layout/src/para/proof.rs`).
pub const MAX_TEXT_BYTES: usize = 100_000;
/// Most language spans in one `check` request (caps a hostile payload).
pub const MAX_SPANS: usize = 4096;
/// Most unchecked ranges in one `check` request (caps a hostile payload).
pub const MAX_SKIP_RANGES: usize = 4096;
/// Most issues a `check` reply carries (caps a pathological paragraph).
pub const MAX_ISSUES: usize = 4096;
/// Most words one `configure` message adds, removes or ignores (caps a hostile payload).
pub const MAX_DICTIONARY_WORDS: usize = 10_000;
/// Longest dictionary word accepted, in bytes (the proofing option caps at 64 characters;
/// bytes are the cheap bound here).
pub const MAX_WORD_BYTES: usize = 256;

/// Largest file the codec worker converts, in bytes (§3.3). Well below the 512 MiB open cap
/// in `apps/wordcraft-web/src/host.rs`: the input is copied into the worker once and the
/// converted document copies back out.
pub const MAX_CONVERT_BYTES: usize = 64 << 20;
/// Largest `.docx` interchange package the PDF worker renders, in bytes (§3.4).
pub const MAX_PDF_DOC_BYTES: usize = 64 << 20;
/// Most font copies one `renderPdf` request carries (the bundled four plus installed packs and
/// user fonts: a document can reference many, but not hundreds).
pub const MAX_FONT_COPIES: usize = 256;
/// Total font bytes one `renderPdf` request carries (user fonts cap at
/// `wordcraft_fonts::MAX_FONT_FILE_BYTES` each; this bounds the batch).
pub const MAX_FONT_TOTAL_BYTES: usize = 128 << 20;
/// Longest format name (`rtf`, `odt`, `docx`…) a `convert` request accepts, in bytes.
pub const MAX_FORMAT_BYTES: usize = 32;
/// Longest password a `convert` request carries, in bytes.
pub const MAX_PASSWORD_BYTES: usize = 256;
/// Most parts one binary-framed payload declares (framing sanity bound, see [`pack_binary`]).
pub const MAX_BINARY_PARTS: usize = 1024;

/// The `kind` field of an [`Envelope`]. Request kinds are `check`, `cancel`, `configure`,
/// `convert`, `renderPdf` and `ping`; everything else only ever travels worker → client. A
/// worker answers a request kind it doesn't implement with [`ErrorCode::UnknownKind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    /// Spell/grammar check one paragraph → [`Kind::Issues`].
    Check,
    /// Best-effort cancel of an earlier request id → [`Kind::Ack`]. Cooperative only: it can
    /// never preempt a running job (see the crate docs).
    Cancel,
    /// Update options and the user/ignore dictionaries → [`Kind::Ack`].
    Configure,
    /// Convert a document between a foreign format and `.docx` → [`Kind::Converted`].
    Convert,
    /// Lay out a `.docx` document and write it as PDF → [`Kind::Pdf`].
    RenderPdf,
    /// Liveness probe → [`Kind::Pong`].
    Ping,
    /// [`CheckRequest`] outcome ([`CheckReply`]).
    Issues,
    /// [`ConvertRequest`] outcome: raw converted bytes (`.docx` towards the client, the named
    /// format away from it).
    Converted,
    /// [`PdfRenderRequest`] outcome: the raw `.pdf` file.
    Pdf,
    /// `Cancel`/`Configure` accepted.
    Ack,
    /// `Ping` answer.
    Pong,
    /// Sent once, unprompted, when the worker's `main` has run and it can take requests.
    Ready,
    /// Any failure ([`ErrorReply`]).
    Error,
}

impl Kind {
    /// Whether this kind is a request (client → worker). The rest are worker → client only.
    pub fn is_request(self) -> bool {
        matches!(self, Kind::Check | Kind::Cancel | Kind::Configure | Kind::Convert | Kind::RenderPdf | Kind::Ping)
    }
}

/// The JSON header every message carries. Replies echo the request's `id` and `revision`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// [`PROTOCOL_VERSION`].
    pub v: u32,
    /// Request id; `0` on unsolicited notices (`ready`) and undecodable input.
    pub id: u32,
    pub kind: Kind,
    /// Document revision the request was made for. `0` on messages that carry none (`ping`,
    /// `ready`, `configure`); replies echo the request's value.
    #[serde(default)]
    pub revision: u64,
}

impl Envelope {
    /// A request envelope at the current version.
    pub fn request(id: u32, kind: Kind, revision: u64) -> Envelope {
        Envelope { v: PROTOCOL_VERSION, id, kind, revision }
    }

    /// The reply to `request` (same `id` and `revision`).
    pub fn reply(request: &Envelope, kind: Kind) -> Envelope {
        Envelope { v: PROTOCOL_VERSION, id: request.id, kind, revision: request.revision }
    }
}

/// A half-open byte range in a paragraph's text, `start < end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: u32,
    pub end: u32,
}

impl ByteRange {
    pub fn new(range: std::ops::Range<usize>) -> Option<ByteRange> {
        if range.start >= range.end {
            return None;
        }
        Some(ByteRange { start: u32::try_from(range.start).ok()?, end: u32::try_from(range.end).ok()? })
    }
}

/// The language of a byte range of `text`, as a BCP 47 tag (`en-US`, `pl`, …). Ranges are in
/// order and do not overlap; text outside them is checked in the default language.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LangSpan {
    pub start: u32,
    pub end: u32,
    pub tag: String,
}

/// The File › Options › Proofing choices a check runs under. Defaults match Word.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofOptions {
    /// Words in capitals (`NASA`) are taken as correct.
    #[serde(default = "yes")]
    pub ignore_uppercase: bool,
    /// Words containing digits (`B2B`) are taken as correct.
    #[serde(default = "yes")]
    pub ignore_numbers: bool,
    /// Web and email addresses and file paths are skipped.
    #[serde(default = "yes")]
    pub ignore_internet: bool,
    /// Check grammar as well as spelling.
    #[serde(default = "yes")]
    pub grammar: bool,
}

fn yes() -> bool {
    true
}

impl Default for ProofOptions {
    fn default() -> Self {
        ProofOptions { ignore_uppercase: true, ignore_numbers: true, ignore_internet: true, grammar: true }
    }
}

/// `check` payload: proof one paragraph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckRequest {
    /// The caller's identity for the paragraph (opaque to the worker); results key on
    /// `(revision, para_id)` — `revision` from the envelope, `para_id` from here.
    pub para_id: u64,
    /// The paragraph text as proofing reads it (layout's `proof_text`: inline objects and
    /// tracked deletions already replaced).
    pub text: String,
    /// Language of each byte range, in order.
    #[serde(default)]
    pub spans: Vec<LangSpan>,
    /// Byte ranges never checked ("Do not check spelling or grammar", hidden text, hyperlinks).
    #[serde(default)]
    pub skip: Vec<ByteRange>,
    /// Options for this check; `None` (or absent) means the worker's configured defaults
    /// ([`ConfigureRequest::options`], itself defaulting to Word's).
    #[serde(default)]
    pub options: Option<ProofOptions>,
    /// The word being typed: spelling issues overlapping it are dropped until the caret leaves,
    /// matching the inline check (`#1552`). Grammar issues are kept.
    #[serde(default)]
    pub typing: Option<ByteRange>,
    /// List items are often fragments, so the capital-first-word grammar rule is off for them.
    #[serde(default)]
    pub list_item: bool,
}

/// A proofing issue's seriousness: spelling (red squiggle) or grammar (the grammar colour).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IssueKind {
    Spelling,
    Grammar,
}

/// One squiggle span inside a checked paragraph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub start: u32,
    pub end: u32,
    pub kind: IssueKind,
    /// In the language of the text it was found in.
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub suggestions: Vec<String>,
    /// BCP 47 tag of the language the issue was checked in.
    #[serde(default)]
    pub lang: String,
}

/// `issues` reply payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CheckReply {
    /// Echo of the request's `para_id`; with the envelope's `revision` it keys the result.
    pub para_id: u64,
    #[serde(default)]
    pub issues: Vec<Issue>,
}

/// `cancel` payload: the request `id` to drop if it hasn't run yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelRequest {
    pub id: u32,
}

/// `configure` payload: option and dictionary changes for later checks. Dictionaries live
/// inside the worker (it is a separate heap from the main module), so the client replays the
/// deltas it has applied since the worker started.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfigureRequest {
    /// Replace the default options for later `check` requests (`None`: keep current). A
    /// request's own `options` always win.
    #[serde(default)]
    pub options: Option<ProofOptions>,
    /// Words to add to the user dictionary ("Add to Dictionary").
    #[serde(default)]
    pub add_words: Vec<String>,
    /// Words to remove from the user dictionary.
    #[serde(default)]
    pub remove_words: Vec<String>,
    /// Words to ignore for this session ("Ignore All"): never listed, saved or loaded.
    #[serde(default)]
    pub ignore_words: Vec<String>,
}

/// Why an `error` reply was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    /// The envelope or payload could not be decoded, failed validation or exceeded a bound.
    BadRequest,
    /// The envelope's `v` isn't [`PROTOCOL_VERSION`].
    UnsupportedVersion,
    /// A reply kind (`issues`, `ready`, `error`…) arrived as a request.
    UnknownKind,
    /// The request was cancelled before it ran.
    Cancelled,
    /// The input was valid JSON but too large for the worker ([`MAX_TEXT_BYTES`], payload caps).
    TooLarge,
    /// The job itself failed (a proofing error surfaced as data, never a panic).
    Internal,
}

/// `error` reply payload. The envelope carries the request's `id` and `revision` so the client
/// can still match and drop it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorReply {
    pub code: ErrorCode,
    #[serde(default)]
    pub message: String,
}

/// A failure of the protocol helpers themselves (encode/decode/validation), surfaced to callers
/// as a typed value so they can turn it into an [`ErrorReply`] or a client-side error.
#[derive(Debug)]
pub enum ProtoError {
    /// JSON decode failed.
    Decode(String),
    /// JSON encode failed (shouldn't happen for these types; kept so no path panics).
    Encode(String),
    /// `v` on the wire isn't [`PROTOCOL_VERSION`].
    Version { expected: u32, got: u32 },
    /// A byte budget was exceeded.
    TooLarge { limit: usize, got: usize },
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoError::Decode(e) => write!(f, "bad message: {e}"),
            ProtoError::Encode(e) => write!(f, "cannot encode message: {e}"),
            ProtoError::Version { expected, got } => write!(f, "protocol version {got}, expected {expected}"),
            ProtoError::TooLarge { limit, got } => write!(f, "message is {got} bytes, over the {limit}-byte limit"),
        }
    }
}

impl std::error::Error for ProtoError {}

/// Encode an envelope's JSON header (UTF-8 bytes).
pub fn encode_envelope(env: &Envelope) -> Result<Vec<u8>, ProtoError> {
    serde_json::to_vec(env).map_err(|e| ProtoError::Encode(e.to_string()))
}

/// Decode a JSON header. Version mismatches are [`ProtoError::Version`], not a generic decode
/// failure, so a peer can be told apart from a corrupt one.
pub fn decode_envelope(bytes: &[u8]) -> Result<Envelope, ProtoError> {
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Err(ProtoError::TooLarge { limit: MAX_PAYLOAD_BYTES, got: bytes.len() });
    }
    let env: Envelope = serde_json::from_slice(bytes).map_err(|e| ProtoError::Decode(e.to_string()))?;
    if env.v != PROTOCOL_VERSION {
        return Err(ProtoError::Version { expected: PROTOCOL_VERSION, got: env.v });
    }
    Ok(env)
}

/// Encode a typed payload (UTF-8 JSON) for transport beside the header.
pub fn encode_payload<T: Serialize>(payload: &T) -> Result<Vec<u8>, ProtoError> {
    serde_json::to_vec(payload).map_err(|e| ProtoError::Encode(e.to_string()))
}

/// Decode a typed payload. Over-size buffers are rejected before serde sees them.
pub fn decode_payload<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ProtoError> {
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Err(ProtoError::TooLarge { limit: MAX_PAYLOAD_BYTES, got: bytes.len() });
    }
    serde_json::from_slice(bytes).map_err(|e| ProtoError::Decode(e.to_string()))
}

/// The `(header, payload)` byte pair one `postMessage` carries, as `[headerJson, payloadBuffer]`.
pub fn encode_message<T: Serialize>(env: &Envelope, payload: &T) -> Result<(Vec<u8>, Vec<u8>), ProtoError> {
    Ok((encode_envelope(env)?, encode_payload(payload)?))
}

/// Per-worker request id source. `u32` keeps every id far below 2^53, the exact-integer limit
/// for a JavaScript number; ids start at 1 and wrap to 1, skipping `0` (the "no request" id).
#[derive(Debug, Default)]
pub struct RequestIds {
    next: u32,
}

impl RequestIds {
    pub fn new() -> RequestIds {
        RequestIds::default()
    }

    /// The next request id.
    pub fn next_id(&mut self) -> u32 {
        self.next = self.next.wrapping_add(1);
        if self.next == 0 {
            self.next = 1;
        }
        self.next
    }
}

/// Validate a [`CheckRequest`] against the protocol bounds before a worker runs it. Returns the
/// [`ErrorCode`] the reply should carry — `BadRequest` for malformed input, `TooLarge` past the
/// byte budgets — so validation stays the same wherever it runs.
pub fn check_request_error(req: &CheckRequest) -> Option<ErrorCode> {
    let text_len = req.text.len();
    if text_len > MAX_TEXT_BYTES || req.spans.len() > MAX_SPANS || req.skip.len() > MAX_SKIP_RANGES {
        return Some(ErrorCode::TooLarge);
    }
    if req.spans.iter().any(|s| s.start >= s.end || s.end as usize > text_len) {
        return Some(ErrorCode::BadRequest);
    }
    if req.skip.iter().any(|r| r.start >= r.end || r.end as usize > text_len) {
        return Some(ErrorCode::BadRequest);
    }
    if req.typing.is_some_and(|r| r.start >= r.end || r.end as usize > text_len) {
        return Some(ErrorCode::BadRequest);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_check() -> CheckRequest {
        CheckRequest {
            para_id: 42,
            text: "Ths is a tst.".into(),
            spans: vec![LangSpan { start: 0, end: 13, tag: "en-US".into() }],
            skip: vec![ByteRange { start: 4, end: 6 }],
            options: Some(ProofOptions::default()),
            typing: Some(ByteRange { start: 10, end: 13 }),
            list_item: true,
        }
    }

    #[test]
    fn envelope_roundtrip() {
        let env = Envelope::request(7, Kind::Check, 123_456);
        let bytes = encode_envelope(&env).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // The header is exactly the contract shape: {v, id, kind, revision}.
        assert_eq!(json["v"], PROTOCOL_VERSION);
        assert_eq!(json["id"], 7);
        assert_eq!(json["kind"], "check");
        assert_eq!(json["revision"], 123_456);
        assert_eq!(json.as_object().map(|o| o.len()), Some(4));
        assert_eq!(decode_envelope(&bytes).unwrap(), env);
    }

    #[test]
    fn envelope_rejects_other_versions() {
        let mut env = Envelope::request(1, Kind::Ping, 0);
        env.v = PROTOCOL_VERSION + 1;
        let err = decode_envelope(&encode_envelope(&env).unwrap()).unwrap_err();
        assert!(matches!(err, ProtoError::Version { got, .. } if got == PROTOCOL_VERSION + 1));
    }

    #[test]
    fn payload_roundtrips_every_typed_body() {
        let check = sample_check();
        assert_eq!(decode_payload::<CheckRequest>(&encode_payload(&check).unwrap()).unwrap(), check);

        let reply = CheckReply {
            para_id: 42,
            issues: vec![Issue {
                start: 0,
                end: 3,
                kind: IssueKind::Spelling,
                message: "Possible spelling error".into(),
                suggestions: vec!["This".into()],
                lang: "en".into(),
            }],
        };
        assert_eq!(decode_payload::<CheckReply>(&encode_payload(&reply).unwrap()).unwrap(), reply);
        let json: serde_json::Value = serde_json::from_slice(&encode_payload(&reply.issues[0].kind).unwrap()).unwrap();
        assert_eq!(json, "spelling");

        let cancel = CancelRequest { id: 9 };
        assert_eq!(decode_payload::<CancelRequest>(&encode_payload(&cancel).unwrap()).unwrap(), cancel);

        let configure = ConfigureRequest {
            options: Some(ProofOptions { grammar: false, ..Default::default() }),
            add_words: vec!["wordcraft".into()],
            remove_words: vec!["recieve".into()],
            ignore_words: vec!["asdf".into()],
        };
        assert_eq!(decode_payload::<ConfigureRequest>(&encode_payload(&configure).unwrap()).unwrap(), configure);

        let error = ErrorReply { code: ErrorCode::BadRequest, message: "nope".into() };
        assert_eq!(decode_payload::<ErrorReply>(&encode_payload(&error).unwrap()).unwrap(), error);
        let json: serde_json::Value = serde_json::from_slice(&encode_payload(&ErrorCode::UnsupportedVersion).unwrap()).unwrap();
        assert_eq!(json, "unsupportedVersion");
    }

    #[test]
    fn request_ids_stay_u32_and_skip_zero() {
        let mut ids = RequestIds::new();
        assert_eq!(ids.next_id(), 1);
        assert_eq!(ids.next_id(), 2);
        ids.next = u32::MAX;
        assert_eq!(ids.next_id(), 1, "wraps to 1, never 0");
    }

    #[test]
    fn oversized_payload_rejected_before_decode() {
        let big = vec![b' '; MAX_PAYLOAD_BYTES + 1];
        assert!(matches!(decode_payload::<CheckRequest>(&big), Err(ProtoError::TooLarge { .. })));
        assert!(matches!(decode_envelope(&big), Err(ProtoError::TooLarge { .. })));
    }

    #[test]
    fn decode_never_panics_on_junk() {
        for junk in [&b""[..], b"{", b"[1,2]", b"\"x\"", &[0xff, 0xfe, 0xfd], b"{\"v\":1}"] {
            let _ = decode_envelope(junk);
            let _ = decode_payload::<CheckRequest>(junk);
            let _ = decode_payload::<ErrorReply>(junk);
        }
    }

    #[test]
    fn check_request_bounds() {
        assert_eq!(check_request_error(&sample_check()), None);
        assert_eq!(check_request_error(&CheckRequest { text: "x".repeat(MAX_TEXT_BYTES + 1), ..sample_check() }), Some(ErrorCode::TooLarge));
        assert_eq!(
            check_request_error(&CheckRequest { spans: vec![LangSpan { start: 5, end: 5, tag: "en".into() }], ..sample_check() }),
            Some(ErrorCode::BadRequest)
        );
        assert_eq!(
            check_request_error(&CheckRequest { spans: vec![LangSpan { start: 0, end: 14, tag: "en".into() }], ..sample_check() }),
            Some(ErrorCode::BadRequest)
        );
        assert_eq!(
            check_request_error(&CheckRequest { skip: vec![ByteRange { start: 20, end: 30 }], ..sample_check() }),
            Some(ErrorCode::BadRequest)
        );
        assert_eq!(
            check_request_error(&CheckRequest { typing: Some(ByteRange { start: 9, end: 9 }), ..sample_check() }),
            Some(ErrorCode::BadRequest)
        );
    }

    #[test]
    fn kinds_split_requests_from_replies() {
        for k in [Kind::Check, Kind::Cancel, Kind::Configure, Kind::Ping] {
            assert!(k.is_request(), "{k:?}");
        }
        for k in [Kind::Issues, Kind::Ack, Kind::Pong, Kind::Ready, Kind::Error] {
            assert!(!k.is_request(), "{k:?}");
        }
    }
}
