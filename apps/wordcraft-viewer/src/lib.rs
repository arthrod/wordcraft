//! The view-only module (plan Phase 8): opens a `.docx` and paints its pages, with no editor in
//! it.
//!
//! Host-agnostic: the host passes bytes in and gets RGBA bytes out. Nothing here touches the
//! DOM, the network or the file system, so one module serves a browser worker, the main thread
//! or a server. Underneath are plain Rust functions ([`open`] to a [`Session`], then
//! [`Session::render`] one page at a time), which the host tests call with no wasm and no
//! mocks; the `wasm-bindgen` handle in the crate's bin (`main.rs`, `wasm.rs`) is a thin layer
//! over them.
//!
//! It builds on three entry points, unchanged: [`wordcraft_docx::read_with_password`],
//! [`wordcraft_layout::layout`] and [`wordcraft_render::render_page`]. A page is laid out and
//! drawn as `wordcraft-cli render` does it, with these differences, which follow from leaving
//! the engine out:
//!
//! - `DATE` and `TIME` fields show the result stored in the file (the engine refreshes them on
//!   open), and `SEQ`, `STYLEREF` and `REF` fields are not refilled for output.
//! - `FILENAME` fields show their stored result too (there is no file name here), and digits
//!   are drawn as stored (the engine applies the session's Numeral setting).
//! - Alternative format chunks (`w:altChunk`) are left out: the engine's importers bring them in.
//! - In the wasm build, layout is linked without hyphenation and only the four bundled faces
//!   are present until the host installs more ([`Session::install_font`]).
//!
//! Every input is hostile: document bytes, scale, pixels per call, page index, font bytes and
//! font count are all bounded, and every rejection is a typed [`ViewerError`]. Nothing here
//! panics.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use wordcraft_doc::Document;
use wordcraft_layout::{DocLayout, LayoutCache, LayoutOptions};

/// Largest document accepted, in bytes: the bound the PDF worker puts on the same package
/// (`wordcraft_worker_proto::MAX_PDF_DOC_BYTES`), so a file one opens the other exports.
pub const MAX_DOCUMENT_BYTES: usize = 64 << 20;

/// Largest scale accepted, in pixels per point (16 is 1152 dpi). For ordinary pages
/// [`MAX_RENDER_PIXELS`] binds long before this; the cap keeps a tiny page from being asked for
/// at a scale where the arithmetic stops meaning anything.
pub const MAX_SCALE: f32 = 16.0;

/// Most pixels one [`Session::render`] call returns: 32 million, which is 128 MiB of RGBA. An
/// A3 page at 4 pixels per point (288 dpi, a 2× display at 200% zoom) is 16 million, so every
/// on-screen use fits twice over, while one call can never ask for gigabytes.
pub const MAX_RENDER_PIXELS: u64 = 32_000_000;

/// Largest font file [`install_font`] accepts: the limit the font database applies itself.
pub const MAX_FONT_BYTES: usize = wordcraft_fonts::MAX_FONT_FILE_BYTES;

/// Most fonts [`install_font`] adds in one process, and their total size once decoded: the
/// bounds the PDF worker puts on the font copies of one request
/// (`wordcraft_worker_proto::MAX_FONT_COPIES`, `MAX_FONT_TOTAL_BYTES`).
pub const MAX_FONTS: usize = 256;
pub const MAX_FONT_TOTAL_BYTES: usize = 128 << 20;

/// Longest font name [`install_font`] accepts, in characters: what layout records of a name a
/// document asks for (`wordcraft_layout::para::reported_font_name`), so a longer one could
/// never match.
pub const MAX_FONT_NAME_CHARS: usize = 256;

