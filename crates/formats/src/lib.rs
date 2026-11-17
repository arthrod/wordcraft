//! WordCraft import/export for the "other" text formats: plain text, Markdown (CommonMark subset
//! plus GFM tables and strikethrough), HTML (and MHT web archives, read-only), RTF, OpenDocument
//! Text and LaTeX — and, read-only, spreadsheet cells for mail-merge recipient lists ([`sheet`]:
//! .xlsx/.xlsm/.xls/.ods).
//!
//! Every format maps to a small flow model ([`model::Flow`]: paragraphs with a kind, list
//! membership, alignment and formatted spans; tables of cells), which [`model::to_doc`] and
//! [`model::from_doc`] convert to and from the full [`Document`]. All parsers are written from
//! the public specifications, are lenient by design and never panic on hostile input: nesting is
//! depth-limited and sizes are capped.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod comments;
#[cfg(feature = "all-formats")]
pub mod html;
#[cfg(feature = "all-formats")]
pub mod latex;
pub mod markdown;
#[cfg(feature = "all-formats")]
pub mod mathml;
#[cfg(feature = "all-formats")]
pub mod mht;
pub mod model;
#[cfg(feature = "all-formats")]
pub mod odt;
#[cfg(feature = "all-formats")]
pub mod rtf;
#[cfg(feature = "all-formats")]
pub mod sheet;
#[cfg(not(feature = "all-formats"))]
pub mod sheet {
    use std::collections::{HashMap, HashSet};

    /// Field names from a header row, as the spreadsheet codec's `sheet::field_names` gives them:
    /// trimmed, blanks named `Column N` and repeats numbered, ignoring case. Delimited text
    /// recipient lists need it in builds without the codec.
    pub fn field_names(row: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(row.len());
        let mut taken: HashSet<String> = HashSet::with_capacity(row.len());
        let mut next: HashMap<String, usize> = HashMap::new();
        for (i, h) in row.iter().enumerate() {
            let base = match h.trim() {
                "" => format!("Column {}", i.saturating_add(1)),
                t => t.to_string(),
            };
            let key = base.to_ascii_lowercase();
            if taken.insert(key.clone()) {
                out.push(base);
                continue;
            }
            let mut n = next.get(&key).copied().unwrap_or(2);
            loop {
                let candidate = format!("{base} {n}");
                n = n.saturating_add(1);
                if taken.insert(candidate.to_ascii_lowercase()) {
                    next.insert(key, n);
                    out.push(candidate);
                    break;
                }
            }
        }
        out
    }

    /// File extensions read as spreadsheets.
    pub const EXTENSIONS: &[&str] = &["xlsx", "xlsm", "xls", "ods"];

    /// Whether `ext` (no dot, any case) names a spreadsheet read here.
    pub fn is_extension(ext: &str) -> bool {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        EXTENSIONS.contains(&ext.as_str())
    }
}
pub mod txt;

use wordcraft_doc::Document;

/// Most entries an ODT or spreadsheet package may declare, as many as a Word package
/// (`wordcraft_docx::package::MAX_ENTRIES`); checked before `zip` parses them (#3660).
pub const MAX_ZIP_ENTRIES: u64 = 20_000;

/// Extensions this crate handles.
#[cfg(feature = "all-formats")]
pub const EXTENSIONS: &[&str] =
    &["txt", "text", "md", "markdown", "html", "htm", "xhtml", "mht", "mhtml", "rtf", "odt", "ott", "fodt", "tex", "latex", "ltx"];

/// Extensions this crate handles in core builds (`txt` and `markdown` only).
#[cfg(not(feature = "all-formats"))]
pub const EXTENSIONS: &[&str] = &["txt", "text", "md", "markdown"];

/// Extensions of formats this crate knows but the active build leaves out (`all-formats` off):
/// callers report them as unavailable instead of opening them as plain text.
#[cfg(feature = "all-formats")]
pub const UNAVAILABLE_EXTENSIONS: &[&str] = &[];

/// Extensions of formats this crate knows but the active build leaves out (`all-formats` off):
/// callers report them as unavailable instead of opening them as plain text.
#[cfg(not(feature = "all-formats"))]
pub const UNAVAILABLE_EXTENSIONS: &[&str] = &["html", "htm", "xhtml", "mht", "mhtml", "rtf", "odt", "ott", "fodt", "tex", "latex", "ltx"];

