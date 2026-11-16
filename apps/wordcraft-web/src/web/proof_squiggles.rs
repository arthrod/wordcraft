//! The proofing-squiggle wiring (deferred-items plan §3.2): spelling and grammar checks run in
//! the dedicated worker, and its spans draw as overlays in the page.
//!
//! [`tick`](fn.tick) runs once per shell frame with the session in hand. It scans a small
//! round-robin window of body paragraphs, sends a `check` for each whose check inputs (text,
//! languages, unchecked ranges, list status — [`check_key`]) changed since it was last checked
//! (or were never checked), and applies the replies that arrived since the last frame —
//! storing a paragraph's spans only while its inputs are still the ones that were checked, so
//! a late or raced reply can only be dropped, never drawn misaligned. Applying on the next
//! tick, not in the reply closure, is what makes that comparison possible: the closure has no
//! session, the tick does.
//!
//! The spans go to [`wordcraft_layout::proof_overlay`], which the display walk reads wherever
//! inline issues would be drawn — the same byte space, the same marks (§3.2's "overlays"). A
//! repaint is requested when anything applied, so a reply shows up without waiting for input.
//!
//! Bounds: at most [`IN_FLIGHT`] unanswered checks, [`SCAN_PER_TICK`] paragraphs compared per
//! frame, [`MAX_TRACKED`] paragraphs remembered, and the worker protocol caps each request; the
//! worker itself starts on the first edit ([`StartGate`]), never on page load: a document that
//! is opened and only read fetches no proofing files (plan 10.1).

use std::cell::RefCell;
use std::collections::HashMap;

#[cfg(target_arch = "wasm32")]
use egui::Context;
use wordcraft_doc::{Path, StoryRef};
use wordcraft_worker_proto as proto;

/// Checks awaiting a reply. Word checks as you type and moves on; a small window keeps the
/// worker busy without queueing a whole document behind one edit.
const IN_FLIGHT: usize = 8;

/// Paragraphs compared per frame. At 60 fps this covers ~240 paragraphs a second; the scan is
/// round-robin, so a long document's far end is reached eventually rather than never.
const SCAN_PER_TICK: usize = 4;

/// The body's paragraph list is walked fresh every [`PATHS_REFRESH`] frames (new paragraphs,
/// deleted ones); comparing it every frame would cost more than the checks.
const PATHS_REFRESH: u32 = 30;

/// Paragraphs remembered (check-input hash + pending state). When it is full, a new paragraph
/// evicts one completed entry (pending ones are kept: their replies are due), so the
/// round-robin scan still reaches the end of a longer document instead of stopping at the cap
/// forever; overlays are bounded independently by `wordcraft_layout::proof_overlay`.
const MAX_TRACKED: usize = 2048;

/// One remembered paragraph: the [`check_key`] hash it was last checked at, and whether a
/// reply is due.
struct Tracked {
    key: u64,
    pending: bool,
}

/// A reply waiting for the next tick to apply: the request's paragraph, the check-input hash
/// it was checked at, and what came back.
struct Apply {
    /// The request's id, to free its in-flight slot (an error reply names no paragraph).
    id: u32,
    story: StoryRef,
    path: Path,
    /// The [`check_key`] hash the check ran at: the reply applies only while the paragraph
    /// still checks the same.
    checked_key: u64,
    reply: Result<proto::CheckReply, proto::ErrorReply>,
}

struct Scheduler {
    paths: Vec<Path>,
    /// Position of the round-robin scan in `paths`.
    scan: usize,
    ticks: u32,
    tracked: HashMap<(StoryRef, Path), Tracked>,
    /// Request id → its paragraph, while a reply is due.
    ids: HashMap<u32, (StoryRef, Path)>,
    replies: Vec<Apply>,
}

thread_local! {
    static SCHED: RefCell<Scheduler> = RefCell::new(Scheduler {
        paths: Vec::new(),
        scan: 0,
        ticks: 0,
        tracked: HashMap::new(),
        ids: HashMap::new(),
        replies: Vec::new(),
    });
}

