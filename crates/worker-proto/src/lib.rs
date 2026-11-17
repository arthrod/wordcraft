//! The WordCraft web worker contract (deferred-items plan §3.1): one wire protocol for the
//! dedicated workers (`apps/wordcraft-worker-proof`, `-codecs`, `-pdf`, `-viewer`) and the
//! clients that drive them.
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
/// Most faces one `renderPdf` request lists as drawn ([`PdfRenderRequest::drawn`]): the
/// engine's own bound on that list (`wordcraft_engine::strict::MAX_PDF_FACES`).
pub const MAX_DRAWN_FACES: usize = 4096;
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

/// Largest `.docx` the view-only worker opens, in bytes (plan 10.3): the bound the view-only
/// core applies itself (`wordcraft_viewer::MAX_DOCUMENT_BYTES`) and the PDF worker puts on the
/// same package, so a file one opens the other exports.
pub const MAX_VIEW_DOC_BYTES: usize = 64 << 20;
/// Largest scale a `viewRender` request asks for, in pixels per point (the core's
/// `wordcraft_viewer::MAX_SCALE`).
pub const MAX_VIEW_SCALE: f32 = 16.0;
/// Most pixels one `viewPage` reply carries (the core's `wordcraft_viewer::MAX_RENDER_PIXELS`):
/// 32 million, which is 128 MiB of RGBA.
pub const MAX_VIEW_PIXELS: u64 = 32_000_000;
/// Longest side, in pixels, of a page a `viewRender` request may ask for: what the core's
/// rasteriser draws (`wordcraft_render::MAX_SIDE`, through `wordcraft_viewer::MAX_RASTER_SIDE`;
/// the viewer worker's tests hold them equal). The page module reads it here, so it never links
/// the renderer.
pub const MAX_VIEW_SIDE: u32 = 16_000;
/// Most page sizes a `viewOpened` reply lists. A longer document reports its real page count
/// and the sizes of its first pages; at about 40 bytes of JSON a size, a reply at the cap is
/// still under [`MAX_PAYLOAD_BYTES`].
pub const MAX_VIEW_PAGES: usize = 50_000;
/// Largest font file a `viewInstallFont` request carries, in bytes (the font database's own
/// limit, `wordcraft_fonts::MAX_FONT_FILE_BYTES`).
pub const MAX_VIEW_FONT_BYTES: usize = 50 << 20;
/// Longest font name a `viewInstallFont` request carries, in bytes: four bytes for each of the
/// 256 characters the core accepts (`wordcraft_viewer::MAX_FONT_NAME_CHARS`).
pub const MAX_VIEW_FONT_NAME_BYTES: usize = 1024;
/// Most names a `viewFonts` reply lists.
pub const MAX_VIEW_MISSING_FONTS: usize = 1024;
/// Size of the trailer that ends a `viewPage` payload ([`finish_view_page`]).
pub const VIEW_PAGE_TRAILER_BYTES: usize = 16;

/// The `kind` field of an [`Envelope`]. Request kinds are `check`, `cancel`, `configure`,
/// `convert`, `renderPdf`, `ping` and the view-only worker's `viewOpen`, `viewRender`,
/// `viewInstallFont` and `viewMissingFonts`; everything else only ever travels worker → client.
/// A worker answers a request kind it doesn't implement with [`ErrorCode::UnknownKind`].
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
    /// Open a `.docx` in the view-only worker ([`ViewOpenRequest`]) → [`Kind::ViewOpened`]. It
    /// replaces the document the worker held.
    ViewOpen,
    /// Paint one page of the open document ([`ViewRenderRequest`]) → [`Kind::ViewPage`].
    ViewRender,
    /// Make a font file available and lay the open document out again
    /// ([`ViewInstallFontRequest`]) → [`Kind::ViewOpened`], or [`Kind::Ack`] when no document
    /// is open.
    ViewInstallFont,
    /// The fonts the open document asks for that aren't available → [`Kind::ViewFonts`].
    ViewMissingFonts,
    /// [`CheckRequest`] outcome ([`CheckReply`]).
    Issues,
    /// [`ConvertRequest`] outcome: raw converted bytes (`.docx` towards the client, the named
    /// format away from it).
    Converted,
    /// [`PdfRenderRequest`] outcome: the raw `.pdf` file.
    Pdf,
    /// The open document's page count and page sizes ([`ViewOpenedReply`]).
    ViewOpened,
    /// One painted page: RGBA bytes then a trailer ([`finish_view_page`]).
    ViewPage,
    /// [`Kind::ViewMissingFonts`] outcome ([`ViewFontsReply`]).
    ViewFonts,
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
        matches!(
            self,
            Kind::Check
                | Kind::Cancel
                | Kind::Configure
                | Kind::Convert
                | Kind::RenderPdf
                | Kind::Ping
                | Kind::ViewOpen
                | Kind::ViewRender
                | Kind::ViewInstallFont
                | Kind::ViewMissingFonts
        )
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
    /// For a strict job, every face the client's own PDF of this document draws text in, as
    /// `(typographic family, style)` (`wordcraft_engine::strict::pdf_faces`), at most
    /// [`MAX_DRAWN_FACES`]: the worker fails the job when a copy it could not install is one of
    /// them. `None` when the client does not know the whole list (an older client, or a list it
    /// had to cut): a strict job then fails when any copy could not be installed. Rides in the
    /// JSON part as `drawn`.
    pub drawn: Option<Vec<(String, String)>>,
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
    /// is drawn in ([`PdfRenderRequest::drawn`]; any, without that list), or a character then has
    /// no face to draw it, rather than let that text fall back. Off, such a copy is skipped and
    /// the export runs. An older worker ignores it.
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
    /// [`PdfRenderRequest::drawn`]; absent from an older client's request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    drawn: Option<Vec<FontMeta>>,
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
        drawn: req.drawn.as_ref().map(|d| d.iter().map(|(family, style)| FontMeta { family: family.clone(), style: style.clone() }).collect()),
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
    if let Some(drawn) = &meta.drawn
        && drawn.len() > MAX_DRAWN_FACES
    {
        return Err(ProtoError::TooLarge { limit: MAX_DRAWN_FACES, got: drawn.len() });
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
        drawn: meta.drawn.map(|d| d.into_iter().map(|f| (f.family, f.style)).collect()),
    })
}

