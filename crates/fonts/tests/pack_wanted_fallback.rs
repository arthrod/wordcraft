//! Wanted-pack recording when a fallback list settles on a later installed family: the minimal
//! bundle lacks JetBrains Mono, so "JetBrains Mono;Source Sans 3" draws in Source Sans 3 while
//! JetBrains Mono stays wanted and downloadable. Separate process: `init_test_fonts` must run
//! before any font lookup, and the minimal bundle is what leaves a pack family uninstalled.
#![cfg(all(feature = "test-support", feature = "minimal-wasm-fonts"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use wordcraft_fonts::FontDb;
use wordcraft_fonts::word::resolve;

#[test]
fn fallback_list_notes_the_skipped_pack_family() {
    assert!(FontDb::init_test_fonts());
    let db = FontDb::global();
    db.take_wanted_packs();

    let r = resolve("JetBrains Mono;Source Sans 3", false, false);
    assert!(!r.substituted, "Source Sans 3 is bundled: {:?}", r.face.family);
    assert_eq!(r.face.family, "Source Sans 3");

    let wanted = db.take_wanted_packs();
    assert!(
        wanted.iter().any(|w| w.family == "JetBrains Mono" && !w.bold && !w.italic),
        "the skipped preferred pack family stays wanted: {wanted:?}"
    );
}
