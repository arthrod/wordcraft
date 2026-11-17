//! The viewer against the path `wordcraft-cli render` and `wordcraft-cli info` take: the engine
//! opens the same bytes, `file.info` counts the pages and `file.exportPng` writes the page. Both
//! run in this process, so they share one font database.

use wordcraft_doc::{CharProps, Document, Paragraph, para_block};
use wordcraft_viewer::{MAX_DOCUMENT_BYTES, MAX_FONT_BYTES, MAX_RENDER_PIXELS, MAX_SCALE, PageImage, Session, ViewerError, install_font, open};

/// The engine's sample documents as `.docx` packages, plus a generated one long enough to run
/// over several pages whatever fonts are installed.
fn samples() -> Vec<(&'static str, Vec<u8>)> {
    let long: String = (1..=400).map(|i| format!("Paragraph {i} of a document that runs over several pages.\n")).collect();
    [
        ("handbook", wordcraft_engine::sample::sample_document()),
        ("letter", wordcraft_engine::sample::letter()),
        ("resume", wordcraft_engine::sample::resume()),
        ("report", wordcraft_engine::sample::report()),
        ("long", Document::from_text(&long)),
    ]
    .into_iter()
    .map(|(name, doc)| (name, wordcraft_docx::write(&doc).unwrap()))
    .collect()
}

/// A session opened the way `wordcraft-cli` opens a file.
fn cli_session(docx: &[u8], password: Option<&str>) -> wordcraft_engine::Session {
    let (doc, _) = wordcraft_engine::io::open_bytes_with("sample.docx", docx, password).unwrap();
    wordcraft_engine::Session::new(doc)
}

/// The page count `wordcraft-cli info` prints.
fn cli_page_count(docx: &[u8]) -> usize {
    let info = cli_session(docx, None).run("file.info", &serde_json::json!({})).unwrap();
    usize::try_from(info["pages"].as_u64().unwrap()).unwrap()
}

