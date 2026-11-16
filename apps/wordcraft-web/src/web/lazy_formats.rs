//! Which files the web shell hands to its on-demand workers (implementation plan 10.2), and the
//! bookkeeping around each job that needs no browser.
//!
//! The browser build leaves the extra formats and the PDF writer out of the main module; the
//! codecs worker converts those formats to and from `.docx` bytes and the PDF worker writes a
//! PDF from `.docx` bytes. This module decides, from a file's name and first bytes, which of
//! them a file belongs to, words what the user is told when a job fails, and keeps the one-answer
//! and deadline rules of a job — no browser types, so host tests cover it. The workers
//! themselves are driven from `lazy_io`.

use std::cell::RefCell;
use std::rc::Rc;

/// What the codecs worker reads, as `(extension, the format id sent to it)`: the format crate's
/// `all-formats` readers, which the main module leaves out, under every extension they answer
/// to. Legacy `.doc` is not here: it is routed by content ([`import_format`]). The ids are the
/// ones `wordcraft_formats::import_on` dispatches on (tests check each against it).
const IMPORTS: &[(&str, &str)] = &[
    ("rtf", "rtf"),
    ("odt", "odt"),
    ("ott", "odt"),
    ("fodt", "fodt"),
    ("html", "html"),
    ("htm", "html"),
    ("xhtml", "html"),
    ("mht", "mht"),
    ("mhtml", "mht"),
    ("tex", "tex"),
    ("latex", "tex"),
    ("ltx", "tex"),
];

/// What the codecs worker writes, as `(extension, the format id sent to it)`: the format crate's
/// `all-formats` writers. A web archive (`.mht`) is read but never written, so it is not here
/// and its save stays with the engine, which refuses it at once. `.ott` keeps its own id: the
/// worker writes a template for it.
const EXPORTS: &[(&str, &str)] = &[
    ("rtf", "rtf"),
    ("odt", "odt"),
    ("ott", "ott"),
    ("fodt", "fodt"),
    ("html", "html"),
    ("htm", "html"),
    ("xhtml", "html"),
    ("tex", "tex"),
    ("latex", "tex"),
    ("ltx", "tex"),
];

const ZIP_SIG: &[u8] = b"PK\x03\x04";
const OLE_SIG: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const RTF_SIG: &[u8] = b"{\\rtf";

/// The part every Word package (OOXML) carries; its name sits in the zip's directory.
const OOXML_CONTENT_TYPES: &[u8] = b"[Content_Types].xml";

/// The lower-case extension of `name`'s last path component (no dot), empty when it has none.
/// A hidden file (`.rtf`, `/home/u/.rtf`) has no extension: its name is all stem.
fn ext_of(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// The OpenDocument text kind a zip package says it is (ODF 1.3 Part 2 §3.3: its first entry is
/// `mimetype`, stored, holding the media type): `odt` for a text document or its template, `None`
/// for anything else (another ODF kind, or no stored `mimetype` first).
fn odf_text_kind(bytes: &[u8]) -> Option<&'static str> {
    let u16_at = |at: usize| bytes.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let u32_at = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    if !bytes.starts_with(ZIP_SIG) || u16_at(8)? != 0 {
        return None;
    }
    let (size, name_len, extra_len) = (u32_at(18)?, u16_at(26)?, u16_at(28)?);
    if bytes.get(30..30 + name_len)? != b"mimetype" {
        return None;
    }
    let start = 30usize.checked_add(name_len)?.checked_add(extra_len)?;
    match bytes.get(start..start.checked_add(size)?)? {
        b"application/vnd.oasis.opendocument.text" | b"application/vnd.oasis.opendocument.text-template" => Some("odt"),
        _ => None,
    }
}

/// Whether a zip package is a Word package: its directory names `[Content_Types].xml`.
fn is_ooxml_package(bytes: &[u8]) -> bool {
    bytes.starts_with(ZIP_SIG) && bytes.windows(OOXML_CONTENT_TYPES.len()).any(|w| w == OOXML_CONTENT_TYPES)
}

/// The format to ask the codecs worker to read `bytes` as, when opening `name` needs it.
///
/// `None` leaves the file to the engine: formats it reads itself, and files whose content says
/// they are a Word package whatever their name (the engine opens those by content, and asks for
/// a password when one is needed). A zip package is what its content says before what its name
/// says: an OpenDocument text under any name is the worker's, a Word package under any name the
/// engine's. `encrypted_package` is whether `bytes` is a password-protected Word package
/// (`wordcraft_docx::is_encrypted`: a compound file holding an `EncryptedPackage` stream),
/// passed in so this stays free of the reader. A password-protected Word 97–2003 binary file is
/// not one of those (its encryption is inside the `WordDocument` stream): it goes to the worker as
/// `doc`, which cannot ask for the password ([`failure_message`] says so).
pub(crate) fn import_format(name: &str, bytes: &[u8], encrypted_package: bool) -> Option<String> {
    if bytes.starts_with(ZIP_SIG) {
        if let Some(kind) = odf_text_kind(bytes) {
            return Some(kind.into());
        }
        if is_ooxml_package(bytes) {
            return None;
        }
    }
    if encrypted_package {
        return None;
    }
    let ext = ext_of(name);
    match ext.as_str() {
        // Word's own extensions are trusted least: an `.rtf` or `.doc` is whatever its first
        // bytes say (Word writes RTF under `.doc`, and packages under either).
        "doc" | "dot" | "rtf" => {
            if bytes.starts_with(RTF_SIG) {
                Some("rtf".into())
            } else if bytes.starts_with(OLE_SIG) {
                Some("doc".into())
            } else {
                None
            }
        }
        other => IMPORTS.iter().find(|(e, _)| *e == other).map(|(_, id)| (*id).to_string()),
    }
}

/// Where a save or export under `name` goes when the main module cannot write it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExportRoute {
    /// The codecs worker writes this format from `.docx` bytes.
    Codec(String),
    /// The PDF worker lays the `.docx` bytes out and writes a PDF.
    Pdf,
}

/// The worker that writes `name`'s format, or `None` when the engine writes it itself (or
/// refuses it at once, as for a format nothing writes).
pub(crate) fn export_route(name: &str) -> Option<ExportRoute> {
    let ext = ext_of(name);
    match ext.as_str() {
        "pdf" => Some(ExportRoute::Pdf),
        other => EXPORTS.iter().find(|(e, _)| *e == other).map(|(_, id)| ExportRoute::Codec((*id).to_string())),
    }
}

/// The `renderPdf` options for the export options a save or export resolved
/// (`wordcraft_engine::cmd::file::export_options`): what the engine's PDF writer applies, and a
/// Word package does not carry. The comments only show with Print Markup, as in the engine.
/// `strict` goes too: the app refuses a strict PDF that would leave something out before the job
/// starts, and the worker refuses one whose font copies it could not install.
pub(crate) fn pdf_job_options(opts: &wordcraft_engine::io::ExportOptions) -> wordcraft_worker_proto::PdfJobOptions {
    use wordcraft_layout::NumeralMode;
    use wordcraft_worker_proto::Numeral;
    let numeral = match opts.numeral {
        NumeralMode::Hindi => Numeral::Hindi,
        NumeralMode::Context => Numeral::Context,
        // `System` is resolved before an export (`export_options`); unresolved, it is the default.
        NumeralMode::Arabic | NumeralMode::System => Numeral::Arabic,
    };
    wordcraft_worker_proto::PdfJobOptions { markup: opts.markup, comments: opts.markup && opts.comments, numeral, strict: opts.strict }
}

/// How many font copies, and how many of their bytes, one `renderPdf` job may carry.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FontBudget {
    pub(crate) count: usize,
    pub(crate) bytes: usize,
}

/// The protocol's budget ([`wordcraft_worker_proto::MAX_FONT_COPIES`],
/// [`wordcraft_worker_proto::MAX_FONT_TOTAL_BYTES`]).
pub(crate) const FONT_BUDGET: FontBudget =
    FontBudget { count: wordcraft_worker_proto::MAX_FONT_COPIES, bytes: wordcraft_worker_proto::MAX_FONT_TOTAL_BYTES };

/// Families a refusal names before it says "…".
const NAMED_FACES: usize = 4;

/// The font copies one PDF job carries ([`font_batch`]), and what was left behind.
#[derive(Debug, Default)]
pub(crate) struct FontBatch {
    pub(crate) copies: Vec<wordcraft_worker_proto::FontCopy>,
    /// Faces the PDF draws text in that did not fit: how many, and the first few families.
    pub(crate) left_used: usize,
    pub(crate) left_used_named: Vec<String>,
    /// Faces the PDF never draws in that did not fit.
    pub(crate) left_unused: usize,
}

impl FontBatch {
    /// A strict export's refusal: some face the PDF draws text in stays behind, so that text
    /// would fall back in the worker. Faces it never draws in refuse nothing.
    pub(crate) fn strict_refusal(&self) -> Option<String> {
        if self.left_used == 0 {
            return None;
        }
        let more = if self.left_used > self.left_used_named.len() { ", …" } else { "" };
        Some(format!(
            "strict: {} font face(s) this PDF draws text in are too many to send to the PDF export, and their text would fall back: {}{more}",
            self.left_used,
            self.left_used_named.join(", ")
        ))
    }
}

