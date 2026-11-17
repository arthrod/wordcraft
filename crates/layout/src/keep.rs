//! [`LayoutOptions::keep_pages`]: the content of pages outside the window is let go while the
//! layout runs, so a long document is paginated without holding every page's shaped text.

use std::sync::Arc;

use super::Page;
use super::cache::LayoutCache;
use super::ctx::Ctx;
use super::page::PageBuilder;

/// Pages behind the one being filled that keep their content whatever the window: placing a
/// block can still move what the page before holds (keeping lines together, balancing columns).
const BEHIND: usize = 2;

/// How many pages are passed between two sweeps of the measured paragraphs.
const SWEEP_EVERY: usize = 8;

/// Let go of the content of a page outside the window. Its size, number and section stay.
fn empty(page: &mut Page) {
    page.items = Vec::new();
    page.header = Vec::new();
    page.footer = Vec::new();
}

/// Drop the measured paragraphs no page holds any more. Only with a window: an editor's cache
/// keeps them for the next layout.
fn sweep(cache: &mut LayoutCache) {
    cache.paras.retain(|_, para| Arc::strong_count(para) > 1);
}

/// After a block is placed: the pages now more than [`BEHIND`] behind the current one, and
/// outside the window, give up their content.
pub(crate) fn discard_passed(ctx: &mut Ctx, pb: &mut PageBuilder) {
    let Some(keep) = &ctx.opts.keep_pages else { return };
    let upto = pb.pages.len().saturating_sub(BEHIND + 1);
    // Balancing columns can take the builder back: start again from where it is now.
    let from = pb.discarded.min(upto);
    if from == upto {
        return;
    }
    for (i, page) in pb.pages.iter_mut().enumerate().take(upto).skip(from) {
        if !keep.contains(&i) {
            empty(page);
        }
    }
    pb.discarded = upto;
    if from / SWEEP_EVERY != upto / SWEEP_EVERY {
        sweep(ctx.cache);
    }
}

/// The pagination is finished: every page outside the window gives up its content, the last
/// few and their headers and footers included.
pub(crate) fn discard_outside(ctx: &mut Ctx, pages: &mut [Page]) {
    let Some(keep) = &ctx.opts.keep_pages else { return };
    for (i, page) in pages.iter_mut().enumerate() {
        if !keep.contains(&i) {
            empty(page);
        }
    }
    sweep(ctx.cache);
}