/// A hash of every effective input of a worker check of `p`: the text (with the raw
/// tracked-deletion marks that `wordcraft_layout::para::proof_text` turns into U+0001
/// placeholders), each run's language and unchecked flag as the styles resolve them at call
/// time (so style edits count too), and the list-item flag — everything
/// `wordcraft_layout::para::check_parts` feeds the worker. The text alone is not a freshness
/// key: `review.language`, "Do not check spelling" or list membership change the check without
/// changing the text, and the scan must send those again rather than keep the old check's
/// spans forever. Hashing the resolved runs instead of rebuilding the parts keeps the
/// per-frame cost near the text compare it replaces.
fn check_key(doc: &wordcraft_doc::Document, p: &wordcraft_doc::Paragraph) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.text.hash(&mut h);
    for r in wordcraft_layout::para::proof_runs(doc, p, None) {
        (r.range.start, r.range.end).hash(&mut h);
        r.lang.tag().hash(&mut h);
        r.skip.hash(&mut h);
    }
    for r in &p.runs {
        (r.len, r.props.del.is_some()).hash(&mut h);
    }
    p.mark.del.is_some().hash(&mut h);
    wordcraft_layout::para::is_list_item(doc, p).hash(&mut h);
    h.finish()
}

/// Commands that ask for proofing outright: they start the worker even on a document nobody
/// has edited. Read from the engine and the UI: Spelling & Grammar and Editor (both open the
/// spelling pane, `WordApp::execute_user`), the issue and suggestion lists a script or the pane
/// reads, the pane's Add to Dictionary and Ignore All, and Check Spelling as You Type. Most are
/// `pure` (they change nothing a later frame could see), so the gate hears them as they run
/// ([`StartGate::observe`], fed by `Services::on_command`), not from the session afterwards.
const PROOFING_COMMANDS: &[&str] =
    &["review.spelling", "review.editor", "review.issues", "review.suggestions", "review.addToDictionary", "review.ignoreAll", "review.proofing"];

/// What [`StartGate::poll`] answers for one frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    /// Nothing has asked for proofing yet: the worker's files stay unfetched.
    Wait,
    /// The first edit (or proofing command) was just seen: start the worker now. Answered once.
    Start,
    /// The worker was started on an earlier frame.
    Started,
}

/// Holds the proofing worker back until the first use (plan 10.1): the worker is about
/// 0.64 MB gzip that a reader never needs. Once open it stays open — a later save or a newly
/// opened document does not close it, so squiggles behave as they did before the gate.
#[derive(Default)]
pub(crate) struct StartGate {
    started: bool,
    /// A proofing command ran ([`PROOFING_COMMANDS`]); latched, however many commands follow
    /// before the next frame.
    asked: bool,
}

impl StartGate {
    /// Hear one command as it runs.
    pub(crate) fn observe(&mut self, id: &str) {
        if PROOFING_COMMANDS.contains(&id) {
            self.asked = true;
        }
    }