/// The copies of `faces` (`(family, style, file bytes)`, the page's runtime faces) that go with
/// a PDF job within `budget`. When they all fit they all go, in their own order, and `used` is
/// never asked (it lays the document out). Otherwise the faces the PDF draws text in (`used`,
/// `(family, style)`) go first, so the budget leaves out faces the document never draws in
/// before any it does; within each group the order is kept.
pub(crate) fn font_batch(
    faces: Vec<(String, String, std::sync::Arc<Vec<u8>>)>,
    used: impl FnOnce() -> Vec<(String, String)>,
    budget: FontBudget,
) -> FontBatch {
    let total = faces.iter().map(|f| f.2.len()).fold(0usize, usize::saturating_add);
    let fits = faces.len() <= budget.count && total <= budget.bytes;
    let ordered: Vec<(bool, (String, String, std::sync::Arc<Vec<u8>>))> = if fits {
        faces.into_iter().map(|f| (false, f)).collect()
    } else {
        let used: std::collections::HashSet<(String, String)> = used().into_iter().collect();
        let (mut first, rest): (Vec<_>, Vec<_>) = faces.into_iter().map(|f| (used.contains(&(f.0.clone(), f.1.clone())), f)).partition(|(u, _)| *u);
        first.extend(rest);
        first
    };
    let mut batch = FontBatch::default();
    let mut bytes = 0usize;
    for (is_used, (family, style, data)) in ordered {
        if batch.copies.len() >= budget.count || bytes.saturating_add(data.len()) > budget.bytes {
            if is_used {
                batch.left_used = batch.left_used.saturating_add(1);
                if batch.left_used_named.len() < NAMED_FACES && !batch.left_used_named.contains(&family) {
                    batch.left_used_named.push(family);
                }
            } else {
                batch.left_unused = batch.left_unused.saturating_add(1);
            }
            continue;
        }
        bytes = bytes.saturating_add(data.len());
        batch.copies.push(wordcraft_worker_proto::FontCopy { family, style, bytes: (*data).clone() });
    }
    batch
}

/// What one PDF job carries for its fonts ([`pdf_fonts`]).
#[derive(Debug)]
pub(crate) struct PdfFonts {
    pub(crate) batch: FontBatch,
    /// [`wordcraft_worker_proto::PdfRenderRequest::drawn`]: for a strict job, the faces its PDF
    /// draws text in; `None` otherwise.
    pub(crate) drawn: Option<Vec<(String, String)>>,
}

/// The font copies of `faces` one PDF job carries within `budget` ([`font_batch`]) and, for a
/// `strict` job, the faces its PDF draws text in (`used`, which lays the document out), so the
/// worker refuses when a copy it cannot install is one of them. A strict job with no copies to
/// lose is not laid out (its list is empty: the worker never reads it). `Err` is a strict
/// job's refusal: a face the text is drawn in does not fit the budget, or the list was cut
/// ([`wordcraft_engine::strict::PdfFaces::cut`]) and some face does not fit, which could be one
/// of those. A cut list whose copies all fit is sent as unknown (`drawn: None`), so the worker
/// refuses on any copy it cannot install.
pub(crate) fn pdf_fonts(
    faces: Vec<(String, String, std::sync::Arc<Vec<u8>>)>,
    used: impl FnOnce() -> wordcraft_engine::strict::PdfFaces,
    budget: FontBudget,
    strict: bool,
) -> Result<PdfFonts, String> {
    if !strict {
        return Ok(PdfFonts { batch: font_batch(faces, || used().faces, budget), drawn: None });
    }
    if faces.is_empty() {
        return Ok(PdfFonts { batch: FontBatch::default(), drawn: Some(Vec::new()) });
    }
    let wordcraft_engine::strict::PdfFaces { faces: drawn, cut } = used();
    let batch = font_batch(faces, || drawn.clone(), budget);
    if let Some(refusal) = batch.strict_refusal() {
        return Err(refusal);
    }
    let left = batch.left_used.saturating_add(batch.left_unused);
    if cut && left > 0 {
        return Err(format!(
            "strict: this PDF draws text in more than {} font faces, so whether the {left} face(s) too many to send to the PDF export are among them cannot be checked",
            wordcraft_engine::strict::MAX_PDF_FACES
        ));
    }
    Ok(PdfFonts { batch, drawn: (!cut).then_some(drawn) })
}

/// The length of the base64 a package of `raw` bytes becomes (padded, as
/// `wordcraft_engine::cmd::insert::base64_encode` writes it); saturates.
pub(crate) fn base64_len(raw: usize) -> usize {
    raw.div_ceil(3).saturating_mul(4)
}

/// The page API's `loadDocx` refusal for a package whose base64 is `base64_len` bytes, before
/// it is copied or encoded: `doc.loadDocx`'s own bound and error
/// (`wordcraft_engine::cmd::file::check_docx_base64_len`), as the command would answer it.
pub(crate) fn load_docx_error(base64_len: usize) -> Option<String> {
    wordcraft_engine::cmd::file::check_docx_base64_len(base64_len).err().map(|e| e.to_string())
}

/// A conversion's refusal for an input of `len` bytes, past
/// [`wordcraft_worker_proto::MAX_CONVERT_BYTES`]: the one check, which the page API makes before
/// copying the input and `lazy_io::convert` before sending it.
pub(crate) fn convert_input_error(len: usize) -> Option<String> {
    (len > wordcraft_worker_proto::MAX_CONVERT_BYTES).then(|| format!("the document is too large to convert in the browser ({len} bytes)"))
}

/// A conversion's refusal for a base64 string of `len` characters, before it is copied or
/// decoded: its shortest decoding (`len / 4 * 3 - 2` bytes, every character in the alphabet,
/// two of padding) past [`wordcraft_worker_proto::MAX_CONVERT_BYTES`], with the conversion's
/// own refusal ([`convert_input_error`]). Whitespace, which decodes to nothing, counts as
/// alphabet here: a string padded with that much of it is refused although its bytes would fit.
pub(crate) fn convert_base64_error(len: usize) -> Option<String> {
    convert_input_error((len / 4).saturating_mul(3).saturating_sub(2))
}

/// A base64 conversion input, decoded: `Err` for a string that is not base64, or whose bytes
/// are past the conversion's cap ([`convert_input_error`]).
pub(crate) fn decode_convert_base64(s: &str) -> Result<Vec<u8>, String> {
    if let Some(refused) = convert_base64_error(s.len()) {
        return Err(refused);
    }
    let bytes = wordcraft_engine::cmd::insert::base64_decode(s).ok_or("convertDocument: `data` is not valid base64")?;
    match convert_input_error(bytes.len()) {
        Some(refused) => Err(refused),
        None => Ok(bytes),
    }
}

/// Most page-API PDF exports waiting at once ([`ApiExports`]).
pub(crate) const MAX_API_EXPORTS: usize = 16;

/// The PDF exports the page API (`exportPdf`) asked for, waiting on the app's own export: the
/// API runs `file.exportPdf` with a tag claimed here (`hostTag`, which the command hands back
/// in its export options), so the document, the options, the strict check and the font copies
/// are the command's, and the finished bytes go back to the API's promise (`sink`) instead of a
/// download. The tag, not the file name, identifies the claim: any caller can choose a name,
/// but the tag is a random number only the API call holds, so an export of the same name is
/// delivered as its own.
pub(crate) struct ApiExports<S> {
    next: u64,
    waiting: Vec<(u64, S)>,
}

impl<S> Default for ApiExports<S> {
    fn default() -> Self {
        ApiExports { next: 0, waiting: Vec::new() }
    }
}

impl<S> ApiExports<S> {
    /// Claim `tag` (a random number from the caller, never 0) for `sink`, with a file name for
    /// the export; `None` when `tag` is 0 or already claimed, or [`MAX_API_EXPORTS`] are
    /// already waiting.
    pub(crate) fn claim(&mut self, tag: u64, sink: S) -> Option<String> {
        if tag == 0 || self.waiting.len() >= MAX_API_EXPORTS || self.waiting.iter().any(|(t, _)| *t == tag) {
            return None;
        }
        self.next = self.next.wrapping_add(1);
        self.waiting.push((tag, sink));
        Some(format!("wordcraft-api-export-{}.pdf", self.next))
    }

    /// The sink waiting on the export tagged `tag`, taken: once only. `None` for an export no
    /// claim holds (a user's own, tag 0, whatever its name).
    pub(crate) fn take(&mut self, tag: u64) -> Option<S> {
        if tag == 0 {
            return None;
        }
        let at = self.waiting.iter().position(|(t, _)| *t == tag)?;
        Some(self.waiting.swap_remove(at).1)
    }
}