/// The page `wordcraft-cli render --page <page + 1> --scale <scale>` writes, decoded.
fn cli_page(docx: &[u8], password: Option<&str>, page: u32, scale: f32) -> PageImage {
    let dir = std::env::temp_dir().join(format!("wc_viewer_test_{}_{page}_{scale}_{}", std::process::id(), docx.len()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("page.png");
    cli_session(docx, password).run("file.exportPng", &serde_json::json!({"path": path, "page": page + 1, "scale": scale})).unwrap();
    let png = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let img = image::load_from_memory(&png).unwrap().into_rgba8();
    PageImage { width: img.width(), height: img.height(), pixels: img.into_raw() }
}

/// Where two page images first differ, for a failure message that isn't megabytes long.
fn first_difference(a: &PageImage, b: &PageImage) -> Option<String> {
    if (a.width, a.height) != (b.width, b.height) {
        return Some(format!("{}×{} against {}×{}", a.width, a.height, b.width, b.height));
    }
    if a.pixels.len() != b.pixels.len() {
        return Some(format!("{} bytes against {}", a.pixels.len(), b.pixels.len()));
    }
    let at = a.pixels.iter().zip(&b.pixels).position(|(x, y)| x != y)?;
    let differing = a.pixels.as_chunks::<4>().0.iter().zip(b.pixels.as_chunks::<4>().0).filter(|(x, y)| x != y).count();
    Some(format!("{differing} pixels differ, the first at byte {at}: {:?} against {:?}", a.pixels.get(at), b.pixels.get(at)))
}

#[test]
fn page_count_equals_the_cli_export_path() {
    let mut most = 0;
    for (name, docx) in samples() {
        let session = open(&docx, None).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(session.page_count(), cli_page_count(&docx), "{name}");
        assert!(session.page_count() >= 1, "{name}");
        most = most.max(session.page_count());
    }
    assert!(most > 1, "no sample is multi-page");
}

#[test]
fn page_zero_pixels_equal_the_cli_export_path() {
    for (name, docx) in samples() {
        let session = open(&docx, None).unwrap_or_else(|e| panic!("{name}: {e}"));
        // 2 is the scale `wordcraft-cli render` uses when none is given.
        for scale in [1.0, 2.0] {
            let ours = session.render(0, scale).unwrap_or_else(|e| panic!("{name} at {scale}: {e}"));
            let cli = cli_page(&docx, None, 0, scale);
            assert_eq!(first_difference(&ours, &cli), None, "{name} at {scale}");
            assert_eq!(ours.pixels.len() as u64, u64::from(ours.width) * u64::from(ours.height) * 4, "{name} at {scale}");
            // Something is drawn: a page that is one flat colour would also "match".
            assert!(ours.pixels.as_chunks::<4>().0.iter().any(|px| *px != [255; 4]), "{name} at {scale}: blank page");
        }
    }
}

#[test]
fn a_later_page_and_the_page_size_equal_the_cli_export_path() {
    let (_, docx) = samples().pop().unwrap();
    let session = open(&docx, None).unwrap();
    let last = u32::try_from(session.page_count() - 1).unwrap();
    assert!(last >= 1);
    assert_eq!(first_difference(&session.render(last, 1.0).unwrap(), &cli_page(&docx, None, last, 1.0)), None);
    // A Letter page, in points; at scale 1 the raster is that size rounded up.
    let size = session.page_size(0).unwrap();
    assert_eq!((size.width, size.height), (612.0, 792.0));
    let img = session.render(0, 1.0).unwrap();
    assert_eq!((img.width, img.height), (612, 792));
}

fn code<T>(r: Result<T, ViewerError>) -> &'static str {
    match r {
        Ok(_) => "ok",
        Err(e) => {
            assert!(!e.to_string().is_empty());
            e.code()
        }
    }
}

#[test]
fn junk_scales_and_pages_are_errors_never_panics() {
    let (_, docx) = samples().remove(1);
    let session = open(&docx, None).unwrap();
    for scale in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -0.0, -1.0, -1e9, 1e9, f32::MAX, f32::MIN, MAX_SCALE + 0.001, f32::MIN_POSITIVE] {
        let got = code(session.render(0, scale));
        // The smallest positive scale is a legal scale: a 1×1 raster.
        let want = if scale == f32::MIN_POSITIVE { "ok" } else { "bad-scale" };
        assert_eq!(got, want, "scale {scale}");
    }
    let pages = session.page_count();
    for page in [u32::try_from(pages).unwrap(), u32::MAX, u32::MAX - 1, 1 << 31] {
        assert_eq!(session.render(page, 1.0), Err(ViewerError::NoSuchPage { page, pages }), "page {page}");
        assert_eq!(session.page_size(page), Err(ViewerError::NoSuchPage { page, pages }), "page {page}");
    }
    // A bad page and a bad scale together are still one typed error.
    assert_ne!(code(session.render(u32::MAX, f32::NAN)), "ok");
}

#[test]
fn a_render_over_the_pixel_cap_is_refused_before_anything_is_drawn() {
    let (_, docx) = samples().remove(1);
    let session = open(&docx, None).unwrap();
    // Letter at the largest scale is 9792×12672: within the rasteriser's side, over the cap.
    let Err(ViewerError::TooManyPixels { width, height, limit }) = session.render(0, MAX_SCALE) else { panic!("not refused") };
    assert_eq!((width, height, limit), (9792, 12672, MAX_RENDER_PIXELS));
    // The largest scale that fits is drawn whole, at exactly the size asked for.
    let img = session.render(0, 8.0).unwrap();
    assert_eq!((img.width, img.height), (4896, 6336));
    assert!(u64::from(img.width) * u64::from(img.height) <= MAX_RENDER_PIXELS);
}

