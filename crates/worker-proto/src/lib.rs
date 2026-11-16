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
//!   types below; binary kinds (codec input, rendered output) carry raw bytes framed by
//!   [`pack_binary`].
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
/// Largest total size of a [`pack_binary`] frame — the widest payload any request kind accepts
/// (`request_payload_limit`'s `renderPdf` arm): a frame at every cap at once, with document,
/// font batch, JSON metadata part and framing headroom.
pub const MAX_FRAME_BYTES: usize = MAX_PDF_DOC_BYTES + MAX_FONT_TOTAL_BYTES + MAX_PAYLOAD_BYTES + 8 + 4 * (MAX_FONT_COPIES + 2);

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

/// Which side of a `convert` job `.docx` is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConvertDirection {
    /// The named foreign format → `.docx` (the Open direction; the `converted` reply is a
    /// `.docx` package).
    Import,
    /// `.docx` → the named foreign format (Save As; the `converted` reply is that format's
    /// bytes).
    Export,
}

/// `convert` payload (§3.3): convert `input` between `.docx` and the format `format` names.
///
/// On the wire the payload is a [`pack_binary`] frame of two parts — the JSON metadata
/// (`{format, direction, password}`) and the raw `input` bytes — so a file of up to
/// [`MAX_CONVERT_BYTES`] never has to survive JSON.
#[derive(Clone, Debug, PartialEq)]
pub struct ConvertRequest {
    /// Format of the non-`.docx` side, as a lower-case extension (`rtf`, `odt`, `doc`, `html`,
    /// `xml`…), at most [`MAX_FORMAT_BYTES`]. `.docx` and its sibling flavors (`.docm`, `.dotx`,
    /// `.dotm`) are also accepted, for flavor changes and (re)encryption.
    pub format: String,
    pub direction: ConvertDirection,
    /// Password for an encrypted input, or for an encrypted output where the format supports
    /// one (Word packages, ODT, `.doc`); at most [`MAX_PASSWORD_BYTES`].
    pub password: Option<String>,
    /// The file to convert, at most [`MAX_CONVERT_BYTES`].
    pub input: Vec<u8>,
    /// Import only: the page a file that gives no page size of its own opens on — the paper of
    /// the user's region, as the engine opens such a file in its session. `None` (also what a
    /// client that predates the field sends) is a new blank document's page.
    pub page: Option<PageSize>,
}

/// A page size in points, exactly as the client's document model holds it (an `f32` crosses
/// JSON and back bit for bit), each side finite and within
/// [`MIN_PAGE_POINTS`]..=[`MAX_PAGE_POINTS`]. Not twips: rounding the region's A4
/// (595.28 × 841.89 pt) to twips would open a file on a page 0.02 pt off the one the engine
/// opens it on in process.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PageSize {
    pub width: f32,
    pub height: f32,
}

/// The narrowest page side a `convert` request may ask for: 0.1 inch.
pub const MIN_PAGE_POINTS: f32 = 7.2;
/// The widest page side a `convert` request may ask for: 22 inches.
pub const MAX_PAGE_POINTS: f32 = 1584.0;

impl PageSize {
    /// Both sides finite and within [`MIN_PAGE_POINTS`]..=[`MAX_PAGE_POINTS`] (NaN is not).
    pub fn is_valid(&self) -> bool {
        (MIN_PAGE_POINTS..=MAX_PAGE_POINTS).contains(&self.width) && (MIN_PAGE_POINTS..=MAX_PAGE_POINTS).contains(&self.height)
    }

    /// The size of a `width` × `height` point page, unchanged; `None` for a side that is not
    /// finite or falls outside the bounds.
    pub fn from_points(width: f32, height: f32) -> Option<PageSize> {
        let page = PageSize { width, height };
        page.is_valid().then_some(page)
    }

    /// `(width, height)` in points.
    pub fn points(&self) -> (f32, f32) {
        (self.width, self.height)
    }
}

/// One font the client sends the PDF worker (§3.4): the file's bytes plus the family and style
/// the document resolves it under.
///
/// `family` names what the face answers to once installed — a substitute copy (a
/// metric-compatible replacement for a family the browser can't install) answers to the name
/// the document uses, the same rule a `.docx` file's embedded fonts follow
/// (`FontDb::add_document_fonts`). `style` is advisory: the faces inside the file carry their
/// own style names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontCopy {
    pub family: String,
    pub style: String,
    /// TTF, OTF, TTC, WOFF or WOFF2 file bytes.
    pub bytes: Vec<u8>,
}

/// `renderPdf` payload (§3.4): lay out `docx` and write it as PDF.
///
/// On the wire the payload is a [`pack_binary`] frame of `[metaJson, docxBytes, font0Bytes, …]`
/// where `metaJson` is `{"fonts":[{family, style}, …]}` listing `fonts` in order; each font's
/// raw bytes are the part at the same index after the `.docx`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PdfRenderRequest {
    /// The document as a `.docx` interchange package, at most [`MAX_PDF_DOC_BYTES`].
    pub docx: Vec<u8>,
    /// Font copies to install before layout: at most [`MAX_FONT_COPIES`], totalling at most
    /// [`MAX_FONT_TOTAL_BYTES`].
    pub fonts: Vec<FontCopy>,
    /// How the PDF shows the document. Not part of the document, so not in the package.
    pub options: PdfJobOptions,
}