/// The `file.exportPdf` parameters for a page-API export under `name`, claimed by `tag`:
/// `markup` as the API call gave it, else what File › Print's Print Markup says, as the command
/// decides.
pub(crate) fn api_pdf_params(name: &str, tag: u64, markup: Option<bool>) -> serde_json::Value {
    let mut params = serde_json::json!({"path": name, "hostTag": tag});
    if let Some(markup) = markup {
        params["markup"] = serde_json::json!(markup);
    }
    params
}

/// What a worker client does with one message from its worker ([`inbound`]).
#[derive(Debug, PartialEq)]
pub(crate) enum Inbound {
    /// Route it to the job `id` waits on (or, for `ready`, open the gate): its payload may be
    /// copied now, its length checked.
    Route(wordcraft_worker_proto::Envelope),
    /// It answers a job but cannot be taken (no bytes, too many): that job gets this typed
    /// `error` reply instead, and the payload is never copied.
    Refuse(wordcraft_worker_proto::Envelope, wordcraft_worker_proto::ErrorReply),
    /// It answers no job a client can name (not a `[header, payload]` pair, no readable header,
    /// a request kind): logged and dropped; every job still ends by its deadline.
    Drop(String),
}

/// Judge a message from a worker by its shape before anything is copied, as the workers judge
/// a request (`unpack`): `header` is its JSON header when the message is an array whose first
/// item is a string no longer than a JSON payload may be, `payload_len` the length of its second
/// item when that is an `ArrayBuffer` or a `Uint8Array`. No browser types, so host tests cover
/// it; both clients read messages through it.
pub(crate) fn inbound(header: Option<&str>, payload_len: Option<usize>) -> Inbound {
    use wordcraft_worker_proto::{Envelope, ErrorCode, ErrorReply, Kind, decode_envelope, reply_payload_limit};
    let Some(header) = header else {
        return Inbound::Drop("a message that is not a [header, payload] pair".into());
    };
    let env = match decode_envelope(header.as_bytes()) {
        Ok(env) => env,
        Err(e) => return Inbound::Drop(format!("an undecodable header ({e})")),
    };
    if env.kind.is_request() {
        return Inbound::Drop(format!("a `{:?}` request, which a worker never sends", env.kind));
    }
    // `ready` carries nothing the client reads.
    if env.kind == Kind::Ready {
        return Inbound::Route(env);
    }
    let refuse = |code, message: String| {
        Inbound::Refuse(Envelope { kind: Kind::Error, ..env.clone() }, ErrorReply { code, message: format!("the worker's reply {message}") })
    };
    let limit = reply_payload_limit(env.kind);
    match payload_len {
        None => refuse(ErrorCode::BadRequest, "carried no bytes".into()),
        Some(got) if got > limit => refuse(ErrorCode::TooLarge, format!("is {got} bytes, over the {limit}-byte limit")),
        Some(_) => Inbound::Route(env),
    }
}

/// The `convert` request that opens `input`, a file in format `format`, as a Word package. A
/// file that gives no page size opens on the paper of `region` (the user's), as the engine opens
/// such a file in its session (`io::open_bytes_in`); the worker has no session to ask.
pub(crate) fn open_request(format: String, input: Vec<u8>, region: &wordcraft_engine::Region) -> wordcraft_worker_proto::ConvertRequest {
    let page = wordcraft_engine::io::region_page(region);
    wordcraft_worker_proto::ConvertRequest {
        format,
        direction: wordcraft_worker_proto::ConvertDirection::Import,
        password: None,
        input,
        page: wordcraft_worker_proto::PageSize::from_points(page.page_w, page.page_h),
    }
}

/// The name a converted file opens under: the engine picks a reader by extension, and what
/// it is handed is a Word package, so `report.odt` opens as `report.docx`. The original name
/// stays in the messages the user sees.
pub(crate) fn opened_name(name: &str) -> String {
    let base_at = name.rfind(['/', '\\']).map_or(0, |i| i + 1);
    let stem = match name.rsplit_once('.') {
        Some((stem, _)) if stem.len() > base_at => stem,
        _ => name,
    };
    format!("{stem}.docx")
}

/// The worker's files never arrived (a 404, no network, a blocked module worker): it never said
/// it was ready. The clients put this in their error.
pub(crate) const NOT_LOADED: &str = "could not be loaded";
/// The worker said it was ready, then died with a job in hand (out of memory on a large
/// document, a fault): the clients put this in their error.
pub(crate) const STOPPED_WORKING: &str = "stopped while working";
/// The worker held a job past its deadline ([`JobQueue`]).
pub(crate) const NO_ANSWER: &str = "did not answer";

/// What the user is told when a worker did not deliver, short enough for the status bar: the
/// file and what was being done, then the reason in plain words. The worker's own wording goes
/// to the console instead. A worker that could not be loaded is a connection problem; one that
/// stopped while working, or never answered, is about this file; a file that needs a password
/// cannot be opened by the worker at all.
pub(crate) fn failure_message(action: &str, name: &str, error: &str) -> String {
    let reason = if error.contains(NOT_LOADED) || error.contains("unavailable in this browser") {
        "this format's converter could not be loaded. Check the connection and try again.".to_string()
    } else if error.contains(STOPPED_WORKING) {
        "the converter stopped while working on it. The file may be too large or damaged to convert in the browser.".to_string()
    } else if error.contains(NO_ANSWER) {
        "the converter did not finish in time. Try again, or try a smaller file.".to_string()
    } else if wordcraft_engine::io::needs_password(error) {
        "it is protected with a password, and the browser version can't open protected files in this format yet.".to_string()
    } else {
        error.to_string()
    };
    format!("Couldn't {action} {name}: {reason}")
}

/// A reply continuation that runs at most once, from whichever path gets there first: the
/// worker's reply, the send that failed, or the deadline.
pub(crate) struct Once<F>(Rc<RefCell<Option<F>>>);

impl<F: FnOnce(Result<Vec<u8>, String>)> Once<F> {
    pub(crate) fn new(f: F) -> Self {
        Once(Rc::new(RefCell::new(Some(f))))
    }

    pub(crate) fn share(&self) -> Self {
        Once(self.0.clone())
    }

    /// Something already answered: the job is over.
    pub(crate) fn answered(&self) -> bool {
        self.0.borrow().is_none()
    }

    /// Answer with `reply`; `true` when this was the answer (nothing had answered yet).
    pub(crate) fn call(&self, reply: Result<Vec<u8>, String>) -> bool {
        // Taken out before it runs: the continuation may start another job.
        let f = self.0.borrow_mut().take();
        match f {
            Some(f) => {
                f(reply);
                true
            }
            None => false,
        }
    }
}

/// The shortest a conversion may take before it counts as stuck.
const CONVERT_BASE_MS: u64 = 2 * 60_000;
/// More time per MiB of input: large documents convert slowly in a worker.
const CONVERT_PER_MIB_MS: u64 = 10_000;
/// The shortest a PDF export may take: it lays out every page first.
const PDF_BASE_MS: u64 = 3 * 60_000;
/// More time per MiB of the Word package laid out.
const PDF_PER_MIB_MS: u64 = 15_000;
/// No single job gets longer than this, however large.
const MAX_JOB_MS: u64 = 15 * 60_000;

/// How long an on-demand worker may take to load — fetch and compile its script and wasm (the PDF
/// worker is about 3 MB compressed) and say it is ready — before its waiting jobs fail as "could
/// not be loaded". Generous: a slow connection is not a stuck worker. A job's own deadline
/// ([`job_budget_ms`]) starts only when it is posted to a ready worker ([`ReadyGate`]).
pub(crate) const LOAD_MS: u64 = 10 * 60_000;

/// The jobs a worker client holds until the worker says it is ready (a module worker still
/// running its top-level `await init()` drops what is posted to it), oldest first. A job leaves
/// here when it is posted, and only then does its deadline start: the time a slow connection
/// takes to bring the worker in is never counted against a conversion. Plain state, so host
/// tests cover it; the clients post what it hands back.
pub(crate) struct ReadyGate<J> {
    ready: bool,
    held: Vec<J>,
}

impl<J> ReadyGate<J> {
    pub(crate) const fn new() -> Self {
        ReadyGate { ready: false, held: Vec::new() }
    }

    /// The worker said it is ready.
    pub(crate) fn is_ready(&self) -> bool {
        self.ready
    }

    /// `Some(job)`: the worker is ready, post it now. `None`: held until [`ReadyGate::open`].
    pub(crate) fn submit(&mut self, job: J) -> Option<J> {
        if self.ready {
            return Some(job);
        }
        self.held.push(job);
        None
    }

    /// The worker is ready: the held jobs, oldest first, to post now.
    pub(crate) fn open(&mut self) -> Vec<J> {
        self.ready = true;
        std::mem::take(&mut self.held)
    }

    /// The worker is gone: the held jobs, never to be posted.
    pub(crate) fn close(&mut self) -> Vec<J> {
        std::mem::take(&mut self.held)
    }
}

/// Which worker a job runs in: each runs its jobs one at a time, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JobKind {
    Convert,
    Pdf,
}