/// Why the viewer refused something. Each has a stable [`ViewerError::code`] a host can branch
/// on; the `Display` text is for people.
#[derive(Clone, Debug, PartialEq)]
pub enum ViewerError {
    /// No document bytes at all.
    EmptyDocument,
    /// More document bytes than [`MAX_DOCUMENT_BYTES`].
    DocumentTooLarge { bytes: usize, limit: usize },
    /// The package is encrypted and no password was given.
    PasswordRequired,
    /// The password doesn't open the package.
    WrongPassword,
    /// The bytes aren't a Word package this reads: not a zip, truncated, a part missing or
    /// malformed, over one of the reader's limits, or encrypted in a way it can't read.
    Unreadable(String),
    /// A scale that isn't a finite number in `(0, MAX_SCALE]`.
    BadScale { scale: f32 },
    /// A page index at or past the page count.
    NoSuchPage { page: u32, pages: usize },
    /// The page at this scale is more than [`MAX_RENDER_PIXELS`], or a side is past what the
    /// rasteriser draws (`wordcraft_render::MAX_SIDE`).
    TooManyPixels { width: u64, height: u64, limit: u64 },
    /// A font name that is empty or longer than [`MAX_FONT_NAME_CHARS`].
    BadFontName,
    /// A font file over [`MAX_FONT_BYTES`].
    FontTooLarge { bytes: usize, limit: usize },
    /// Bytes that aren't a usable font (TTF, OTF, TTC, WOFF, WOFF2).
    BadFont(String),
    /// [`MAX_FONTS`] or [`MAX_FONT_TOTAL_BYTES`] is used up.
    TooManyFonts { fonts: usize, bytes: usize },
}

impl ViewerError {
    /// A short stable identifier for this kind of refusal (the JS `Error`'s `code`).
    pub fn code(&self) -> &'static str {
        match self {
            ViewerError::EmptyDocument => "empty-document",
            ViewerError::DocumentTooLarge { .. } => "document-too-large",
            ViewerError::PasswordRequired => "password-required",
            ViewerError::WrongPassword => "wrong-password",
            ViewerError::Unreadable(_) => "unreadable",
            ViewerError::BadScale { .. } => "bad-scale",
            ViewerError::NoSuchPage { .. } => "no-such-page",
            ViewerError::TooManyPixels { .. } => "too-many-pixels",
            ViewerError::BadFontName => "bad-font-name",
            ViewerError::FontTooLarge { .. } => "font-too-large",
            ViewerError::BadFont(_) => "bad-font",
            ViewerError::TooManyFonts { .. } => "too-many-fonts",
        }
    }
}

impl std::fmt::Display for ViewerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ViewerError::EmptyDocument => write!(f, "the document is empty"),
            ViewerError::DocumentTooLarge { bytes, limit } => write!(f, "the document is {bytes} bytes; at most {limit} bytes"),
            ViewerError::PasswordRequired => write!(f, "{}", wordcraft_docx::PASSWORD_REQUIRED),
            ViewerError::WrongPassword => write!(f, "{}", wordcraft_docx::WRONG_PASSWORD),
            ViewerError::Unreadable(why) => write!(f, "not a readable Word document: {why}"),
            ViewerError::BadScale { scale } => write!(f, "scale {scale} is not a number above 0 and at most {MAX_SCALE}"),
            ViewerError::NoSuchPage { page, pages } => write!(f, "no page at index {page}: the document has {pages}"),
            ViewerError::TooManyPixels { width, height, limit } => {
                write!(f, "the page at this scale is {width}×{height} pixels; at most {limit} pixels and {} a side", wordcraft_render::MAX_SIDE)
            }
            ViewerError::BadFontName => write!(f, "a font name is 1 to {MAX_FONT_NAME_CHARS} characters"),
            ViewerError::FontTooLarge { bytes, limit } => write!(f, "the font file is {bytes} bytes; at most {limit} bytes"),
            ViewerError::BadFont(why) => write!(f, "not a usable font: {why}"),
            ViewerError::TooManyFonts { fonts, bytes } => {
                write!(f, "{fonts} fonts ({bytes} bytes) are installed; at most {MAX_FONTS} fonts and {MAX_FONT_TOTAL_BYTES} bytes")
            }
        }
    }
}

impl std::error::Error for ViewerError {}

impl From<wordcraft_docx::DocxError> for ViewerError {
    fn from(e: wordcraft_docx::DocxError) -> Self {
        match e {
            wordcraft_docx::DocxError::PasswordRequired => ViewerError::PasswordRequired,
            wordcraft_docx::DocxError::WrongPassword => ViewerError::WrongPassword,
            other => ViewerError::Unreadable(other.to_string()),
        }
    }
}

/// A page's size in points (1/72 inch).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageSize {
    pub width: f32,
    pub height: f32,
}

/// One rendered page: `width`×`height` pixels, four bytes each (red, green, blue, alpha, not
/// premultiplied), rows top to bottom. The layout a canvas `ImageData` takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// An opened document and its page layout. Read-only: nothing changes the document.
pub struct Session {
    doc: Document,
    layout: DocLayout,
}