/// `viewOpen` payload (plan 10.3): open `docx` in the view-only worker.
///
/// On the wire the payload is a [`pack_binary`] frame of two parts, the JSON metadata
/// (`{password}`) and the raw file, so the document never has to survive JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewOpenRequest {
    /// Password for an encrypted package, at most [`MAX_PASSWORD_BYTES`].
    pub password: Option<String>,
    /// The `.docx` file, 1 to [`MAX_VIEW_DOC_BYTES`] bytes.
    pub docx: Vec<u8>,
}

/// The JSON part of a `viewOpen` payload.
#[derive(Serialize, Deserialize)]
struct ViewOpenMeta {
    #[serde(default)]
    password: Option<String>,
}

/// Encode a `viewOpen` payload: a two-part [`pack_binary`] frame, JSON metadata then the file.
pub fn encode_view_open_request(req: &ViewOpenRequest) -> Result<Vec<u8>, ProtoError> {
    let meta = encode_payload(&ViewOpenMeta { password: req.password.clone() })?;
    pack_binary(&[&meta, &req.docx])
}

/// Everything of a `viewOpen` payload that comes before the file's bytes, for a file of
/// `docx_len` bytes: `prefix ‖ file` is exactly what [`encode_view_open_request`] returns.
///
/// For a client that holds the file outside its own memory (a page holding a `Uint8Array`):
/// it writes this prefix and the file into one buffer, and the file is never copied through
/// the client's heap. The same bounds as decoding apply.
pub fn view_open_request_prefix(password: Option<&str>, docx_len: usize) -> Result<Vec<u8>, ProtoError> {
    if let Some(password) = password
        && password.len() > MAX_PASSWORD_BYTES
    {
        return Err(ProtoError::TooLarge { limit: MAX_PASSWORD_BYTES, got: password.len() });
    }
    if docx_len > MAX_VIEW_DOC_BYTES {
        return Err(ProtoError::TooLarge { limit: MAX_VIEW_DOC_BYTES, got: docx_len });
    }
    if docx_len == 0 {
        return Err(ProtoError::Decode("viewOpen payload has an empty .docx".into()));
    }
    let meta = encode_payload(&ViewOpenMeta { password: password.map(str::to_string) })?;
    let len = |n: usize| u32::try_from(n).map_err(|_| ProtoError::TooLarge { limit: u32::MAX as usize, got: n });
    let mut out = Vec::with_capacity(BINARY_MAGIC.len() + 12 + meta.len());
    out.extend_from_slice(&BINARY_MAGIC);
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&len(meta.len())?.to_le_bytes());
    out.extend_from_slice(&len(docx_len)?.to_le_bytes());
    out.extend_from_slice(&meta);
    Ok(out)
}

/// Decode a `viewOpen` payload. The bounds are checked on the borrowed frame slices, so an
/// over-limit request is refused before the file is copied.
pub fn decode_view_open_request(bytes: &[u8]) -> Result<ViewOpenRequest, ProtoError> {
    let parts = unpack_binary(bytes)?;
    let [meta, docx] = parts.as_slice() else {
        return Err(ProtoError::Decode(format!("viewOpen payload has {} parts, expected 2", parts.len())));
    };
    let meta: ViewOpenMeta = decode_payload(meta)?;
    if let Some(password) = meta.password.as_deref()
        && password.len() > MAX_PASSWORD_BYTES
    {
        return Err(ProtoError::TooLarge { limit: MAX_PASSWORD_BYTES, got: password.len() });
    }
    if docx.len() > MAX_VIEW_DOC_BYTES {
        return Err(ProtoError::TooLarge { limit: MAX_VIEW_DOC_BYTES, got: docx.len() });
    }
    if docx.is_empty() {
        return Err(ProtoError::Decode("viewOpen payload has an empty .docx".into()));
    }
    Ok(ViewOpenRequest { password: meta.password, docx: docx.to_vec() })
}

/// One page's size in points.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ViewPageSize {
    pub width: f32,
    pub height: f32,
}

/// `viewOpened` reply payload (JSON): what the worker now holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ViewOpenedReply {
    /// How many pages the document has.
    pub page_count: u32,
    /// The size of each page, in order: all of them, or the first [`MAX_VIEW_PAGES`] of a
    /// longer document.
    #[serde(default)]
    pub sizes: Vec<ViewPageSize>,
}

