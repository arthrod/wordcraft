//! The app's own Open, Save As and Export through the on-demand workers (implementation plan
//! 10.2), and the same two jobs for the page API (`convertDocument`, `exportPdf`).
//!
//! The main module leaves the extra formats and the PDF writer out; the codecs worker and the
//! PDF worker hold them and are fetched the first time a job needs them. `.docx` bytes are the
//! only document form that crosses to a worker. Each job answers on a later task turn, so the
//! shell stays responsive: Open puts the converted package into the inbox the file picker and
//! drops already use, and Save As / Export hand the finished bytes to the browser download (or
//! to the host page, when one hosts the app). Progress and failures go to the app's status bar
//! (and the console); a worker that cannot be loaded leaves the open document as it was.
//!
//! Every job answers exactly once ([`Once`]): with the bytes, with the reason there are none, or
//! when it outlives its deadline ([`JobQueue`]), counted from when it is posted to a worker that
//! said it is ready, so a slow first fetch of the worker never eats into it (loading has its own
//! bound, `LOAD_MS`); a worker stuck past a deadline is stopped, so the next job starts a fresh
//! one.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wordcraft_ui_egui::Inbox;
use wordcraft_worker_proto as proto;

use super::{codec_worker, pdf_worker};
use crate::lazy_formats::{
    ApiExports, ExportRoute, FONT_BUDGET, JobKind, JobQueue, NO_ANSWER, Once, export_route, failure_message, font_batch, import_format,
    job_budget_ms, open_request, opened_name, pdf_job_options,
};

/// Status-bar lines waiting for the shell's next frame.
pub(crate) type Messages = Rc<RefCell<Vec<String>>>;

/// Where a finished export goes: the browser download, or the host page.
pub(crate) type Deliver = Rc<dyn Fn(&str, &[u8]) -> Result<(), String>>;

const NO_CODEC_WORKER: &str = "the codec worker is unavailable in this browser";
const NO_PDF_WORKER: &str = "the pdf worker is unavailable in this browser";

thread_local! {
    /// The jobs each worker holds, for their deadlines.
    static CONVERTS: RefCell<JobQueue> = RefCell::new(JobQueue::default());
    static PDFS: RefCell<JobQueue> = RefCell::new(JobQueue::default());
    /// Save As / Export jobs between the request and the delivery: the leave-page guard asks
    /// while there are any ([`exports_in_flight`]).
    static EXPORTS_IN_FLIGHT: Cell<usize> = const { Cell::new(0) };
    /// PDF exports the page API asked for, by the name its `file.exportPdf` runs under.
    static API_EXPORTS: RefCell<ApiExports<ApiSink>> = RefCell::new(ApiExports::default());
}

/// Where a page-API export's bytes (or the reason there are none) go.
pub(crate) type ApiSink = Box<dyn FnOnce(Result<Vec<u8>, String>)>;

/// Claim an export name for the page API ([`ApiExports`]): the app's own `file.exportPdf` run
/// under it hands its result to `sink` instead of delivering it. `None` when too many wait.
pub(crate) fn claim_api_export(sink: ApiSink) -> Option<String> {
    API_EXPORTS.with(|c| c.borrow_mut().claim(sink))
}

/// Give up the claim on `name` (its command failed, so no export will answer it).
pub(crate) fn release_api_export(name: &str) {
    let released = API_EXPORTS.with(|c| c.borrow_mut().take(name));
    drop(released);
}

/// Exports requested and not yet delivered or failed.
pub(crate) fn exports_in_flight() -> usize {
    EXPORTS_IN_FLIGHT.with(Cell::get)
}

/// The answer half of a job ([`Once`]) and what starts its deadline.
type Job = (Once<Box<dyn FnOnce(Result<Vec<u8>, String>)>>, Box<dyn FnOnce()>);