#[test]
fn the_raster_size_is_what_render_draws_or_refuses() {
    let (_, docx) = samples().remove(1);
    let session = open(&docx, None).unwrap();
    for scale in [0.5, 1.0, 1.25, 8.0] {
        let img = session.render(0, scale).unwrap();
        assert_eq!(session.raster_size(0, scale), Ok((img.width, img.height)), "scale {scale}");
    }
    for scale in [MAX_SCALE, 0.0, -1.0, f32::NAN, f32::INFINITY, MAX_SCALE * 2.0] {
        // By code: NaN in `BadScale` never equals itself.
        assert_eq!(code(session.raster_size(0, scale)), code(session.render(0, scale)), "scale {scale}");
        assert_ne!(code(session.raster_size(0, scale)), "ok");
    }
    let pages = session.page_count();
    let past = u32::try_from(pages).unwrap();
    assert_eq!(session.raster_size(past, 1.0), Err(ViewerError::NoSuchPage { page: past, pages }));
}

#[test]
fn junk_documents_are_errors_never_panics() {
    let (_, docx) = samples().remove(1);
    assert_eq!(open(&[], None).err(), Some(ViewerError::EmptyDocument));
    assert_eq!(
        open(&vec![0u8; MAX_DOCUMENT_BYTES + 1], None).err(),
        Some(ViewerError::DocumentTooLarge { bytes: MAX_DOCUMENT_BYTES + 1, limit: MAX_DOCUMENT_BYTES })
    );
    // A zip cut short anywhere: its start, its middle, one byte short of whole.
    for keep in [1, 4, 30, docx.len() / 4, docx.len() / 2, docx.len() - 22, docx.len() - 1] {
        assert_eq!(code(open(&docx[..keep], None)), "unreadable", "truncated to {keep} of {}", docx.len());
    }
    // Bytes that are no package at all (a fixed generator: the same bytes every run).
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut random = |len: usize| -> Vec<u8> {
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (state >> 56) as u8
            })
            .collect()
    };
    for len in [1, 2, 22, 64, 4096, 1 << 20] {
        assert_eq!(code(open(&random(len), None)), "unreadable", "{len} random bytes");
    }
    // A zip signature, a compound-file signature and text, each followed by junk.
    for head in [&b"PK\x03\x04"[..], &b"PK\x05\x06"[..], &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1][..], &b"<?xml version=\"1.0\"?>"[..]] {
        let mut bytes = head.to_vec();
        bytes.extend(random(512));
        assert_eq!(code(open(&bytes, None)), "unreadable", "{head:?}");
    }
    // A password offered for a file that has none is not an error: the file opens.
    assert_eq!(code(open(&docx, Some("unused"))), "ok");
    // Flipping bytes of a real package must never panic, whatever it opens as.
    for at in (0..docx.len()).step_by(docx.len() / 64 + 1) {
        let mut bytes = docx.clone();
        bytes[at] ^= 0xFF;
        if let Ok(session) = open(&bytes, None) {
            let _ = session.render(0, 0.25);
        }
    }
}

#[test]
fn an_encrypted_package_needs_its_password() {
    let (_, docx) = samples().remove(1);
    // A small spin count: the default takes seconds a try in a test build.
    let locked = wordcraft_docx::encrypt_with_spin_count(&docx, "open sesame", 1000).unwrap();
    assert_eq!(open(&locked, None).err(), Some(ViewerError::PasswordRequired));
    assert_eq!(open(&locked, Some("wrong")).err(), Some(ViewerError::WrongPassword));
    assert_eq!(open(&locked, Some("")).err(), Some(ViewerError::WrongPassword));
    // A password past any sane length is refused, not hashed for minutes.
    assert_ne!(code(open(&locked, Some(&"x".repeat(1 << 20)))), "ok");
    // An encrypted package cut short is unreadable, with or without the password.
    assert_ne!(code(open(&locked[..locked.len() / 2], Some("open sesame"))), "ok");

    let session = open(&locked, Some("open sesame")).unwrap();
    assert_eq!(session.page_count(), cli_page_count(&docx));
    assert_eq!(first_difference(&session.render(0, 1.0).unwrap(), &cli_page(&locked, Some("open sesame"), 0, 1.0)), None);
}