impl ViewOpenedReply {
    /// The reply for a document whose pages have these sizes: the real count, and at most
    /// [`MAX_VIEW_PAGES`] sizes.
    pub fn new(sizes: impl IntoIterator<Item = ViewPageSize>) -> ViewOpenedReply {
        let mut kept = Vec::new();
        let mut count = 0usize;
        for size in sizes {
            if count < MAX_VIEW_PAGES {
                kept.push(size);
            }
            count = count.saturating_add(1);
        }
        ViewOpenedReply { page_count: u32::try_from(count).unwrap_or(u32::MAX), sizes: kept }
    }
}

/// Validate a [`ViewOpenedReply`] before a client lays pages out from it: `TooLarge` past
/// [`MAX_VIEW_PAGES`] sizes, `BadRequest` for more sizes than pages or a size that is not a
/// positive finite number.
pub fn view_opened_reply_error(reply: &ViewOpenedReply) -> Option<ErrorCode> {
    if reply.sizes.len() > MAX_VIEW_PAGES {
        return Some(ErrorCode::TooLarge);
    }
    if reply.sizes.len() > reply.page_count as usize {
        return Some(ErrorCode::BadRequest);
    }
    let usable = |v: f32| v.is_finite() && v > 0.0;
    if reply.sizes.iter().any(|s| !usable(s.width) || !usable(s.height)) {
        return Some(ErrorCode::BadRequest);
    }
    None
}

/// `viewRender` payload (JSON): paint page `page` (0-based) at `scale` pixels per point.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ViewRenderRequest {
    pub page: u32,
    pub scale: f32,
}

/// Validate a [`ViewRenderRequest`] before a worker runs it: `BadRequest` for a scale that is
/// not a finite number in `(0, MAX_VIEW_SCALE]`. The page index is checked against the open
/// document, and the pixel count against [`MAX_VIEW_PIXELS`], by the worker.
pub fn view_render_request_error(req: &ViewRenderRequest) -> Option<ErrorCode> {
    if !req.scale.is_finite() || req.scale <= 0.0 || req.scale > MAX_VIEW_SCALE {
        return Some(ErrorCode::BadRequest);
    }
    None
}

/// What the trailer of a `viewPage` payload says about the pixels before it.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewPageHeader {
    /// The page painted (0-based), echoed from the request.
    pub page: u32,
    /// The scale it was painted at, echoed from the request.
    pub scale: f32,
    /// Width and height in pixels.
    pub width: u32,
    pub height: u32,
}

impl ViewPageHeader {
    /// `width * height * 4`, or the error a header outside the bounds gets: a side of zero,
    /// more than [`MAX_VIEW_PIXELS`], or a scale that is not a positive finite number.
    fn rgba_len(&self) -> Result<usize, ProtoError> {
        let pixels = u64::from(self.width).saturating_mul(u64::from(self.height));
        if pixels > MAX_VIEW_PIXELS {
            return Err(ProtoError::TooLarge { limit: MAX_VIEW_PIXELS as usize, got: usize::try_from(pixels).unwrap_or(usize::MAX) });
        }
        if pixels == 0 {
            return Err(ProtoError::Decode("viewPage is zero pixels".into()));
        }
        if !self.scale.is_finite() || self.scale <= 0.0 {
            return Err(ProtoError::Decode("viewPage scale is not a positive number".into()));
        }
        usize::try_from(pixels.saturating_mul(4)).map_err(|_| ProtoError::TooLarge { limit: MAX_VIEW_PIXELS as usize, got: usize::MAX })
    }
}

/// Turn a page's pixels into a `viewPage` payload: the RGBA bytes (not premultiplied, rows top
/// to bottom, what `ImageData` takes) followed by a [`VIEW_PAGE_TRAILER_BYTES`] trailer of four
/// little-endian words: width, height, page, and the scale's `f32` bits.
///
/// The pixels lead so a client builds its `ImageData` over the transferred buffer in place,
/// and the description trails so appending it does not move them. `pixels` must be exactly
/// `width * height * 4` bytes.
pub fn finish_view_page(mut pixels: Vec<u8>, header: &ViewPageHeader) -> Result<Vec<u8>, ProtoError> {
    let expected = header.rgba_len()?;
    if pixels.len() != expected {
        return Err(ProtoError::Decode(format!("viewPage has {} pixel bytes, expected {expected}", pixels.len())));
    }
    pixels
        .try_reserve_exact(VIEW_PAGE_TRAILER_BYTES)
        .map_err(|_| ProtoError::TooLarge { limit: expected, got: expected + VIEW_PAGE_TRAILER_BYTES })?;
    pixels.extend_from_slice(&header.width.to_le_bytes());
    pixels.extend_from_slice(&header.height.to_le_bytes());
    pixels.extend_from_slice(&header.page.to_le_bytes());
    pixels.extend_from_slice(&header.scale.to_bits().to_le_bytes());
    Ok(pixels)
}

