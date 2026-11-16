//! Strict export (#6005): the report of what a PDF or PNG would leave out silently (characters no
//! face can draw, pictures that are missing or can't be decoded), and the refusal built from it.
//!
//! The check runs on the layout and display options the export itself uses, so what it reports is
//! what the output lacks.

use std::ops::RangeInclusive;

use serde_json::{Value, json};
use wordcraft_doc::{Document, MAX_MISSING_PICTURES, MissingPicture};
use wordcraft_layout::display::DisplayOptions;
use wordcraft_layout::{DocLayout, LayoutCache, StrictOutput};

use crate::io::ExportOptions;

/// The report's JSON schema version; fields may be added without raising it.
pub const SCHEMA: u32 = 1;

/// What a strict export found missing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StrictReport {
    /// (code point, times drawn), sorted by code point.
    pub glyphs: Vec<(char, u64)>,
    pub pictures: Vec<MissingPicture>,
    /// More distinct characters were missing than are listed.
    pub glyphs_truncated: bool,
    /// More pictures were missing than are listed.
    pub pictures_truncated: bool,
    /// Distinct characters found missing, those past the list's cap included.
    pub total_glyphs: usize,
    /// Pictures found missing, those past the list's cap included.
    pub total_pictures: u64,
}

impl StrictReport {
    /// Nothing is missing: the strict export passes.
    pub fn passed(&self) -> bool {
        self.glyphs.is_empty() && self.pictures.is_empty() && !self.truncated()
    }

    /// Either list was cut at its cap.
    pub fn truncated(&self) -> bool {
        self.glyphs_truncated || self.pictures_truncated
    }

    /// The schema-1 JSON of the report.
    pub fn to_json(&self) -> Value {
        json!({
            "schema": SCHEMA,
            "strict": if self.passed() { "passed" } else { "failed" },
            "missing_glyphs": self.glyphs.iter().map(|(c, n)| json!({"cp": code_point(*c), "count": n})).collect::<Vec<_>>(),
            "missing_pictures": self.pictures.iter().map(|p| json!({"part": p.part, "source_part": p.story, "reason": p.reason.code()})).collect::<Vec<_>>(),
            "truncated": self.truncated(),
            "total_missing_glyphs": self.total_glyphs,
            "total_missing_pictures": self.total_pictures,
            "glyphs_truncated": self.glyphs_truncated,
            "pictures_truncated": self.pictures_truncated,
        })
    }

    /// The human lines (without the `strict: ` prefix): one for the characters, one for the
    /// pictures; none when nothing is missing.
    fn parts(&self) -> Vec<String> {
        let glyphs: Vec<(String, u64)> = self.glyphs.iter().map(|(c, n)| (code_point(*c), *n)).collect();
        let pictures: Vec<(String, String, String)> =
            self.pictures.iter().map(|p| (p.part.clone(), story_label(&p.story).to_string(), p.reason.label().to_string())).collect();
        human_parts(&glyphs, &pictures, (self.glyphs_truncated, self.pictures_truncated), (self.total_glyphs as u64, self.total_pictures))
    }

    /// The human lines, each `strict: …`.
    pub fn lines(&self) -> Vec<String> {
        self.parts().into_iter().map(|p| format!("strict: {p}")).collect()
    }

    /// The refusal's message: `strict: …`, the lines joined.
    pub fn refusal(&self) -> String {
        format!("strict: {}", self.parts().join("; "))
    }
}

