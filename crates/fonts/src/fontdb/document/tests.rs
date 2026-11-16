use super::*;
use crate::fontdb::user::tests::renamed;

/// A document's fonts are its own: withdrawn when another document takes its place, so they
/// never answer for one that only names the family, and back if that document fails to open.
#[test]
fn a_documents_fonts_leave_with_it_and_come_back_if_the_next_one_fails() {
    let db = FontDb::with_font_dirs(vec![]);
    let bold = include_bytes!("../../../../../assets/fonts/SourceSans3-Bold.ttf").to_vec();
    let faces = db.add_document_fonts_scoped(vec![(bold.clone(), "Contract Bold".to_string())], usize::MAX);
    assert_eq!(faces.len(), 1, "the document's face is new under its own name");
    assert!(db.has_family("Contract Bold"));
    let generation = db.generation();
    db.withdraw_document_fonts(&faces);
    assert!(!db.has_family("Contract Bold"), "a later document does not see it");
    assert!(db.runtime_faces().is_empty());
    assert!(db.generation() > generation, "lookups made while it was there are forgotten");
    db.reinstate_document_fonts(&faces);
    assert!(db.has_family("Contract Bold"), "the next document failed: it is back");
    db.reinstate_document_fonts(&faces);
    assert_eq!(db.runtime_faces().len(), 1, "back once, however often it is reinstated");

    // The next document embeds the same family and style in other bytes (Word embeds subsets):
    // with the first withdrawn, its own face is added, not skipped as already present.
    db.withdraw_document_fonts(&faces);
    let mut subset = bold.clone();
    subset.extend_from_slice(&[0; 4]);
    let next = db.add_document_fonts_scoped(vec![(subset, "Contract Bold".to_string())], usize::MAX);
    assert_eq!(next.len(), 1, "the next document's own face");
    // Reinstating the first beside it adds nothing under a family and style already present.
    db.reinstate_document_fonts(&faces);
    assert_eq!(db.runtime_faces().len(), 1);
    // Withdrawing faces it does not hold changes nothing.
    db.withdraw_document_fonts(&faces);
    assert_eq!(db.runtime_faces().len(), 1);
}

#[test]
fn a_document_font_file_is_kept_once_and_its_faces_reused() {
    let db = FontDb::with_font_dirs(vec![]);
    let bold = include_bytes!("../../../../../assets/fonts/SourceSans3-Bold.ttf").to_vec();
    let first = db.add_document_fonts_scoped(vec![(bold.clone(), "Kept Bold".to_string())], usize::MAX);
    assert_eq!((first.len(), first.new_bytes()), (1, bold.len()), "a new file is kept");
    // The same file again (the document opened again, or another embedding it): the same
    // faces come back and nothing new is kept.
    db.withdraw_document_fonts(&first);
    let again = db.add_document_fonts_scoped(vec![(bold.clone(), "Kept Bold".to_string())], usize::MAX);
    assert_eq!((again.len(), again.new_bytes()), (1, 0));
    let ids = |f: &FontDb| f.read_faces().iter().filter(|g| g.named("Kept Bold")).map(|g| g.id).collect::<Vec<_>>();
    assert_eq!(ids(&db), first.faces.iter().map(|f| f.id).collect::<Vec<_>>(), "the faces made the first time");
    // A file whose faces are all present already adds nothing and is not kept.
    let regular = include_bytes!("../../../../../assets/fonts/SourceSans3-Regular.ttf").to_vec();
    assert!(db.add_document_fonts(vec![(regular.clone(), "Hosted Regular".to_string())]) > 0);
    let skipped = db.add_document_fonts_scoped(vec![(regular.clone(), "Hosted Regular".to_string())], usize::MAX);
    assert_eq!((skipped.len(), skipped.new_bytes()), (0, 0));
    // The name as it is kept finds the file; the same bytes under another name are another.
    db.withdraw_document_fonts(&again);
    let trimmed = db.add_document_fonts_scoped(vec![(bold.clone(), " Kept Bold ".to_string())], 0);
    assert_eq!((trimmed.len(), trimmed.new_bytes()), (1, 0), "a kept file needs no room");
    let renamed_file = db.add_document_fonts_scoped(vec![(bold.clone(), "Other Bold".to_string())], usize::MAX);
    assert_eq!((renamed_file.len(), renamed_file.new_bytes()), (1, bold.len()));
}

#[test]
fn an_install_over_a_document_takes_the_place_of_its_face_of_that_family_and_style() {
    // A face one document embeds is that document's for as long as it is shown. A pack, a user
    // font or a host's font of the same family and style, installed over the document, is not
    // skipped as already present: it takes the document's face's place, so the family stays
    // when the document goes.
    for (how, family) in [("pack", "Pack Sans Abc"), ("user", "User Sans Abc"), ("host", "Host Sans Abc")] {
        let db = FontDb::with_font_dirs(vec![]);
        let font = renamed("SourceSans3-Regular.ttf", family);
        let doc = db.add_document_fonts_scoped(vec![(font.clone(), family.to_string())], usize::MAX);
        assert_eq!(doc.len(), 1, "{how}");
        let added = db.installing_over(&doc, || match how {
            "pack" => db.install_pack_font("pack.ttf", &font).map(|f| f.len()).unwrap_or(0),
            "user" => db.install_font("user.ttf", &font).map(|i| i.font.faces.len()).unwrap_or(0),
            _ => db.add_document_fonts(vec![(font.clone(), family.to_string())]),
        });
        assert_eq!(added, 1, "{how}: its face is added in the document's place");
        assert_eq!(db.read_faces().iter().filter(|f| f.named(family)).count(), 1, "{how}: one face for the family and style");
        db.withdraw_document_fonts(&doc);
        assert_eq!(db.styles(family), ["Regular"], "{how}: the family stays when the document goes");
        db.reinstate_document_fonts(&doc);
        assert_eq!(db.read_faces().iter().filter(|f| f.named(family)).count(), 1, "{how}: the document's face does not come back beside it");
    }
}