/// Open the `.docx` package in `bytes` (decrypted with `password` when it is encrypted) and lay
/// it out. The document's embedded fonts are loaded first, as when the app opens it.
pub fn open(bytes: &[u8], password: Option<&str>) -> Result<Session, ViewerError> {
    open_with(bytes, password, true)
}

/// [`open`], without loading the document's embedded fonts: its text is laid out with the fonts
/// already available, as if nothing were embedded. For a host that opens documents from
/// strangers in a long-lived process (a server): the font database is process-wide and nothing
/// removes a font from it, so the fonts [`open`] loads would stay for every later document.
pub fn open_without_embedded_fonts(bytes: &[u8], password: Option<&str>) -> Result<Session, ViewerError> {
    open_with(bytes, password, false)
}

fn open_with(bytes: &[u8], password: Option<&str>, embedded_fonts: bool) -> Result<Session, ViewerError> {
    if bytes.is_empty() {
        return Err(ViewerError::EmptyDocument);
    }
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(ViewerError::DocumentTooLarge { bytes: bytes.len(), limit: MAX_DOCUMENT_BYTES });
    }
    let mut doc = wordcraft_docx::read_with_password(bytes, password)?;
    doc.ensure_nonempty();
    if embedded_fonts {
        load_embedded_fonts(&doc);
    }
    let layout = lay_out(&doc);
    Ok(Session { doc, layout })
}

/// Load the fonts `doc` embeds for this run, in memory only, as the engine does when it opens
/// a file: text set in a font that isn't installed is then laid out in it. The reader has
/// already bounded them; they don't count against [`MAX_FONTS`].
fn load_embedded_fonts(doc: &Document) {
    let files: Vec<(Vec<u8>, String)> = wordcraft_docx::embedded_fonts(doc)
        .into_iter()
        .filter_map(|f| wordcraft_fonts::decode_font_file(&f.data).ok().map(|data| (data, f.name)))
        .collect();
    if !files.is_empty() {
        wordcraft_fonts::FontDb::global().add_document_fonts(files);
    }
}

impl Session {
    /// How many pages the document has.
    pub fn page_count(&self) -> usize {
        self.layout.pages.len()
    }

    /// The size of page `page` (0-based), in points.
    pub fn page_size(&self, page: u32) -> Result<PageSize, ViewerError> {
        let p = self.page(page)?;
        Ok(PageSize { width: p.w, height: p.h })
    }

    /// The size in pixels [`Session::render`] draws page `page` (0-based) at `scale`, or the
    /// error it would refuse with, without drawing anything: for a host that sizes an image
    /// before asking for its pixels.
    pub fn raster_size(&self, page: u32, scale: f32) -> Result<(u32, u32), ViewerError> {
        raster_size(self.page(page)?, scale)
    }

    /// Page `page` (0-based) drawn at `scale` pixels per point.
    pub fn render(&self, page: u32, scale: f32) -> Result<PageImage, ViewerError> {
        let p = self.page(page)?;
        let (width, height) = raster_size(p, scale)?;
        let drawn = wordcraft_render::render_page(&self.doc, p, scale, &page_image_options());
        let mut pixels = drawn.pixels;
        // The rasteriser draws the size worked out above; anything else would hand the host a
        // buffer that doesn't match the size it is told.
        let bytes = u64::from(width).checked_mul(u64::from(height)).and_then(|n| n.checked_mul(4));
        if (drawn.width, drawn.height) != (width, height) || bytes != Some(pixels.len() as u64) {
            return Err(ViewerError::TooManyPixels { width: u64::from(width), height: u64::from(height), limit: MAX_RENDER_PIXELS });
        }
        straighten(&mut pixels);
        Ok(PageImage { width, height, pixels })
    }

    /// The fonts the laid-out text asks for that aren't available, so it is drawn in other
    /// faces: in name order, names differing only in case listed once. Install them
    /// ([`Session::install_font`]) to see the document as it was written.
    pub fn missing_fonts(&self) -> Vec<String> {
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        for (recorded, _) in self.layout.substituted_fonts() {
            let name = wordcraft_layout::para::reported_font_name(&recorded).trim();
            out.entry(name.to_lowercase()).or_insert_with(|| name.to_string());
        }
        out.into_values().collect()
    }