    /// Whether the session has been used in a way that wants squiggles: the document was edited
    /// (`dirty` — opening a file, loading the sample or a new blank document leave it clear), or
    /// a proofing command ran.
    pub(crate) fn poll(&mut self, session: &wordcraft_engine::Session) -> Gate {
        if self.started {
            return Gate::Started;
        }
        if !(session.dirty || self.asked) {
            return Gate::Wait;
        }
        self.started = true;
        Gate::Start
    }
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    static GATE: RefCell<StartGate> = RefCell::new(StartGate::default());
    /// `?proof-worker`: log what the smoke harness waits for ([`enable_smoke`]).
    static SMOKE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The smoke path (`?proof-worker`): when the worker starts, run one known-bad check through
/// it and log its issue count, and log each document paragraph's issue count as replies
/// arrive. The flag adds console lines only; it never starts the worker by itself.
#[cfg(target_arch = "wasm32")]
pub(crate) fn enable_smoke() {
    SMOKE.with(|s| s.set(true));
}

/// `Services::on_command`: every command the app runs passes the gate ([`StartGate::observe`]).
#[cfg(target_arch = "wasm32")]
pub(crate) fn observe_command(id: &str) {
    GATE.with(|g| g.borrow_mut().observe(id));
}

/// The known-bad check of the smoke path, sent once when the worker starts.
#[cfg(target_arch = "wasm32")]
fn smoke_check() {
    crate::web::proof_worker::with(|w| {
        if let Err(e) = w.ping() {
            log::warn!("proofing worker ping failed to send: {e}");
        }
        let req = proto::CheckRequest {
            para_id: 0,
            text: "Ths is a tst.".to_string(),
            spans: Vec::new(),
            skip: Vec::new(),
            options: None,
            typing: None,
            list_item: false,
        };
        if let Err(e) = w.check(&req, |res| match res {
            Ok(reply) => log::info!("proofing worker: {} issue(s) in the smoke check", reply.issues.len()),
            Err(e) => log::warn!("proofing worker smoke check: {} ({:?})", e.message, e.code),
        }) {
            log::warn!("proofing worker check failed to send: {e}");
        }
    });
}

/// One frame: apply what arrived, then send what changed. `session.doc` is only read.
#[cfg(target_arch = "wasm32")]
pub(crate) fn tick(session: &wordcraft_engine::Session, asset_base: &str, ctx: &Context) {
    let gate = GATE.with(|g| g.borrow_mut().poll(session));
    if gate == Gate::Wait {
        return; // Nobody has edited or asked for proofing: fetch nothing.
    }
    if !crate::web::proof_worker::ensure(asset_base) {
        return; // Unavailable in this browser (§3.1): squiggles are off, never a panic.
    }
    let smoke = SMOKE.with(std::cell::Cell::get);
    if gate == Gate::Start {
        log::info!("proofing worker: started on first use");
        if smoke {
            smoke_check();
        }
    }
    let (applied, sends) = pump(session);
    // Send outside the borrow: the reply closures push into the same scheduler.
    for (id, req, story, path, checked_key) in sends {
        let key = (story, path.clone());
        let sent = crate::web::proof_worker::with(|w| {
            w.check(&req, move |reply| {
                if smoke && let Ok(r) = &reply {
                    log::info!("proofing squiggles: {} issue(s) in a document paragraph", r.issues.len());
                }
                SCHED.with(|s| {
                    s.borrow_mut().replies.push(Apply { id, story, path, checked_key, reply });
                });
            })
        });
        if !matches!(sent, Some(Ok(_))) {
            // Nowhere to send (or the send failed): the reply never comes, so the paragraph
            // must not stay pending — the scan would never re-check it.
            SCHED.with(|s| {
                let mut s = s.borrow_mut();
                s.ids.remove(&id);
                if let Some(t) = s.tracked.get_mut(&key) {
                    t.pending = false;
                }
            });
        }
    }
    if applied {
        ctx.request_repaint();
    }
}

/// The scheduler half of a frame, split from the worker send so tests can drive it without a
/// browser: apply arrived replies, refresh the paragraph list on its interval, and collect the
/// checks worth sending. Returns whether a reply applied (the caller repaints) and the
/// `(id, request, story, path, checked-input hash)` sends to post.
fn pump(session: &wordcraft_engine::Session) -> (bool, Vec<(u32, proto::CheckRequest, StoryRef, Path, u64)>) {
    let mut applied = false;
    let mut sends: Vec<(u32, proto::CheckRequest, StoryRef, Path, u64)> = Vec::new();
    SCHED.with(|s| {
        let mut s = s.borrow_mut();
        s.ticks = s.ticks.wrapping_add(1);
        // Apply replies: only while the paragraph still says what was checked.
        let replies = std::mem::take(&mut s.replies);
        for apply in replies {
            s.ids.remove(&apply.id);
            let key = (apply.story, apply.path.clone());
            let Some(t) = s.tracked.get_mut(&key) else { continue };
            t.pending = false;
            let spans = match &apply.reply {
                Ok(reply) => {
                    reply.issues.iter().filter(|i| i.start < i.end).map(|i| (i.start, i.end, i.kind == proto::IssueKind::Grammar)).collect::<Vec<_>>()
                }
                Err(_) => Vec::new(),
            };
            if session.doc.para(apply.story, &apply.path).is_some_and(|p| check_key(&session.doc, p) == apply.checked_key) {
                wordcraft_layout::proof_overlay::set_overlay(apply.story, apply.path, spans);
                applied = true;
            } else {
                // Changed since it was checked (text, language, skips): the reply's spans
                // are for another check. The next scan re-checks it; remove any stale
                // overlay AND the remembered check, so text an undo restores is sent again
                // instead of matching the stale entry and never being checked.
                wordcraft_layout::proof_overlay::set_overlay(apply.story, apply.path, std::iter::empty::<wordcraft_layout::proof_overlay::Span>());
                s.tracked.remove(&key);
            }
        }
        // Refresh the paragraph list now and then.
        if s.ticks % PATHS_REFRESH == 0 {
            s.paths = session.doc.para_paths(StoryRef::Body);
            // `scan` must stay a valid index into the refreshed list: 0 when it is empty,
            // otherwise the old position clamped below the end — paragraphs may have been
            // deleted since the last refresh.
            s.scan = if s.paths.is_empty() { 0 } else { s.scan.min(s.paths.len() - 1) };
            // Paragraphs gone from the document drop their tracked state and any in-flight
            // request slot: a reply for them could never apply, and a path that later
            // reappears must be checked fresh rather than inherit stale bookkeeping.
            let live: std::collections::HashSet<Path> = s.paths.iter().cloned().collect();
            s.tracked.retain(|(story, path), _| *story != StoryRef::Body || live.contains(path));
            s.ids.retain(|_, (story, path)| *story != StoryRef::Body || live.contains(path));
        }
        let in_flight = s.ids.len();
        let mut budget = SCAN_PER_TICK.min(s.paths.len());
        let mut window = IN_FLIGHT.saturating_sub(in_flight);
        while budget > 0 && !s.paths.is_empty() {
            budget -= 1;
            let path = s.paths[s.scan].clone();
            s.scan = (s.scan + 1) % s.paths.len();
            let Some(p) = session.doc.para(StoryRef::Body, &path) else { continue };
            let input = check_key(&session.doc, p);
            let fresh = match s.tracked.get(&(StoryRef::Body, path.clone())) {
                // A reply is still due for these inputs; don't send it twice.
                Some(t) => !t.pending && t.key != input,
                None => true,
            };
            if !fresh || window == 0 {
                continue;
            }
            window -= 1;
            if s.tracked.len() >= MAX_TRACKED && !s.tracked.contains_key(&(StoryRef::Body, path.clone())) {
                // The window is full. Admit this paragraph by evicting one completed entry:
                // refusing new ones would leave paragraphs past the cap never checked, the
                // scan stopping at the cap forever. Pending entries are kept (their replies
                // are due); if every entry is pending there is nothing to evict and a later
                // tick retries. An evicted paragraph is simply re-checked when the scan
                // reaches it again; its overlay, if any, lives in `proof_overlay`, untouched.
                let evicted = s.tracked.iter().find(|(_, t)| !t.pending).map(|(k, _)| k.clone());
                match evicted {
                    Some(evicted) => {
                        s.tracked.remove(&evicted);
                    }
                    None => continue,
                }
            }
            let Some((text, spans, skip, list_item)) = wordcraft_layout::para::check_parts(&session.doc, p) else {
                // Nothing to check (empty or past the protocol's bound): remember it so the
                // scan doesn't rebuild the parts every frame.
                s.tracked.insert((StoryRef::Body, path), Tracked { key: input, pending: false });
                continue;
            };
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
            // Ids must skip 0 (the "no request" id) and stay under 2^53 for JavaScript; the
            // u32 counter does both.
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed).max(1);
            let req = proto::CheckRequest {
                para_id: id as u64,
                text,
                spans: spans.into_iter().map(|(start, end, tag)| proto::LangSpan { start, end, tag }).collect(),
                skip: skip.into_iter().map(|(start, end)| proto::ByteRange { start, end }).collect(),
                options: None,
                typing: None,
                list_item,
            };
            s.ids.insert(id, (StoryRef::Body, path.clone()));
            s.tracked.insert((StoryRef::Body, path.clone()), Tracked { key: input, pending: true });
            sends.push((id, req, StoryRef::Body, path, input));
        }
    });
    (applied, sends)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wordcraft_doc::{Document, Pos};

