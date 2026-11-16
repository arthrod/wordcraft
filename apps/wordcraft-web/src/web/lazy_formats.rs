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
}