/// How a `renderPdf` job shows the document: the engine's export options for a PDF (Print
/// Markup, its comments, the Numeral setting), as plain data the worker applies to its own
/// `PdfOptions` the way the engine's export does.
///
/// It rides in the JSON part of the payload as `options`. Every field defaults, and the whole
/// field may be absent: a request from a client that predates it renders as it always did (no
/// markup, Western digits), and a worker that predates it ignores it — the same additive rule
/// `convert`'s `password` follows, so the envelope's [`PROTOCOL_VERSION`] stays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PdfJobOptions {
    /// Print Markup: tracked changes shown marked up.
    pub markup: bool,
    /// With `markup`, the comments in balloons beside each page (ignored without it).
    pub comments: bool,
    /// How digits look.
    pub numeral: Numeral,
    /// A strict export: the page already checked that nothing would be left out with its own
    /// faces, so the worker fails the job when a font copy it could not install is one the text
    /// asks for, or a character then has no face to draw it, rather than let that text fall
    /// back. Off, such a copy is skipped and the export runs. An older worker ignores it.
    pub strict: bool,
}

/// The digits a PDF shows (the Numeral setting, with `System` already resolved by the client).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Numeral {
    /// Western digits everywhere.
    #[default]
    Arabic,
    /// Eastern Arabic-Indic digits everywhere.
    Hindi,
    /// Hindi digits in and around right-to-left text, Western elsewhere.
    Context,
}

/// The JSON part of a `convert` payload (everything but `input`).
#[derive(Serialize, Deserialize)]
struct ConvertMeta {
    format: String,
    direction: ConvertDirection,
    #[serde(default)]
    password: Option<String>,
    /// Absent from an older client's request: the default page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    page: Option<PageSize>,
}

/// The JSON part of a `renderPdf` payload: the fonts' names, in `fonts` order, and the job's
/// options (absent from an older client's request: the defaults).
#[derive(Serialize, Deserialize)]
struct PdfMeta {
    fonts: Vec<FontMeta>,
    #[serde(default)]
    options: PdfJobOptions,
}

/// One font's names inside [`PdfMeta`]; its bytes ride as a binary part.
#[derive(Serialize, Deserialize)]
struct FontMeta {
    family: String,
    style: String,
}

/// Magic at the head of a [`pack_binary`] frame: `WCB` plus the framing version byte `1`.
const BINARY_MAGIC: [u8; 4] = *b"WCB\x01";

/// Pack byte parts into one binary-framed payload: `WCB\x01`, a `u32` part count, a `u32`
/// length per part (little-endian), then the part bytes in order.
///
/// The binary kinds (`convert`, `renderPdf`) use this so multi-part payloads — a JSON metadata
/// part beside raw document and font bytes — travel without JSON or base64 overhead. The
/// `converted` and `pdf` replies carry a single output unframed: the envelope's `kind` already
/// says what the bytes are. At most [`MAX_BINARY_PARTS`] parts; each part below 4 GiB; the
/// whole frame at most [`MAX_FRAME_BYTES`]. The total capacity is validated and then reserved
/// fallibly, so a hostile parts list (many views of one slice) is a [`ProtoError::TooLarge`],
/// never a `with_capacity` panic on capacity overflow or an allocation abort.
pub fn pack_binary(parts: &[&[u8]]) -> Result<Vec<u8>, ProtoError> {
    if parts.len() > MAX_BINARY_PARTS {
        return Err(ProtoError::TooLarge { limit: MAX_BINARY_PARTS, got: parts.len() });
    }
    let head = BINARY_MAGIC.len() + 4 + parts.len().saturating_mul(4);
    let body = parts.iter().fold(0usize, |a, p| a.saturating_add(p.len()));
    let total = head.saturating_add(body);
    if total > MAX_FRAME_BYTES {
        return Err(ProtoError::TooLarge { limit: MAX_FRAME_BYTES, got: total });
    }
    let mut out = Vec::new();
    out.try_reserve_exact(total).map_err(|_| ProtoError::TooLarge { limit: MAX_FRAME_BYTES, got: total })?;
    out.extend_from_slice(&BINARY_MAGIC);
    out.extend_from_slice(&(parts.len() as u32).to_le_bytes());
    for part in parts {
        let len = u32::try_from(part.len()).map_err(|_| ProtoError::TooLarge { limit: u32::MAX as usize, got: part.len() })?;
        out.extend_from_slice(&len.to_le_bytes());
    }
    for part in parts {
        out.extend_from_slice(part);
    }
    Ok(out)
}

/// The parts of a [`pack_binary`] frame, or [`ProtoError::Decode`] for anything malformed:
/// wrong magic, a truncated length table, a part running past the frame, trailing bytes, or
/// more than [`MAX_BINARY_PARTS`] parts ([`ProtoError::TooLarge`]).
pub fn unpack_binary(bytes: &[u8]) -> Result<Vec<&[u8]>, ProtoError> {
    fn bad(m: &str) -> ProtoError {
        ProtoError::Decode(format!("bad binary frame: {m}"))
    }
    let rest = bytes.strip_prefix(&BINARY_MAGIC[..]).ok_or_else(|| bad("wrong magic"))?;
    let (count_b, rest) = rest.split_at_checked(4).ok_or_else(|| bad("truncated part count"))?;
    let count = u32::from_le_bytes([count_b[0], count_b[1], count_b[2], count_b[3]]) as usize;
    if count > MAX_BINARY_PARTS {
        return Err(ProtoError::TooLarge { limit: MAX_BINARY_PARTS, got: count });
    }
    let table_len = count.checked_mul(4).ok_or_else(|| bad("part table overflows"))?;
    let (table, mut rest) = rest.split_at_checked(table_len).ok_or_else(|| bad("truncated part table"))?;
    let mut parts = Vec::with_capacity(count);
    let (lens, _) = table.as_chunks::<4>();
    for len_b in lens {
        let len = u32::from_le_bytes(*len_b) as usize;
        let (part, tail) = rest.split_at_checked(len).ok_or_else(|| bad("part overruns the frame"))?;
        parts.push(part);
        rest = tail;
    }
    if !rest.is_empty() {
        return Err(bad("trailing bytes after the last part"));
    }
    Ok(parts)
}