    /// Plan 10.1: a document that is opened and only read never starts the worker; the first
    /// edit starts it exactly once; later frames (and a save, which clears `dirty`) neither
    /// start it again nor close the gate.
    #[test]
    fn worker_starts_on_the_first_edit_only() {
        let mut gate = StartGate::default();
        // Opened, with text, and only read: frames pass, caret moves, nothing starts.
        let mut session = wordcraft_engine::Session::new(Document::from_text("Ths is a tst."));
        for _ in 0..3 {
            assert_eq!(gate.poll(&session), Gate::Wait);
        }
        session.run("select.all", &serde_json::json!({})).unwrap();
        session.run("caret.docEnd", &serde_json::json!({})).unwrap();
        assert_eq!(gate.poll(&session), Gate::Wait, "moving the caret is not an edit");
        // Opening another document is not an edit either.
        session.set_document(Document::from_text("another"));
        assert_eq!(gate.poll(&session), Gate::Wait, "opening a document is not an edit");
        session.set_document(wordcraft_engine::sample::sample_document());
        assert_eq!(gate.poll(&session), Gate::Wait, "loading the sample is not an edit");
        // The first edit starts it, once.
        session.run("text.insert", &serde_json::json!({"text": "x"})).unwrap();
        assert_eq!(gate.poll(&session), Gate::Start);
        assert_eq!(gate.poll(&session), Gate::Started);
        session.run("text.insert", &serde_json::json!({"text": "y"})).unwrap();
        assert_eq!(gate.poll(&session), Gate::Started);
        // Saved, or replaced by a fresh document: the worker stays, nothing starts twice.
        session.dirty = false;
        assert_eq!(gate.poll(&session), Gate::Started);
        session.set_document(Document::from_text("next"));
        assert_eq!(gate.poll(&session), Gate::Started);
    }

