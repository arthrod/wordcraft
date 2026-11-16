//! From bytes to pages: sniff, decrypt, check the inflated size, open with the view-only core,
//! and draw a page as a small PNG.
//!
//! The view-only core ([`wordcraft_viewer::open`]) reads the package with the `.docx` reader's own
//! limits, which allow 512 MiB of inflated parts: right for a desktop, four times an isolate. So
//! before the core sees a package, its zip directory is read here and the sizes its entries
//! declare are added up against [`MAX_INFLATED_BYTES`]. The zip reader refuses an entry that
//! inflates past its declared size, so this bounds what is really inflated. A protected file is
//! decrypted here first, with the decryption the reader itself uses, so its package is checked
//! the same way; the core then opens the plain package.

#[cfg(test)]
use wordcraft_viewer::PageImage;
use wordcraft_viewer::{PageSize, Session, ViewerError};

use crate::limits::{MAX_BODY_BYTES, MAX_INFLATED_BYTES, MAX_PAGE_PIXELS, MAX_ZIP_ENTRIES};

/// Why a document couldn't be opened or drawn. [`OpenError::status`] is the HTTP status.
#[derive(Clone, Debug, PartialEq)]
pub enum OpenError {
    /// Neither a zip package nor an encrypted Office file (415).
    NotDocx,
    /// More than [`MAX_BODY_BYTES`] (413).
    TooLarge { bytes: usize },
    /// Parts declaring more than [`MAX_INFLATED_BYTES`] once inflated, or more than
    /// [`MAX_ZIP_ENTRIES`] entries (413).
    Inflated,
    /// Encrypted, and no password was given (422, with the form again).
    PasswordRequired,
    /// Encrypted, and the password doesn't open it (422, with the form again).
    WrongPassword,
    /// A zip or encrypted file this can't read as a Word document (422).
    Unreadable(String),
    /// The page can't be drawn within the limits (422).
    Unrenderable(String),
}

impl OpenError {
    pub fn status(&self) -> u16 {
        match self {
            OpenError::NotDocx => 415,
            OpenError::TooLarge { .. } | OpenError::Inflated => 413,
            OpenError::PasswordRequired | OpenError::WrongPassword | OpenError::Unreadable(_) | OpenError::Unrenderable(_) => 422,
        }
    }
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::NotDocx => write!(f, "This file is not a Word document (.docx)."),
            OpenError::TooLarge { bytes } => write!(f, "The document is {bytes} bytes; this viewer takes at most {MAX_BODY_BYTES} bytes."),
            OpenError::Inflated => write!(
                f,
                "The document unpacks to more than {} MiB or has more than {MAX_ZIP_ENTRIES} parts, more than this viewer opens.",
                MAX_INFLATED_BYTES >> 20
            ),
            OpenError::PasswordRequired => write!(f, "This document is protected with a password."),
            OpenError::WrongPassword => write!(f, "The password is incorrect."),
            OpenError::Unreadable(why) => write!(f, "The document can't be read: {why}"),
            OpenError::Unrenderable(why) => write!(f, "The page can't be drawn: {why}"),
        }
    }
}

/// Open `bytes` as a `.docx`, decrypting it with `password` when it is protected.
pub fn open_document(bytes: &[u8], password: Option<&str>) -> Result<Session, OpenError> {
    if bytes.len() > MAX_BODY_BYTES {
        return Err(OpenError::TooLarge { bytes: bytes.len() });
    }
    let decrypted;
    let package: &[u8] = if bytes.starts_with(b"PK\x03\x04") {
        bytes
    } else if craft_offcrypto::is_encrypted(bytes) {
        decrypted = craft_offcrypto::decrypt(bytes, password).map_err(|e| match e {
            craft_offcrypto::Error::PasswordRequired => OpenError::PasswordRequired,
            craft_offcrypto::Error::WrongPassword => OpenError::WrongPassword,
            other => OpenError::Unreadable(other.to_string()),
        })?;
        &decrypted
    } else {
        return Err(OpenError::NotDocx);
    };
    check_inflated_size(package)?;
    wordcraft_viewer::open(package, None).map_err(from_viewer)
}