/// Encode a `convert` payload: a two-part [`pack_binary`] frame, JSON metadata then `input`.
pub fn encode_convert_request(req: &ConvertRequest) -> Result<Vec<u8>, ProtoError> {
    let meta = encode_payload(&ConvertMeta { format: req.format.clone(), direction: req.direction, password: req.password.clone(), page: req.page })?;
    pack_binary(&[&meta, &req.input])
}

/// Decode a `convert` payload. Framing, JSON structure and the [`convert_request_error`]
/// bounds, checked on the borrowed frame slices so an over-limit request is rejected before
/// `input` is copied.
pub fn decode_convert_request(bytes: &[u8]) -> Result<ConvertRequest, ProtoError> {
    let parts = unpack_binary(bytes)?;
    let [meta, input] = parts.as_slice() else {
        return Err(ProtoError::Decode(format!("convert payload has {} parts, expected 2", parts.len())));
    };
    let meta: ConvertMeta = decode_payload(meta)?;
    if meta.format.len() > MAX_FORMAT_BYTES {
        return Err(ProtoError::TooLarge { limit: MAX_FORMAT_BYTES, got: meta.format.len() });
    }
    if let Some(password) = meta.password.as_deref()
        && password.len() > MAX_PASSWORD_BYTES
    {
        return Err(ProtoError::TooLarge { limit: MAX_PASSWORD_BYTES, got: password.len() });
    }
    if input.len() > MAX_CONVERT_BYTES {
        return Err(ProtoError::TooLarge { limit: MAX_CONVERT_BYTES, got: input.len() });
    }
    if meta.format.is_empty() {
        return Err(ProtoError::Decode("convert payload has an empty format".into()));
    }
    if meta.page.is_some_and(|p| !p.is_valid()) {
        return Err(ProtoError::Decode(format!("convert payload has a page size outside {MIN_PAGE_POINTS}..={MAX_PAGE_POINTS} points")));
    }
    Ok(ConvertRequest { format: meta.format, direction: meta.direction, password: meta.password, input: input.to_vec(), page: meta.page })
}

/// Encode a `renderPdf` payload: `[metaJson, docx, font0, font1, …]`.
pub fn encode_pdf_request(req: &PdfRenderRequest) -> Result<Vec<u8>, ProtoError> {
    let meta = encode_payload(&PdfMeta {
        fonts: req.fonts.iter().map(|f| FontMeta { family: f.family.clone(), style: f.style.clone() }).collect(),
        options: req.options,
    })?;
    let mut parts: Vec<&[u8]> = Vec::with_capacity(req.fonts.len().saturating_add(2));
    parts.push(&meta);
    parts.push(&req.docx);
    parts.extend(req.fonts.iter().map(|f| f.bytes.as_slice()));
    pack_binary(&parts)
}

/// Decode a `renderPdf` payload. Framing, part/font-count agreement, JSON structure and the
/// [`pdf_request_error`] bounds, checked on the borrowed frame slices so an over-limit request
/// is rejected before its parts are copied.
pub fn decode_pdf_request(bytes: &[u8]) -> Result<PdfRenderRequest, ProtoError> {
    let parts = unpack_binary(bytes)?;
    if parts.len() < 2 {
        return Err(ProtoError::Decode(format!("renderPdf payload has {} parts, expected at least 2", parts.len())));
    }
    let meta: PdfMeta = decode_payload(parts[0])?;
    if meta.fonts.len() != parts.len() - 2 {
        return Err(ProtoError::Decode(format!("renderPdf declares {} fonts but carries {} font parts", meta.fonts.len(), parts.len() - 2)));
    }
    let docx = parts[1];
    if docx.len() > MAX_PDF_DOC_BYTES {
        return Err(ProtoError::TooLarge { limit: MAX_PDF_DOC_BYTES, got: docx.len() });
    }
    if meta.fonts.len() > MAX_FONT_COPIES {
        return Err(ProtoError::TooLarge { limit: MAX_FONT_COPIES, got: meta.fonts.len() });
    }
    let font_total = parts[2..].iter().fold(0usize, |a, p| a.saturating_add(p.len()));
    if font_total > MAX_FONT_TOTAL_BYTES {
        return Err(ProtoError::TooLarge { limit: MAX_FONT_TOTAL_BYTES, got: font_total });
    }
    if docx.is_empty() {
        return Err(ProtoError::Decode("renderPdf payload has an empty .docx".into()));
    }
    Ok(PdfRenderRequest {
        docx: docx.to_vec(),
        fonts: meta
            .fonts
            .into_iter()
            .zip(&parts[2..])
            .map(|(f, bytes)| FontCopy { family: f.family, style: f.style, bytes: bytes.to_vec() })
            .collect(),
        options: meta.options,
    })
}