    /// Review finding 7: an explicit proofing command starts the worker on a document nobody
    /// edited, heard as it runs through the app — even a `pure` one that leaves no trace in the
    /// session, and even when other commands run before the next frame. Other commands do not.
    #[test]
    fn a_proofing_command_starts_the_worker_without_an_edit() {
        use std::{cell::RefCell, rc::Rc};
        for id in PROOFING_COMMANDS {
            let gate = Rc::new(RefCell::new(StartGate::default()));
            let hear = gate.clone();
            let services =
                wordcraft_ui_egui::Services { on_command: Some(Box::new(move |id: &str| hear.borrow_mut().observe(id))), ..Default::default() };
            let mut app = wordcraft_ui_egui::WordApp::new(wordcraft_engine::Session::new(Document::from_text("Ths is a tst.")), services);
            for other in ["view.zoom", "review.newComment", "caret.docEnd"] {
                let _ = app.execute(other, serde_json::json!({}));
            }
            app.session.dirty = false;
            assert_eq!(gate.borrow_mut().poll(&app.session), Gate::Wait, "no proofing asked yet ({id})");
            let _ = app.execute(id, serde_json::json!({"word": "Ths", "value": true}));
            let _ = app.execute("caret.docEnd", serde_json::json!({}));
            app.session.dirty = false;
            assert_eq!(gate.borrow_mut().poll(&app.session), Gate::Start, "{id}, then another command, before the frame");
            assert_eq!(gate.borrow_mut().poll(&app.session), Gate::Started);
        }
    }

