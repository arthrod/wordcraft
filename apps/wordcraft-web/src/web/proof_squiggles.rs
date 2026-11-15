//! The proofing-squiggle wiring (deferred-items plan §3.2): spelling and grammar checks run in
//! the dedicated worker, and its spans draw as overlays in the page.
//!
//! [`tick`](fn.tick) runs once per shell frame with the session in hand. It scans a small
//! round-robin window of body paragraphs, sends a `check` for each whose text changed since it
//! was last checked (or was never checked), and applies the replies that arrived since the last
//! frame — storing a paragraph's spans only while its text is still the text that was checked,
//! so a late or raced reply can only be dropped, never drawn misaligned. Applying on the next
//! tick, not in the reply closure, is what makes that comparison possible: the closure has no
//! session, the tick does.
//!
//! The spans go to [`wordcraft_layout::proof_overlay`], which the display walk reads wherever
//! inline issues would be drawn — the same byte space, the same marks (§3.2's "overlays"). A
//! repaint is requested when anything applied, so a reply shows up without waiting for input.
//!
//! Bounds: at most [`IN_FLIGHT`] unanswered checks, [`SCAN_PER_TICK`] paragraphs compared per
//! frame, [`MAX_TRACKED`] paragraphs remembered, and the worker protocol caps each request; the
//! worker itself starts lazily on the first tick with a document ([`proof_worker::ensure`]).

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

/// Paragraphs remembered (checked text + pending state). Past it, new paragraphs are not
/// checked until earlier ones drop out of the window; overlays are bounded independently by
/// `wordcraft_layout::proof_overlay`.
const MAX_TRACKED: usize = 2048;

/// One remembered paragraph: the text it was last checked at, and whether a reply is due.
struct Tracked {
    text: String,
    pending: bool,
}

/// A reply waiting for the next tick to apply: the request's paragraph, the text it was checked
/// at, and what came back.
struct Apply {
    /// The request's id, to free its in-flight slot (an error reply names no paragraph).
    id: u32,
    story: StoryRef,
    path: Path,
    checked_text: String,
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

/// One frame: apply what arrived, then send what changed. `session.doc` is only read.
#[cfg(target_arch = "wasm32")]
pub(crate) fn tick(session: &wordcraft_engine::Session, asset_base: &str, ctx: &Context) {
    if !crate::web::proof_worker::ensure(asset_base) {
        return; // Unavailable in this browser (§3.1): squiggles are off, never a panic.
    }
    let (applied, sends) = pump(session);
    // Send outside the borrow: the reply closures push into the same scheduler.
    for (id, req, story, path, text) in sends {
        let key = (story, path.clone());
        let sent = crate::web::proof_worker::with(|w| {
            w.check(&req, move |reply| {
                SCHED.with(|s| {
                    s.borrow_mut().replies.push(Apply { id, story, path, checked_text: text, reply });
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
/// `(id, request, story, path, checked text)` sends to post.
fn pump(session: &wordcraft_engine::Session) -> (bool, Vec<(u32, proto::CheckRequest, StoryRef, Path, String)>) {
    let mut applied = false;
    let mut sends: Vec<(u32, proto::CheckRequest, StoryRef, Path, String)> = Vec::new();
    SCHED.with(|s| {
        let mut s = s.borrow_mut();
        s.ticks = s.ticks.wrapping_add(1);
        // Apply replies: only while the paragraph still says what was checked.
        let replies = std::mem::take(&mut s.replies);
        for apply in replies {
            s.ids.remove(&apply.id);
            if let Some(t) = s.tracked.get_mut(&(apply.story, apply.path.clone())) {
                t.pending = false;
                let spans = match &apply.reply {
                    Ok(reply) => reply
                        .issues
                        .iter()
                        .filter(|i| i.start < i.end)
                        .map(|i| (i.start, i.end, i.kind == proto::IssueKind::Grammar))
                        .collect::<Vec<_>>(),
                    Err(_) => Vec::new(),
                };
                if session.doc.para(apply.story, &apply.path).is_some_and(|p| p.text == apply.checked_text) {
                    wordcraft_layout::proof_overlay::set_overlay(apply.story, apply.path, spans);
                    applied = true;
                } else {
                    // Changed since it was checked: the reply's offsets are for another text.
                    // The next scan re-checks it; remove any stale overlay.
                    wordcraft_layout::proof_overlay::set_overlay(
                        apply.story,
                        apply.path,
                        std::iter::empty::<wordcraft_layout::proof_overlay::Span>(),
                    );
                }
            }
        }
        // Refresh the paragraph list now and then.
        if s.ticks % PATHS_REFRESH == 0 {
            s.paths = session.doc.para_paths(StoryRef::Body);
            // `scan` must stay a valid index into the refreshed list: 0 when it is empty,
            // otherwise the old position clamped below the end — paragraphs may have been
            // deleted since the last refresh.
            s.scan = if s.paths.is_empty() { 0 } else { s.scan.min(s.paths.len() - 1) };
        }
        let in_flight = s.ids.len();
        let mut budget = SCAN_PER_TICK.min(s.paths.len());
        let mut window = IN_FLIGHT.saturating_sub(in_flight);
        while budget > 0 && !s.paths.is_empty() {
            budget -= 1;
            let path = s.paths[s.scan].clone();
            s.scan = (s.scan + 1) % s.paths.len();
            let Some(p) = session.doc.para(StoryRef::Body, &path) else { continue };
            let fresh = match s.tracked.get(&(StoryRef::Body, path.clone())) {
                // A reply is still due for this text; don't send it twice.
                Some(t) => !t.pending && t.text != p.text,
                None => true,
            };
            if !fresh || window == 0 {
                continue;
            }
            window -= 1;
            let Some((text, spans, skip, list_item)) = wordcraft_layout::para::check_parts(&session.doc, p) else {
                // Nothing to check (empty or past the protocol's bound): remember it so the
                // scan doesn't rebuild the parts every frame.
                s.tracked.insert((StoryRef::Body, path), Tracked { text: p.text.clone(), pending: false });
                continue;
            };
            if s.tracked.len() >= MAX_TRACKED && !s.tracked.contains_key(&(StoryRef::Body, path.clone())) {
                continue;
            }
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
            s.tracked.insert((StoryRef::Body, path.clone()), Tracked { text: p.text.clone(), pending: true });
            sends.push((id, req, StoryRef::Body, path, p.text.clone()));
        }
    });
    (applied, sends)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wordcraft_doc::{Document, Pos};

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
}