/// The human text of a report: (code point text, count) and (part, story label, reason words).
fn human_parts(glyphs: &[(String, u64)], pictures: &[(String, String, String)], cut: (bool, bool), totals: (u64, u64)) -> Vec<String> {
    let (glyphs_cut, pictures_cut) = cut;
    let mut out = Vec::new();
    if !glyphs.is_empty() {
        let list: Vec<String> = glyphs.iter().map(|(cp, n)| format!("{cp} (×{n})")).collect();
        let more = if glyphs_cut { ", …" } else { "" };
        let listed = glyphs.len() as u64;
        let n = if glyphs_cut { totals.0.max(listed) } else { listed };
        let first = if n > listed { format!(" (first {listed} listed)") } else { String::new() };
        out.push(format!("{n} character{} cannot be drawn{first}: {}{more}", if n == 1 { "" } else { "s" }, list.join(", ")));
    }
    if !pictures.is_empty() {
        let list: Vec<String> = pictures
            .iter()
            .map(|(part, at, reason)| if at.is_empty() { format!("{part}: {reason}") } else { format!("{part} ({at}): {reason}") })
            .collect();
        let more = if pictures_cut { "; …" } else { "" };
        let listed = pictures.len() as u64;
        let n = if pictures_cut { totals.1.max(listed) } else { listed };
        let first = if n > listed { format!(" (first {listed} listed)") } else { String::new() };
        out.push(format!("{n} picture(s) left out{first}: {}{more}", list.join("; ")));
    }
    out
}

/// The `strict: …` human lines of a report in its JSON form (what `file.strictReport` returns).
/// Anything the JSON lacks is left out.
pub fn lines_from_json(report: &Value) -> Vec<String> {
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let glyphs: Vec<(String, u64)> = report
        .get("missing_glyphs")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|g| (text(g, "cp"), g.get("count").and_then(Value::as_u64).unwrap_or(0))).collect())
        .unwrap_or_default();
    let pictures: Vec<(String, String, String)> = report
        .get("missing_pictures")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|p| (text(p, "part"), story_label(&text(p, "source_part")).to_string(), reason_label(&text(p, "reason")))).collect())
        .unwrap_or_default();
    let flag = |k: &str| report.get(k).and_then(Value::as_bool).unwrap_or(false);
    // A report without the per-list flags (the top-level one alone) marks both lists.
    let (glyphs_cut, pictures_cut) = match (report.get("glyphs_truncated"), report.get("pictures_truncated")) {
        (None, None) => (flag("truncated"), flag("truncated")),
        _ => (flag("glyphs_truncated"), flag("pictures_truncated")),
    };
    let total = |k: &str, listed: usize| report.get(k).and_then(Value::as_u64).unwrap_or(listed as u64);
    let totals = (total("total_missing_glyphs", glyphs.len()), total("total_missing_pictures", pictures.len()));
    human_parts(&glyphs, &pictures, (glyphs_cut, pictures_cut), totals).into_iter().map(|p| format!("strict: {p}")).collect()
}

/// The words for a JSON reason code; a code this version does not know is shown as it is.
fn reason_label(code: &str) -> String {
    wordcraft_doc::MissingPictureReason::from_code(code).map_or_else(|| code.to_string(), |r| r.label().to_string())
}

/// `U+2D30`: upper-case hex, at least four digits.
fn code_point(c: char) -> String {
    format!("U+{:04X}", c as u32)
}

/// The part a picture is in, as the report names it: `header2.xml` for `word/header2.xml`.
fn story_label(story: &str) -> &str {
    story.strip_prefix("word/").unwrap_or(story)
}