/// Refuse a package whose entries declare more than [`MAX_INFLATED_BYTES`] in all, or that has
/// more than [`MAX_ZIP_ENTRIES`] entries. Reads the zip directory only; nothing is inflated.
fn check_inflated_size(package: &[u8]) -> Result<(), OpenError> {
    let limits = craft_zip::Limits { max_entries: MAX_ZIP_ENTRIES, max_total_bytes: MAX_INFLATED_BYTES };
    let mut archive = craft_zip::Archive::open(package, limits).map_err(|e| match e {
        craft_zip::Error::TooManyEntries { .. } => OpenError::Inflated,
        other => OpenError::Unreadable(other.to_string()),
    })?;
    let names: Vec<String> = archive.names().map(|n| n.into_owned()).collect();
    let mut total: u64 = 0;
    for name in names {
        total = total.saturating_add(archive.declared_size(&name).unwrap_or(0));
        if total > MAX_INFLATED_BYTES {
            return Err(OpenError::Inflated);
        }
    }
    Ok(())
}

/// The size in pixels the core draws a page of `size` points at `scale`: its own arithmetic
/// (single precision, rounded up, at least one pixel), so the `width`/`height` attributes on an
/// `<img>` match the PNG.
pub fn raster_size(size: PageSize, scale: f32) -> (u32, u32) {
    // A float-to-integer cast saturates and sends NaN to 0.
    let side = |points: f32| ((points * scale).ceil() as u32).max(1);
    (side(size.width), side(size.height.min(1e6)))
}

/// `scale`, or a smaller one that keeps a page of `size` points under [`MAX_PAGE_PIXELS`]: a
/// poster is drawn smaller rather than refused.
pub fn fit_scale(size: PageSize, scale: f32) -> f32 {
    let area = f64::from(size.width) * f64::from(size.height);
    if !(area.is_finite() && area > 0.0) {
        return scale;
    }
    // One pixel of rounding up on each side, so stay a little under the cap.
    let fit = ((MAX_PAGE_PIXELS as f64 * 0.98) / area).sqrt();
    if f64::from(scale) <= fit { scale } else { (fit as f32).max(f32::MIN_POSITIVE) }
}

/// Page `index` (0-based) at `scale` (fitted), as PNG bytes with its width and height.
pub fn page_png(session: &Session, index: u32, scale: f32) -> Result<(Vec<u8>, u32, u32), OpenError> {
    let size = session.page_size(index).map_err(from_viewer)?;
    let image = session.render(index, fit_scale(size, scale)).map_err(|e| OpenError::Unrenderable(e.to_string()))?;
    let (png, _) = crate::png_out::encode(&image).map_err(|e| OpenError::Unrenderable(e.to_string()))?;
    Ok((png, image.width, image.height))
}