/// Validate a [`ConvertRequest`] against the protocol bounds before a worker runs it, like
/// [`check_request_error`]: `TooLarge` past [`MAX_FORMAT_BYTES`], [`MAX_PASSWORD_BYTES`] or
/// [`MAX_CONVERT_BYTES`], `BadRequest` for an empty format name.
pub fn convert_request_error(req: &ConvertRequest) -> Option<ErrorCode> {
    if req.format.len() > MAX_FORMAT_BYTES
        || req.password.as_deref().is_some_and(|p| p.len() > MAX_PASSWORD_BYTES)
        || req.input.len() > MAX_CONVERT_BYTES
    {
        return Some(ErrorCode::TooLarge);
    }
    if req.format.is_empty() || req.page.is_some_and(|p| !p.is_valid()) {
        return Some(ErrorCode::BadRequest);
    }
    None
}

/// The largest payload buffer a `kind` request may arrive in, in bytes — the document bound
/// plus the [`pack_binary`] framing and JSON metadata overhead — so a worker can refuse an
/// oversized message before copying it into its heap. `0` for kinds that are never requests
/// (a worker never reads their payload).
pub fn request_payload_limit(kind: Kind) -> usize {
    match kind {
        // JSON bodies: `decode_payload`'s own cap is the whole bound.
        Kind::Check | Kind::Cancel | Kind::Configure | Kind::Ping => MAX_PAYLOAD_BYTES,
        // `WCB\x01` + part count + two length words (16 bytes of framing), the metadata part
        // (a JSON payload, `MAX_PAYLOAD_BYTES`) and the input (`MAX_CONVERT_BYTES`).
        Kind::Convert => MAX_CONVERT_BYTES + MAX_PAYLOAD_BYTES + 16,
        // `meta + docx + fonts`: at most `MAX_FONT_COPIES` font parts, so the frame overhead
        // is 8 + 4·(MAX_FONT_COPIES + 2) bytes; the meta part is a JSON payload. This is the
        // widest limit, and [`MAX_FRAME_BYTES`] is the same bound `pack_binary` enforces.
        Kind::RenderPdf => MAX_FRAME_BYTES,
        _ => 0,
    }
}

/// Most bytes a converted file or a PDF may come back in: a client refuses a larger reply before
/// copying it ([`reply_payload_limit`]).
pub const MAX_OUTPUT_BYTES: usize = 256 << 20;

/// The largest payload buffer a `kind` reply may arrive in, in bytes, so a client can refuse a
/// malformed or oversized message from a worker before copying it into its heap, as a worker
/// refuses a request ([`request_payload_limit`]). `0` for request kinds, which a worker never
/// sends.
pub fn reply_payload_limit(kind: Kind) -> usize {
    match kind {
        // Raw file bytes.
        Kind::Converted | Kind::Pdf => MAX_OUTPUT_BYTES,
        // JSON bodies (or none): `decode_payload`'s own cap.
        Kind::Issues | Kind::Ack | Kind::Pong | Kind::Ready | Kind::Error => MAX_PAYLOAD_BYTES,
        Kind::Check | Kind::Cancel | Kind::Configure | Kind::Convert | Kind::RenderPdf | Kind::Ping => 0,
    }
}

