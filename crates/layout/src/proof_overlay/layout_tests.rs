//! Proofing marks through a whole layout: worker overlays on the display list, and layout
//! without the `inline-proof` feature. Kept out of `tests.rs` so the two files change
//! independently.
use crate::*;
use wordcraft_doc::Pos;

/// Worker proofing overlays (§3.2) draw where inline issues draw, only for a paragraph the
/// inline check left empty, and never past the text they were found in.
#[test]
fn worker_overlays_draw_squiggles_where_inline_issues_would() {
    crate::test_fonts();
    let mut d = Document::from_text("nice text");
    d.format_range(&Pos::body(0, 0), &Pos::body(0, 9), &|c| c.lang = Some("pl-PL".into())).unwrap();
    let mut c = LayoutCache::new();
    // The browser build's shape: inline proofing off, so `ParaLayout::issues` is empty and the
    // display is the only place worker spans can reach.
    let l = layout(&d, &mut c, &LayoutOptions { proofing: false, ..LayoutOptions::default() });
    let marks =
        |l: &crate::DocLayout| -> Vec<(crate::display::Stroke, f32, f32, wordcraft_doc::Rgb)> {
            l.pages
                .first()
                .map(|p| {
                    crate::display::page_layers(&d, p, &crate::display::DisplayOptions::default())
                        .into_iter()
                        .flat_map(|(_, draws)| draws)
                        .filter_map(|x| {
                            if let crate::display::Draw::Line { x0, x1, color, stroke, .. } = x { Some((stroke, x0, x1, color)) } else { None }
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
    let is_mark = |stroke: crate::display::Stroke| move |m: &(crate::display::Stroke, f32, f32, wordcraft_doc::Rgb)| m.0 == stroke && m.1 < m.2;
    let no_overlays = marks(&l);
    assert!(no_overlays.iter().filter(|m| is_mark(crate::display::Stroke::Wave)(m) || is_mark(crate::display::Stroke::Double)(m)).count() == 0);

    // Spelling (wave) over "nice", grammar (double) over "text": proof-text bytes, like issues.
    crate::proof_overlay::set_overlays([(StoryRef::Body, Path::top(0), vec![(0, 4, false), (5, 9, true)])]);
    let with_overlays = marks(&l);
    assert!(with_overlays.iter().filter(|m| is_mark(crate::display::Stroke::Wave)(m)).count() >= 1, "{with_overlays:?}");
    assert!(with_overlays.iter().filter(|m| is_mark(crate::display::Stroke::Double)(m)).count() >= 1, "{with_overlays:?}");

    // A span past the text's end never draws: worker replies are hostile-input-bound.
    crate::proof_overlay::set_overlays([(StoryRef::Body, Path::top(0), vec![(100, 200, false)])]);
    assert!(marks(&l).iter().filter(|m| is_mark(crate::display::Stroke::Wave)(m)).count() == 0);

    // With inline proofing on, the paragraph was checked inline: overlays must not add marks to
    // one that has issues of its own (stale by design where both could run).
    wordcraft_proof::pl::install_dictionary("SET UTF-8\nSFX B Y 1\nSFX B y ego y\n", "2\nżeka\n").unwrap();
    let mut c2 = LayoutCache::new();
    let inline = layout(&d, &mut c2, &LayoutOptions { proofing: true, ..LayoutOptions::default() });
    let para =
        inline.pages[0].items.iter().find_map(|i| if let Placed::Lines { para, .. } = i { Some(para.clone()) } else { None }).expect("a paragraph");
    if !para.issues.is_empty() {
        crate::proof_overlay::set_overlays([(StoryRef::Body, Path::top(0), vec![(0, 4, false), (5, 9, true)])]);
        let drawn = marks(&inline);
        let waves = drawn.iter().filter(|m| is_mark(crate::display::Stroke::Wave)(m)).count();
        let doubles = drawn.iter().filter(|m| is_mark(crate::display::Stroke::Double)(m)).count();
        let inline_spelling = para.issues.iter().filter(|i| !i.2).count();
        let inline_grammar = para.issues.iter().filter(|i| i.2).count();
        assert_eq!((waves, doubles), (inline_spelling, inline_grammar), "overlays must not add marks to an inline-checked paragraph");
    }
    crate::proof_overlay::clear_overlays();
}

#[test]
fn layout_runs_without_inline_proof() {
    crate::test_fonts();
    let doc = Document::from_text("Teh quick brown foxx.");
    let mut cache = LayoutCache::default();
    let l = layout(&doc, &mut cache, &LayoutOptions { proofing: true, ..Default::default() });
    assert_eq!(l.pages.len(), 1);
    let para = l.pages[0]
        .items
        .iter()
        .find_map(|it| match it {
            Placed::Lines { para, .. } => Some(para),
            _ => None,
        })
        .unwrap();
    assert!(!para.lines.is_empty());
    #[cfg(not(feature = "inline-proof"))]
    assert!(para.issues.is_empty(), "without inline-proof, layout emits no proofing squiggles");
}