/// How long one job of `kind` on `input_bytes` may run before it counts as stuck: minutes, more
/// for a larger input, capped at [`MAX_JOB_MS`]. The protocol has no progress messages, so this
/// is the whole job's time, not time since the last sign of life.
pub(crate) fn job_budget_ms(kind: JobKind, input_bytes: usize) -> u64 {
    let (base, per_mib) = match kind {
        JobKind::Convert => (CONVERT_BASE_MS, CONVERT_PER_MIB_MS),
        JobKind::Pdf => (PDF_BASE_MS, PDF_PER_MIB_MS),
    };
    let mib = (input_bytes as u64).div_ceil(1 << 20);
    base.saturating_add(mib.saturating_mul(per_mib)).min(MAX_JOB_MS)
}

/// The jobs one worker holds, oldest first, for their deadlines. A worker runs one job at a time,
/// so a job sent behind others waits for them: its deadline is its own budget after theirs.
/// Then a job queued behind a slow one never times out before the slow one does, and the job
/// that times out first is the one the worker is stuck on, so stopping the worker for it is
/// right. Plain state: the caller arms a timer with what [`JobQueue::admit`] answers.
#[derive(Default)]
pub(crate) struct JobQueue {
    next: u64,
    jobs: Vec<(u64, u64)>,
}

impl JobQueue {
    /// A new job with its own `budget_ms`: its token, and how long from now it may take before
    /// it is stuck (the budgets of the jobs ahead of it, then its own).
    pub(crate) fn admit(&mut self, budget_ms: u64) -> (u64, u64) {
        self.next = self.next.wrapping_add(1);
        let ahead = self.jobs.iter().fold(0u64, |sum, (_, b)| sum.saturating_add(*b));
        self.jobs.push((self.next, budget_ms));
        (self.next, ahead.saturating_add(budget_ms))
    }

    /// Job `token` was answered (or failed, or timed out): it no longer holds up the others.
    pub(crate) fn finish(&mut self, token: u64) {
        self.jobs.retain(|(t, _)| *t != token);
    }

    /// Jobs held.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.jobs.len()
    }
}

/// A job's reply continuation.
pub(crate) type Reply = Box<dyn FnOnce(Result<Vec<u8>, String>)>;

/// The answer half of a job ([`Once`]) and what starts its deadline ([`deadline_job`]).
pub(crate) type Job = (Once<Reply>, Box<dyn FnOnce()>);

