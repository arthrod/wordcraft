//! Proofing squiggle overlays (deferred-items plan §3.2): spans a *worker* found, drawn by a
//! build whose [`ParaLayout::issues`](crate::para::ParaLayout::issues) is empty because
//! `inline-proof` is compiled out (the browser build — spelling and grammar run in the
//! dedicated worker instead, so typing never waits on them).
//!
//! The store is deliberately outside [`ParaLayout`]: layouts are cache-shared by content, so
//! per-paragraph state cannot live there without one paragraph's squiggles leaking onto
//! another with the same key. The *display* walk knows which paragraph it is drawing
//! ([`LineContext`](crate::display::lines)'s story and path), so the overlays hang off a
//! thread-local keyed by `(StoryRef, Path)` and are read at the same place inline issues are
//! drawn ([`proofing_marks`](crate::display::lines)): identical squiggles, identical offsets —
//! both are byte ranges in [`proof_text`](crate::para::proof_text).
//!
//! The host owns freshness: it stores a paragraph's spans only while the paragraph's text is
//! what was checked, re-checks what changes, and clears or replaces on document switches
//! ([`set_overlays`] replaces the whole set). Overlays never affect layout — a late or stale
//! reply can only move a squiggle, never a line break. Bounds: a hostile or runaway caller
//! cannot grow the store past [`MAX_PARAS`] paragraphs or [`MAX_SPANS`] spans in one.

use std::cell::RefCell;
use std::collections::HashMap;

use wordcraft_doc::{Path, StoryRef};

/// Most paragraphs with overlays. A document can be longer; the host decides what to check, and
/// the first [`MAX_PARAS`] in its set are kept (the host sends the visible and edited ones
/// first, so the cap costs the far end of a very long document its squiggles, nothing else).
pub const MAX_PARAS: usize = 4096;

/// Most stored spans per paragraph — the same bound a worker reply carries
/// (`wordcraft-worker-proto`'s `MAX_ISSUES`).
pub const MAX_SPANS: usize = 4096;

/// One overlay span: a half-open byte range in the paragraph's proof text, and whether it is a
/// grammar issue (double underline) rather than spelling (squiggle).
pub type Span = (u32, u32, bool);

thread_local! {
    /// The overlays, replaced wholesale by the host. Empty until a worker has answered.
    static OVERLAYS: RefCell<HashMap<(StoryRef, Path), Vec<Span>>> = RefCell::new(HashMap::new());
}

/// Replace every overlay with `paras` — the host's complete, current set (each entry: the
/// paragraph and its spans, in proof-text bytes). A paragraph with no entry draws no overlay
/// squiggles; an empty span list is the same as none. Spans are normalized: dropped when empty,
/// inverted or (after clamping to `u32`) past [`MAX_SPANS`] per paragraph, and the store keeps
/// at most [`MAX_PARAS`] paragraphs of what the host sent, in order.
pub fn set_overlays(paras: impl IntoIterator<Item = (StoryRef, Path, Vec<Span>)>) {
    OVERLAYS.with(|store| {
        let mut store = store.borrow_mut();
        store.clear();
        for (story, path, spans) in paras {
            if store.len() >= MAX_PARAS {
                break;
            }
            let spans: Vec<Span> = spans.into_iter().filter(|(a, b, _)| a < b).take(MAX_SPANS).collect();
            if spans.is_empty() {
                continue;
            }
            store.insert((story, path), spans);
        }
    });
}

/// One paragraph's overlays, set as its worker reply arrives (the host checks paragraphs one
/// at a time). Same span normalization as [`set_overlays`]; an empty list removes the
/// paragraph's overlays (the reply said clean).
pub fn set_overlay(story: StoryRef, path: Path, spans: impl IntoIterator<Item = Span>) {
    let spans: Vec<Span> = spans.into_iter().filter(|(a, b, _)| a < b).take(MAX_SPANS).collect();
    OVERLAYS.with(|store| {
        let mut store = store.borrow_mut();
        if spans.is_empty() {
            store.remove(&(story, path));
        } else if store.len() < MAX_PARAS || store.contains_key(&(story, path.clone())) {
            store.insert((story, path), spans);
        }
    });
}