    /// The gate's list names commands that exist: every id is one the engine registers.
    #[test]
    fn every_proofing_command_is_registered() {
        let session = wordcraft_engine::Session::new(Document::new());
        for id in PROOFING_COMMANDS {
            assert!(session.registry.get(id).is_some(), "{id} is not an engine command");
        }
    }

    /// Paragraphs deleted between path refreshes must not leave `scan` indexing past the new
    /// list: the refresh clamps it to a valid position (0 when nothing is left).
    #[test]
    fn refresh_after_deleting_paragraphs_keeps_scan_in_bounds() {
        let mut session = wordcraft_engine::Session::new(Document::from_text("a\nb\nc\nd\ne\nf"));
        // The first refresh lands on tick PATHS_REFRESH; run up to it so `paths` is filled.
        for _ in 0..PATHS_REFRESH {
            pump(&session);
        }
        let len = SCHED.with(|s| s.borrow().paths.len());
        assert_eq!(len, 6);
        // Walk the scan to the last paragraph and line the next pump up on a refresh.
        SCHED.with(|s| {
            let mut s = s.borrow_mut();
            s.scan = len - 1;
            s.ticks = PATHS_REFRESH - 1;
        });
        // Delete paragraphs so the refreshed list is shorter than the old scan position.
        session.doc.delete_range(&Pos::body(1, 0), &Pos::body(len - 2, 0)).unwrap();
        assert!(session.doc.para_paths(StoryRef::Body).len() < len);
        let _ = pump(&session);
        SCHED.with(|s| {
            let s = s.borrow();
            assert!(s.paths.is_empty() || s.scan < s.paths.len());
        });
    }

    /// Empty paragraphs take the "nothing to check" insert in `pump`, which must respect
    /// [`MAX_TRACKED`] like a real check does — otherwise a document of blank paragraphs
    /// grows `tracked` without bound.
    #[test]
    fn empty_paragraphs_cannot_bypass_the_tracking_cap() {
        // One more paragraph than the cap, all empty: every one hits the `check_parts` `None`
        // branch. `from_text` makes one paragraph per line.
        let session = wordcraft_engine::Session::new(Document::from_text(&"\n".repeat(MAX_TRACKED + 8)));
        // The scan covers SCAN_PER_TICK paragraphs per pump and `paths` only fills on a
        // PATHS_REFRESH tick, so enough pumps must pass for it to reach the cap.
        for _ in 0..(2 * PATHS_REFRESH as usize + (MAX_TRACKED + 8) / SCAN_PER_TICK) {
            pump(&session);
            SCHED.with(|s| assert!(s.borrow().tracked.len() <= MAX_TRACKED));
        }
        SCHED.with(|s| {
            let s = s.borrow();
            assert!(s.paths.len() > MAX_TRACKED, "the fixture must exceed the cap to prove it");
            assert_eq!(s.tracked.len(), MAX_TRACKED);
        });
    }

