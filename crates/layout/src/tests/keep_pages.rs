//! [`LayoutOptions::keep_pages`]: a layout that keeps the content of a window of pages only.

use super::*;

/// A document of many pages: paragraphs of several lines, some with a manual page break.
fn long_document() -> Document {
    let para = "The quick brown fox jumps over the lazy dog and keeps running along the river bank until evening. ".repeat(6);
    let mut text = String::new();
    for i in 0..400 {
        text.push_str(&format!("Paragraph {i}. {para}\n"));
    }
    Document::from_text(&text)
}

fn lay(doc: &Document, keep: Option<std::ops::Range<usize>>) -> (DocLayout, LayoutCache) {
    let mut cache = LayoutCache::new();
    let l = layout(doc, &mut cache, &LayoutOptions { keep_pages: keep, ..LayoutOptions::default() });
    (l, cache)
}

/// The pages kept are the pages a whole layout makes, every page keeps its size and number,
/// and the others hold nothing: neither their own content nor a measured paragraph in the cache.
#[test]
fn a_window_of_pages_is_laid_out_as_in_the_whole_document() {
    let doc = long_document();
    let (whole, whole_cache) = lay(&doc, None);
    assert!(whole.pages.len() > 40, "{} pages", whole.pages.len());
    let last = whole.pages.len() - 1;
    for window in [0..1, 17..19, last..last + 1, 5..5, 3..usize::MAX] {
        let (kept, cache) = lay(&doc, Some(window.clone()));
        assert_eq!(kept.pages.len(), whole.pages.len(), "{window:?}");
        for (i, (k, w)) in kept.pages.iter().zip(&whole.pages).enumerate() {
            assert_eq!((k.w, k.h, k.number, k.section, k.first_block), (w.w, w.h, w.number, w.section, w.first_block), "page {i} of {window:?}");
            if window.contains(&i) {
                assert_eq!(format!("{k:?}"), format!("{w:?}"), "page {i} of {window:?}");
            } else {
                assert!(k.items.is_empty() && k.header.is_empty() && k.footer.is_empty(), "page {i} of {window:?} keeps content");
            }
        }
        if window.len() <= 2 {
            assert!(
                cache.paras.len() * 4 < whole_cache.paras.len(),
                "{window:?}: {} of {} paragraphs kept",
                cache.paras.len(),
                whole_cache.paras.len()
            );
            assert!(kept.index.len() * 4 < whole.index.len(), "{window:?}");
        }
    }
}

/// Without the option nothing is discarded (the editor's layout).
#[test]
fn a_layout_without_a_window_keeps_every_page() {
    let (whole, _) = lay(&long_document(), None);
    assert!(whole.pages.iter().all(|p| !p.items.is_empty()));
}