/// A document whose text asks for `font`.
fn document_in(font: &str) -> Vec<u8> {
    let mut doc = Document::new();
    let props = CharProps { font: Some(font.into()), ..Default::default() };
    doc.body = vec![para_block(Paragraph::with_text("The quick brown fox jumps over the lazy dog.", props))];
    wordcraft_docx::write(&doc).unwrap()
}

fn open_in(font: &str) -> Session {
    open(&document_in(font), None).unwrap()
}

#[test]
fn installing_a_font_changes_the_missing_fonts() {
    const FONT: &str = "Viewer Test Face Zq";
    let face = *wordcraft_fonts::bundled().first().unwrap();
    let mut session = open_in(FONT);
    assert_eq!(session.missing_fonts(), vec![FONT.to_string()]);
    let before = session.render(0, 1.0).unwrap();

    // Junk is refused and changes nothing.
    assert_eq!(code(session.install_font(FONT, b"not a font")), "bad-font");
    assert_eq!(code(session.install_font(FONT, &[])), "bad-font");
    assert_eq!(code(session.install_font(FONT, &face[..face.len() / 2])), "bad-font");
    assert_eq!(code(session.install_font("", face)), "bad-font-name");
    assert_eq!(code(session.install_font("   ", face)), "bad-font-name");
    assert_eq!(code(session.install_font(&"n".repeat(257), face)), "bad-font-name");
    assert_eq!(
        session.install_font(FONT, &vec![0u8; MAX_FONT_BYTES + 1]),
        Err(ViewerError::FontTooLarge { bytes: MAX_FONT_BYTES + 1, limit: MAX_FONT_BYTES })
    );
    assert_eq!(session.missing_fonts(), vec![FONT.to_string()]);

    // Another session, opened before the font arrives, to show what it needs to see it.
    let mut other = open_in(FONT);
    session.install_font(FONT, face).unwrap();
    assert_eq!(session.missing_fonts(), Vec::<String>::new());
    assert_eq!(other.missing_fonts(), vec![FONT.to_string()]);
    other.relayout();
    assert_eq!(other.missing_fonts(), Vec::<String>::new());

    // Installing it again is not an error, and a document opened afterwards has it.
    install_font(FONT, face).unwrap();
    assert_eq!(open_in(FONT).missing_fonts(), Vec::<String>::new());
    // The page is still drawn, at the same size.
    let after = session.render(0, 1.0).unwrap();
    assert_eq!((after.width, after.height), (before.width, before.height));
}

#[test]
fn missing_fonts_are_listed_once_in_name_order() {
    let mut doc = Document::new();
    let para = |font: &str| para_block(Paragraph::with_text("Some text.", CharProps { font: Some(font.into()), ..Default::default() }));
    doc.body = vec![para("Viewer Absent Bb"), para("viewer absent aa"), para("VIEWER ABSENT BB"), para("Viewer Absent Aa")];
    let session = open(&wordcraft_docx::write(&doc).unwrap(), None).unwrap();
    let missing = session.missing_fonts();
    assert_eq!(missing.len(), 2, "{missing:?}");
    assert!(missing[0].eq_ignore_ascii_case("viewer absent aa") && missing[1].eq_ignore_ascii_case("viewer absent bb"), "{missing:?}");
}