/// Read the trailer of a `viewPage` payload that is `payload_len` bytes long in all. A client
/// copies only these [`VIEW_PAGE_TRAILER_BYTES`] bytes out of the transferred buffer; the
/// pixels are the `payload_len - VIEW_PAGE_TRAILER_BYTES` bytes before them.
pub fn decode_view_page_trailer(trailer: &[u8], payload_len: usize) -> Result<ViewPageHeader, ProtoError> {
    let Ok(words) = <&[u8; VIEW_PAGE_TRAILER_BYTES]>::try_from(trailer) else {
        return Err(ProtoError::Decode(format!("viewPage trailer is {} bytes, expected {VIEW_PAGE_TRAILER_BYTES}", trailer.len())));
    };
    let word = |i: usize| u32::from_le_bytes([words[i], words[i + 1], words[i + 2], words[i + 3]]);
    let header = ViewPageHeader { width: word(0), height: word(4), page: word(8), scale: f32::from_bits(word(12)) };
    let expected = header.rgba_len()?;
    if payload_len.checked_sub(VIEW_PAGE_TRAILER_BYTES) != Some(expected) {
        return Err(ProtoError::Decode(format!(
            "viewPage payload is {payload_len} bytes; {}×{} pixels need {expected} and the trailer",
            header.width, header.height
        )));
    }
    Ok(header)
}

/// The header and the pixels of a `viewPage` payload ([`finish_view_page`]).
pub fn split_view_page(payload: &[u8]) -> Result<(ViewPageHeader, &[u8]), ProtoError> {
    let Some(at) = payload.len().checked_sub(VIEW_PAGE_TRAILER_BYTES) else {
        return Err(ProtoError::Decode(format!("viewPage payload is {} bytes, shorter than its trailer", payload.len())));
    };
    let (pixels, trailer) = payload.split_at_checked(at).ok_or_else(|| ProtoError::Decode("viewPage payload is truncated".into()))?;
    let header = decode_view_page_trailer(trailer, payload.len())?;
    Ok((header, pixels))
}

/// `viewInstallFont` payload: make the font file `bytes` available as `name`.
///
/// On the wire the payload is a [`pack_binary`] frame of two parts, the JSON metadata
/// (`{name}`) and the raw file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewInstallFontRequest {
    /// The family the face answers to, 1 to [`MAX_VIEW_FONT_NAME_BYTES`] bytes.
    pub name: String,
    /// TTF, OTF, TTC, WOFF or WOFF2 file bytes, 1 to [`MAX_VIEW_FONT_BYTES`].
    pub bytes: Vec<u8>,
}

/// The JSON part of a `viewInstallFont` payload.
#[derive(Serialize, Deserialize)]
struct ViewFontMeta {
    name: String,
}

/// Encode a `viewInstallFont` payload: JSON metadata then the font file.
pub fn encode_view_install_font_request(req: &ViewInstallFontRequest) -> Result<Vec<u8>, ProtoError> {
    let meta = encode_payload(&ViewFontMeta { name: req.name.clone() })?;
    pack_binary(&[&meta, &req.bytes])
}

/// Decode a `viewInstallFont` payload, bounds checked on the borrowed frame slices.
pub fn decode_view_install_font_request(bytes: &[u8]) -> Result<ViewInstallFontRequest, ProtoError> {
    let parts = unpack_binary(bytes)?;
    let [meta, font] = parts.as_slice() else {
        return Err(ProtoError::Decode(format!("viewInstallFont payload has {} parts, expected 2", parts.len())));
    };
    let meta: ViewFontMeta = decode_payload(meta)?;
    if meta.name.len() > MAX_VIEW_FONT_NAME_BYTES {
        return Err(ProtoError::TooLarge { limit: MAX_VIEW_FONT_NAME_BYTES, got: meta.name.len() });
    }
    if font.len() > MAX_VIEW_FONT_BYTES {
        return Err(ProtoError::TooLarge { limit: MAX_VIEW_FONT_BYTES, got: font.len() });
    }
    if meta.name.is_empty() {
        return Err(ProtoError::Decode("viewInstallFont payload has an empty name".into()));
    }
    if font.is_empty() {
        return Err(ProtoError::Decode("viewInstallFont payload has an empty font file".into()));
    }
    Ok(ViewInstallFontRequest { name: meta.name, bytes: font.to_vec() })
}

/// `viewFonts` reply payload (JSON): the fonts the open document asks for that aren't
/// available, at most [`MAX_VIEW_MISSING_FONTS`] of them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewFontsReply {
    #[serde(default)]
    pub missing: Vec<String>,
}

impl ViewFontsReply {
    /// The reply for these names, cut at [`MAX_VIEW_MISSING_FONTS`].
    pub fn new(missing: impl IntoIterator<Item = String>) -> ViewFontsReply {
        ViewFontsReply { missing: missing.into_iter().take(MAX_VIEW_MISSING_FONTS).collect() }
    }
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
        Kind::Check | Kind::Cancel | Kind::Configure | Kind::Ping | Kind::ViewRender | Kind::ViewMissingFonts => MAX_PAYLOAD_BYTES,
        // Two-part frames like `convert`: 16 bytes of framing, the JSON metadata part and the
        // file.
        Kind::ViewOpen => MAX_VIEW_DOC_BYTES + MAX_PAYLOAD_BYTES + 16,
        Kind::ViewInstallFont => MAX_VIEW_FONT_BYTES + MAX_PAYLOAD_BYTES + 16,
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
        Kind::Issues | Kind::Ack | Kind::Pong | Kind::Ready | Kind::Error | Kind::ViewOpened | Kind::ViewFonts => MAX_PAYLOAD_BYTES,
        // RGBA pixels and their trailer.
        Kind::ViewPage => usize::try_from(MAX_VIEW_PIXELS.saturating_mul(4)).unwrap_or(usize::MAX).saturating_add(VIEW_PAGE_TRAILER_BYTES),
        Kind::Check
        | Kind::Cancel
        | Kind::Configure
        | Kind::Convert
        | Kind::RenderPdf
        | Kind::Ping
        | Kind::ViewOpen
        | Kind::ViewRender
        | Kind::ViewInstallFont
        | Kind::ViewMissingFonts => 0,
    }
}