/// The report of `doc` as drawn by `lay` with `display` into `output`, on `pages` (0-based; `None`
/// is every page). The watermark is checked with the page, and the pictures dropped when the
/// document was opened are added to those that can't be decoded now, capped like each.
pub fn strict_report(
    doc: &Document,
    lay: &DocLayout,
    display: &DisplayOptions,
    pages: Option<RangeInclusive<usize>>,
    output: StrictOutput,
) -> StrictReport {
    let mut missing = lay.missing_glyphs(doc, display, pages.clone());
    let any_page = lay.pages.iter().enumerate().any(|(n, _)| pages.as_ref().is_none_or(|r| r.contains(&n)));
    if any_page && let Some(wm) = &doc.settings.watermark {
        let max = match output {
            StrictOutput::Pdf => wordcraft_pdf::WATERMARK_MAX_CHARS,
            StrictOutput::Png => usize::MAX,
        };
        missing.merge(wordcraft_layout::missing_watermark_glyphs(wm, max));
    }
    let (undecodable, mut pictures_truncated, undecodable_total) = wordcraft_layout::undecodable_pictures(doc, lay, pages, display, output);
    pictures_truncated |= doc.missing_pictures.truncated;
    // The two lists overlap where the same picture is in both; those are counted once.
    let mut overlap: u64 = 0;
    let mut pictures: Vec<MissingPicture> = Vec::new();
    for p in doc.missing_pictures.list.iter().cloned().chain(undecodable) {
        if pictures.contains(&p) {
            overlap = overlap.saturating_add(1);
            continue;
        }
        if pictures.len() >= MAX_MISSING_PICTURES {
            pictures_truncated = true;
            break;
        }
        pictures.push(p);
    }
    let total_pictures = doc.missing_pictures.total.saturating_add(undecodable_total).saturating_sub(overlap).max(pictures.len() as u64);
    StrictReport {
        glyphs: missing.chars,
        pictures,
        glyphs_truncated: missing.truncated,
        pictures_truncated,
        total_glyphs: missing.total,
        total_pictures,
    }
}

/// The PDF export's options for `opts`: the one place they are built, shared by the export and
/// its strict check.
pub(crate) fn pdf_options(opts: &ExportOptions) -> wordcraft_pdf::PdfOptions {
    wordcraft_pdf::PdfOptions { include_markup: opts.markup, comments: opts.markup && opts.comments, numeral: opts.numeral, ..Default::default() }
}

/// What a page image shows: the display options `file.exportPng` renders with.
pub(crate) fn png_display() -> DisplayOptions {
    // A page image is the printed page: headers and footers at full strength.
    let mut opts = wordcraft_render::RenderOptions::default();
    opts.display.dim_header = false;
    opts.display
}

/// The strict report of a PDF export of `doc`: every page, laid out as the export lays it out.
pub(crate) fn pdf_report(doc: &Document, opts: &ExportOptions) -> StrictReport {
    let pdf = pdf_options(opts);
    let lay = wordcraft_layout::layout(doc, &mut LayoutCache::new(), &pdf.layout_options());
    strict_report(doc, &lay, &pdf.display_options(), None, StrictOutput::Pdf)
}

/// The refusal a strict PDF export of `doc` meets ([`export_pdf`]'s message), or `None` when
/// nothing is missing: for a host that writes the PDF elsewhere (the web app's PDF worker) and
/// must refuse exactly as the engine does. `doc` is the document as printed (`output_fields`).
pub fn pdf_refusal(doc: &Document, opts: &ExportOptions) -> Option<String> {
    let report = pdf_report(doc, opts);
    (!report.passed()).then(|| report.refusal())
}

/// Most faces [`pdf_faces`] lists. The font database bounds faces per file and font files, not
/// faces (a document may embed many collections), so this bounds the list a hostile document
/// can make a host hold; a longer list is cut and says so ([`PdfFaces::cut`]).
pub const MAX_PDF_FACES: usize = 4096;

/// The faces a PDF export draws text in ([`pdf_faces`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PdfFaces {
    /// `(typographic family, style)`, each once, in the order first drawn.
    pub faces: Vec<(String, String)>,
    /// The PDF draws text in more faces than the list holds: some are left out, so the list
    /// cannot show that a face is unused. A strict host refuses what it cannot check.
    pub cut: bool,
}