/// Validate a [`PdfRenderRequest`] against the protocol bounds before a worker runs it, like
/// [`check_request_error`]: `TooLarge` past [`MAX_PDF_DOC_BYTES`], [`MAX_FONT_COPIES`] or
/// [`MAX_FONT_TOTAL_BYTES`], `BadRequest` for an empty package.
pub fn pdf_request_error(req: &PdfRenderRequest) -> Option<ErrorCode> {
    let font_total = req.fonts.iter().fold(0usize, |a, f| a.saturating_add(f.bytes.len()));
    if req.docx.len() > MAX_PDF_DOC_BYTES || req.fonts.len() > MAX_FONT_COPIES || font_total > MAX_FONT_TOTAL_BYTES {
        return Some(ErrorCode::TooLarge);
    }
    if req.docx.is_empty() {
        return Some(ErrorCode::BadRequest);
    }
    None
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
        for k in [Kind::Check, Kind::Cancel, Kind::Configure, Kind::Convert, Kind::RenderPdf, Kind::Ping] {
            assert!(k.is_request(), "{k:?}");
        }
        for k in [Kind::Issues, Kind::Converted, Kind::Pdf, Kind::Ack, Kind::Pong, Kind::Ready, Kind::Error] {
            assert!(!k.is_request(), "{k:?}");
        }
    }

    fn sample_convert() -> ConvertRequest {
        ConvertRequest {
            format: "rtf".into(),
            direction: ConvertDirection::Import,
            password: Some("open-sesame".into()),
            input: b"{\\rtf1 hi}".to_vec(),
            page: Some(PageSize { width: 595.28, height: 841.89 }),
        }
    }

    #[test]
    fn binary_frame_roundtrip() {
        let parts: [&[u8]; 3] = [b"meta", &[0x50, 0x4b, 1, 2, 3, 4], &[]];
        let packed = pack_binary(&parts).unwrap();
        assert_eq!(&packed[..4], b"WCB\x01");
        assert_eq!(unpack_binary(&packed).unwrap(), parts);
        // An empty payload is a valid zero-part frame.
        assert_eq!(unpack_binary(&pack_binary(&[]).unwrap()).unwrap(), Vec::<&[u8]>::new());
    }

    #[test]
    fn binary_frame_rejects_junk_without_panicking() {
        for junk in [
            &b""[..],
            b"WCB",
            b"WCB\x02\x00\x00\x00\x00",                        // wrong framing version
            b"WCB\x01\x03\x00\x00",                            // truncated count
            b"WCB\x01\x02\x00\x00\x00\x04\x00\x00\x00",        // truncated length table
            b"WCB\x01\x01\x00\x00\x00\xff\xff\xff\xffpartial", // part overruns the frame
            b"WCB\x01\x00\x00\x00\x00leftover",                // trailing bytes
        ] {
            assert!(unpack_binary(junk).is_err(), "{junk:?}");
        }
        // A part count beyond the sanity bound is TooLarge, not an allocation.
        let mut over = b"WCB\x01".to_vec();
        over.extend_from_slice(&(MAX_BINARY_PARTS as u32 + 1).to_le_bytes());
        over.extend_from_slice(&vec![0u8; (MAX_BINARY_PARTS + 1) * 4]);
        assert!(matches!(unpack_binary(&over), Err(ProtoError::TooLarge { .. })));
    }

    #[test]
    fn pack_binary_rejects_oversized_total_before_reserving() {
        // `MAX_BINARY_PARTS` views of one backing slice pass the part-count bound, but their
        // summed capacity (1 GiB here) is far past `MAX_FRAME_BYTES`: the frame is refused as
        // `TooLarge` before any reservation, where `Vec::with_capacity` would have tried to
        // allocate the sum — panicking on capacity overflow once it saturates.
        let slice = [0u8; 1024 * 1024];
        let parts: Vec<&[u8]> = vec![slice.as_slice(); MAX_BINARY_PARTS];
        let err = pack_binary(&parts).unwrap_err();
        assert!(
            matches!(err, ProtoError::TooLarge { limit: MAX_FRAME_BYTES, got, .. } if got > MAX_FRAME_BYTES),
            "oversized frame capacity must map to TooLarge, got {err:?}"
        );
    }

    #[test]
    fn convert_request_roundtrip() {
        let req = sample_convert();
        let bytes = encode_convert_request(&req).unwrap();
        // The wire frame is [metaJson, inputBytes]: the file bytes are raw, not JSON.
        let parts = unpack_binary(&bytes).unwrap();
        assert_eq!(parts.len(), 2);
        let meta: serde_json::Value = serde_json::from_slice(parts[0]).unwrap();
        assert_eq!(meta["format"], "rtf");
        assert_eq!(meta["direction"], "import");
        assert_eq!(parts[1], req.input.as_slice());
        assert_eq!(decode_convert_request(&bytes).unwrap(), req);

        let req = ConvertRequest { password: None, ..sample_convert() };
        let meta: serde_json::Value = serde_json::from_slice(unpack_binary(&encode_convert_request(&req).unwrap()).unwrap()[0]).unwrap();
        assert_eq!(meta["password"], serde_json::Value::Null);
        assert_eq!(decode_convert_request(&encode_convert_request(&req).unwrap()).unwrap(), req);
    }

    /// Review finding (the page of an opened file): the page an import opens on travels in the
    /// JSON part, in points as the client holds them; a request without one, as an older client sends it, has none, and an
    /// older worker reads a new request (serde ignores the field it does not know).
    #[test]
    fn convert_request_carries_the_page_compatibly() {
        let req = sample_convert();
        let meta: serde_json::Value = serde_json::from_slice(unpack_binary(&encode_convert_request(&req).unwrap()).unwrap()[0]).unwrap();
        assert_eq!(meta["page"].to_string(), r#"{"height":841.89,"width":595.28}"#, "the shortest form that reads back exactly");
        let none = ConvertRequest { page: None, ..sample_convert() };
        let meta: serde_json::Value = serde_json::from_slice(unpack_binary(&encode_convert_request(&none).unwrap()).unwrap()[0]).unwrap();
        assert!(meta.get("page").is_none(), "no page, no field: {meta}");
        assert_eq!(decode_convert_request(&encode_convert_request(&none).unwrap()).unwrap(), none);
        #[derive(Deserialize)]
        struct OldConvertMeta {
            format: String,
        }
        let old: OldConvertMeta = decode_payload(unpack_binary(&encode_convert_request(&req).unwrap()).unwrap()[0]).unwrap();
        assert_eq!(old.format, "rtf");
        // Points cross as they are, never rounded: A4 and Letter as the engine's regions give
        // them come back bit for bit, on the wire too.
        for (w, h) in [(595.28f32, 841.89f32), (612.0, 792.0), (7.2, 1584.0), (419.53, 595.28), (0.1 + 600.0, 800.3)] {
            let page = PageSize::from_points(w, h).unwrap();
            assert_eq!(page.points(), (w, h));
            let req = ConvertRequest { page: Some(page), ..sample_convert() };
            let back = decode_convert_request(&encode_convert_request(&req).unwrap()).unwrap().page.unwrap().points();
            assert_eq!((back.0.to_bits(), back.1.to_bits()), (w.to_bits(), h.to_bits()), "{w} x {h}");
        }
        // Every f32 in the bounds survives the JSON, not just the regions' sizes.
        let mut pt = MIN_PAGE_POINTS;
        while pt <= MAX_PAGE_POINTS {
            let req = ConvertRequest { page: PageSize::from_points(pt, MAX_PAGE_POINTS), ..sample_convert() };
            let back = decode_convert_request(&encode_convert_request(&req).unwrap()).unwrap().page.unwrap().points();
            assert_eq!(back.0.to_bits(), pt.to_bits(), "{pt}");
            pt = f32::from_bits(pt.to_bits() + 997);
        }
        for (w, h) in [(f32::NAN, 792.0), (612.0, f32::INFINITY), (-612.0, 792.0), (0.0, 792.0), (612.0, 1e9), (7.1, 792.0), (1584.1, 792.0)] {
            assert_eq!(PageSize::from_points(w, h), None, "{w} x {h}");
        }
        assert!(PageSize::from_points(7.2, 1584.0).is_some(), "the bounds themselves fit");
    }

    /// A hostile page is a typed error, never a panic or a page laid out at any size.
    #[test]
    fn convert_request_rejects_a_hostile_page() {
        for page in [
            serde_json::json!({"width": 0, "height": 792}),
            serde_json::json!({"width": 612, "height": 1584.5}),
            serde_json::json!({"width": 7.1, "height": 792}),
            serde_json::json!({"width": u64::MAX, "height": 792}),
            serde_json::json!({"width": 1e300, "height": 792}),
            serde_json::json!({"width": -612, "height": 792}),
            serde_json::json!({"width": 612}),
            serde_json::json!({"width": "612", "height": 792}),
            serde_json::json!("A4"),
        ] {
            let meta = encode_payload(&serde_json::json!({"format": "html", "direction": "import", "page": page})).unwrap();
            let frame = pack_binary(&[&meta, b"<p>x</p>"]).unwrap();
            assert!(matches!(decode_convert_request(&frame), Err(ProtoError::Decode(_))), "{page}");
        }
        for (w, h) in [(612.0, 0.0), (f32::NAN, 792.0), (612.0, f32::INFINITY), (-612.0, 792.0)] {
            let req = ConvertRequest { page: Some(PageSize { width: w, height: h }), ..sample_convert() };
            assert_eq!(convert_request_error(&req), Some(ErrorCode::BadRequest), "{w} x {h}");
        }
        let req = ConvertRequest { page: Some(PageSize { width: 612.0, height: 792.0 }), ..sample_convert() };
        assert_eq!(convert_request_error(&req), None);
    }

    #[test]
    fn convert_request_rejects_bad_shapes() {
        // Not a frame at all.
        assert!(matches!(decode_convert_request(b"{}"), Err(ProtoError::Decode(_))));
        // One part (metadata only, no input bytes).
        let meta = encode_payload(&serde_json::json!({"format": "rtf", "direction": "import"})).unwrap();
        assert!(matches!(decode_convert_request(&pack_binary(&[&meta]).unwrap()), Err(ProtoError::Decode(_))));
        // Two parts but a broken metadata part.
        assert!(matches!(decode_convert_request(&pack_binary(&[b"{broken", b"x"]).unwrap()), Err(ProtoError::Decode(_))));
    }

    #[test]
    fn pdf_request_roundtrip() {
        let req = PdfRenderRequest {
            docx: b"PK\x03\x04 fake package".to_vec(),
            fonts: vec![
                FontCopy { family: "Inter".into(), style: "Regular".into(), bytes: vec![0, 1, 0, 0, 9] },
                FontCopy { family: "Liter".into(), style: "Italic".into(), bytes: vec![0, 1, 0, 0, 8, 8, 8] },
            ],
            options: PdfJobOptions { markup: true, comments: true, numeral: Numeral::Context, strict: false },
        };
        let bytes = encode_pdf_request(&req).unwrap();
        let parts = unpack_binary(&bytes).unwrap();
        assert_eq!(parts.len(), 4);
        let meta: serde_json::Value = serde_json::from_slice(parts[0]).unwrap();
        assert_eq!(meta["fonts"][0]["family"], "Inter");
        assert_eq!(meta["fonts"][1]["style"], "Italic");
        assert_eq!(parts[1], req.docx.as_slice());
        assert_eq!(parts[2], req.fonts[0].bytes.as_slice());
        assert_eq!(parts[3], req.fonts[1].bytes.as_slice());
        assert_eq!(decode_pdf_request(&bytes).unwrap(), req);
        // A request without fonts is meta + docx and nothing else.
        let bare = PdfRenderRequest { fonts: Vec::new(), ..req };
        assert_eq!(decode_pdf_request(&encode_pdf_request(&bare).unwrap()).unwrap(), bare);
    }

    /// Review finding (PDF options): the options travel in the JSON part, each value as sent.
    #[test]
    fn pdf_request_carries_its_options() {
        for options in [
            PdfJobOptions::default(),
            PdfJobOptions { markup: true, comments: false, numeral: Numeral::Arabic, strict: false },
            PdfJobOptions { markup: true, comments: true, numeral: Numeral::Hindi, strict: true },
            PdfJobOptions { markup: false, comments: true, numeral: Numeral::Context, strict: false },
        ] {
            let req = PdfRenderRequest { docx: b"PK".to_vec(), fonts: Vec::new(), options };
            let bytes = encode_pdf_request(&req).unwrap();
            let meta: serde_json::Value = serde_json::from_slice(unpack_binary(&bytes).unwrap()[0]).unwrap();
            assert_eq!(meta["options"]["markup"], options.markup);
            assert_eq!(meta["options"]["strict"], options.strict);
            assert_eq!(decode_pdf_request(&bytes).unwrap().options, options);
        }
        let meta: serde_json::Value = serde_json::from_slice(
            unpack_binary(
                &encode_pdf_request(&PdfRenderRequest {
                    docx: b"PK".to_vec(),
                    fonts: Vec::new(),
                    options: PdfJobOptions { numeral: Numeral::Hindi, ..Default::default() },
                })
                .unwrap(),
            )
            .unwrap()[0],
        )
        .unwrap();
        assert_eq!(meta["options"]["numeral"], "hindi");
    }

    /// The options are additive: a request from a client that predates them (no `options`, or
    /// only some fields) renders as before, and a worker that predates them reads a new request
    /// (serde ignores the field it does not know). The envelope's version is unchanged.
    #[test]
    fn pdf_options_are_compatible_both_ways() {
        let old_client = encode_payload(&serde_json::json!({"fonts": []})).unwrap();
        let frame = pack_binary(&[&old_client, b"PK"]).unwrap();
        assert_eq!(decode_pdf_request(&frame).unwrap().options, PdfJobOptions::default());
        let partial = encode_payload(&serde_json::json!({"fonts": [], "options": {"markup": true}})).unwrap();
        let frame = pack_binary(&[&partial, b"PK"]).unwrap();
        assert_eq!(decode_pdf_request(&frame).unwrap().options, PdfJobOptions { markup: true, ..Default::default() });
        // A later client's extra option is ignored here, as this one's are by an older worker.
        let later = encode_payload(&serde_json::json!({"fonts": [], "options": {"markup": true, "future": 1}, "more": [1]})).unwrap();
        let frame = pack_binary(&[&later, b"PK"]).unwrap();
        assert!(decode_pdf_request(&frame).unwrap().options.markup);
        #[derive(Deserialize)]
        struct OldPdfMeta {
            fonts: Vec<serde_json::Value>,
        }
        let new_meta = unpack_binary(
            &encode_pdf_request(&PdfRenderRequest {
                docx: b"PK".to_vec(),
                fonts: Vec::new(),
                options: PdfJobOptions { markup: true, comments: true, numeral: Numeral::Hindi, strict: true },
            })
            .unwrap(),
        )
        .unwrap()[0]
            .to_vec();
        let old: OldPdfMeta = decode_payload(&new_meta).unwrap();
        assert!(old.fonts.is_empty());
    }

    /// Hostile options are a typed decode error, never a panic or a silent default.
    #[test]
    fn pdf_options_reject_junk() {
        for options in [
            serde_json::json!(5),
            serde_json::json!("markup"),
            serde_json::json!({"markup": "yes"}),
            serde_json::json!({"comments": 1}),
            serde_json::json!({"numeral": "system"}),
            serde_json::json!({"numeral": "klingon"}),
            serde_json::json!({"numeral": 2}),
        ] {
            let meta = encode_payload(&serde_json::json!({"fonts": [], "options": options})).unwrap();
            let frame = pack_binary(&[&meta, b"PK"]).unwrap();
            assert!(matches!(decode_pdf_request(&frame), Err(ProtoError::Decode(_))), "{options}");
        }
    }

    #[test]
    fn pdf_request_rejects_font_count_mismatch() {
        // Two font parts but a metadata part naming one.
        let meta = encode_payload(&serde_json::json!({"fonts": [{"family": "A", "style": "B"}]})).unwrap();
        let frame = pack_binary(&[&meta, b"docx", b"font0", b"font1"]).unwrap();
        assert!(matches!(decode_pdf_request(&frame), Err(ProtoError::Decode(_))));
    }

    #[test]
    fn decode_rejects_over_limit_before_copying() {
        // The bounds run on the borrowed frame slices: an over-limit part fails in decode
        // itself (TooLarge), before `to_vec` copies it — same limits `*_request_error` reports.
        let big = vec![0u8; MAX_PDF_DOC_BYTES + 1];
        let meta = encode_payload(&serde_json::json!({"fonts": []})).unwrap();
        let frame = pack_binary(&[&meta, &big]).unwrap();
        assert!(matches!(decode_pdf_request(&frame), Err(ProtoError::TooLarge { limit: MAX_PDF_DOC_BYTES, .. })));

        let meta = encode_payload(&serde_json::json!({"format": "rtf", "direction": "import"})).unwrap();
        let frame = pack_binary(&[&meta, &big]).unwrap();
        assert!(matches!(decode_convert_request(&frame), Err(ProtoError::TooLarge { limit: MAX_CONVERT_BYTES, .. })));

        // Empty bodies fail as decode errors, matching the BadRequest the request checks give.
        let pdf_meta = encode_payload(&serde_json::json!({"fonts": []})).unwrap();
        let frame = pack_binary(&[&pdf_meta, &[][..]]).unwrap();
        assert!(matches!(decode_pdf_request(&frame), Err(ProtoError::Decode(_))));
        let convert_meta = encode_payload(&serde_json::json!({"format": "", "direction": "import"})).unwrap();
        let frame = pack_binary(&[&convert_meta, b"x"]).unwrap();
        assert!(matches!(decode_convert_request(&frame), Err(ProtoError::Decode(_))));
    }

    #[test]
    fn convert_and_pdf_request_bounds() {
        let ok = sample_convert();
        assert_eq!(convert_request_error(&ok), None);
        assert_eq!(convert_request_error(&ConvertRequest { format: "x".repeat(MAX_FORMAT_BYTES + 1), ..ok.clone() }), Some(ErrorCode::TooLarge));
        assert_eq!(
            convert_request_error(&ConvertRequest { password: Some("p".repeat(MAX_PASSWORD_BYTES + 1)), ..ok.clone() }),
            Some(ErrorCode::TooLarge)
        );
        assert_eq!(convert_request_error(&ConvertRequest { input: vec![0; MAX_CONVERT_BYTES + 1], ..ok.clone() }), Some(ErrorCode::TooLarge));
        assert_eq!(convert_request_error(&ConvertRequest { format: String::new(), ..ok.clone() }), Some(ErrorCode::BadRequest));

        let ok = PdfRenderRequest {
            docx: b"PK".to_vec(),
            fonts: vec![FontCopy { family: "a".into(), style: "b".into(), bytes: vec![1; 8] }],
            options: PdfJobOptions::default(),
        };
        assert_eq!(pdf_request_error(&ok), None);
        assert_eq!(
            pdf_request_error(&PdfRenderRequest { docx: vec![0; MAX_PDF_DOC_BYTES + 1], fonts: Vec::new(), ..ok.clone() }),
            Some(ErrorCode::TooLarge)
        );
        assert_eq!(
            pdf_request_error(&PdfRenderRequest {
                fonts: (0..MAX_FONT_COPIES + 1).map(|_| FontCopy { family: String::new(), style: String::new(), bytes: Vec::new() }).collect(),
                ..ok.clone()
            }),
            Some(ErrorCode::TooLarge)
        );
        assert_eq!(
            pdf_request_error(&PdfRenderRequest {
                fonts: vec![FontCopy { bytes: vec![0; MAX_FONT_TOTAL_BYTES], ..ok.fonts[0].clone() }],
                ..ok.clone()
            }),
            None,
            "a font batch at the limit fits"
        );
        assert_eq!(
            pdf_request_error(&PdfRenderRequest {
                fonts: vec![FontCopy { bytes: vec![0; MAX_FONT_TOTAL_BYTES + 1], ..ok.fonts[0].clone() }],
                ..ok.clone()
            }),
            Some(ErrorCode::TooLarge)
        );
        assert_eq!(pdf_request_error(&PdfRenderRequest { docx: Vec::new(), fonts: Vec::new(), ..ok.clone() }), Some(ErrorCode::BadRequest));
    }

    #[test]
    fn request_payload_limit_covers_everything_decode_accepts() {
        // Reply kinds never carry a request payload.
        for kind in [Kind::Issues, Kind::Converted, Kind::Pdf, Kind::Ack, Kind::Pong, Kind::Ready, Kind::Error] {
            assert_eq!(request_payload_limit(kind), 0, "{kind:?}");
        }
        // The JSON kinds are bounded by `decode_payload` itself.
        for kind in [Kind::Check, Kind::Cancel, Kind::Configure, Kind::Ping] {
            assert_eq!(request_payload_limit(kind), MAX_PAYLOAD_BYTES, "{kind:?}");
        }
        // A convert frame at the document cap plus a maximal metadata part still fits; the
        // same frame one byte larger does not — the worker's pre-copy check relies on this.
        let meta = encode_payload(&serde_json::json!({"format": "rtf", "direction": "import"})).unwrap();
        let input = vec![0u8; MAX_CONVERT_BYTES];
        let frame = pack_binary(&[&meta, &input]).unwrap();
        assert!(frame.len() <= request_payload_limit(Kind::Convert));
        let meta = vec![b' '; MAX_PAYLOAD_BYTES];
        let frame = pack_binary(&[&meta, &input]).unwrap();
        assert!(frame.len() <= request_payload_limit(Kind::Convert));
        let over = pack_binary(&[&meta, &vec![0u8; MAX_CONVERT_BYTES + 1]]).unwrap();
        assert!(over.len() > request_payload_limit(Kind::Convert));
        // renderPdf: docx at its cap, the font batch at its cap, every allowed font part —
        // the limit must cover the largest frame `decode_pdf_request` can accept.
        let fonts_meta: Vec<serde_json::Value> = (0..MAX_FONT_COPIES).map(|_| serde_json::json!({"family": "f", "style": "s"})).collect();
        let meta = encode_payload(&serde_json::json!({"fonts": fonts_meta})).unwrap();
        let docx = vec![0u8; MAX_PDF_DOC_BYTES];
        let font = vec![0u8; MAX_FONT_TOTAL_BYTES / MAX_FONT_COPIES];
        let mut parts: Vec<&[u8]> = vec![meta.as_slice(), docx.as_slice()];
        parts.extend((0..MAX_FONT_COPIES).map(|_| font.as_slice()));
        let frame = pack_binary(&parts).unwrap();
        assert!(frame.len() <= request_payload_limit(Kind::RenderPdf), "{} > {}", frame.len(), request_payload_limit(Kind::RenderPdf));
    }

    /// Every reply kind has a bound a client checks before copying; request kinds have none
    /// (a worker never sends them), and file replies are the widest.
    #[test]
    fn reply_payload_limits_bound_every_reply_kind() {
        for kind in [Kind::Issues, Kind::Ack, Kind::Pong, Kind::Ready, Kind::Error] {
            assert_eq!(reply_payload_limit(kind), MAX_PAYLOAD_BYTES, "{kind:?}");
        }
        assert_eq!(reply_payload_limit(Kind::Pdf), MAX_OUTPUT_BYTES);
        assert_eq!(reply_payload_limit(Kind::Converted), MAX_OUTPUT_BYTES);
        for kind in [Kind::Check, Kind::Cancel, Kind::Configure, Kind::Convert, Kind::RenderPdf, Kind::Ping] {
            assert!(kind.is_request());
            assert_eq!(reply_payload_limit(kind), 0, "{kind:?}");
        }
    }
}