    /// Paragraphs deleted between path refreshes lose their tracked state and their in-flight
    /// request slot, so nothing stale is left behind for a path that reappears later. Paths
    /// are positional, so the fixture deletes the trailing paragraphs — the only ones whose
    /// keys actually leave the refreshed list.
    #[test]
    fn refresh_evicts_tracked_state_for_deleted_paragraphs() {
        let mut session = wordcraft_engine::Session::new(Document::from_text("aaa\nbbb\nccc\nddd"));
        // Warm up past the first refresh and let the scan send all four checks.
        for _ in 0..=PATHS_REFRESH {
            pump(&session);
        }
        let (tracked, ids, paths) = SCHED.with(|s| {
            let s = s.borrow();
            (s.tracked.len(), s.ids.len(), s.paths.clone())
        });
        assert_eq!((tracked, ids, paths.len()), (4, 4, 4));
        let gone = paths[3].clone();
        // Delete from the start of "ccc" to the end of "ddd": the last two paragraphs' marks
        // collapse into one empty paragraph, so path [3] is gone from the refreshed list.
        session.doc.delete_range(&Pos::body(2, 0), &Pos::body(3, 3)).unwrap();
        // Line the next pump up on a refresh.
        SCHED.with(|s| s.borrow_mut().ticks = PATHS_REFRESH - 1);
        pump(&session);
        SCHED.with(|s| {
            let s = s.borrow();
            assert!(s.paths.len() < 4);
            assert_eq!(s.tracked.len(), s.paths.len());
            assert_eq!(s.ids.len(), s.paths.len());
            assert!(!s.tracked.contains_key(&(StoryRef::Body, gone.clone())));
            assert!(s.tracked.keys().all(|(_, p)| s.paths.contains(p)));
            assert!(s.ids.values().all(|(_, p)| s.paths.contains(p)));
        });
    }

    /// A reply applied after the text changed clears the overlay AND the remembered check:
    /// undoing back to the checked text makes the next scan send it again instead of skipping
    /// it as already checked.
    #[test]
    fn stale_reply_invalidates_the_tracked_check() {
        let mut session = wordcraft_engine::Session::new(Document::from_text("aaa"));
        // The first refresh tick already scans, so collect sends from the warmup too.
        let mut sent = Vec::new();
        for _ in 0..=PATHS_REFRESH {
            let (_, s) = pump(&session);
            sent.extend(s);
        }
        for _ in 0..4 {
            let (_, s) = pump(&session);
            sent.extend(s);
            if !sent.is_empty() {
                break;
            }
        }
        assert_eq!(sent.len(), 1);
        let (id, _, _, path, checked) = sent.remove(0);
        assert_eq!(
            checked,
            check_key(&session.doc, session.doc.para(StoryRef::Body, &path).unwrap()),
            "the send carries the paragraph's check-input hash"
        );
        // Edit to "bbb" through the command layer so Undo can restore "aaa".
        session.run("select.all", &serde_json::json!({})).unwrap();
        session.run("text.insert", &serde_json::json!({"text": "bbb"})).unwrap();
        assert_eq!(session.doc.para(StoryRef::Body, &path).unwrap().text, "bbb");
        // Empty `paths` keeps the apply pump from scanning "bbb" before the reply lands.
        SCHED.with(|s| {
            let mut s = s.borrow_mut();
            s.paths.clear();
            s.replies.push(Apply {
                id,
                story: StoryRef::Body,
                path: path.clone(),
                checked_key: checked,
                reply: Ok(proto::CheckReply { para_id: id as u64, issues: Vec::new() }),
            });
        });
        pump(&session);
        SCHED.with(|s| assert!(!s.borrow().tracked.contains_key(&(StoryRef::Body, path.clone()))));
        // Undo back to "aaa" and line the next pump up on a path refresh.
        session.run("edit.undo", &serde_json::json!({})).unwrap();
        assert_eq!(session.doc.para(StoryRef::Body, &path).unwrap().text, "aaa");
        SCHED.with(|s| s.borrow_mut().ticks = PATHS_REFRESH - 1);
        let (_, sends) = pump(&session);
        assert!(sends.iter().any(|(_, req, _, _, _)| req.text == "aaa"), "the restored text was not re-checked");
    }