/// Drop every overlay (document switched, worker gone).
pub fn clear_overlays() {
    OVERLAYS.with(|store| store.borrow_mut().clear());
}

/// The overlay spans of one paragraph, for the display walk. Inline issues always win: when
/// [`ParaLayout::issues`](crate::para::ParaLayout::issues) is non-empty the paragraph was
/// checked inline and its overlays — stale by definition of a build with both — are ignored.
pub(crate) fn spans_for(story: StoryRef, path: &Path) -> impl Iterator<Item = Span> + '_ {
    // Cloned per paragraph is fine: the whole store is bounded, and the display holds no borrow
    // across the host's next set_overlays.
    let spans = OVERLAYS.with(|store| store.borrow().get(&(story, path.clone())).cloned()).unwrap_or_default();
    spans.into_iter()
}

#[cfg(test)]
mod layout_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn path(v: &[u32]) -> Path {
        Path(v.to_vec())
    }

    /// Overlays replace wholesale, keep only sane spans, and cap paragraphs and spans.
    #[test]
    fn set_overlays_replaces_bounds_and_normalizes() {
        clear_overlays();
        set_overlays([
            (StoryRef::Body, path(&[0]), vec![(0, 3, false), (3, 3, false), (5, 2, true), (7, 9, true)]),
            (StoryRef::Body, path(&[1]), vec![]),
            (StoryRef::Part(1), path(&[0]), vec![(0, 1, false)]),
        ]);
        let body: Vec<Span> = spans_for(StoryRef::Body, &path(&[0])).collect();
        assert_eq!(body, vec![(0, 3, false), (7, 9, true)], "empty and inverted spans dropped");
        assert_eq!(spans_for(StoryRef::Body, &path(&[1])).count(), 0, "an empty span list stores nothing");
        assert_eq!(spans_for(StoryRef::Part(1), &path(&[0])).count(), 1);
        assert_eq!(spans_for(StoryRef::Body, &path(&[9])).count(), 0, "an unknown paragraph has none");

        // A second set replaces the first entirely.
        set_overlays([(StoryRef::Body, path(&[2]), vec![(1, 2, false)])]);
        assert_eq!(spans_for(StoryRef::Body, &path(&[0])).count(), 0);
        assert_eq!(spans_for(StoryRef::Body, &path(&[2])).count(), 1);

        // The caps hold.
        let many: Vec<(StoryRef, Path, Vec<Span>)> = (0..MAX_PARAS + 8).map(|i| (StoryRef::Body, path(&[i as u32]), vec![(0, 1, false)])).collect();
        set_overlays(many);
        assert_eq!(spans_for(StoryRef::Body, &path(&[0])).count(), 1);
        assert_eq!(spans_for(StoryRef::Body, &path(&[(MAX_PARAS + 7) as u32])).count(), 0, "past MAX_PARAS");
        set_overlays([(StoryRef::Body, path(&[0]), (0..MAX_SPANS + 8).map(|i| (i as u32, i as u32 + 1, false)).collect())]);
        assert_eq!(spans_for(StoryRef::Body, &path(&[0])).count(), MAX_SPANS);
        clear_overlays();
        assert_eq!(spans_for(StoryRef::Body, &path(&[0])).count(), 0);
    }

    /// The incremental setter normalizes like the batch one and removes on empty.
    #[test]
    fn set_overlay_updates_one_paragraph() {
        clear_overlays();
        set_overlay(StoryRef::Body, path(&[0]), [(0, 2, false), (2, 2, false)]);
        set_overlay(StoryRef::Body, path(&[1]), [(4, 3, true)]);
        assert_eq!(spans_for(StoryRef::Body, &path(&[0])).collect::<Vec<_>>(), vec![(0, 2, false)]);
        assert_eq!(spans_for(StoryRef::Body, &path(&[1])).count(), 0, "inverted spans store nothing");
        set_overlay(StoryRef::Body, path(&[0]), []);
        assert_eq!(spans_for(StoryRef::Body, &path(&[0])).count(), 0, "an empty update removes");
        clear_overlays();
    }
}