/// Import `bytes` in the format named by `ext` (no dot, any case). `None` = not our format.
pub fn import(ext: &str, bytes: &[u8]) -> Option<Result<Document, String>> {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    let r = match ext.as_str() {
        "txt" | "text" => Ok(txt::import(bytes)),
        "md" | "markdown" => Ok(markdown::import(&txt::decode(bytes))),
        #[cfg(feature = "all-formats")]
        "html" | "htm" | "xhtml" => Ok(html::import_page(&html::decode(bytes), &|_| None)),
        #[cfg(feature = "all-formats")]
        "mht" | "mhtml" => Ok(mht::import(bytes)),
        #[cfg(feature = "all-formats")]
        "rtf" => rtf::import(bytes),
        #[cfg(feature = "all-formats")]
        "odt" | "ott" => odt::import(bytes),
        #[cfg(feature = "all-formats")]
        "fodt" => odt::import_flat(bytes),
        #[cfg(feature = "all-formats")]
        "tex" | "latex" | "ltx" => Ok(latex::import(bytes)),
        _ => return None,
    };
    Some(r.map(|mut d| {
        d.ensure_nonempty();
        d
    }))
}

/// [`import`] onto `page` (its size and margins) for the formats whose files give no page size:
/// plain text, Markdown, HTML and MHT start on it, and their tables size to it (#1381). The other
/// formats keep the page their file gives.
pub fn import_on(ext: &str, bytes: &[u8], page: &wordcraft_doc::SectionProps) -> Option<Result<Document, String>> {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    let mut doc = match ext.as_str() {
        #[cfg(feature = "all-formats")]
        "odt" | "ott" => return Some(odt::import_on(bytes, page)),
        #[cfg(feature = "all-formats")]
        "fodt" => return Some(odt::import_flat_on(bytes, page)),
        "txt" | "text" => txt::import_on(bytes, page),
        "md" | "markdown" => markdown::import_on(&txt::decode(bytes), page),
        #[cfg(feature = "all-formats")]
        "html" | "htm" | "xhtml" => html::import_on(&html::decode(bytes), &|_| None, page),
        #[cfg(feature = "all-formats")]
        "mht" | "mhtml" => mht::import_on(bytes, page),
        _ => return import(&ext, bytes),
    };
    doc.ensure_nonempty();
    Some(Ok(doc))
}

/// Export `doc` in the format named by `ext`. `None` = not our format.
pub fn export(ext: &str, doc: &Document) -> Option<Result<Vec<u8>, String>> {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    Some(match ext.as_str() {
        "txt" | "text" => Ok(txt::export(doc)),
        "md" | "markdown" => Ok(markdown::export(doc).into_bytes()),
        #[cfg(feature = "all-formats")]
        "html" | "htm" | "xhtml" => Ok(html::export(doc).into_bytes()),
        #[cfg(feature = "all-formats")]
        "rtf" => Ok(rtf::export(doc).into_bytes()),
        #[cfg(feature = "all-formats")]
        "odt" => odt::export(doc),
        #[cfg(feature = "all-formats")]
        "ott" => odt::export_template(doc),
        #[cfg(feature = "all-formats")]
        "fodt" => odt::export_flat(doc),
        #[cfg(feature = "all-formats")]
        "tex" | "latex" | "ltx" => Ok(latex::export(doc).into_bytes()),
        _ => return None,
    })
}

/// How [`export_with`] writes a copy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportOptions {
    /// Print Markup: HTML, Markdown and plain text end with the document's comments (who wrote
    /// each, the text it comments on, what it says, its replies). Other formats keep comments
    /// their own way or not at all.
    pub comments: bool,
}

/// [`export`] with options.
pub fn export_with(ext: &str, doc: &Document, opts: &ExportOptions) -> Option<Result<Vec<u8>, String>> {
    let out = export(ext, doc)?;
    if !opts.comments {
        return Some(out);
    }
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    Some(out.map(|bytes| comments::append(&ext, doc, bytes)))
}

#[cfg(test)]
#[cfg(feature = "all-formats")]
mod test_log;
#[cfg(test)]
#[cfg(feature = "all-formats")]
mod tests;

#[cfg(test)]
#[cfg(not(feature = "all-formats"))]
mod core_tests {
    #[test]
    fn txt_and_markdown_work_without_all_formats() {
        let md = b"# Hello\n\nSome **bold** text.";
        let doc = crate::import("md", md).expect("md").expect("valid md");
        let out = crate::export("md", &doc).expect("md export").expect("ok");
        assert!(String::from_utf8_lossy(&out).contains("bold"));
        let txt = crate::export("txt", &doc).expect("txt export").expect("ok");
        assert!(String::from_utf8_lossy(&txt).contains("Hello"));
        assert!(crate::import("rtf", b"{\\rtf1}").is_none());
    }
}