    /// [`install_font`], then lay this document out again so it uses the font. Other open
    /// sessions see the font once they [`Session::relayout`].
    pub fn install_font(&mut self, name: &str, bytes: &[u8]) -> Result<(), ViewerError> {
        install_font(name, bytes)?;
        self.relayout();
        Ok(())
    }

    /// Lay the document out again with the fonts now available.
    pub fn relayout(&mut self) {
        self.layout = lay_out(&self.doc);
    }

    fn page(&self, page: u32) -> Result<&wordcraft_layout::Page, ViewerError> {
        let no_page = || ViewerError::NoSuchPage { page, pages: self.layout.pages.len() };
        self.layout.pages.get(usize::try_from(page).map_err(|_| no_page())?).ok_or_else(no_page)
    }
}

/// What a page image shows, as `file.exportPng` draws it: the printed page, so headers and
/// footers at full strength (dimming them is the editor's cue while the body is edited).
fn page_image_options() -> wordcraft_render::RenderOptions {
    let mut opts = wordcraft_render::RenderOptions::default();
    opts.display.dim_header = false;
    opts
}

/// Premultiplied RGBA to straight RGBA, in place: the arithmetic of
/// `wordcraft_render::Rendered::to_straight`, which the PNG export uses, without its copy.
/// Paper is opaque, so on an ordinary page this changes nothing.
fn straighten(pixels: &mut [u8]) {
    for px in pixels.as_chunks_mut::<4>().0 {
        let [r, g, b, a] = px;
        let alpha = u32::from(*a);
        if alpha != 0 && alpha != 255 {
            for c in [r, g, b] {
                *c = ((u32::from(*c) * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
}

/// The raster [`wordcraft_render::render_page`] makes of `page` at `scale`, refused instead of
/// clamped or drawn smaller when it is out of bounds. The arithmetic is `render_page`'s own
/// (single precision, rounded up), so the size checked is the size drawn.
fn raster_size(page: &wordcraft_layout::Page, scale: f32) -> Result<(u32, u32), ViewerError> {
    if !(scale.is_finite() && scale > 0.0 && scale <= MAX_SCALE) {
        return Err(ViewerError::BadScale { scale });
    }
    // A float-to-integer cast saturates and sends NaN to 0: a hostile page size can't wrap.
    let side = |points: f32| ((points * scale).ceil() as u64).max(1);
    let (width, height) = (side(page.w), side(page.h.min(1e6)));
    let too_many = || ViewerError::TooManyPixels { width, height, limit: MAX_RENDER_PIXELS };
    let max_side = u64::from(wordcraft_render::MAX_SIDE);
    if width > max_side || height > max_side || width.checked_mul(height).is_none_or(|n| n > MAX_RENDER_PIXELS) {
        return Err(too_many());
    }
    Ok((u32::try_from(width).map_err(|_| too_many())?, u32::try_from(height).map_err(|_| too_many())?))
}

/// The page layout `wordcraft-cli render` draws from: print view, no markup area.
fn lay_out(doc: &Document) -> DocLayout {
    wordcraft_layout::layout(doc, &mut LayoutCache::new(), &LayoutOptions::default())
}

/// The fonts [`install_font`] has added in this process.
static FONT_BUDGET: FontBudget = FontBudget::new();

/// Counts installed fonts and their decoded bytes against [`MAX_FONTS`] and
/// [`MAX_FONT_TOTAL_BYTES`].
struct FontBudget {
    /// `(fonts, bytes)`.
    used: Mutex<(usize, usize)>,
}

impl FontBudget {
    const fn new() -> Self {
        FontBudget { used: Mutex::new((0, 0)) }
    }

    /// The counts, also after a panic elsewhere while they were held: they are two numbers,
    /// valid whatever happened.
    fn used(&self) -> MutexGuard<'_, (usize, usize)> {
        self.used.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Take room for one more font of `len` bytes, or say what is already used.
    fn reserve(&self, len: usize) -> Result<(), ViewerError> {
        let mut used = self.used();
        let (fonts, bytes) = *used;
        match (fonts.checked_add(1).filter(|n| *n <= MAX_FONTS), bytes.checked_add(len).filter(|n| *n <= MAX_FONT_TOTAL_BYTES)) {
            (Some(fonts), Some(bytes)) => {
                *used = (fonts, bytes);
                Ok(())
            }
            _ => Err(ViewerError::TooManyFonts { fonts, bytes }),
        }
    }

    /// Return what [`FontBudget::reserve`] took for a font that wasn't added after all.
    fn release(&self, len: usize) {
        let mut used = self.used();
        *used = (used.0.saturating_sub(1), used.1.saturating_sub(len));
    }
}

/// Make the font file `bytes` (TTF, OTF, TTC, WOFF or WOFF2) available to every session of this
/// process under `name`, the name documents ask for it by (its faces answer to their own
/// family names too). In memory only. A font that is already available is not an error. Open
/// sessions use it after [`Session::relayout`].
pub fn install_font(name: &str, bytes: &[u8]) -> Result<(), ViewerError> {
    let name = name.trim();
    if name.is_empty() || name.chars().nth(MAX_FONT_NAME_CHARS).is_some() {
        return Err(ViewerError::BadFontName);
    }
    if bytes.len() > MAX_FONT_BYTES {
        return Err(ViewerError::FontTooLarge { bytes: bytes.len(), limit: MAX_FONT_BYTES });
    }
    let data = wordcraft_fonts::decode_font_file(bytes).map_err(|e| ViewerError::BadFont(e.to_string()))?;
    let len = data.len();
    FONT_BUDGET.reserve(len)?;
    let fonts = wordcraft_fonts::FontDb::global();
    if fonts.add_document_fonts(vec![(data, name.to_string())]) > 0 {
        return Ok(());
    }
    // Nothing was added: every face was there already (fine), or the file has no usable face.
    FONT_BUDGET.release(len);
    if fonts.has_family(name) { Ok(()) } else { Err(ViewerError::BadFont("the file has no usable face".into())) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_budget_bounds_count_and_bytes() {
        let b = FontBudget::new();
        for _ in 0..MAX_FONTS {
            b.reserve(1).unwrap();
        }
        assert_eq!(b.reserve(1), Err(ViewerError::TooManyFonts { fonts: MAX_FONTS, bytes: MAX_FONTS }));
        // A refused font takes nothing; a released one makes room again.
        assert_eq!(*b.used(), (MAX_FONTS, MAX_FONTS));
        b.release(1);
        b.reserve(1).unwrap();

        let b = FontBudget::new();
        b.reserve(MAX_FONT_TOTAL_BYTES).unwrap();
        assert_eq!(b.reserve(1), Err(ViewerError::TooManyFonts { fonts: 1, bytes: MAX_FONT_TOTAL_BYTES }));
        assert!(matches!(b.reserve(usize::MAX), Err(ViewerError::TooManyFonts { .. })));
        assert_eq!(*b.used(), (1, MAX_FONT_TOTAL_BYTES));
    }

    #[test]
    fn straightening_matches_the_png_export() {
        let pixels: Vec<u8> = (0..=255u8).flat_map(|a| [a / 2, a, a / 3, a]).chain([10, 20, 30, 0, 200, 100, 50, 255, 9, 9, 9, 3]).collect();
        let rendered = wordcraft_render::Rendered { width: u32::try_from(pixels.len() / 4).unwrap(), height: 1, pixels: pixels.clone() };
        let mut ours = pixels;
        straighten(&mut ours);
        assert_eq!(ours, rendered.to_straight());
    }

    #[test]
    fn every_error_has_its_own_code_and_text() {
        let all = [
            ViewerError::EmptyDocument,
            ViewerError::DocumentTooLarge { bytes: 2, limit: 1 },
            ViewerError::PasswordRequired,
            ViewerError::WrongPassword,
            ViewerError::Unreadable("x".into()),
            ViewerError::BadScale { scale: f32::NAN },
            ViewerError::NoSuchPage { page: 9, pages: 1 },
            ViewerError::TooManyPixels { width: 1, height: 1, limit: 0 },
            ViewerError::BadFontName,
            ViewerError::FontTooLarge { bytes: 2, limit: 1 },
            ViewerError::BadFont("x".into()),
            ViewerError::TooManyFonts { fonts: 1, bytes: 1 },
        ];
        let codes: std::collections::HashSet<&str> = all.iter().map(ViewerError::code).collect();
        assert_eq!(codes.len(), all.len());
        assert!(all.iter().all(|e| !e.to_string().is_empty()));
    }
}