/// Validate a [`PdfRenderRequest`] against the protocol bounds before a worker runs it, like
/// [`check_request_error`]: `TooLarge` past [`MAX_PDF_DOC_BYTES`], [`MAX_FONT_COPIES`],
/// [`MAX_FONT_TOTAL_BYTES`] or [`MAX_DRAWN_FACES`], `BadRequest` for an empty package.
pub fn pdf_request_error(req: &PdfRenderRequest) -> Option<ErrorCode> {
    let font_total = req.fonts.iter().fold(0usize, |a, f| a.saturating_add(f.bytes.len()));
    let drawn = req.drawn.as_ref().map_or(0, Vec::len);
    if req.docx.len() > MAX_PDF_DOC_BYTES || req.fonts.len() > MAX_FONT_COPIES || font_total > MAX_FONT_TOTAL_BYTES || drawn > MAX_DRAWN_FACES {
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
    /// The document is encrypted and the request carried no password (view-only worker).
    PasswordRequired,
    /// The password doesn't open the document (view-only worker).
    WrongPassword,
    /// The request needs an open document and the worker holds none (view-only worker).
    NoDocument,
    /// The bytes aren't a document, or a font, the worker can read (view-only worker).
    Unreadable,
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
            drawn: Some(vec![("Inter".into(), "Regular".into()), ("Source Serif 4".into(), "Bold Italic".into())]),
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
        assert_eq!(meta["drawn"][1]["style"], "Bold Italic");
        let bare = PdfRenderRequest { fonts: Vec::new(), drawn: None, ..req };
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
            let req = PdfRenderRequest { docx: b"PK".to_vec(), fonts: Vec::new(), options, drawn: None };
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
                    drawn: None,
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
                drawn: Some(vec![("Inter".into(), "Regular".into())]),
            })
            .unwrap(),
        )
        .unwrap()[0]
            .to_vec();
        let old: OldPdfMeta = decode_payload(&new_meta).unwrap();
        assert!(old.fonts.is_empty());
    }

    /// Review finding (gate 3, the strict font check): a strict job carries the faces the
    /// client's PDF draws text in, so the worker decides from what the layout resolved rather
    /// than from the font names the document asks for. The list is additive both ways (an older
    /// client sends none: `None`, "not known"), bounded, and a junk list is a decode error.
    #[test]
    fn pdf_request_carries_the_faces_its_text_is_drawn_in() {
        let frame = |meta: serde_json::Value| pack_binary(&[&encode_payload(&meta).unwrap(), b"PK"]).unwrap();
        assert_eq!(decode_pdf_request(&frame(serde_json::json!({"fonts": []}))).unwrap().drawn, None);
        assert_eq!(decode_pdf_request(&frame(serde_json::json!({"fonts": [], "drawn": []}))).unwrap().drawn, Some(Vec::new()));
        let drawn = decode_pdf_request(&frame(serde_json::json!({"fonts": [], "drawn": [{"family": "Corrupt Copy Sans", "style": "SemiBold"}]})))
            .unwrap()
            .drawn;
        assert_eq!(drawn, Some(vec![("Corrupt Copy Sans".to_string(), "SemiBold".to_string())]));
        // No list on the wire when there is none.
        let none = PdfRenderRequest { docx: b"PK".to_vec(), fonts: Vec::new(), options: PdfJobOptions::default(), drawn: None };
        let meta: serde_json::Value = serde_json::from_slice(unpack_binary(&encode_pdf_request(&none).unwrap()).unwrap()[0]).unwrap();
        assert!(meta.get("drawn").is_none());
        for junk in [serde_json::json!(5), serde_json::json!(["Inter"]), serde_json::json!([{"family": "Inter"}])] {
            assert!(matches!(decode_pdf_request(&frame(serde_json::json!({"fonts": [], "drawn": junk}))), Err(ProtoError::Decode(_))), "{junk}");
        }
        let many: Vec<serde_json::Value> = (0..=MAX_DRAWN_FACES).map(|_| serde_json::json!({"family": "a", "style": "b"})).collect();
        assert!(matches!(
            decode_pdf_request(&frame(serde_json::json!({"fonts": [], "drawn": many}))),
            Err(ProtoError::TooLarge { limit: MAX_DRAWN_FACES, .. })
        ));
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
            drawn: Some(vec![("a".into(), "b".into()); MAX_DRAWN_FACES]),
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
        assert_eq!(
            pdf_request_error(&PdfRenderRequest { drawn: Some(vec![(String::new(), String::new()); MAX_DRAWN_FACES + 1]), ..ok.clone() }),
            Some(ErrorCode::TooLarge)
        );
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

    // ---- The view-only worker's kinds (plan 10.3) ----

    #[test]
    fn view_kinds_split_requests_from_replies() {
        for k in [Kind::ViewOpen, Kind::ViewRender, Kind::ViewInstallFont, Kind::ViewMissingFonts] {
            assert!(k.is_request(), "{k:?}");
        }
        for k in [Kind::ViewOpened, Kind::ViewPage, Kind::ViewFonts] {
            assert!(!k.is_request(), "{k:?}");
            assert_eq!(request_payload_limit(k), 0, "{k:?}");
        }
        // The names on the wire are camelCase, like every other kind.
        let h = encode_envelope(&Envelope::request(4, Kind::ViewInstallFont, 0)).unwrap();
        assert!(std::str::from_utf8(&h).unwrap().contains(r#""kind":"viewInstallFont""#));
    }

    #[test]
    fn view_envelopes_roundtrip_and_reject_other_versions() {
        for kind in
            [Kind::ViewOpen, Kind::ViewRender, Kind::ViewInstallFont, Kind::ViewMissingFonts, Kind::ViewOpened, Kind::ViewPage, Kind::ViewFonts]
        {
            let env = Envelope::request(7, kind, 3);
            assert_eq!(decode_envelope(&encode_envelope(&env).unwrap()).unwrap(), env);
            let mut other = env.clone();
            other.v = PROTOCOL_VERSION + 1;
            assert!(matches!(decode_envelope(&encode_envelope(&other).unwrap()), Err(ProtoError::Version { .. })), "{kind:?}");
        }
    }

    #[test]
    fn view_open_request_roundtrip_and_bounds() {
        let req = ViewOpenRequest { password: Some("pw".into()), docx: b"PK\x03\x04 not really".to_vec() };
        let frame = encode_view_open_request(&req).unwrap();
        assert_eq!(decode_view_open_request(&frame).unwrap(), req);
        let plain = ViewOpenRequest { password: None, docx: vec![1, 2, 3] };
        assert_eq!(decode_view_open_request(&encode_view_open_request(&plain).unwrap()).unwrap(), plain);

        // An empty package and a missing part are decode errors; over the caps is TooLarge,
        // decided on the borrowed frame before the bytes are copied.
        let meta = encode_payload(&serde_json::json!({})).unwrap();
        assert!(matches!(decode_view_open_request(&pack_binary(&[&meta, &[][..]]).unwrap()), Err(ProtoError::Decode(_))));
        assert!(matches!(decode_view_open_request(&pack_binary(&[&meta]).unwrap()), Err(ProtoError::Decode(_))));
        assert!(matches!(decode_view_open_request(&pack_binary(&[&meta, b"x", b"y"]).unwrap()), Err(ProtoError::Decode(_))));
        let big = vec![0u8; MAX_VIEW_DOC_BYTES + 1];
        assert!(matches!(
            decode_view_open_request(&pack_binary(&[&meta, &big]).unwrap()),
            Err(ProtoError::TooLarge { limit: MAX_VIEW_DOC_BYTES, .. })
        ));
        let long = encode_payload(&serde_json::json!({"password": "p".repeat(MAX_PASSWORD_BYTES + 1)})).unwrap();
        assert!(matches!(
            decode_view_open_request(&pack_binary(&[&long, b"x"]).unwrap()),
            Err(ProtoError::TooLarge { limit: MAX_PASSWORD_BYTES, .. })
        ));

        // The pre-copy limit covers the largest frame decode accepts, and nothing larger.
        let at_cap = vec![0u8; MAX_VIEW_DOC_BYTES];
        let wide_meta = vec![b' '; MAX_PAYLOAD_BYTES];
        assert!(pack_binary(&[&wide_meta, &at_cap]).unwrap().len() <= request_payload_limit(Kind::ViewOpen));
        assert!(pack_binary(&[&wide_meta, &big]).unwrap().len() > request_payload_limit(Kind::ViewOpen));
    }

    #[test]
    fn view_open_prefix_plus_the_file_is_the_encoded_request() {
        for password in [None, Some("pw"), Some("")] {
            let req = ViewOpenRequest { password: password.map(str::to_string), docx: (0..=255).collect() };
            let mut built = view_open_request_prefix(password, req.docx.len()).unwrap();
            built.extend_from_slice(&req.docx);
            assert_eq!(built, encode_view_open_request(&req).unwrap(), "{password:?}");
            assert_eq!(decode_view_open_request(&built).unwrap(), req);
        }
        assert!(matches!(view_open_request_prefix(None, 0), Err(ProtoError::Decode(_))));
        assert!(matches!(view_open_request_prefix(None, MAX_VIEW_DOC_BYTES + 1), Err(ProtoError::TooLarge { limit: MAX_VIEW_DOC_BYTES, .. })));
        assert!(view_open_request_prefix(None, MAX_VIEW_DOC_BYTES).is_ok());
        let long = "p".repeat(MAX_PASSWORD_BYTES + 1);
        assert!(matches!(view_open_request_prefix(Some(&long), 1), Err(ProtoError::TooLarge { limit: MAX_PASSWORD_BYTES, .. })));
    }

    #[test]
    fn view_render_request_roundtrip_and_bounds() {
        let req = ViewRenderRequest { page: 3, scale: 1.3333334 };
        let back: ViewRenderRequest = decode_payload(&encode_payload(&req).unwrap()).unwrap();
        assert_eq!(back, req, "the scale crosses as the same f32");
        assert_eq!(view_render_request_error(&req), None);
        assert_eq!(view_render_request_error(&ViewRenderRequest { page: 0, scale: MAX_VIEW_SCALE }), None);
        for scale in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, MAX_VIEW_SCALE * 1.01] {
            assert_eq!(view_render_request_error(&ViewRenderRequest { page: 0, scale }), Some(ErrorCode::BadRequest), "{scale}");
        }
        // JSON has no NaN: a host that sends `null`, a string or nothing gets a decode error.
        for junk in [
            &br#"{"page":0,"scale":null}"#[..],
            br#"{"page":0,"scale":"2"}"#,
            br#"{"page":0}"#,
            br#"{"page":-1,"scale":1}"#,
            br#"{"page":4294967296,"scale":1}"#,
            b"[]",
            b"",
        ] {
            assert!(decode_payload::<ViewRenderRequest>(junk).is_err(), "{}", String::from_utf8_lossy(junk));
        }
        assert_eq!(request_payload_limit(Kind::ViewRender), MAX_PAYLOAD_BYTES);
        assert_eq!(request_payload_limit(Kind::ViewMissingFonts), MAX_PAYLOAD_BYTES);
    }

    #[test]
    fn view_page_payload_roundtrip() {
        let header = ViewPageHeader { page: 2, scale: 1.5, width: 3, height: 2 };
        let pixels: Vec<u8> = (0..24).collect();
        let payload = finish_view_page(pixels.clone(), &header).unwrap();
        assert_eq!(payload.len(), pixels.len() + VIEW_PAGE_TRAILER_BYTES);
        assert_eq!(&payload[..pixels.len()], &pixels[..], "the pixels lead the payload, so a host views them in place");
        let (back, rgba) = split_view_page(&payload).unwrap();
        assert_eq!(back, header);
        assert_eq!(rgba, &pixels[..]);
        // A client reads only the trailer out of the transferred buffer.
        let trailer = &payload[payload.len() - VIEW_PAGE_TRAILER_BYTES..];
        assert_eq!(decode_view_page_trailer(trailer, payload.len()).unwrap(), header);
    }

    #[test]
    fn view_page_payload_rejects_junk_without_panicking() {
        let header = ViewPageHeader { page: 0, scale: 1.0, width: 3, height: 2 };
        // The pixel count must be exactly width × height × 4.
        assert!(finish_view_page(vec![0; 23], &header).is_err());
        assert!(finish_view_page(vec![0; 25], &header).is_err());
        assert!(finish_view_page(Vec::new(), &ViewPageHeader { width: 0, ..header.clone() }).is_err(), "an empty page is not a page");
        // Over the pixel cap is refused from the header alone, before any buffer is sized.
        let huge = ViewPageHeader { page: 0, scale: 1.0, width: u32::MAX, height: u32::MAX };
        assert!(matches!(finish_view_page(Vec::new(), &huge), Err(ProtoError::TooLarge { .. })));

        let good = finish_view_page(vec![9; 24], &header).unwrap();
        for cut in 0..good.len() {
            assert!(split_view_page(&good[..cut]).is_err(), "truncated at {cut}");
        }
        let mut longer = good.clone();
        longer.insert(0, 0);
        assert!(split_view_page(&longer).is_err(), "one stray byte");
        // A trailer that claims more pixels than the payload carries, or more than the cap.
        let mut lying = good.clone();
        let at = lying.len() - VIEW_PAGE_TRAILER_BYTES;
        lying[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(split_view_page(&lying).is_err());
        assert!(decode_view_page_trailer(&lying[at..], lying.len()).is_err());
        assert!(decode_view_page_trailer(&[], 0).is_err());
        assert!(decode_view_page_trailer(&[0; VIEW_PAGE_TRAILER_BYTES], VIEW_PAGE_TRAILER_BYTES).is_err(), "zero by zero");
        // A scale that is not a usable number.
        let mut nan = good.clone();
        nan[at + 12..at + 16].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(split_view_page(&nan).is_err());
    }

    #[test]
    fn view_opened_reply_roundtrip_and_bounds() {
        let reply = ViewOpenedReply {
            page_count: 2,
            sizes: vec![ViewPageSize { width: 612.0, height: 792.0 }, ViewPageSize { width: 595.3, height: 841.9 }],
        };
        let back: ViewOpenedReply = decode_payload(&encode_payload(&reply).unwrap()).unwrap();
        assert_eq!(back, reply);
        assert_eq!(view_opened_reply_error(&reply), None);
        // More sizes than pages, more than the cap, or a size that is not a positive number.
        assert!(view_opened_reply_error(&ViewOpenedReply { page_count: 1, ..reply.clone() }).is_some());
        let many = ViewOpenedReply { page_count: u32::MAX, sizes: vec![ViewPageSize { width: 1.0, height: 1.0 }; MAX_VIEW_PAGES + 1] };
        assert_eq!(view_opened_reply_error(&many), Some(ErrorCode::TooLarge));
        for bad in [0.0, -5.0, f32::NAN, f32::INFINITY] {
            let r = ViewOpenedReply { page_count: 1, sizes: vec![ViewPageSize { width: bad, height: 10.0 }] };
            assert_eq!(view_opened_reply_error(&r), Some(ErrorCode::BadRequest), "{bad}");
        }
        // A long document reports its real count and the first MAX_VIEW_PAGES sizes.
        let capped = ViewOpenedReply::new((0..MAX_VIEW_PAGES + 5).map(|_| ViewPageSize { width: 10.0, height: 20.0 }));
        assert_eq!(capped.page_count as usize, MAX_VIEW_PAGES + 5);
        assert_eq!(capped.sizes.len(), MAX_VIEW_PAGES);
        assert_eq!(view_opened_reply_error(&capped), None);
        assert!(encode_payload(&capped).unwrap().len() <= MAX_PAYLOAD_BYTES, "a reply at the cap still fits a JSON payload");
    }

    #[test]
    fn view_install_font_request_roundtrip_and_bounds() {
        let req = ViewInstallFontRequest { name: "Alder Sans".into(), bytes: vec![0, 1, 0, 0, 7] };
        assert_eq!(decode_view_install_font_request(&encode_view_install_font_request(&req).unwrap()).unwrap(), req);

        let meta = encode_payload(&serde_json::json!({"name": "A"})).unwrap();
        assert!(matches!(decode_view_install_font_request(&pack_binary(&[&meta, &[][..]]).unwrap()), Err(ProtoError::Decode(_))));
        assert!(matches!(decode_view_install_font_request(&pack_binary(&[&meta]).unwrap()), Err(ProtoError::Decode(_))));
        let empty_name = encode_payload(&serde_json::json!({"name": ""})).unwrap();
        assert!(matches!(decode_view_install_font_request(&pack_binary(&[&empty_name, b"x"]).unwrap()), Err(ProtoError::Decode(_))));
        let long_name = encode_payload(&serde_json::json!({"name": "n".repeat(MAX_VIEW_FONT_NAME_BYTES + 1)})).unwrap();
        assert!(matches!(
            decode_view_install_font_request(&pack_binary(&[&long_name, b"x"]).unwrap()),
            Err(ProtoError::TooLarge { limit: MAX_VIEW_FONT_NAME_BYTES, .. })
        ));
        let big = vec![0u8; MAX_VIEW_FONT_BYTES + 1];
        assert!(matches!(
            decode_view_install_font_request(&pack_binary(&[&meta, &big]).unwrap()),
            Err(ProtoError::TooLarge { limit: MAX_VIEW_FONT_BYTES, .. })
        ));
        let wide_meta = vec![b' '; MAX_PAYLOAD_BYTES];
        assert!(pack_binary(&[&wide_meta, &big[..MAX_VIEW_FONT_BYTES]]).unwrap().len() <= request_payload_limit(Kind::ViewInstallFont));
        assert!(pack_binary(&[&wide_meta, &big]).unwrap().len() > request_payload_limit(Kind::ViewInstallFont));
    }

    #[test]
    fn view_fonts_reply_roundtrip_and_cap() {
        let reply = ViewFontsReply::new(["Calibri".to_string(), "Aptos".to_string()]);
        let back: ViewFontsReply = decode_payload(&encode_payload(&reply).unwrap()).unwrap();
        assert_eq!(back, reply);
        let capped = ViewFontsReply::new((0..MAX_VIEW_MISSING_FONTS + 9).map(|i| format!("Font {i}")));
        assert_eq!(capped.missing.len(), MAX_VIEW_MISSING_FONTS);
    }

    #[test]
    fn view_decoders_never_panic_on_junk() {
        for junk in [&b""[..], b"\0", b"WCB", b"WCB\x01", b"WCB\x01\xff\xff\xff\xff", b"{}", b"null", &[0xff; 64]] {
            assert!(decode_view_open_request(junk).is_err());
            assert!(decode_view_install_font_request(junk).is_err());
            assert!(split_view_page(junk).is_err());
            let _ = decode_payload::<ViewRenderRequest>(junk);
            let _ = decode_payload::<ViewOpenedReply>(junk);
            let _ = decode_payload::<ViewFontsReply>(junk);
        }
    }

    #[test]
    fn view_error_codes_cross_the_wire() {
        for code in [ErrorCode::PasswordRequired, ErrorCode::WrongPassword, ErrorCode::NoDocument, ErrorCode::Unreadable] {
            let reply = ErrorReply { code, message: "why".into() };
            let back: ErrorReply = decode_payload(&encode_payload(&reply).unwrap()).unwrap();
            assert_eq!(back, reply);
        }
        let json = String::from_utf8(encode_payload(&ErrorReply { code: ErrorCode::PasswordRequired, message: String::new() }).unwrap()).unwrap();
        assert!(json.contains(r#""passwordRequired""#), "{json}");
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
        assert_eq!(reply_payload_limit(Kind::ViewOpened), MAX_PAYLOAD_BYTES);
        assert_eq!(reply_payload_limit(Kind::ViewFonts), MAX_PAYLOAD_BYTES);
        assert_eq!(reply_payload_limit(Kind::ViewPage), MAX_VIEW_PIXELS as usize * 4 + VIEW_PAGE_TRAILER_BYTES);
        for kind in [Kind::ViewOpen, Kind::ViewRender, Kind::ViewInstallFont, Kind::ViewMissingFonts] {
            assert_eq!(reply_payload_limit(kind), 0, "{kind:?}");
        }
        for kind in [Kind::Check, Kind::Cancel, Kind::Configure, Kind::Convert, Kind::RenderPdf, Kind::Ping] {
            assert!(kind.is_request());
            assert_eq!(reply_payload_limit(kind), 0, "{kind:?}");
        }
    }
}