/// Wrap `on_reply` with job `kind`'s deadline. The deadline starts when the second half runs,
/// which the worker client does when it posts the job to a worker that said it is ready — never
/// while the worker is still being fetched (that has its own bound, `LOAD_MS`). From then a timer
/// answers "did not answer" when the job outlives its budget (the budgets of the jobs posted
/// ahead of it, then its own), and stops the stuck worker so the next job starts a fresh one. A
/// reply first makes the timer do nothing; a job answered before it was posted starts no clock.
fn with_deadline(kind: JobKind, input_bytes: usize, on_reply: impl FnOnce(Result<Vec<u8>, String>) + 'static) -> Job {
    let queue = match kind {
        JobKind::Convert => &CONVERTS,
        JobKind::Pdf => &PDFS,
    };
    // Set when the job is posted: until then it holds up no other job.
    let token: Rc<Cell<Option<u64>>> = Rc::default();
    let finish = token.clone();
    let once: Once<Box<dyn FnOnce(Result<Vec<u8>, String>)>> = Once::new(Box::new(move |reply| {
        if let Some(token) = finish.take() {
            queue.with(|q| q.borrow_mut().finish(token));
        }
        on_reply(reply);
    }));
    let on_timeout = once.share();
    let start = Box::new(move || {
        if on_timeout.answered() {
            return;
        }
        let (admitted, due_ms) = queue.with(|q| q.borrow_mut().admit(job_budget_ms(kind, input_bytes)));
        token.set(Some(admitted));
        let minutes = due_ms.div_ceil(60_000);
        let fire = wasm_bindgen::closure::Closure::once_into_js(move || {
            let what = match kind {
                JobKind::Convert => "codec",
                JobKind::Pdf => "pdf",
            };
            if on_timeout.call(Err(format!("the {what} worker {NO_ANSWER} in {minutes} minutes"))) {
                log::warn!("the {what} worker held a job past its deadline; stopping it");
                match kind {
                    JobKind::Convert => codec_worker::stop(),
                    JobKind::Pdf => pdf_worker::stop(),
                }
            }
        });
        let ms = i32::try_from(due_ms).unwrap_or(i32::MAX);
        let armed = web_sys::window().map(|w| w.set_timeout_with_callback_and_timeout_and_arguments_0(fire.unchecked_ref(), ms));
        if !matches!(armed, Some(Ok(_))) {
            log::warn!("no timer for a worker job's deadline; it waits for the worker alone");
        }
    });
    (once, start)
}

/// Convert between `.docx` and another format in the codecs worker, starting it on first
/// use. `on_reply` runs exactly once with the converted bytes or the reason there are none —
/// at once when the job cannot be sent, otherwise on a later task turn, at the latest at the
/// job's deadline.
pub(crate) fn convert(asset_base: &str, req: &proto::ConvertRequest, on_reply: impl FnOnce(Result<Vec<u8>, String>) + 'static) {
    if req.input.len() > proto::MAX_CONVERT_BYTES {
        return on_reply(Err(format!("the document is too large to convert in the browser ({} bytes)", req.input.len())));
    }
    if !codec_worker::ensure(asset_base) {
        return on_reply(Err(NO_CODEC_WORKER.into()));
    }
    let (on_reply, posted) = with_deadline(JobKind::Convert, req.input.len(), on_reply);
    let for_reply = on_reply.share();
    let sent = codec_worker::with(|w| {
        w.convert(req, posted, move |reply| {
            for_reply.call(reply.map_err(|e| format!("conversion failed: {}", e.message)));
        })
    });
    match sent {
        Some(Ok(_)) => {}
        Some(Err(e)) => {
            on_reply.call(Err(format!("the codec worker could not take the conversion: {e}")));
        }
        None => {
            on_reply.call(Err(NO_CODEC_WORKER.into()));
        }
    }
}