/// Wrap `on_reply` with a deadline of `budget_ms` in `queue`, the jobs one `what` worker holds.
/// The deadline starts when the second half runs, which the worker client does when it posts
/// the job to a worker that said it is ready — never while the worker is still being fetched
/// (that has its own bound, [`LOAD_MS`]). From then `arm` is asked to call its continuation in
/// `due_ms` (the budgets of the jobs posted ahead of it, then its own); if the job is still
/// unanswered by then it answers "did not answer" and `stop`s the stuck worker, so the next job
/// starts a fresh one. A reply first makes the timer do nothing; a job answered before it was
/// posted starts no clock and holds up no other job; every answer, whichever path gives it,
/// releases the job's place in `queue`. No browser types: `lazy_io` passes `setTimeout` as
/// `arm` and the worker's stop, and host tests drive both by hand.
pub(crate) fn deadline_job(
    queue: Rc<RefCell<JobQueue>>,
    budget_ms: u64,
    what: &'static str,
    on_reply: impl FnOnce(Result<Vec<u8>, String>) + 'static,
    arm: impl FnOnce(u64, Box<dyn FnOnce()>) + 'static,
    stop: impl FnOnce() + 'static,
) -> Job {
    // Set when the job is posted: until then it holds up no other job.
    let token: Rc<std::cell::Cell<Option<u64>>> = Rc::default();
    let finish = (token.clone(), queue.clone());
    let once: Once<Reply> = Once::new(Box::new(move |reply| {
        let (token, queue) = finish;
        if let Some(token) = token.take() {
            queue.borrow_mut().finish(token);
        }
        on_reply(reply);
    }));
    let on_timeout = once.share();
    let start = Box::new(move || {
        if on_timeout.answered() {
            return;
        }
        let (admitted, due_ms) = queue.borrow_mut().admit(budget_ms);
        token.set(Some(admitted));
        let minutes = due_ms.div_ceil(60_000);
        arm(
            due_ms,
            Box::new(move || {
                if on_timeout.call(Err(format!("the {what} worker {NO_ANSWER} in {minutes} minutes"))) {
                    stop();
                }
            }),
        );
    });
    (once, start)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLE: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0, 0];

    /// A zip whose first entry is `mimetype`, stored, holding `mime` (an ODF package's head).
    fn odf(mime: &str) -> Vec<u8> {
        let mut z = ZIP_SIG.to_vec();
        z.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // version, flags, method 0, time, date, crc
        z.extend_from_slice(&(mime.len() as u32).to_le_bytes()); // compressed size
        z.extend_from_slice(&(mime.len() as u32).to_le_bytes()); // uncompressed size
        z.extend_from_slice(&8u16.to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes());
        z.extend_from_slice(b"mimetype");
        z.extend_from_slice(mime.as_bytes());
        z.extend_from_slice(b"PK\x03\x04 content.xml ...");
        z
    }

    /// A Word package as the engine writes one.
    fn docx() -> Vec<u8> {
        wordcraft_engine::io::save_bytes("x.docx", &wordcraft_doc::Document::new()).unwrap()
    }

    #[test]
    fn extra_formats_open_through_the_codecs_worker() {
        for (name, format) in [
            ("letter.rtf", "rtf"),
            ("Letter.RTF", "rtf"),
            ("notes.odt", "odt"),
            ("template.ott", "odt"),
            ("flat.fodt", "fodt"),
            ("page.html", "html"),
            ("page.htm", "html"),
            ("page.xhtml", "html"),
            ("archive.mht", "mht"),
            ("archive.mhtml", "mht"),
            ("paper.tex", "tex"),
            ("paper.latex", "tex"),
            ("paper.ltx", "tex"),
        ] {
            let bytes: &[u8] = if format == "rtf" { b"{\\rtf1 hello}" } else { b"<p>hello</p>" };
            assert_eq!(import_format(name, bytes, false).as_deref(), Some(format), "{name}");
        }
        // ODT is a zip package, and still the worker's.
        assert_eq!(import_format("notes.odt", b"PK\x03\x04rest", false).as_deref(), Some("odt"));
        assert_eq!(import_format("old.doc", OLE, false).as_deref(), Some("doc"));
        assert_eq!(import_format("old.dot", OLE, false).as_deref(), Some("doc"));
    }

    #[test]
    fn what_the_engine_reads_stays_with_the_engine() {
        for name in ["report.docx", "macro.docm", "notes.md", "plain.txt", "data.json", "flat.xml", "picture.png", "list.csv", "noextension", ".rtf"]
        {
            assert_eq!(import_format(name, b"anything", false), None, "{name}");
        }
    }

    #[test]
    fn word_extensions_follow_their_content() {
        // A package under a legacy name is the engine's, password prompt included.
        assert_eq!(import_format("really-docx.doc", b"PK\x03\x04rest", false), None);
        assert_eq!(import_format("really-docx.rtf", b"PK\x03\x04rest", false), None);
        assert_eq!(import_format("locked.doc", OLE, true), None);
        // RTF saved as .doc, and a binary document saved as .rtf.
        assert_eq!(import_format("really-rtf.doc", b"{\\rtf1 hello}", false).as_deref(), Some("rtf"));
        assert_eq!(import_format("really-doc.rtf", OLE, false).as_deref(), Some("doc"));
        // Neither: the engine decides (plain text, or its own refusal).
        assert_eq!(import_format("text.rtf", b"just text", false), None);
        assert_eq!(import_format("text.doc", b"just text", false), None);
    }

    /// Review finding 4(b): a zip package is what its content says, whatever its name.
    #[test]
    fn zip_packages_follow_their_content_whatever_the_name() {
        let word = docx();
        for name in ["really-docx.odt", "really-docx.ott", "really-docx.html", "really-docx.doc", "really-docx.docx"] {
            assert_eq!(import_format(name, &word, false), None, "a Word package is the engine's: {name}");
        }
        let text = odf("application/vnd.oasis.opendocument.text");
        let template = odf("application/vnd.oasis.opendocument.text-template");
        for name in ["really-odt.docx", "really-odt.doc", "really-odt.rtf", "really-odt.odt", "noextension"] {
            assert_eq!(import_format(name, &text, false).as_deref(), Some("odt"), "an OpenDocument text is the worker's: {name}");
            assert_eq!(import_format(name, &template, false).as_deref(), Some("odt"), "{name}");
        }
        // Another ODF kind under a Word name is not a text document: the engine says what it is.
        assert_eq!(import_format("sheet.docx", &odf("application/vnd.oasis.opendocument.spreadsheet"), false), None);
    }

    /// Review finding 12: what `wordcraft_docx::is_encrypted` detects. A Word package encrypted
    /// with a password ([MS-OFFCRYPTO], written here by our own encryptor) is detected under any
    /// name and left to the engine, which asks for the password. A password-protected Word
    /// 97–2003 binary file is not detected by it (its encryption is a flag in the
    /// `WordDocument` stream, not an `EncryptedPackage` stream), and nothing in this repository
    /// writes one; such a file goes to the worker, whose refusal is worded below.
    #[test]
    fn an_encrypted_word_package_stays_with_the_engine() {
        let locked = wordcraft_docx::encrypt(&docx(), "secret").unwrap();
        assert!(locked.starts_with(OLE_SIG), "an encrypted package is a compound file");
        assert!(wordcraft_docx::is_encrypted(&locked));
        for name in ["locked.docx", "locked.doc", "locked.dot", "locked.rtf"] {
            assert_eq!(import_format(name, &locked, wordcraft_docx::is_encrypted(&locked)), None, "{name}");
        }
        // A plain compound file is not an encrypted package.
        assert!(!wordcraft_docx::is_encrypted(OLE));
        let m = failure_message("open", "locked.doc", &format!("conversion failed: {}", wordcraft_docx::PASSWORD_REQUIRED));
        assert_eq!(
            m,
            "Couldn't open locked.doc: it is protected with a password, and the browser version can't open protected files in this format yet."
        );
    }

    /// Review finding 11: a hidden file has no extension, with or without a directory.
    #[test]
    fn a_hidden_file_has_no_extension() {
        for name in [".rtf", "/home/u/.rtf", "C:\\Users\\u\\.odt", "dir.v2/.html"] {
            assert_eq!(ext_of(name), "", "{name}");
            assert_eq!(import_format(name, b"{\\rtf1 hello}", false), None, "{name}");
            assert_eq!(export_route(name), None, "{name}");
        }
        assert_eq!(ext_of("/home/u/letter.RTF"), "rtf");
        assert_eq!(ext_of("dir.v2/letter"), "");
    }

    #[test]
    fn exports_route_to_the_worker_that_writes_them() {
        assert_eq!(export_route("Report.pdf"), Some(ExportRoute::Pdf));
        assert_eq!(export_route("report.PDF"), Some(ExportRoute::Pdf));
        for (name, id) in [
            ("report.rtf", "rtf"),
            ("report.odt", "odt"),
            ("report.ott", "ott"),
            ("report.fodt", "fodt"),
            ("report.html", "html"),
            ("report.htm", "html"),
            ("report.xhtml", "html"),
            ("report.tex", "tex"),
            ("report.latex", "tex"),
            ("report.ltx", "tex"),
        ] {
            assert_eq!(export_route(name), Some(ExportRoute::Codec(id.into())), "{name}");
        }
        // Review finding 5: nothing writes a web archive, so its save is the engine's refusal.
        for name in [
            "report.docx",
            "report.dotx",
            "report.md",
            "report.txt",
            "page.png",
            "report.xml",
            "report.json",
            "report",
            "pdf",
            "page.mht",
            "page.mhtml",
        ] {
            assert_eq!(export_route(name), None, "{name}");
        }
    }

    /// Reviews 4 and 5: the routing tables are checked against the readers and writers the
    /// codecs worker calls (`wordcraft_formats` with `all-formats`), so they cannot drift: each
    /// id sent is one the format crate answers to, every format the crate reads is routed for
    /// opening, and a format is routed for saving exactly when the crate writes it.
    #[test]
    fn routing_tables_match_what_the_worker_reads_and_writes() {
        let page = wordcraft_doc::SectionProps::default();
        let doc = wordcraft_doc::Document::new();
        for (ext, id) in IMPORTS {
            assert!(wordcraft_formats::import_on(id, b"x", &page).is_some(), "the worker reads `{id}` (for .{ext})");
            assert!(wordcraft_formats::import_on(ext, b"x", &page).is_some(), "and `.{ext}` itself");
        }
        for (ext, id) in EXPORTS {
            assert!(wordcraft_formats::export(id, &doc).is_some(), "the worker writes `{id}` (for .{ext})");
        }
        // What the main module reads itself (txt, Markdown) stays out of both tables.
        let core = ["txt", "text", "md", "markdown"];
        for ext in wordcraft_formats::EXTENSIONS.iter().filter(|e| !core.contains(e)) {
            assert!(IMPORTS.iter().any(|(e, _)| e == ext), ".{ext} is read by the worker but not routed to it");
            let writes = wordcraft_formats::export(ext, &doc).is_some();
            assert_eq!(EXPORTS.iter().any(|(e, _)| e == ext), writes, ".{ext}: routed for saving exactly when the worker writes it");
        }
        for (ext, _) in IMPORTS.iter().chain(EXPORTS) {
            assert!(wordcraft_formats::EXTENSIONS.contains(ext), ".{ext} is not a format the worker knows");
        }
    }

    #[test]
    fn converted_files_open_as_word_documents() {
        assert_eq!(opened_name("report.odt"), "report.docx");
        assert_eq!(opened_name("my.report.v2.rtf"), "my.report.v2.docx");
        assert_eq!(opened_name("docs/report.rtf"), "docs/report.docx");
        assert_eq!(opened_name("dir.v2/report"), "dir.v2/report.docx");
    }

    /// Review finding 10: a worker whose files never arrived and one that died while working are
    /// told apart, and the PDF and codec paths are worded alike.
    #[test]
    fn a_failed_worker_is_named_in_plain_words() {
        for error in [
            "conversion failed: the codec worker could not be loaded",
            "pdf export failed: the pdf worker could not be loaded",
            "the codec worker is unavailable in this browser",
            "the pdf worker is unavailable in this browser",
        ] {
            let m = failure_message("open", "letter.rtf", error);
            assert_eq!(m, "Couldn't open letter.rtf: this format's converter could not be loaded. Check the connection and try again.", "{error}");
        }
        for error in ["conversion failed: the codec worker stopped while working", "pdf export failed: the pdf worker stopped while working"] {
            let m = failure_message("export", "big.pdf", error);
            assert_eq!(
                m,
                "Couldn't export big.pdf: the converter stopped while working on it. The file may be too large or damaged to convert in the browser."
            );
        }
        let m = failure_message("export", "big.pdf", "the pdf worker did not answer in 4 minutes");
        assert_eq!(m, "Couldn't export big.pdf: the converter did not finish in time. Try again, or try a smaller file.");
        // A conversion error is the file's own problem and is passed on as it is.
        let m = failure_message("open", "letter.rtf", "conversion failed: not RTF");
        assert_eq!(m, "Couldn't open letter.rtf: conversion failed: not RTF");
    }

    /// Review finding 6: one answer per job, whichever comes first.
    #[test]
    fn a_job_answers_once_whichever_path_is_first() {
        let seen: Rc<RefCell<Vec<Result<Vec<u8>, String>>>> = Rc::default();
        let sink = seen.clone();
        let reply = Once::new(move |r| sink.borrow_mut().push(r));
        let deadline = reply.share();
        assert!(deadline.call(Err("did not answer".into())), "the deadline answered first");
        assert!(!reply.call(Ok(vec![1])), "a late reply is dropped");
        assert_eq!(*seen.borrow(), vec![Err("did not answer".to_string())]);

        let seen: Rc<RefCell<Vec<Result<Vec<u8>, String>>>> = Rc::default();
        let sink = seen.clone();
        let reply = Once::new(move |r| sink.borrow_mut().push(r));
        let deadline = reply.share();
        assert!(reply.call(Ok(vec![1])));
        assert!(!deadline.call(Err("did not answer".into())), "a deadline after the reply does nothing (and stops nothing)");
        assert_eq!(*seen.borrow(), vec![Ok(vec![1])]);
    }

    /// Review finding 6: deadlines are minutes, grow with the input, and are capped.
    #[test]
    fn job_budgets_are_minutes_and_grow_with_the_input() {
        assert_eq!(job_budget_ms(JobKind::Convert, 0), 120_000);
        assert_eq!(job_budget_ms(JobKind::Convert, 1), 130_000);
        assert_eq!(job_budget_ms(JobKind::Convert, 5 << 20), 170_000);
        assert_eq!(job_budget_ms(JobKind::Pdf, 2 << 20), 210_000);
        assert_eq!(job_budget_ms(JobKind::Convert, usize::MAX), MAX_JOB_MS);
        assert_eq!(job_budget_ms(JobKind::Pdf, 1 << 30), MAX_JOB_MS);
    }

    /// Review finding 6: a job queued behind another waits for it, so it never times out first.
    #[test]
    fn a_queued_job_waits_for_the_jobs_ahead() {
        let mut q = JobQueue::default();
        let (a, a_due) = q.admit(120_000);
        let (b, b_due) = q.admit(60_000);
        assert_eq!((a_due, b_due), (120_000, 180_000));
        assert!(b_due > a_due, "the job ahead times out first");
        q.finish(a);
        let (_, c_due) = q.admit(30_000);
        assert_eq!(c_due, 90_000, "a finished job no longer counts");
        q.finish(b);
        q.finish(b);
        assert_eq!(q.len(), 1);
        let mut q = JobQueue::default();
        q.admit(u64::MAX);
        assert_eq!(q.admit(5).1, u64::MAX, "saturates");
    }

    /// Review finding (the job deadline): a job submitted while its worker is still loading is
    /// held, not posted, so its deadline has not started; it is posted (and its clock starts) when
    /// the worker says it is ready, oldest first. Once ready, a job is posted at once. A worker
    /// that never loads hands its held jobs back unposted. Loading has its own, longer bound.
    #[test]
    fn a_jobs_deadline_starts_when_it_is_posted_to_a_ready_worker() {
        let posted: Rc<RefCell<Vec<&str>>> = Rc::default();
        // A job is its name and what posting it starts: its deadline, here a record.
        let job = |name: &'static str| {
            let posted = posted.clone();
            (name, Box::new(move || posted.borrow_mut().push(name)) as Box<dyn FnOnce()>)
        };
        let post = |j: (&str, Box<dyn FnOnce()>)| (j.1)();
        let mut gate = ReadyGate::new();
        assert!(!gate.is_ready());
        for name in ["first", "second"] {
            assert!(gate.submit(job(name)).is_none(), "held while the worker loads");
        }
        assert!(posted.borrow().is_empty(), "no clock runs while the worker loads, however long that takes");
        gate.open().into_iter().for_each(post);
        assert_eq!(*posted.borrow(), ["first", "second"], "posted, oldest first, when ready");
        assert!(gate.is_ready());
        gate.submit(job("third")).map(post).unwrap();
        assert_eq!(posted.borrow().last(), Some(&"third"), "a ready worker takes a job at once");

        let mut never = ReadyGate::new();
        assert!(never.submit(job("lost")).is_none());
        assert_eq!(never.close().len(), 1, "a worker that never loads hands its jobs back");
        assert!(never.close().is_empty() && !never.is_ready());
        assert!(posted.borrow().iter().all(|n| *n != "lost"));
        // Loading is bounded on its own, and more generously than the shortest job.
        assert!(LOAD_MS >= job_budget_ms(JobKind::Pdf, 0));
    }

    #[test]
    fn once_knows_it_was_answered() {
        let once = Once::new(|_: Result<Vec<u8>, String>| {});
        let other = once.share();
        assert!(!other.answered());
        assert!(once.call(Ok(Vec::new())));
        assert!(other.answered() && !other.call(Err("late".into())));
    }

    /// Review finding (the lazy PDF's options): the export options the command resolved reach the
    /// PDF job, each as the engine's PDF writer reads it (`strict::pdf_options`).
    #[test]
    fn a_pdf_job_carries_the_export_options() {
        use wordcraft_engine::io::ExportOptions;
        use wordcraft_layout::NumeralMode;
        use wordcraft_worker_proto::{Numeral, PdfJobOptions};
        let job = |markup, comments, numeral| pdf_job_options(&ExportOptions { markup, comments, numeral, ..Default::default() });
        assert_eq!(job(false, false, NumeralMode::Arabic), PdfJobOptions::default());
        assert_eq!(job(true, false, NumeralMode::Arabic), PdfJobOptions { markup: true, comments: false, numeral: Numeral::Arabic, strict: false });
        assert_eq!(job(true, true, NumeralMode::Hindi), PdfJobOptions { markup: true, comments: true, numeral: Numeral::Hindi, strict: false });
        assert_eq!(
            job(false, true, NumeralMode::Context),
            PdfJobOptions { markup: false, comments: false, numeral: Numeral::Context, strict: false }
        );
        assert_eq!(job(false, false, NumeralMode::System).numeral, Numeral::Arabic);
        // Strict goes to the worker (it refuses copies it could not install); the 2003 XML flag
        // is no PDF option.
        let other = ExportOptions { strict: true, word2003: true, ..Default::default() };
        assert_eq!(pdf_job_options(&other), PdfJobOptions { strict: true, ..Default::default() });
    }

    /// Review finding (the page of an opened file): a file opened through the codecs worker asks
    /// for the paper of the user's region, the page the engine would open it on in-process.
    #[test]
    fn an_opened_file_asks_for_the_regions_paper() {
        use wordcraft_engine::Region;
        use wordcraft_worker_proto::ConvertDirection;
        for (tag, w, h) in [("pl-PL", 595.28f32, 841.89f32), ("en-US", 612.0, 792.0), ("fr-CA", 612.0, 792.0)] {
            let region = Region::from_tag(tag);
            let req = open_request("html".into(), b"<p>x</p>".to_vec(), &region);
            // Exactly the engine's page, unrounded, on the wire and back.
            let engine = wordcraft_engine::io::region_page(&region);
            assert_eq!((engine.page_w, engine.page_h), (w, h), "{tag}");
            let sent = wordcraft_worker_proto::encode_convert_request(&req).unwrap();
            let got = wordcraft_worker_proto::decode_convert_request(&sent).unwrap().page.map(|p| p.points());
            assert_eq!(got.map(|(w, h)| (w.to_bits(), h.to_bits())), Some((engine.page_w.to_bits(), engine.page_h.to_bits())), "{tag}");
            assert_eq!((req.direction, req.password.as_deref(), req.format.as_str()), (ConvertDirection::Import, None, "html"));
        }
    }

    /// `n` runtime faces of one byte each, families `Family 0`… in order.
    fn faces(n: usize) -> Vec<(String, String, std::sync::Arc<Vec<u8>>)> {
        (0..n).map(|i| (format!("Family {i}"), "Regular".to_string(), std::sync::Arc::new(vec![1u8]))).collect()
    }

    /// The protocol budget bites the machine's spare faces, never the document's: many faces the
    /// document never draws in leave a strict export alone, and the faces it does draw in go
    /// first, whatever their place in the font database.
    #[test]
    fn the_font_batch_sends_the_documents_faces_first_and_refuses_only_for_them() {
        // Everything fits: everything goes in its own order, and the document is not laid out.
        let batch = font_batch(faces(3), || panic!("asked for the document's faces"), FONT_BUDGET);
        assert_eq!(batch.copies.iter().map(|c| c.family.as_str()).collect::<Vec<_>>(), ["Family 0", "Family 1", "Family 2"]);
        assert_eq!((batch.left_used, batch.left_unused), (0, 0));
        assert!(batch.strict_refusal().is_none());

        // More faces than the protocol carries, none of them the document's: a strict export goes.
        let many = FONT_BUDGET.count + 4;
        let batch = font_batch(faces(many), Vec::new, FONT_BUDGET);
        assert_eq!((batch.copies.len(), batch.left_used, batch.left_unused), (FONT_BUDGET.count, 0, 4));
        assert_eq!(batch.strict_refusal(), None, "faces this PDF never draws in refuse nothing");

        // The document draws in the last face of the database: it goes, an unrelated one stays.
        let last = format!("Family {}", many - 1);
        let wanted = last.clone();
        let batch = font_batch(faces(many), move || vec![(wanted, "Regular".into())], FONT_BUDGET);
        assert_eq!(batch.copies.first().map(|c| c.family.as_str()), Some(last.as_str()), "the document's face goes first");
        assert_eq!((batch.left_used, batch.left_unused), (0, 4));
        assert!(batch.strict_refusal().is_none());

        // The document's own faces are over the budget: refused, naming them.
        let budget = FontBudget { count: 2, bytes: FONT_BUDGET.bytes };
        let batch = font_batch(faces(8), || (3..8).map(|i| (format!("Family {i}"), "Regular".to_string())).collect(), budget);
        assert_eq!(batch.copies.iter().map(|c| c.family.as_str()).collect::<Vec<_>>(), ["Family 3", "Family 4"]);
        assert_eq!((batch.left_used, batch.left_unused), (3, 3));
        assert_eq!(
            batch.strict_refusal().as_deref(),
            Some(
                "strict: 3 font face(s) this PDF draws text in are too many to send to the PDF export, and their text would fall back: Family 5, Family 6, Family 7"
            )
        );

        // The byte budget counts the same way: a face too large for it stays, smaller ones still go.
        let mut big = faces(3);
        big.insert(0, ("Huge".into(), "Regular".into(), std::sync::Arc::new(vec![0u8; 10])));
        let batch = font_batch(big, || vec![("Huge".into(), "Regular".into())], FontBudget { count: 10, bytes: 5 });
        assert_eq!(batch.copies.len(), 3);
        assert_eq!((batch.left_used, batch.left_used_named.as_slice()), (1, ["Huge".to_string()].as_slice()));
    }

    /// Review finding (gate 3, the strict font check): a strict job tells the worker which faces
    /// its PDF draws text in, so the worker can tell a lost copy the text needs from one it does
    /// not, whatever the document names the face. A job that is not strict sends no list and
    /// lays nothing out while the copies fit; with no copies at all nothing can be lost, so
    /// nothing is laid out either.
    #[test]
    fn a_strict_pdf_job_sends_the_faces_its_text_is_drawn_in() {
        use wordcraft_engine::strict::PdfFaces;
        let drawn = || vec![("Family 1".to_string(), "Regular".to_string())];
        let job = pdf_fonts(faces(3), || PdfFaces { faces: drawn(), cut: false }, FONT_BUDGET, true).unwrap();
        assert_eq!(job.drawn, Some(drawn()));
        assert_eq!(job.batch.copies.len(), 3);
        let job = pdf_fonts(faces(3), || panic!("laid out for a job that is not strict"), FONT_BUDGET, false).unwrap();
        assert_eq!((job.drawn, job.batch.copies.len()), (None, 3));
        let job = pdf_fonts(Vec::new(), || panic!("laid out with no copies to lose"), FONT_BUDGET, true).unwrap();
        assert_eq!((job.drawn, job.batch.copies.len()), (Some(Vec::new()), 0));
        // The budget's own refusal still comes first.
        let budget = FontBudget { count: 1, bytes: FONT_BUDGET.bytes };
        let two = || PdfFaces { faces: vec![("Family 1".into(), "Regular".into()), ("Family 2".into(), "Regular".into())], cut: false };
        let err = pdf_fonts(faces(3), two, budget, true).unwrap_err();
        assert!(err.starts_with("strict: 1 font face(s)"), "{err}");
    }

    /// Review finding (gate 3, F1): the list of faces the PDF draws text in is bounded, and a
    /// face past the bound looked unused, so a strict export decided from a cut list. A cut list
    /// now says so: a strict job whose copies do not all fit refuses (a face left out may be one
    /// its text is drawn in), and one whose copies all fit runs but tells the worker the list is
    /// not whole, so the worker refuses on any copy it cannot install.
    #[test]
    fn a_strict_pdf_job_refuses_what_a_cut_face_list_cannot_check() {
        use wordcraft_engine::strict::PdfFaces;
        let cut = || PdfFaces { faces: vec![("Family 0".into(), "Regular".into())], cut: true };
        let job = pdf_fonts(faces(3), cut, FONT_BUDGET, true).unwrap();
        assert_eq!((job.drawn, job.batch.copies.len()), (None, 3), "all fit: sent, the list marked unknown");
        let budget = FontBudget { count: 2, bytes: FONT_BUDGET.bytes };
        let err = pdf_fonts(faces(3), cut, budget, true).unwrap_err();
        assert!(err.starts_with("strict: ") && err.contains("cannot be checked"), "{err}");
        let job = pdf_fonts(faces(3), cut, budget, false).unwrap();
        assert_eq!((job.drawn, job.batch.copies.len()), (None, 2), "not strict: what fits goes");
        // The same budget with the whole list: the face left out is one the text never uses.
        let whole = || PdfFaces { faces: vec![("Family 0".into(), "Regular".into())], cut: false };
        let job = pdf_fonts(faces(3), whole, budget, true).unwrap();
        assert_eq!((job.drawn.map(|d| d.len()), job.batch.left_unused), (Some(1), 1));
    }

    /// Review finding (the clients' inbound guards): a worker's message is judged by its shape
    /// before any copy. Junk is dropped, a reply with no bytes or too many fails its own job with
    /// a typed error, and only a well-formed reply within its kind's bound is routed.
    #[test]
    fn a_workers_message_is_judged_before_it_is_copied() {
        use wordcraft_worker_proto::{Envelope, ErrorCode, Kind, MAX_OUTPUT_BYTES, MAX_PAYLOAD_BYTES, encode_envelope};
        let header = |id, kind| {
            String::from_utf8(encode_envelope(&Envelope { v: wordcraft_worker_proto::PROTOCOL_VERSION, id, kind, revision: 0 }).unwrap()).unwrap()
        };
        // Well formed, within bounds: routed.
        let pdf = header(7, Kind::Pdf);
        assert!(matches!(inbound(Some(&pdf), Some(1_000)), Inbound::Route(e) if e.id == 7 && e.kind == Kind::Pdf));
        assert!(matches!(inbound(Some(&pdf), Some(MAX_OUTPUT_BYTES)), Inbound::Route(_)));
        // `ready` needs no payload at all.
        assert!(matches!(inbound(Some(&header(0, Kind::Ready)), None), Inbound::Route(e) if e.kind == Kind::Ready));
        // Too many bytes, or none: that job fails, typed, and nothing is copied.
        match inbound(Some(&pdf), Some(MAX_OUTPUT_BYTES + 1)) {
            Inbound::Refuse(env, err) => {
                assert_eq!((env.id, env.kind, err.code), (7, Kind::Error, ErrorCode::TooLarge));
                assert!(err.message.contains("over the"), "{}", err.message);
            }
            other => panic!("{other:?}"),
        }
        let error = header(8, Kind::Error);
        assert!(matches!(inbound(Some(&error), Some(MAX_PAYLOAD_BYTES + 1)), Inbound::Refuse(e, r) if e.id == 8 && r.code == ErrorCode::TooLarge));
        assert!(matches!(inbound(Some(&header(9, Kind::Converted)), None), Inbound::Refuse(e, r) if e.id == 9 && r.code == ErrorCode::BadRequest));
        // Junk answers no job: dropped.
        for junk in [None, Some("not json"), Some("{}"), Some(header(3, Kind::RenderPdf).as_str())] {
            assert!(matches!(inbound(junk, Some(10)), Inbound::Drop(_)), "{junk:?}");
        }
    }

    /// Review finding (the API's PDF options): the page API's export claims a tag, runs the
    /// app's own `file.exportPdf` with it, and gets the bytes back once; a user's export is not
    /// taken, and the claims are bounded.
    #[test]
    fn an_api_export_claims_its_own_tag_once() {
        let mut claims: ApiExports<u32> = ApiExports::default();
        let a = claims.claim(0xA11CE, 1).unwrap();
        let b = claims.claim(0xB0B, 2).unwrap();
        assert_ne!(a, b);
        assert!(a.ends_with(".pdf") && export_route(&a) == Some(ExportRoute::Pdf), "the command routes it to the PDF worker");
        assert_eq!(claims.take(0), None, "a user's export (no tag) is delivered, not claimed");
        assert_eq!(claims.take(0xB0B), Some(2));
        assert_eq!(claims.take(0xB0B), None, "once");
        assert_eq!(claims.take(0xA11CE), Some(1));
        assert_eq!(claims.claim(0, 3), None, "0 is no tag");
        for n in 1..=MAX_API_EXPORTS {
            assert!(claims.claim(n as u64, n as u32).is_some());
        }
        assert_eq!(claims.claim(99, 99), None, "bounded");
        assert_eq!(claims.take(1), Some(1));
        assert_eq!(claims.claim(2, 2), None, "a tag already held is not claimed twice");
        // The parameters are the command's own: the same export options the app's File › Export
        // resolves for the same `markup`, the Print Markup setting deciding when none is given,
        // plus the tag the export hands back.
        let mut s = wordcraft_engine::Session::new(wordcraft_doc::Document::from_text("Figures"));
        s.prefs.print_markup = true;
        for markup in [None, Some(true), Some(false)] {
            let api = wordcraft_engine::cmd::file::export_options(&s, &api_pdf_params(&a, 0xA11CE, markup));
            assert_eq!(api.host_tag, 0xA11CE);
            let mut user = serde_json::json!({"path": "report.pdf"});
            if let Some(m) = markup {
                user["markup"] = serde_json::json!(m);
            }
            let user = wordcraft_engine::cmd::file::export_options(&s, &user);
            assert_eq!(user.host_tag, 0);
            assert_eq!(wordcraft_engine::io::ExportOptions { host_tag: 0, ..api }, user, "{markup:?}");
        }
        assert!(wordcraft_engine::cmd::file::export_options(&s, &api_pdf_params(&a, 1, None)).markup);
    }

    /// Review finding (gate 3, F5): the claim was found by file name, so a caller exporting under
    /// the API's (predictable) name — `execute("file.exportPdf", {path: name})` — took the API's
    /// promise with its own bytes, lost its own download, and the API's export was downloaded
    /// instead. The claim is the API's tag now: the same-name export, untagged, is delivered as
    /// its own, and the API's export still reaches its promise.
    #[test]
    fn an_export_under_the_apis_name_is_delivered_as_its_own() {
        let mut claims: ApiExports<&str> = ApiExports::default();
        let name = claims.claim(0x5EC_2E7, "the API's promise").unwrap();
        let mut s = wordcraft_engine::Session::new(wordcraft_doc::Document::from_text("Figures"));
        let user = wordcraft_engine::cmd::file::export_options(&s, &serde_json::json!({"path": name}));
        let api = wordcraft_engine::cmd::file::export_options(&s, &api_pdf_params(&name, 0x5EC_2E7, None));
        // The user's export finishes first: the finished export is routed by its own options.
        assert_eq!(claims.take(user.host_tag), None, "the same-name export is the user's: delivered");
        assert_eq!(claims.take(api.host_tag), Some("the API's promise"), "the API's export reaches its promise");
        // A caller guessing a tag has no claim to take once it is answered.
        s.prefs.print_markup = false;
        let guess = wordcraft_engine::cmd::file::export_options(&s, &serde_json::json!({"path": name, "hostTag": 0x5EC_2E7}));
        assert_eq!(claims.take(guess.host_tag), None);
    }

    /// A job's answers, its armed timers and the worker stops, recorded for the deadline tests.
    #[derive(Default)]
    struct Rig {
        answers: Rc<RefCell<Vec<(&'static str, Result<Vec<u8>, String>)>>>,
        timers: Rc<RefCell<Vec<(&'static str, u64, Box<dyn FnOnce()>)>>>,
        stops: Rc<std::cell::Cell<usize>>,
    }

    impl Rig {
        fn job(&self, queue: &Rc<RefCell<JobQueue>>, name: &'static str, budget_ms: u64) -> Job {
            let (answers, timers, stops) = (self.answers.clone(), self.timers.clone(), self.stops.clone());
            deadline_job(
                queue.clone(),
                budget_ms,
                "pdf",
                move |reply| answers.borrow_mut().push((name, reply)),
                move |due, fire| timers.borrow_mut().push((name, due, fire)),
                move || stops.set(stops.get() + 1),
            )
        }

        /// Fire the timer job `name` armed.
        fn fire(&self, name: &str) {
            let at = self.timers.borrow().iter().position(|(n, _, _)| *n == name).unwrap();
            let (_, _, fire) = self.timers.borrow_mut().remove(at);
            fire();
        }

        fn answers_of(&self, name: &str) -> Vec<Result<Vec<u8>, String>> {
            self.answers.borrow().iter().filter(|(n, _)| *n == name).map(|(_, r)| r.clone()).collect()
        }
    }

    /// Review finding (the deadline glue was browser-only): the handshake between a job's
    /// reply, its posting and its timer, driven by hand. A timer that fires after its job
    /// answered does nothing; a timer only ever fails its own job; a job answered before it was
    /// posted arms nothing; and a worker stopped with jobs queued fails each exactly once, the
    /// timers firing later changing nothing.
    #[test]
    fn a_jobs_deadline_answers_only_its_own_unanswered_job() {
        let rig = Rig::default();
        let queue: Rc<RefCell<JobQueue>> = Rc::default();

        // A answered before its timer fires: the timer does nothing, stops nothing.
        let (a, start_a) = rig.job(&queue, "a", 1_000);
        start_a();
        assert_eq!(queue.borrow().len(), 1);
        assert!(a.call(Ok(b"%PDF a".to_vec())));
        assert_eq!(queue.borrow().len(), 0, "the answer releases its place");
        rig.fire("a");
        assert_eq!(rig.answers_of("a"), [Ok(b"%PDF a".to_vec())]);
        assert_eq!(rig.stops.get(), 0);

        // B and C posted; B's timer fires: B fails, the stuck worker is stopped, C is untouched
        // and answers with its own reply. C's deadline counted B's budget ahead of its own.
        let (b, start_b) = rig.job(&queue, "b", 1_000);
        let (c, start_c) = rig.job(&queue, "c", 2_000);
        start_b();
        start_c();
        let due: Vec<(&str, u64)> = rig.timers.borrow().iter().map(|(n, d, _)| (*n, *d)).collect();
        assert_eq!(due, [("b", 1_000), ("c", 3_000)]);
        rig.fire("b");
        assert_eq!(rig.answers_of("b"), [Err("the pdf worker did not answer in 1 minutes".to_string())]);
        assert!(!b.call(Ok(b"late".to_vec())), "a late reply is dropped");
        assert_eq!(rig.stops.get(), 1);
        assert!(rig.answers_of("c").is_empty(), "B's timer never fails C");
        assert!(c.call(Ok(b"%PDF c".to_vec())));
        rig.fire("c");
        assert_eq!(rig.answers_of("c"), [Ok(b"%PDF c".to_vec())]);
        assert_eq!(rig.stops.get(), 1);

        // A job answered before it was posted (its send failed) arms nothing and holds nothing.
        let (d, start_d) = rig.job(&queue, "d", 1_000);
        assert!(d.call(Err("could not take it".into())));
        start_d();
        assert!(rig.timers.borrow().iter().all(|(n, _, _)| *n != "d"));
        assert_eq!(queue.borrow().len(), 0);

        // A worker stopped with jobs queued — E posted, F still held before `ready` — fails
        // each once (as a client's `Drop` does through every pending sink); E's timer then
        // fires into nothing, and F, never posted, never armed one.
        let (e, start_e) = rig.job(&queue, "e", 1_000);
        let (f, _start_f) = rig.job(&queue, "f", 1_000);
        start_e();
        for sink in [&e, &f] {
            assert!(sink.call(Err("the pdf worker was stopped".into())));
            assert!(!sink.call(Err("the pdf worker was stopped".into())), "once");
        }
        rig.fire("e");
        assert_eq!(rig.answers_of("e").len(), 1);
        assert_eq!(rig.answers_of("f").len(), 1);
        assert!(rig.timers.borrow().iter().all(|(n, _, _)| *n != "f"));
        assert_eq!(rig.stops.get(), 1, "only a timer that answered stops the worker");
        assert_eq!(queue.borrow().len(), 0, "nothing is left holding up a later job");
    }

    /// Review item (the page API's input copies): `loadDocx` and `convertDocument` refuse bytes
    /// past the bound the later step enforces before copying them, with that step's own error:
    /// `doc.loadDocx`'s cap on its base64, and the conversion's cap on its input.
    #[test]
    fn page_api_input_is_bounded_before_it_is_copied_with_the_later_steps_error() {
        use wordcraft_engine::cmd::insert::base64_encode;
        // The base64 a package of `n` bytes becomes, as `loadDocx` writes it.
        for n in 0..12 {
            assert_eq!(base64_len(n), base64_encode(&vec![7u8; n]).len(), "{n}");
        }
        assert_eq!(base64_len(usize::MAX), usize::MAX, "saturates");
        // The command's own refusal, word for word, for the largest base64 it refuses.
        let max = wordcraft_engine::cmd::file::MAX_DOCX_BASE64_BYTES;
        let over = "A".repeat(max + 1);
        let engine = wordcraft_engine::Session::new(wordcraft_doc::Document::default())
            .run("doc.loadDocx", &serde_json::json!({"docxBase64": over}))
            .unwrap_err()
            .to_string();
        assert_eq!(load_docx_error(max + 1).as_deref(), Some(engine.as_str()));
        assert_eq!(load_docx_error(max), None);
        // In raw bytes: the largest package whose base64 fits goes, one more byte is refused.
        let fits = max / 4 * 3;
        assert_eq!((base64_len(fits), base64_len(fits + 1) > max), (max, true));
        assert_eq!(load_docx_error(base64_len(fits)), None);
        assert_eq!(load_docx_error(base64_len(fits + 1)).as_deref(), Some(engine.as_str()));
        // The conversion's cap, with the words the conversion itself refuses with.
        let cap = wordcraft_worker_proto::MAX_CONVERT_BYTES;
        assert_eq!(convert_input_error(cap), None);
        assert_eq!(
            convert_input_error(cap + 1).as_deref(),
            Some(format!("the document is too large to convert in the browser ({} bytes)", cap + 1).as_str())
        );
    }

    /// Review finding (gate 3, F6): `convertDocument`'s base64 string was decoded with no bound
    /// first (a `Vec` of three quarters of its length, then filled), so a huge string cost that
    /// allocation before the conversion's cap refused it. The string is bounded by its length
    /// before it is copied or decoded — a base64 string of `len` characters decodes to at least
    /// `len / 4 * 3 - 2` bytes when it is all alphabet — with the conversion's own refusal; the
    /// exact check on the decoded bytes stays. A malformed string is refused as such, not turned
    /// into an empty input.
    #[test]
    fn a_base64_conversion_input_is_bounded_before_it_is_decoded() {
        use wordcraft_engine::cmd::insert::base64_encode;
        let cap = wordcraft_worker_proto::MAX_CONVERT_BYTES;
        // The base64 of an input at the cap is never refused by its length.
        assert_eq!(convert_base64_error(base64_len(cap)), None);
        assert_eq!(convert_base64_error(base64_len(cap) - 1), None);
        // Three bytes past the cap make a string whose shortest decoding is past it too.
        let over = base64_len(cap + 3);
        assert_eq!(convert_base64_error(over), convert_input_error(over / 4 * 3 - 2));
        assert!(convert_base64_error(over).is_some());
        assert!(convert_base64_error(1 << 30).is_some(), "a gigabyte of base64 is refused before any copy");
        assert!(convert_base64_error(usize::MAX).is_some());
        assert_eq!(convert_base64_error(0), None);
        // Decoding: the bytes, a typed refusal for junk, and the exact cap on what decoded.
        assert_eq!(decode_convert_base64(&base64_encode(b"{\\rtf1 x}")), Ok(b"{\\rtf1 x}".to_vec()));
        assert_eq!(decode_convert_base64("not base64!"), Err("convertDocument: `data` is not valid base64".to_string()));
        assert_eq!(decode_convert_base64(&base64_encode(&[0u8; 7])).map(|v| v.len()), Ok(7));
    }
}