fn from_viewer(e: ViewerError) -> OpenError {
    match e {
        ViewerError::PasswordRequired => OpenError::PasswordRequired,
        ViewerError::WrongPassword => OpenError::WrongPassword,
        ViewerError::DocumentTooLarge { bytes, .. } => OpenError::TooLarge { bytes },
        ViewerError::EmptyDocument => OpenError::NotDocx,
        other => OpenError::Unreadable(other.to_string()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The bundled faces only, no system scan, as on the Worker. Every test that opens a
    /// document calls this first, so whichever runs first sets the font database up this way
    /// (the call returns `false` to the others; the database is the same).
    pub(crate) fn fonts() {
        let _ = wordcraft_fonts::FontDb::init_test_fonts();
    }

    /// A small real package: the paragraphs as a `.docx`, through the same zip writer and
    /// content types a `.docx` has. Built by hand so the tests need no writer crate.
    pub(crate) fn docx(paragraphs: &[&str]) -> Vec<u8> {
        let body: String = paragraphs.iter().map(|p| format!("<w:p><w:r><w:t xml:space=\"preserve\">{p}</w:t></w:r></w:p>")).collect();
        let parts: [(&str, String); 3] = [
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#.to_string(),
            ),
            (
                "_rels/.rels",
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#.to_string(),
            ),
            (
                "word/document.xml",
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440"/></w:sectPr></w:body></w:document>"#
                ),
            ),
        ];
        let mut w = craft_zip::Writer::new();
        for (name, data) in parts {
            w.add(name, data.as_bytes(), craft_zip::Compression::Deflated).unwrap();
        }
        w.finish().unwrap()
    }

    /// Enough paragraphs to run over several pages.
    pub(crate) fn long_docx() -> Vec<u8> {
        let paras: Vec<String> = (1..=150).map(|i| format!("Paragraph {i} of a document that runs over several pages.")).collect();
        docx(&paras.iter().map(String::as_str).collect::<Vec<_>>())
    }

    #[test]
    fn opens_a_package_and_counts_pages() {
        fonts();
        let s = open_document(&docx(&["Hello, world."]), None).unwrap();
        assert_eq!(s.page_count(), 1);
        let s = open_document(&long_docx(), None).unwrap();
        assert!(s.page_count() >= 3, "{} pages", s.page_count());
    }

    #[test]
    fn refuses_what_is_not_a_docx() {
        fonts();
        assert_eq!(open_document(b"", None).err(), Some(OpenError::NotDocx));
        assert_eq!(open_document(b"Just some text.\n", None).err(), Some(OpenError::NotDocx));
        assert_eq!(open_document(b"%PDF-1.7\n", None).err(), Some(OpenError::NotDocx));
        assert_eq!(open_document(&[0u8; 4096], None).err(), Some(OpenError::NotDocx));
        // A zip that isn't a Word package, and a truncated one.
        let mut w = craft_zip::Writer::new();
        w.add("hello.txt", b"hi", craft_zip::Compression::Deflated).unwrap();
        assert!(matches!(open_document(&w.finish().unwrap(), None), Err(OpenError::Unreadable(_))));
        let pkg = docx(&["x"]);
        assert!(matches!(open_document(&pkg[..pkg.len() / 2], None), Err(OpenError::Unreadable(_))));
        assert!(matches!(open_document(b"PK\x03\x04", None), Err(OpenError::Unreadable(_))));
        // Too large, before anything is parsed.
        let big = vec![0u8; MAX_BODY_BYTES + 1];
        assert_eq!(open_document(&big, None).err(), Some(OpenError::TooLarge { bytes: MAX_BODY_BYTES + 1 }));
    }

    #[test]
    fn a_zip_bomb_is_refused_before_it_is_inflated() {
        fonts();
        // 25 MiB of zeros deflate to about 25 KB: a valid package around them still declares
        // more than the isolate can afford.
        let zeros = vec![0u8; (MAX_INFLATED_BYTES as usize) + 1];
        let mut w = craft_zip::Writer::new();
        let good = docx(&["x"]);
        let archive = craft_zip::Archive::open(&good, craft_zip::Limits { max_entries: 10, max_total_bytes: 1 << 20 }).unwrap();
        let names: Vec<String> = archive.names().map(|n| n.into_owned()).collect();
        let mut archive = archive;
        for n in &names {
            let data = archive.read(n, 1 << 20).unwrap().unwrap();
            w.add(n, &data, craft_zip::Compression::Deflated).unwrap();
        }
        w.add("word/media/bomb.bin", &zeros, craft_zip::Compression::Deflated).unwrap();
        let bomb = w.finish().unwrap();
        assert!(bomb.len() < 100_000, "{} bytes", bomb.len());
        assert_eq!(open_document(&bomb, None).err(), Some(OpenError::Inflated));
        // Too many entries.
        let mut w = craft_zip::Writer::new();
        for i in 0..=MAX_ZIP_ENTRIES {
            w.add(&format!("x/{i}"), b"", craft_zip::Compression::Stored).unwrap();
        }
        assert_eq!(open_document(&w.finish().unwrap(), None).err(), Some(OpenError::Inflated));
    }

    #[test]
    fn protected_documents_need_the_right_password() {
        fonts();
        let plain = docx(&["Secret text."]);
        let locked = craft_offcrypto::encrypt(&plain, "s3cret").unwrap();
        assert_eq!(open_document(&locked, None).err(), Some(OpenError::PasswordRequired));
        assert_eq!(open_document(&locked, Some("guess")).err(), Some(OpenError::WrongPassword));
        assert_eq!(open_document(&locked, Some("s3cret")).map(|s| s.page_count()).ok(), Some(1));
        // A password on a plain package is ignored.
        assert_eq!(open_document(&plain, Some("whatever")).map(|s| s.page_count()).ok(), Some(1));
    }

    #[test]
    fn sizes_match_the_core_and_posters_are_fitted() {
        let letter = PageSize { width: 612.0, height: 792.0 };
        assert_eq!(raster_size(letter, 1.0), (612, 792));
        assert_eq!(raster_size(letter, 1.25), (765, 990));
        assert_eq!(raster_size(PageSize { width: 595.3, height: 841.9 }, 1.0), (596, 842));
        assert_eq!(fit_scale(letter, 2.0), 2.0);
        // A 2 × 3 m poster: drawn under the pixel cap, not refused.
        let poster = PageSize { width: 5669.0, height: 8504.0 };
        let s = fit_scale(poster, 1.0);
        let (w, h) = raster_size(poster, s);
        assert!(s > 0.0 && s < 1.0 && u64::from(w) * u64::from(h) <= MAX_PAGE_PIXELS, "{s} {w}×{h}");
        // Junk page sizes give a positive, finite scale, never NaN.
        for size in [PageSize { width: 0.0, height: 0.0 }, PageSize { width: f32::NAN, height: 1.0 }, PageSize { width: f32::INFINITY, height: 1.0 }]
        {
            let s = fit_scale(size, 1.0);
            assert!(s.is_finite() && s > 0.0, "{size:?} → {s}");
        }
    }

    /// `decoded` (a page PNG, decoded) against what the core drew: exactly the same pixels when
    /// the encoder stored them exactly (grey, or 256 colours at most); otherwise exactly the core's
    /// pixels with the low bits the encoder says it dropped, so no channel is off by more than
    /// [`crate::png_out::Kind::max_error`].
    fn assert_same_page(decoded: &[u8], core: &wordcraft_viewer::PageImage, what: &str) {
        let (_, kind) = crate::png_out::encode(core).unwrap();
        match kind {
            crate::png_out::Kind::ReducedPalette { bits } => {
                assert!(decoded == crate::png_out::reduce(&core.pixels, bits).as_slice(), "{what}: not the core's pixels reduced by {bits} bits");
                let worst = decoded.iter().zip(&core.pixels).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
                assert!(worst <= kind.max_error(), "{what}: off by {worst}");
                eprintln!("{what}: {kind:?}, every channel within {worst} of the core's pixels");
            }
            _ => assert!(decoded == core.pixels.as_slice(), "{what}: {kind:?} is not the core's pixels"),
        }
    }

    #[test]
    fn a_page_png_decodes_to_the_rgba_the_core_draws() {
        fonts();
        let s = open_document(&long_docx(), None).unwrap();
        for (index, scale) in [(0u32, 1.0f32), (1, 1.25), (0, 0.5)] {
            let (png, w, h) = page_png(&s, index, scale).unwrap();
            let core = s.render(index, scale).unwrap();
            assert_eq!((w, h), (core.width, core.height));
            assert_eq!((w, h), raster_size(s.page_size(index).unwrap(), scale));
            let decoded = crate::png_out::tests::decode_rgba(&png);
            assert_eq!(decoded.0, (w, h));
            assert_same_page(&decoded.1, &core, &format!("page {index} at {scale}"));
        }
        // Out of range.
        let pages = u32::try_from(s.page_count()).unwrap();
        assert!(matches!(s.page_size(pages), Err(ViewerError::NoSuchPage { .. })));
        assert!(page_png(&s, pages, 1.0).is_err());
    }

    /// The deployed sample, when `dist/web` has been built (`cargo xtask web-assets` or
    /// `packaging/web/package.sh --static`): page 1 at the default scale decodes to the core's
    /// pixels, and the PNG is the size the README reports.
    #[test]
    fn the_deployed_sample_renders_when_present() {
        fonts();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../dist/web/samples/welcome.docx");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("skipped: {} is not built", path.display());
            return;
        };
        let s = open_document(&bytes, None).unwrap();
        let scale = crate::limits::DEFAULT_SCALE;
        let (png, w, h) = page_png(&s, 0, scale).unwrap();
        let core = s.render(0, scale).unwrap();
        let decoded = crate::png_out::tests::decode_rgba(&png);
        assert_eq!(decoded.0, (w, h));
        assert_same_page(&decoded.1, &core, "welcome.docx page 1");
        eprintln!("welcome.docx: {} pages, page 1 {w}×{h} at {scale}: {} PNG bytes", s.page_count(), png.len());
    }

    /// Inputs for `smoke.sh`'s optional checks: a long document and a protected one (password
    /// `s3cret`), written to `$FIXTURES`. Run with
    /// `FIXTURES=dir cargo test -- --ignored write_fixtures`.
    #[test]
    #[ignore = "writes files for the black-box script"]
    fn write_fixtures() {
        let dir = std::path::PathBuf::from(std::env::var("FIXTURES").unwrap());
        let paras: Vec<String> = (1..=800).map(|i| format!("Paragraph {i} of a document that runs over many pages.")).collect();
        std::fs::write(dir.join("long.docx"), docx(&paras.iter().map(String::as_str).collect::<Vec<_>>())).unwrap();
        std::fs::write(dir.join("locked.docx"), craft_offcrypto::encrypt(&docx(&["Secret text."]), "s3cret").unwrap()).unwrap();
    }

    /// The README's size table: page 1 of the deployed sample at several scales, as written here
    /// and as plain RGBA through the same encoder. Run with
    /// `cargo test --release -- --ignored --nocapture measure`.
    #[test]
    #[ignore = "a measurement, not a check; needs dist/web"]
    fn measure_sample_png_sizes() {
        fonts();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../dist/web/samples/welcome.docx");
        let s = open_document(&std::fs::read(&path).unwrap(), None).unwrap();
        for scale in [0.75f32, 1.0, 1.25, 1.5, 2.0] {
            let img = s.render(0, scale).unwrap();
            let t = std::time::Instant::now();
            let (png, kind) = crate::png_out::encode(&img).unwrap();
            let took = t.elapsed();
            let rgba = {
                let mut out = Vec::new();
                let mut e = png::Encoder::new(&mut out, img.width, img.height);
                e.set_color(png::ColorType::Rgba);
                e.set_depth(png::BitDepth::Eight);
                e.write_header().unwrap().write_image_data(&img.pixels).unwrap();
                out
            };
            let colours: std::collections::HashSet<&[u8; 4]> = img.pixels.as_chunks::<4>().0.iter().collect();
            if let Ok(dir) = std::env::var("MEASURE_OUT") {
                std::fs::write(format!("{dir}/page1-{scale}.png"), &png).unwrap();
            }
            for (label, compression) in [("Balanced", png::Compression::Balanced), ("Fast", png::Compression::Fast)] {
                let rgb: Vec<u8> = img.pixels.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
                let t = std::time::Instant::now();
                let mut out = Vec::new();
                let mut e = png::Encoder::new(&mut out, img.width, img.height);
                e.set_color(png::ColorType::Rgb);
                e.set_depth(png::BitDepth::Eight);
                e.set_compression(compression);
                e.write_header().unwrap().write_image_data(&rgb).unwrap();
                eprintln!("  RGB {label}: {} bytes ({:?})", out.len(), t.elapsed());
            }
            // Lossy: drop low bits of each channel until 256 colours or fewer remain.
            for shift in 1..=4u8 {
                let reduced: Vec<u8> = img.pixels.iter().map(|&c| if c == 255 { 255 } else { (c >> shift) << shift }).collect();
                let n: std::collections::HashSet<&[u8; 4]> = reduced.as_chunks::<4>().0.iter().collect();
                let r = PageImage { width: img.width, height: img.height, pixels: reduced.clone() };
                let (p, k) = crate::png_out::encode(&r).unwrap();
                eprintln!("  lossy >>{shift}: {} colours, {k:?} {} bytes", n.len(), p.len());
                if let Ok(dir) = std::env::var("MEASURE_OUT") {
                    std::fs::write(format!("{dir}/page1-{scale}-lossy{shift}.png"), &p).unwrap();
                }
            }
            eprintln!(
                "scale {scale}: {}×{} {kind:?} {} bytes ({took:?}); RGBA default {} bytes; {} colours",
                img.width,
                img.height,
                png.len(),
                rgba.len(),
                colours.len()
            );
        }
    }
}
