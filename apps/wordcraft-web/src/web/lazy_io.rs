//! The app's own Open, Save As and Export through the on-demand workers (implementation plan
//! 10.2), and the same two jobs for the page API (`convertDocument`, `exportPdf`).
//!
//! The main module leaves the extra formats and the PDF writer out; the codecs worker and the
//! PDF worker hold them and are fetched the first time a job needs them. `.docx` bytes are the
//! only document form that crosses to a worker. Each job answers on a later task turn, so the
//! shell stays responsive: Open puts the converted package into the inbox the file picker and
//! drops already use, and Save As / Export hand the finished bytes to the browser download.
//! Progress and failures go to the app's status bar (and the console); a worker that cannot be
//! loaded leaves the open document as it was.

use std::cell::RefCell;
use std::rc::Rc;

use wordcraft_ui_egui::Inbox;
use wordcraft_ui_egui::async_saves::AsyncSaves;
use wordcraft_worker_proto as proto;

use super::{codec_worker, pdf_worker};
use crate::lazy_formats::{ExportRoute, export_route, failure_message, import_format, opened_name};

/// Status-bar lines waiting for the shell's next frame.
pub(crate) type Messages = Rc<RefCell<Vec<String>>>;

const NO_CODEC_WORKER: &str = "the codec worker is unavailable in this browser";
const NO_PDF_WORKER: &str = "the pdf worker is unavailable in this browser";

/// A reply continuation that runs at most once, from whichever path gets there first: the
/// worker's reply, or the send that failed.
struct Once<F>(Rc<RefCell<Option<F>>>);

impl<F: FnOnce(Result<Vec<u8>, String>)> Once<F> {
    fn new(f: F) -> Self {
        Once(Rc::new(RefCell::new(Some(f))))
    }

    fn share(&self) -> Self {
        Once(self.0.clone())
    }

    fn call(&self, reply: Result<Vec<u8>, String>) {
        // Taken out before it runs: the continuation may start another job.
        let f = self.0.borrow_mut().take();
        if let Some(f) = f {
            f(reply);
        }
    }
}

/// Convert between `.docx` and another format in the codecs worker, starting it on first
/// use. `on_reply` runs exactly once with the converted bytes or the reason there are none —
/// at once when the job cannot be sent, otherwise on a later task turn.
pub(crate) fn convert(asset_base: &str, req: &proto::ConvertRequest, on_reply: impl FnOnce(Result<Vec<u8>, String>) + 'static) {
    let on_reply = Once::new(on_reply);
    if req.input.len() > proto::MAX_CONVERT_BYTES {
        return on_reply.call(Err(format!("the document is too large to convert in the browser ({} bytes)", req.input.len())));
    }
    if !codec_worker::ensure(asset_base) {
        return on_reply.call(Err(NO_CODEC_WORKER.into()));
    }
    let for_reply = on_reply.share();
    let sent = codec_worker::with(|w| w.convert(req, move |reply| for_reply.call(reply.map_err(|e| format!("conversion failed: {}", e.message)))));
    match sent {
        Some(Ok(_)) => {}
        Some(Err(e)) => on_reply.call(Err(format!("the codec worker could not take the conversion: {e}"))),
        None => on_reply.call(Err(NO_CODEC_WORKER.into())),
    }
}

/// Write `docx` as a PDF in the PDF worker, starting it on first use. The worker lays out
/// with the faces the page shows: every face beyond its compiled-in set goes along as a copy
/// (user fonts, pack faces, fonts embedded in the document), within the protocol's budgets.
/// `on_reply` runs exactly once, as for [`convert`].
pub(crate) fn render_pdf(asset_base: &str, docx: Vec<u8>, on_reply: impl FnOnce(Result<Vec<u8>, String>) + 'static) {
    let on_reply = Once::new(on_reply);
    if docx.is_empty() {
        return on_reply.call(Err("nothing to export".into()));
    }
    if docx.len() > proto::MAX_PDF_DOC_BYTES {
        return on_reply.call(Err(format!("the document is too large to export as PDF in the browser ({} bytes)", docx.len())));
    }
    // What does not fit the budgets is left out with a log, and the export still runs: those
    // faces fall back inside the worker.
    let mut fonts: Vec<proto::FontCopy> = Vec::new();
    let mut total = 0usize;
    for (family, style, bytes) in wordcraft_fonts::FontDb::global().runtime_faces() {
        if fonts.len() >= proto::MAX_FONT_COPIES || total.saturating_add(bytes.len()) > proto::MAX_FONT_TOTAL_BYTES {
            log::warn!("pdf export: the font copies hit a protocol budget; {family:?} is left to the worker's fallback");
            continue;
        }
        total = total.saturating_add(bytes.len());
        fonts.push(proto::FontCopy { family, style, bytes: (*bytes).clone() });
    }
    let req = proto::PdfRenderRequest { docx, fonts };
    if !pdf_worker::ensure(asset_base) {
        return on_reply.call(Err(NO_PDF_WORKER.into()));
    }
    let for_reply = on_reply.share();
    let sent = pdf_worker::with(|w| {
        w.render_pdf(&req, move |reply| {
            for_reply.call(match reply {
                Ok(bytes) if bytes.starts_with(b"%PDF") => Ok(bytes),
                Ok(_) => Err("the pdf worker answered bytes that are not a PDF".into()),
                Err(e) => Err(format!("pdf export failed: {}", e.message)),
            });
        })
    });
    if !matches!(sent, Some(Ok(_))) {
        on_reply.call(Err(NO_PDF_WORKER.into()));
    }
}