#[test]
fn an_install_over_a_document_leaves_its_other_faces_its_own() {
    // The host installs the family's Bold over a document that embeds its Regular: the Regular
    // is back while the document is shown, and leaves with it.
    let db = FontDb::with_font_dirs(vec![]);
    let doc = db.add_document_fonts_scoped(vec![(renamed("SourceSans3-Regular.ttf", "Twin Sans Abc"), "Twin Sans Abc".to_string())], usize::MAX);
    let bold = renamed("SourceSans3-Bold.ttf", "Twin Sans Abc");
    assert_eq!(db.installing_over(&doc, || db.add_document_fonts(vec![(bold, "Twin Sans Abc".to_string())])), 1);
    assert_eq!(db.styles("Twin Sans Abc"), ["Regular", "Bold"]);
    db.withdraw_document_fonts(&doc);
    assert_eq!(db.styles("Twin Sans Abc"), ["Bold"]);
}

#[test]
fn an_install_over_a_document_that_fails_or_unwinds_puts_its_faces_back() {
    let db = FontDb::with_font_dirs(vec![]);
    let doc = db.add_document_fonts_scoped(vec![(renamed("SourceSans3-Regular.ttf", "Back Sans Abc"), "Back Sans Abc".to_string())], usize::MAX);
    let failed: Result<usize, &str> = db.installing_over(&doc, || Err("not a font"));
    assert!(failed.is_err());
    assert_eq!(db.styles("Back Sans Abc"), ["Regular"], "an install that fails leaves the document's face");
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| db.installing_over(&doc, || -> usize { panic!("the install unwound") })));
    assert!(unwound.is_err());
    assert_eq!(db.styles("Back Sans Abc"), ["Regular"], "an install that unwinds leaves the document's face");
}

#[test]
fn files_of_one_length_and_name_are_told_apart_by_their_bytes() {
    // Two different files of the same length given the same name, and the same file again.
    let db = FontDb::with_font_dirs(vec![]);
    let (a, b) = (renamed("SourceSans3-Regular.ttf", "Aaaa Sans Abc"), renamed("SourceSans3-Regular.ttf", "Bbbb Sans Abc"));
    assert_eq!(a.len(), b.len());
    assert_ne!(a, b);
    let first = db.add_document_fonts_scoped(vec![(a.clone(), "Same Name".to_string())], usize::MAX);
    assert_eq!((first.len(), first.new_bytes()), (1, a.len()));
    db.withdraw_document_fonts(&first);
    let other = db.add_document_fonts_scoped(vec![(b.clone(), "Same Name".to_string())], usize::MAX);
    assert_eq!((other.len(), other.new_bytes()), (1, b.len()), "another file: kept on its own");
    assert_ne!(other.faces[0].id, first.faces[0].id, "with faces of its own");
    db.withdraw_document_fonts(&other);
    for (data, earlier) in [(&a, &first), (&b, &other)] {
        let again = db.add_document_fonts_scoped(vec![(data.clone(), "Same Name".to_string())], 0);
        assert_eq!((again.len(), again.new_bytes()), (1, 0), "the same file again");
        assert_eq!(again.faces[0].id, earlier.faces[0].id, "brings back the faces made for it");
        db.withdraw_document_fonts(&again);
    }
}

#[test]
fn new_document_files_are_kept_while_they_fit_the_room() {
    // Room for two files of one length and a smaller one. A hosted font (its faces present)
    // costs nothing; the third new file is past what is left; the smaller one after it fits.
    let db = FontDb::with_font_dirs(vec![]);
    let font = |family: &str| renamed("SourceSans3-Regular.ttf", family);
    let len = font("Room Sans One").len();
    assert!(db.add_document_fonts(vec![(font("Host Sans Two"), "Host Sans Two".to_string())]) > 0);
    let small = include_bytes!("../../../../../assets/fonts/JetBrainsMono-Regular.ttf").to_vec();
    assert!(small.len() < len);
    let files = ["Room Sans One", "Host Sans Two", "Room Sans Two", "Room Sans Thr"].map(|f| (font(f), f.to_string()));
    let mut files = files.to_vec();
    files.push((small.clone(), "Room Mono".to_string()));
    let faces = db.add_document_fonts_scoped(files, 2 * len + small.len());
    assert_eq!((faces.len(), faces.new_bytes()), (3, 2 * len + small.len()));
    for (family, styles) in [("Room Sans One", 1), ("Room Sans Two", 1), ("Room Sans Thr", 0), ("Room Mono", 1)] {
        assert_eq!(db.styles(family).len(), styles, "{family}");
    }
}