    /// Pumps until the single-paragraph document's check is sent, queues a clean reply for it
    /// and pumps once more to apply it, leaving the tracked entry complete (not pending).
    /// Returns the paragraph's path and the request that went out.
    fn check_and_answer(session: &wordcraft_engine::Session) -> (Path, proto::CheckRequest) {
        let mut sent = None;
        for _ in 0..(PATHS_REFRESH as usize + 8) {
            let (_, sends) = pump(session);
            if let Some((id, req, story, path, checked_key)) = sends.into_iter().next() {
                SCHED.with(|s| {
                    s.borrow_mut().replies.push(Apply {
                        id,
                        story,
                        path: path.clone(),
                        checked_key,
                        reply: Ok(proto::CheckReply { para_id: id as u64, issues: Vec::new() }),
                    });
                });
                sent = Some((path, req));
                break;
            }
        }
        let (path, req) = sent.expect("a check was sent");
        pump(session); // Apply the reply: the entry completes before the caller proceeds.
        (path, req)
    }

    /// A same-text proofing change must re-check the paragraph: the remembered entry is keyed
    /// on every effective check input, not just the text, so setting the run's language
    /// (`review.language`) or "Do not check spelling" sends the worker the new language spans
    /// and unchecked ranges instead of keeping the first reply's forever.
    #[test]
    fn same_text_proofing_changes_recheck() {
        let mut session = wordcraft_engine::Session::new(Document::from_text("hello world"));
        let (path, first) = check_and_answer(&session);
        assert!(first.spans.iter().all(|s| !s.tag.eq_ignore_ascii_case("fr-FR")));
        // Same text, new language on the whole paragraph.
        session.run("select.all", &serde_json::json!({})).unwrap();
        session.run("review.language", &serde_json::json!({"lang": "fr-FR"})).unwrap();
        assert_eq!(session.doc.para(StoryRef::Body, &path).unwrap().text, "hello world", "the fixture must not change the text");
        let (_, second) = check_and_answer(&session);
        assert!(second.spans.iter().any(|s| s.tag.eq_ignore_ascii_case("fr-FR")), "the re-check carried the new language, got {:?}", second.spans);
        // Same text again, now also left unchecked: the re-check carries the range.
        session.run("select.all", &serde_json::json!({})).unwrap();
        session.run("review.language", &serde_json::json!({"lang": "fr-FR", "noProof": true})).unwrap();
        assert_eq!(session.doc.para(StoryRef::Body, &path).unwrap().text, "hello world");
        let (_, third) = check_and_answer(&session);
        assert!(
            third.skip.len() == 1 && third.skip[0].start == 0 && third.skip[0].end == 11,
            "the re-check carried the unchecked range, got {:?}",
            third.skip
        );
    }

    /// Paragraphs past [`MAX_TRACKED`] still get checked: when the window is full, completed
    /// entries are evicted to admit new ones, so the round-robin scan reaches the end of a
    /// longer document instead of stopping at the cap forever.
    #[test]
    fn paragraphs_past_the_cap_still_get_checked() {
        let n = MAX_TRACKED + 8;
        let text = (0..n).map(|i| format!("word{i}")).collect::<Vec<_>>().join("\n");
        let session = wordcraft_engine::Session::new(Document::from_text(&text));
        let last = format!("word{}", n - 1);
        let mut saw_last = false;
        for _ in 0..(PATHS_REFRESH as usize * 2 + n / SCAN_PER_TICK + 16) {
            let (_, sends) = pump(&session);
            SCHED.with(|s| assert!(s.borrow().tracked.len() <= MAX_TRACKED));
            if sends.iter().any(|(_, req, _, _, _)| req.text == last) {
                saw_last = true;
                break;
            }
            // Answer every check (an empty reply): ids free up and the entries complete, so
            // later paragraphs can evict them and enter the window.
            SCHED.with(|s| {
                for (id, _, story, path, checked_key) in sends {
                    s.borrow_mut().replies.push(Apply {
                        id,
                        story,
                        path,
                        checked_key,
                        reply: Ok(proto::CheckReply { para_id: id as u64, issues: Vec::new() }),
                    });
                }
            });
        }
        assert!(saw_last, "the paragraph past the cap was never checked");
    }
}
