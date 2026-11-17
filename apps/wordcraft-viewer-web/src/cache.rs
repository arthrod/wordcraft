//! Which pages to ask the worker for and which painted pages to let go. The painted canvases
//! are the cache; this is its book, kept apart from the DOM so the policy is tested on the
//! host: a page is painted once per scale, no more than [`MAX_IN_FLIGHT`] renders are asked of
//! the worker at once, and pages out of view are released oldest first once the budget is spent.

use std::collections::{BTreeMap, BTreeSet};

/// Most renders waiting on the worker at once. The worker paints one page at a time, so a
/// longer queue would only be pages the reader has already scrolled past.
pub const MAX_IN_FLIGHT: usize = 2;

/// Most failed requests remembered; past this the record starts over.
const MAX_FAILED: usize = 4096;

/// A painted page's identity: the page and the exact scale it was painted at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PageKey {
    pub page: u32,
    scale_bits: u32,
}

impl PageKey {
    pub fn new(page: u32, scale: f32) -> PageKey {
        PageKey { page, scale_bits: scale.to_bits() }
    }

    pub fn scale(self) -> f32 {
        f32::from_bits(self.scale_bits)
    }
}

/// What to do with a page that arrived from the worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Arrival {
    /// Paint it, and release the canvases of the pages in `evict`.
    Paint { evict: Vec<u32> },
    /// Nobody is waiting for it (the document was replaced, or laid out again): drop it.
    Stale,
}

struct Painted {
    key: PageKey,
    bytes: u64,
    /// When the page was last wanted; larger is more recent.
    stamp: u64,
}

/// The book of painted and awaited pages.
pub struct PageCache {
    budget: u64,
    used: u64,
    clock: u64,
    painted: BTreeMap<u32, Painted>,
    pending: BTreeMap<u32, PageKey>,
    failed: BTreeSet<PageKey>,
    /// The entry the last arrival took the place of (the same page at another scale), until
    /// that arrival is known to be on the canvas: see [`PageCache::not_painted`].
    displaced: Option<Painted>,
}

impl PageCache {
    /// A cache that keeps at most `budget_bytes` of pixels beyond the pages in the window. The
    /// window is the pages in view and the overscan around them, the `keep` of
    /// [`PageCache::arrived`]: those are never released, whatever the budget, because a page
    /// released inside the window is asked for again at once.
    pub fn new(budget_bytes: u64) -> PageCache {
        PageCache {
            budget: budget_bytes,
            used: 0,
            clock: 0,
            painted: BTreeMap::new(),
            pending: BTreeMap::new(),
            failed: BTreeSet::new(),
            displaced: None,
        }
    }

    /// The renders to ask the worker for now, given the pages the window wants, most wanted
    /// first. A page already painted at that scale, already asked for, or known to fail is
    /// skipped, as is a page awaited at another scale, and no more than [`MAX_IN_FLIGHT`] wait
    /// at once: call again when a reply lands. Wanting a page also marks it as recently shown.
    pub fn plan(&mut self, wanted: &[PageKey]) -> Vec<PageKey> {
        let mut ask = Vec::new();
        for key in wanted {
            self.clock = self.clock.saturating_add(1);
            if let Some(painted) = self.painted.get_mut(&key.page) {
                painted.stamp = self.clock;
                if painted.key == *key {
                    continue;
                }
            }
            // A page already awaited, at any scale, waits for that answer: the serial worker
            // renders every request it is sent, so asking again at each resize would queue
            // renders without bound. The answer is painted, then replaced at the new scale.
            if self.pending.contains_key(&key.page) || self.failed.contains(key) || self.pending.len() >= MAX_IN_FLIGHT {
                continue;
            }
            self.pending.insert(key.page, *key);
            ask.push(*key);
        }
        ask
    }

    /// A page of `bytes` pixels arrived. `keep` lists the pages in the window, which are never
    /// released.
    pub fn arrived(&mut self, key: PageKey, bytes: u64, keep: &[u32]) -> Arrival {
        if self.pending.get(&key.page) != Some(&key) {
            return Arrival::Stale;
        }
        self.pending.remove(&key.page);
        self.clock = self.clock.saturating_add(1);
        self.displaced = self.painted.insert(key.page, Painted { key, bytes, stamp: self.clock });
        if let Some(old) = &self.displaced {
            self.used = self.used.saturating_sub(old.bytes);
        }
        self.used = self.used.saturating_add(bytes);
        let mut evict = Vec::new();
        while self.used > self.budget {
            let oldest = self
                .painted
                .iter()
                .filter(|(page, _)| **page != key.page && !keep.contains(page))
                .min_by_key(|(_, p)| p.stamp)
                .map(|(page, _)| *page);
            let Some(page) = oldest else { break };
            if let Some(gone) = self.painted.remove(&page) {
                self.used = self.used.saturating_sub(gone.bytes);
            }
            evict.push(page);
        }
        Arrival::Paint { evict }
    }

    /// The worker refused `key` (too many pixels, say), or it arrived and its pixels could not
    /// be put on the canvas: it is neither awaited nor painted, and it is not asked for again
    /// (the same request would fail the same way); another scale of the page still is. A key
    /// that is neither awaited nor painted changes nothing.
    pub fn failed(&mut self, key: PageKey) {
        if self.pending.get(&key.page) == Some(&key) {
            self.pending.remove(&key.page);
        } else if self.painted(key.page) == Some(key) {
            if let Some(gone) = self.painted.remove(&key.page) {
                self.used = self.used.saturating_sub(gone.bytes);
            }
        } else {
            return;
        }
        if self.failed.len() >= MAX_FAILED {
            self.failed.clear();
        }
        self.failed.insert(key);
    }

    /// `key` arrived and its pixels could not be put on the canvas: [`PageCache::failed`], and
    /// with `kept_old`, the canvas still shows what it showed before (the failure came before
    /// the canvas was cleared), so the entry the arrival took the place of is counted again.
    /// Without it the old bitmap would be held by the canvas and by no entry in the book.
    pub fn not_painted(&mut self, key: PageKey, kept_old: bool) {
        let old = self.displaced.take().filter(|old| kept_old && old.key.page == key.page && self.painted(key.page) == Some(key));
        self.failed(key);
        if let Some(old) = old {
            self.used = self.used.saturating_add(old.bytes);
            self.painted.insert(key.page, old);
        }
    }

    /// Page `page` has nowhere to be painted (its canvas went before its answer came): it is
    /// neither awaited nor painted, so it is asked for again when it is wanted.
    pub fn forget(&mut self, page: u32) {
        self.displaced = None;
        self.pending.remove(&page);
        if let Some(gone) = self.painted.remove(&page) {
            self.used = self.used.saturating_sub(gone.bytes);
        }
    }

    /// The key page `page` is painted at, if it is.
    pub fn painted(&self, page: u32) -> Option<PageKey> {
        self.painted.get(&page).map(|p| p.key)
    }

    /// Bytes of pixels held by painted pages.
    #[cfg(test)]
    pub fn used_bytes(&self) -> u64 {
        self.used
    }

    /// Forget everything (the document was replaced, or laid out again): the pages that were
    /// painted, so their canvases can be released.
    pub fn clear(&mut self) -> Vec<u32> {
        let pages = self.painted.keys().copied().collect();
        self.painted.clear();
        self.pending.clear();
        self.failed.clear();
        self.displaced = None;
        self.used = 0;
        pages
    }
}

#[cfg(test)]
mod tests;
