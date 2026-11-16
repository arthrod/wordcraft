//! Which files the web shell hands to its on-demand workers (implementation plan 10.2).
//!
//! The browser build leaves the extra formats and the PDF writer out of the main module; the
//! codecs worker converts those formats to and from `.docx` bytes and the PDF worker writes a
//! PDF from `.docx` bytes. This module only decides, from a file's name and first bytes, which
//! of them a file belongs to — no browser types, so host tests cover it. The workers
//! themselves are driven from `lazy_io`.

/// Formats the codecs worker converts and the main module leaves out, as lower-case
/// extensions (the format crate's `all-formats` set).
const CODEC_FORMATS: &[&str] = &["rtf", "odt", "ott", "fodt", "html", "htm", "xhtml", "mht", "mhtml", "tex", "latex", "ltx"];

const ZIP_SIG: &[u8] = b"PK\x03\x04";
const OLE_SIG: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const RTF_SIG: &[u8] = b"{\\rtf";

/// The lower-case extension of `name` (no dot), empty when it has none.
fn ext_of(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.contains(['/', '\\']) => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// The format to ask the codecs worker to read `bytes` as, when opening `name` needs it.
///
/// `None` leaves the file to the engine: formats it reads itself, and files whose content
/// says they are a Word package whatever their name (the engine opens those by content, and
/// asks for a password when one is needed). `encrypted_package` is whether `bytes` is a
/// password-protected Word package (`wordcraft_docx::is_encrypted`), passed in so this stays
/// free of the reader.
pub(crate) fn import_format(name: &str, bytes: &[u8], encrypted_package: bool) -> Option<String> {
    let ext = ext_of(name);
    match ext.as_str() {
        // Word's own extensions are trusted least: an `.rtf` or `.doc` is whatever its first
        // bytes say (Word writes RTF under `.doc`, and packages under either).
        "doc" | "dot" | "rtf" => {
            if bytes.starts_with(ZIP_SIG) || encrypted_package {
                None
            } else if bytes.starts_with(RTF_SIG) {
                Some("rtf".into())
            } else if bytes.starts_with(OLE_SIG) {
                Some("doc".into())
            } else {
                None
            }
        }
        other if CODEC_FORMATS.contains(&other) => Some(ext),
        _ => None,
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

/// The worker that writes `name`'s format, or `None` when the engine writes it itself.
pub(crate) fn export_route(name: &str) -> Option<ExportRoute> {
    let ext = ext_of(name);
    match ext.as_str() {
        "pdf" => Some(ExportRoute::Pdf),
        other if CODEC_FORMATS.contains(&other) => Some(ExportRoute::Codec(ext)),
        _ => None,
    }
}

/// The name a converted file opens under: the engine picks a reader by extension, and what
/// it is handed is a Word package, so `report.odt` opens as `report.docx`. The original name
/// stays in the messages the user sees.
pub(crate) fn opened_name(name: &str) -> String {
    let stem = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.contains(['/', '\\']) => stem,
        _ => name,
    };
    format!("{stem}.docx")
}

/// What the user is told when a worker did not deliver, short enough for the status bar: the
/// file and what was being done, then the reason in plain words. When the worker's own files
/// could not be loaded the reason is that, with what to do about it; the worker's wording goes
/// to the console instead.
pub(crate) fn failure_message(action: &str, name: &str, error: &str) -> String {
    let unloaded = error.contains("worker stopped") || error.contains("unavailable in this browser");
    if unloaded {
        format!("Couldn't {action} {name}: this format's converter could not be loaded. Check the connection and try again.")
    } else {
        format!("Couldn't {action} {name}: {error}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLE: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0, 0];

    #[test]
    fn extra_formats_open_through_the_codecs_worker() {
        for (name, format) in [
            ("letter.rtf", "rtf"),
            ("Letter.RTF", "rtf"),
            ("notes.odt", "odt"),
            ("template.ott", "ott"),
            ("page.html", "html"),
            ("page.htm", "htm"),
            ("archive.mht", "mht"),
            ("paper.tex", "tex"),
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

    #[test]
    fn exports_route_to_the_worker_that_writes_them() {
        assert_eq!(export_route("Report.pdf"), Some(ExportRoute::Pdf));
        assert_eq!(export_route("report.PDF"), Some(ExportRoute::Pdf));
        assert_eq!(export_route("report.rtf"), Some(ExportRoute::Codec("rtf".into())));
        assert_eq!(export_route("report.odt"), Some(ExportRoute::Codec("odt".into())));
        assert_eq!(export_route("report.html"), Some(ExportRoute::Codec("html".into())));
        for name in ["report.docx", "report.dotx", "report.md", "report.txt", "page.png", "report.xml", "report.json", "report", "pdf"] {
            assert_eq!(export_route(name), None, "{name}");
        }
    }

    #[test]
    fn converted_files_open_as_word_documents() {
        assert_eq!(opened_name("report.odt"), "report.docx");
        assert_eq!(opened_name("my.report.v2.rtf"), "my.report.v2.docx");
        assert_eq!(opened_name("noextension"), "noextension.docx");
        assert_eq!(opened_name(".rtf"), ".rtf.docx");
    }

    #[test]
    fn a_worker_that_did_not_load_is_named_in_plain_words() {
        for error in [
            "conversion failed: the codec worker stopped",
            "pdf export failed: the pdf worker stopped",
            "the codec worker is unavailable in this browser",
        ] {
            let m = failure_message("open", "letter.rtf", error);
            assert_eq!(m, "Couldn't open letter.rtf: this format's converter could not be loaded. Check the connection and try again.");
        }
        // A conversion error is the file's own problem and is passed on as it is.
        let m = failure_message("open", "letter.rtf", "conversion failed: not RTF");
        assert_eq!(m, "Couldn't open letter.rtf: conversion failed: not RTF");
    }
}