/// Write `docx` as a PDF in the PDF worker with `options` (the export options a package does
/// not carry), starting it on first use. The worker lays out with the faces the page shows:
/// every face beyond its compiled-in set goes along as a copy (user fonts, pack faces, fonts
/// embedded in the document), within the protocol's budgets ([`font_batch`]). When they do not
/// all fit, the faces the PDF draws text in (`used`, which lays the document out and is asked
/// only then) go first; a strict export (`options.strict`), already checked against this page's faces, is
/// refused only when one of those would be left behind, since its text would then fall back in
/// the PDF. `on_reply` runs exactly once, as for [`convert`].
pub(crate) fn render_pdf(
    asset_base: &str,
    docx: Vec<u8>,
    options: proto::PdfJobOptions,
    used: &dyn Fn() -> Vec<(String, String)>,
    on_reply: impl FnOnce(Result<Vec<u8>, String>) + 'static,
) {
    if docx.is_empty() {
        return on_reply(Err("nothing to export".into()));
    }
    if docx.len() > proto::MAX_PDF_DOC_BYTES {
        return on_reply(Err(format!("the document is too large to export as PDF in the browser ({} bytes)", docx.len())));
    }
    let batch = font_batch(wordcraft_fonts::FontDb::global().runtime_faces(), used, FONT_BUDGET);
    if options.strict
        && let Some(refusal) = batch.strict_refusal()
    {
        return on_reply(Err(refusal));
    }
    // What does not fit the budgets is left out with a log, and the export still runs: those
    // faces fall back inside the worker.
    if batch.left_used > 0 || batch.left_unused > 0 {
        log::warn!(
            "pdf export: the font copies hit a protocol budget; {} face(s) the PDF draws in and {} other face(s) are left to the worker's fallback",
            batch.left_used,
            batch.left_unused
        );
    }
    let fonts = batch.copies;
    let input_bytes = docx.len();
    let req = proto::PdfRenderRequest { docx, fonts, options };
    if !pdf_worker::ensure(asset_base) {
        return on_reply(Err(NO_PDF_WORKER.into()));
    }
    let (on_reply, posted) = with_deadline(JobKind::Pdf, input_bytes, on_reply);
    let for_reply = on_reply.share();
    let sent = pdf_worker::with(|w| {
        w.render_pdf(&req, posted, move |reply| {
            for_reply.call(match reply {
                Ok(bytes) if bytes.starts_with(b"%PDF") => Ok(bytes),
                Ok(_) => Err("the pdf worker answered bytes that are not a PDF".into()),
                Err(e) => Err(format!("pdf export failed: {}", e.message)),
            });
        })
    });
    // The same wording as the codecs path: the send's own reason, not a guess.
    match sent {
        Some(Ok(_)) => {}
        Some(Err(e)) => {
            on_reply.call(Err(format!("the pdf worker could not take the export: {e}")));
        }
        None => {
            on_reply.call(Err(NO_PDF_WORKER.into()));
        }
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
/// package, which the app opens like any other picked file — beside a document that has work in
/// it, never in its place (`file.open`). Everything else is left where it was, so what the
/// engine reads itself never waits on a worker.
///
/// `region` is the user's: a file that gives no page size opens on its paper ([`open_request`]).
pub(crate) fn take_foreign_opens(inbox: &Inbox, asset_base: &str, messages: &Messages, ctx: &egui::Context, region: &wordcraft_engine::Region) {
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
        let req = open_request(format, bytes, region);
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
    pub(crate) ctx: egui::Context,
    /// Where the finished file goes. In the plain app it is the anchor download, never the save
    /// picker: the worker answers on a later task turn, after the click's user activation has
    /// expired, and Chromium rejects a picker opened then. With a host page it is the host.
    pub(crate) deliver: Deliver,
}

impl Exporter {
    /// Take a save or export under `name` when its format is a worker's: `None` leaves it to
    /// the engine. Otherwise the document — as the `.docx` bytes `docx` writes — goes to the
    /// worker, and the finished file is delivered when it answers; `Some(Err)` is a job that
    /// could not start. `used` lists the faces a PDF of the document draws text in. `password` encrypts the formats that take one (OpenDocument); a PDF is
    /// never encrypted.
    pub(crate) fn export(
        &self,
        name: &str,
        password: Option<&str>,
        opts: &wordcraft_engine::io::ExportOptions,
        docx: &dyn Fn() -> Result<Vec<u8>, String>,
        used: &dyn Fn() -> Vec<(String, String)>,
    ) -> Option<Result<(), String>> {
        let route = export_route(name)?;
        let docx = match docx() {
            Ok(bytes) => bytes,
            Err(e) => return Some(Err(e)),
        };
        say(&self.messages, &self.ctx, format!("Exporting {name}…"));
        // The leave-page guard covers the export from now until it is delivered or failed.
        EXPORTS_IN_FLIGHT.with(|n| n.set(n.get().saturating_add(1)));
        let (messages, ctx, name, deliver) = (self.messages.clone(), self.ctx.clone(), name.to_string(), self.deliver.clone());
        let done = move |reply: Result<Vec<u8>, String>| {
            EXPORTS_IN_FLIGHT.with(|n| n.set(n.get().saturating_sub(1)));
            // The page API's export goes back to its promise, not to a download.
            let claimed = API_EXPORTS.with(|c| c.borrow_mut().take(&name));
            if let Some(sink) = claimed {
                return sink(reply);
            }
            let delivered = reply.and_then(|bytes| deliver(&name, &bytes).map(|()| bytes.len()));
            match delivered {
                Ok(bytes) => say(&messages, &ctx, format!("Exported {name} ({bytes} bytes)")),
                Err(e) => say_failed(&messages, &ctx, "export", &name, &e),
            }
        };
        match route {
            ExportRoute::Pdf => render_pdf(&self.asset_base, docx, pdf_job_options(opts), used, done),
            ExportRoute::Codec(format) => {
                let req = proto::ConvertRequest {
                    format,
                    direction: proto::ConvertDirection::Export,
                    password: password.map(str::to_string),
                    input: docx,
                    page: None,
                };
                convert(&self.asset_base, &req, done);
            }
        }
        Some(Ok(()))
    }
}

/// The plain app's delivery for an export that finished later: an anchor download of the last
/// component of `path` (see [`Exporter::deliver`]).
pub(crate) fn anchor_download(path: &str, bytes: &[u8]) -> Result<(), String> {
    let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "document".into());
    let window = web_sys::window().ok_or("no window")?;
    super::trigger_anchor_download(&window, &name, bytes)
}