/// The faces a PDF export of `doc` draws text in, as `(typographic family, style)`, each once in
/// the order first drawn: the faces its fonts ask for and the fallback faces drawn instead, over
/// the display list the export draws (body, headers, footers, notes, text boxes, tables, charts,
/// diagrams, equations), the text of metafile pictures (the page picture too), which the export
/// draws as text, and the watermark. For a host that writes the PDF elsewhere with copies of its faces (the web app's PDF worker) and
/// must know which copies this document needs. Lays the document out, so a host asks only when
/// it has to choose. `doc` is the document as printed (`output_fields`). At most
/// [`MAX_PDF_FACES`], else [`PdfFaces::cut`].
pub fn pdf_faces(doc: &Document, opts: &ExportOptions) -> PdfFaces {
    pdf_faces_within(doc, opts, MAX_PDF_FACES)
}

/// [`pdf_faces`] holding at most `limit` faces: past it the list is cut and says so.
pub fn pdf_faces_within(doc: &Document, opts: &ExportOptions, limit: usize) -> PdfFaces {
    use wordcraft_layout::display::{Draw, page_display};
    let pdf = pdf_options(opts);
    let lay = wordcraft_layout::layout(doc, &mut LayoutCache::new(), &pdf.layout_options());
    let display = pdf.display_options();
    let mut seen = std::collections::HashSet::new();
    let mut out = PdfFaces::default();
    let mut add = |face: &wordcraft_fonts::FaceRef| {
        if seen.contains(&face.id()) {
            return;
        }
        if out.faces.len() >= limit {
            out.cut = true;
            return;
        }
        seen.insert(face.id());
        let f = face.get();
        out.faces.push((f.family.clone(), f.style.clone()));
    };
    // Each picture is read once: its text draws in the same faces wherever it is placed.
    let mut pictures = std::collections::HashSet::new();
    let mut picture_faces = |media: &str| -> Vec<wordcraft_fonts::FaceRef> {
        if !pictures.insert(media.to_string()) {
            return Vec::new();
        }
        doc.media.get(media).map(|bytes| wordcraft_layout::metafile_text_faces(bytes)).unwrap_or_default()
    };
    for page in &lay.pages {
        // A stack, not recursion: nesting depth comes from the document.
        let mut stack = page_display(doc, page, &display);
        while let Some(d) = stack.pop() {
            match d {
                Draw::Glyphs { face, .. } => add(&face),
                Draw::Image { media, .. } => picture_faces(&media).iter().for_each(&mut add),
                Draw::Figure { draws: items, .. } | Draw::Turned { items, .. } | Draw::Rotated { items, .. } | Draw::Clip { items, .. } => {
                    stack.extend(items.into_iter().rev());
                }
                _ => {}
            }
        }
    }
    // The page picture is drawn behind every page, outside the display list.
    if !lay.pages.is_empty()
        && let Some(pic) = &doc.settings.page_picture
    {
        picture_faces(&pic.media).iter().for_each(&mut add);
    }
    if !lay.pages.is_empty()
        && let Some(wm) = &doc.settings.watermark
        && let Some((face, _, _)) = wordcraft_layout::watermark_run(wm, wordcraft_pdf::WATERMARK_MAX_CHARS)
    {
        add(&face);
    }
    out
}

/// The PDF of `doc`; with `opts.strict`, only if nothing is missing, laid out once for the check
/// and the export.
pub(crate) fn export_pdf(doc: &Document, opts: &ExportOptions) -> Result<Vec<u8>, String> {
    let pdf = pdf_options(opts);
    if !opts.strict {
        return wordcraft_pdf::export(doc, &pdf).map_err(|e| e.to_string());
    }
    let lay = wordcraft_layout::layout(doc, &mut LayoutCache::new(), &pdf.layout_options());
    let report = strict_report(doc, &lay, &pdf.display_options(), None, StrictOutput::Pdf);
    if !report.passed() {
        return Err(report.refusal());
    }
    wordcraft_pdf::export_layout(doc, &lay, &pdf).map_err(|e| e.to_string())
}

/// Is `path` a PDF or PNG output (the only outputs a strict export applies to)?
pub(crate) fn is_strict_output(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".pdf") || lower.ends_with(".png")
}