/// Say `line` on the status bar on the shell's next frame, and in the console.
fn say(messages: &Messages, ctx: &egui::Context, line: String) {
    log::info!("{line}");
    messages.borrow_mut().push(line);
    ctx.request_repaint();
}

/// Say that `action` on `name` failed ([`failure_message`]); the console also gets the
/// worker's own wording.
fn say_failed(messages: &Messages, ctx: &egui::Context, action: &str, name: &str, error: &str) {
    let line = failure_message(action, name, error);
    log::warn!("{line} [{error}]");
    messages.borrow_mut().push(line);
    ctx.request_repaint();
}

/// The Open half: called every frame before the app drains its inbox. Files only the codecs
/// worker can read (an `.rtf`, `.odt`, `.html`, legacy `.doc`… from the file picker, a drop
/// or the host page) are taken out and converted; each comes back into the inbox as a Word
/// package, which the app opens like any other picked file. Everything else is left where it
/// was, so what the engine reads itself never waits on a worker.
pub(crate) fn take_foreign_opens(inbox: &Inbox, asset_base: &str, messages: &Messages, ctx: &egui::Context) {
    let taken = {
        let mut pending = inbox.lock().unwrap_or_else(|e| e.into_inner());
        if pending.is_empty() {
            return;
        }
        let mut taken = Vec::new();
        let mut kept = Vec::with_capacity(pending.len());
        for (name, bytes) in pending.drain(..) {
            match import_format(&name, &bytes, wordcraft_docx::is_encrypted(&bytes)) {
                Some(format) => taken.push((name, bytes, format)),
                None => kept.push((name, bytes)),
            }
        }
        *pending = kept;
        taken
    };
    for (name, bytes, format) in taken {
        say(messages, ctx, format!("Opening {name}…"));
        let req = proto::ConvertRequest { format, direction: proto::ConvertDirection::Import, password: None, input: bytes };
        let (inbox, messages, ctx) = (inbox.clone(), messages.clone(), ctx.clone());
        convert(asset_base, &req, move |reply| match reply {
            Ok(docx) => {
                let opened = opened_name(&name);
                say(&messages, &ctx, format!("Opened {name} as {opened}"));
                inbox.lock().unwrap_or_else(|e| e.into_inner()).push((opened, docx));
            }
            Err(e) => say_failed(&messages, &ctx, "open", &name, &e),
        });
    }
}

/// The Save As / Export half, behind `Services::lazy_export`.
pub(crate) struct Exporter {
    pub(crate) asset_base: String,
    pub(crate) messages: Messages,
    pub(crate) saves: AsyncSaves,
    pub(crate) ctx: egui::Context,
}

impl Exporter {
    /// Take a save or export under `name` when its format is a worker's: `None` leaves it to
    /// the engine. Otherwise the document — as the `.docx` bytes `docx` writes — goes to the
    /// worker, and the finished file is handed to the browser download when it answers;
    /// `Some(Err)` is a job that could not start. `password` encrypts the formats that take
    /// one (OpenDocument); a PDF is never encrypted.
    pub(crate) fn export(&self, name: &str, password: Option<&str>, docx: &dyn Fn() -> Result<Vec<u8>, String>) -> Option<Result<(), String>> {
        let route = export_route(name)?;
        let docx = match docx() {
            Ok(bytes) => bytes,
            Err(e) => return Some(Err(e)),
        };
        say(&self.messages, &self.ctx, format!("Exporting {name}…"));
        let (messages, saves, ctx, name) = (self.messages.clone(), self.saves.clone(), self.ctx.clone(), name.to_string());
        let done = move |reply: Result<Vec<u8>, String>| {
            let delivered = reply.and_then(|bytes| super::download(&saves, &ctx, &name, &bytes).map(|()| bytes.len()));
            match delivered {
                Ok(bytes) => say(&messages, &ctx, format!("Exported {name} ({bytes} bytes)")),
                Err(e) => say_failed(&messages, &ctx, "export", &name, &e),
            }
        };
        match route {
            ExportRoute::Pdf => render_pdf(&self.asset_base, docx, done),
            ExportRoute::Codec(format) => {
                let req =
                    proto::ConvertRequest { format, direction: proto::ConvertDirection::Export, password: password.map(str::to_string), input: docx };
                convert(&self.asset_base, &req, done);
            }
        }
        Some(Ok(()))
    }
}