/// A package whose text is set in `family`, with `font` embedded for it through the font table
/// as a plain `.ttf` (the all-zero key): its faces then answer to the table's name.
fn document_embedding(family: &str, font: &[u8]) -> Vec<u8> {
    const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const PR: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    let rels = |t: &str, g: &str| {
        format!(r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="{PR}"><Relationship Id="rId{t}" Type="{R}/{t}" Target="{g}"/></Relationships>"#)
            .into_bytes()
    };
    let files: [(&str, Vec<u8>); 7] = [
        (
            "[Content_Types].xml",
            br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="ttf" ContentType="application/x-font-ttf"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/fontTable.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.fontTable+xml"/></Types>"#.to_vec(),
        ),
        ("_rels/.rels", rels("officeDocument", "word/document.xml")),
        ("word/_rels/document.xml.rels", rels("fontTable", "fontTable.xml")),
        (
            "word/document.xml",
            format!(r#"<w:document xmlns:w="{W}"><w:body><w:p><w:r><w:rPr><w:rFonts w:ascii="{family}" w:hAnsi="{family}"/></w:rPr><w:t>Embedded.</w:t></w:r></w:p></w:body></w:document>"#).into_bytes(),
        ),
        (
            "word/fontTable.xml",
            format!(r#"<w:fonts xmlns:w="{W}" xmlns:r="{R}"><w:font w:name="{family}"><w:embedRegular r:id="rIdfont" w:fontKey="{{00000000-0000-0000-0000-000000000000}}"/></w:font></w:fonts>"#).into_bytes(),
        ),
        ("word/_rels/fontTable.xml.rels", rels("font", "fonts/font1.ttf")),
        ("word/fonts/font1.ttf", font.to_vec()),
    ];
    let mut zip = craft_zip::Writer::new();
    for (name, data) in files {
        zip.add(name, &data, craft_zip::Compression::Deflated).unwrap();
    }
    zip.finish().unwrap()
}

#[test]
fn a_document_can_be_opened_without_installing_its_embedded_fonts() {
    let face = *wordcraft_fonts::bundled().first().unwrap();
    // Left out: laid out as if nothing were embedded, and the process never gets the font.
    const LEFT_OUT: &str = "Viewer Embedded Left Out Zq";
    let session = wordcraft_viewer::open_without_embedded_fonts(&document_embedding(LEFT_OUT, face), None).unwrap();
    assert_eq!(session.missing_fonts(), vec![LEFT_OUT.to_string()]);
    assert!(!wordcraft_fonts::FontDb::global().has_family(LEFT_OUT));
    assert_eq!(open_in(LEFT_OUT).missing_fonts(), vec![LEFT_OUT.to_string()]);
    // `open` still loads them, for as long as the session lives.
    const LOADED: &str = "Viewer Embedded Loaded Zq";
    let loaded = open(&document_embedding(LOADED, face), None).unwrap();
    assert_eq!(loaded.missing_fonts(), Vec::<String>::new());
    assert!(wordcraft_fonts::FontDb::global().has_family(LOADED));
    drop(loaded);
    assert!(!wordcraft_fonts::FontDb::global().has_family(LOADED), "a closed session's fonts are released");
}

/// A session that keeps a window of pages (a server drawing one page per request) reports the
/// same pages and sizes as a whole one and draws every page the same, the ones outside its
/// window included.
#[test]
fn a_session_keeping_a_window_of_pages_draws_them_as_a_whole_session_does() {
    for (name, docx) in samples() {
        let whole = open(&docx, None).unwrap();
        let pages = u32::try_from(whole.page_count()).unwrap();
        for window in [0..1, pages - 1..pages, 0..pages.min(3)] {
            let kept = wordcraft_viewer::open_pages(&docx, None, window.clone()).unwrap();
            assert_eq!(kept.page_count(), whole.page_count(), "{name} {window:?}");
            // Each page in the window, and one outside it where there is one.
            let outside = (0..pages).find(|p| !window.contains(p));
            for page in window.clone().chain(outside) {
                assert_eq!(kept.page_size(page).unwrap(), whole.page_size(page).unwrap(), "{name} {window:?} page {page}");
                let (a, b) = (kept.render(page, 1.0).unwrap(), whole.render(page, 1.0).unwrap());
                assert_eq!(first_difference(&a, &b), None, "{name} {window:?} page {page}");
            }
            assert!(matches!(kept.render(pages, 1.0), Err(ViewerError::NoSuchPage { .. })), "{name}");
        }
        let kept = wordcraft_viewer::open_pages_without_embedded_fonts(&docx, None, 0..1).unwrap();
        assert_eq!(kept.page_count(), whole.page_count(), "{name}");
    }
}
