//! Page images as small PNGs.
//!
//! A printed page is mostly paper and text. When every pixel is opaque and grey (red, green and
//! blue equal), the image is written as 8-bit greyscale, a quarter of the RGBA bytes before
//! compression. Otherwise, when it has at most 256 colours, as a palette image (one byte a pixel,
//! a transparency chunk only when needed). Both are exact.
//!
//! A page with coloured text has more colours than that: anti-aliased edges blend each ink with
//! the paper. Such a page is written as a palette too, after the lowest bits of its red, green and
//! blue are dropped ([`MAX_REDUCE_BITS`] at most, as few as bring it to 256 colours; white stays
//! white). Each channel is then off by at most 2^bits − 1 (15 at worst, 3 for the deployed
//! sample), which is not visible on a text page, and the file is about half the size of exact RGB
//! (measured in the README). Only when even that leaves too many colours (a photograph) is the page
//! written exactly as RGB, or RGBA when not opaque.

use wordcraft_viewer::PageImage;

/// Why a page couldn't be encoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodeError(pub String);

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PNG encoding failed: {}", self.0)
    }
}

/// How a page was stored, for tests and measurements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Grey,
    Palette,
    /// A palette after dropping this many low bits of each colour channel.
    ReducedPalette {
        bits: u8,
    },
    Rgb,
    Rgba,
}

impl Kind {
    /// The largest difference from the drawn pixels in any channel (what the tests hold the
    /// decoded image to).
    #[cfg(test)]
    pub fn max_error(self) -> u8 {
        match self {
            Kind::ReducedPalette { bits } => (1u8 << bits.min(7)) - 1,
            _ => 0,
        }
    }
}

/// Most low bits dropped from a channel before giving up on a palette.
pub const MAX_REDUCE_BITS: u8 = 4;

/// `pixels` with the low `bits` of red, green and blue cleared, full intensity (255) kept so paper
/// stays white; alpha unchanged.
pub fn reduce(pixels: &[u8], bits: u8) -> Vec<u8> {
    #[cfg(test)]
    tests::REDUCTIONS.set(tests::REDUCTIONS.get() + 1);
    let keep = 0xffu8 << bits.min(7);
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| {
            let r = |c: u8| if c == 255 { 255 } else { c & keep };
            [r(p[0]), r(p[1]), r(p[2]), p[3]]
        })
        .collect()
}

/// `img` as PNG bytes, and how it was stored.
pub fn encode(img: &PageImage) -> Result<(Vec<u8>, Kind), EncodeError> {
    let expected = u64::from(img.width).checked_mul(u64::from(img.height)).and_then(|n| n.checked_mul(4));
    if img.width == 0 || img.height == 0 || expected != Some(img.pixels.len() as u64) {
        return Err(EncodeError(format!("{} bytes for {}×{} pixels", img.pixels.len(), img.width, img.height)));
    }
    let px = img.pixels.as_chunks::<4>().0;
    let opaque = px.iter().all(|p| p[3] == 255);
    if opaque && px.iter().all(|p| p[0] == p[1] && p[1] == p[2]) {
        let grey: Vec<u8> = px.iter().map(|p| p[0]).collect();
        return write(img, png::ColorType::Grayscale, &grey, None).map(|b| (b, Kind::Grey));
    }
    if let Some((palette, alpha, indices)) = palette_of(px) {
        let trns = (!opaque).then_some(alpha);
        return write(img, png::ColorType::Indexed, &indices, Some((palette, trns))).map(|b| (b, Kind::Palette));
    }
    // Dropping more bits never adds colours (`reduce` by more bits is `reduce` by fewer, then by
    // more), so when the coarsest drop still leaves more than 256, no smaller one can: one copy of
    // the page decides, instead of one per bit width. Otherwise the smallest width that fits wins.
    if let Some(coarsest) = palette_of(reduce(&img.pixels, MAX_REDUCE_BITS).as_chunks::<4>().0) {
        let (bits, (palette, alpha, indices)) = (1..MAX_REDUCE_BITS)
            .find_map(|bits| palette_of(reduce(&img.pixels, bits).as_chunks::<4>().0).map(|p| (bits, p)))
            .unwrap_or((MAX_REDUCE_BITS, coarsest));
        let trns = (!opaque).then_some(alpha);
        return write(img, png::ColorType::Indexed, &indices, Some((palette, trns))).map(|b| (b, Kind::ReducedPalette { bits }));
    }
    if opaque {
        let rgb: Vec<u8> = px.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
        return write(img, png::ColorType::Rgb, &rgb, None).map(|b| (b, Kind::Rgb));
    }
    write(img, png::ColorType::Rgba, &img.pixels, None).map(|b| (b, Kind::Rgba))
}

/// The palette (RGB triples), its alpha values and each pixel's index, when the image has at
/// most 256 distinct colours; `None` as soon as a 257th appears.
fn palette_of(px: &[[u8; 4]]) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let mut index: std::collections::HashMap<[u8; 4], u8> = std::collections::HashMap::new();
    let mut colours: Vec<[u8; 4]> = Vec::new();
    let mut indices = Vec::with_capacity(px.len());
    for p in px {
        let i = match index.get(p) {
            Some(i) => *i,
            None => {
                let i = u8::try_from(colours.len()).ok()?;
                index.insert(*p, i);
                colours.push(*p);
                i
            }
        };
        indices.push(i);
    }
    let palette = colours.iter().flat_map(|c| [c[0], c[1], c[2]]).collect();
    let alpha = colours.iter().map(|c| c[3]).collect();
    Some((palette, alpha, indices))
}

/// Write one 8-bit image with the strongest compression the encoder offers (`High`, which also
/// picks its filter): page images are drawn once and cached, so bytes matter more than encode time.
fn write(img: &PageImage, colour: png::ColorType, data: &[u8], palette: Option<(Vec<u8>, Option<Vec<u8>>)>) -> Result<Vec<u8>, EncodeError> {
    let fail = |e: png::EncodingError| EncodeError(e.to_string());
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, img.width, img.height);
    encoder.set_color(colour);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::High);
    if let Some((plte, trns)) = palette {
        encoder.set_palette(plte);
        if let Some(t) = trns {
            encoder.set_trns(t);
        }
    }
    encoder.write_header().map_err(fail)?.write_image_data(data).map_err(fail)?;
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    thread_local! {
        /// Calls to [`reduce`] on this thread.
        pub(crate) static REDUCTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// Decode a PNG to straight RGBA, whatever colour type it was written in.
    pub(crate) fn decode_rgba(png: &[u8]) -> ((u32, u32), Vec<u8>) {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        let rgba = match info.color_type {
            png::ColorType::Rgba => buf,
            png::ColorType::Rgb => buf.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
            png::ColorType::GrayscaleAlpha => buf.as_chunks::<2>().0.iter().flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
            png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
            png::ColorType::Indexed => panic!("EXPAND leaves no indexed output"),
        };
        ((info.width, info.height), rgba)
    }

    fn image(width: u32, height: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> PageImage {
        let f = &f;
        let pixels = (0..height).flat_map(|y| (0..width).flat_map(move |x| f(x, y))).collect();
        PageImage { width, height, pixels }
    }

    #[test]
    fn each_kind_round_trips_exactly() {
        let grey = image(37, 11, |x, y| {
            let g = ((x * 7 + y * 13) % 256) as u8;
            [g, g, g, 255]
        });
        let palette = image(37, 11, |x, _| if x % 3 == 0 { [200, 30, 30, 255] } else { [255, 255, 255, 255] });
        let palette_alpha = image(9, 9, |x, y| if (x + y) % 2 == 0 { [0, 0, 255, 128] } else { [255, 255, 255, 255] });
        // Thousands of colours even with four bits dropped: a photograph, kept exact.
        let rgb = image(256, 64, |x, y| [x as u8, (y * 4) as u8, ((x * 7 + y * 13) % 256) as u8, 255]);
        let rgba = image(256, 64, |x, y| [x as u8, (y * 4) as u8, ((x * 7 + y * 13) % 256) as u8, (x / 2) as u8]);
        for (img, want) in [(grey, Kind::Grey), (palette, Kind::Palette), (palette_alpha, Kind::Palette), (rgb, Kind::Rgb), (rgba, Kind::Rgba)] {
            let (png, kind) = encode(&img).unwrap();
            assert_eq!(kind, want);
            assert_eq!(png.get(..8), Some(&b"\x89PNG\r\n\x1a\n"[..]));
            let ((w, h), pixels) = decode_rgba(&png);
            assert_eq!((w, h), (img.width, img.height));
            assert!(pixels == img.pixels, "{want:?} did not round-trip");
        }
    }

    #[test]
    fn coloured_text_is_a_reduced_palette_within_its_error() {
        // Two inks blended into white paper at every coverage: 2 × 255 + 1 colours, too many for
        // an exact palette, few enough once low bits are dropped.
        let page = image(512, 40, |x, y| {
            let a = x % 256;
            let ink: [u32; 3] = if y < 20 { [20, 60, 120] } else { [180, 30, 30] };
            let mix = |c: u32| ((c * a + 255 * (255 - a)) / 255) as u8;
            if x >= 256 { [255, 255, 255, 255] } else { [mix(ink[0]), mix(ink[1]), mix(ink[2]), 255] }
        });
        let (png, kind) = encode(&page).unwrap();
        let Kind::ReducedPalette { bits } = kind else { panic!("{kind:?}") };
        assert!((1..=MAX_REDUCE_BITS).contains(&bits));
        let (_, pixels) = decode_rgba(&png);
        assert_eq!(pixels, reduce(&page.pixels, bits));
        let worst = pixels.iter().zip(&page.pixels).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
        assert!(worst <= kind.max_error(), "{worst} > {}", kind.max_error());
        // Paper stays exactly white.
        assert_eq!(pixels.get(4 * 300..4 * 301), Some(&[255u8, 255, 255, 255][..]));
    }

    #[test]
    fn grey_text_pages_are_much_smaller_than_rgba() {
        // Paper with grey "text" strokes.
        let page = image(612, 792, |x, y| {
            let ink = (y / 14) % 2 == 0 && (x / 5) % 3 != 0 && x > 72 && x < 540;
            let g = if ink { ((x * 37 + y * 11) % 200) as u8 } else { 255 };
            [g, g, g, 255]
        });
        let (grey, kind) = encode(&page).unwrap();
        assert_eq!(kind, Kind::Grey);
        let rgba = {
            let mut out = Vec::new();
            let mut e = png::Encoder::new(&mut out, page.width, page.height);
            e.set_color(png::ColorType::Rgba);
            e.set_depth(png::BitDepth::Eight);
            e.write_header().unwrap().write_image_data(&page.pixels).unwrap();
            out
        };
        assert!(grey.len() * 2 < rgba.len(), "grey {} bytes, RGBA {} bytes", grey.len(), rgba.len());
    }

    /// The cost of the worst pages an upload can ask for, at the pixel cap (2000 × 2000): noise
    /// (incompressible, so its PNG alone passes the response cap) and a smooth gradient with too
    /// many colours for a palette even with four bits dropped (small once compressed, so twelve
    /// fit the response). Run with `cargo test --release -- --ignored --nocapture measure_worst`.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn measure_worst_pages() {
        let side = 2000u32;
        // A hash of the position: noise with no state, as `image` wants.
        let noisy = image(side, side, |x, y| {
            let mut h = x.wrapping_mul(0x9e37_79b9) ^ y.wrapping_mul(0x85eb_ca6b);
            h ^= h >> 15;
            h = h.wrapping_mul(0x2c1b_3c6d);
            h ^= h >> 12;
            let n = h.to_le_bytes();
            [n[0], n[1], n[2], 255]
        });
        let gradient = image(side, side, |x, y| [(x % 256) as u8, (y % 256) as u8, ((x / 8 + y / 8) % 256) as u8, 255]);
        for (what, img) in [("noise", &noisy), ("gradient", &gradient)] {
            for _ in 0..2 {
                let t = std::time::Instant::now();
                let (png, kind) = encode(img).unwrap();
                let total = t.elapsed();
                let t = std::time::Instant::now();
                for bits in 1..=MAX_REDUCE_BITS {
                    let r = reduce(&img.pixels, bits);
                    let _ = palette_of(r.as_chunks::<4>().0).is_some();
                }
                eprintln!("{what}: {kind:?}, {} bytes, encode {total:?} (of which the four reductions {:?})", png.len(), t.elapsed());
            }
        }
    }

    /// A page with too many colours for a palette even with [`MAX_REDUCE_BITS`] dropped is
    /// found out with one reduction, not one per bit width: each is a copy of the whole page
    /// (16 MB at the pixel cap). A page that does reduce gets the same, smallest, bit width.
    #[test]
    fn a_photograph_costs_one_reduction() {
        let photo = image(256, 64, |x, y| [x as u8, (y * 4) as u8, ((x * 7 + y * 13) % 256) as u8, 255]);
        REDUCTIONS.set(0);
        assert_eq!(encode(&photo).unwrap().1, Kind::Rgb);
        assert_eq!(REDUCTIONS.get(), 1);
        // Two inks blended into paper: reduces at the same width whichever pass finds it.
        let text = image(512, 40, |x, y| {
            let a = x % 256;
            let ink: [u32; 3] = if y < 20 { [20, 60, 120] } else { [180, 30, 30] };
            let mix = |c: u32| ((c * a + 255 * (255 - a)) / 255) as u8;
            [mix(ink[0]), mix(ink[1]), mix(ink[2]), 255]
        });
        let smallest = (1..=MAX_REDUCE_BITS).find(|&b| palette_of(reduce(&text.pixels, b).as_chunks::<4>().0).is_some()).unwrap();
        assert_eq!(encode(&text).unwrap().1, Kind::ReducedPalette { bits: smallest });
    }

    #[test]
    fn a_buffer_that_does_not_match_its_size_is_an_error() {
        assert!(encode(&PageImage { width: 10, height: 10, pixels: vec![0; 12] }).is_err());
        assert!(encode(&PageImage { width: 0, height: 10, pixels: Vec::new() }).is_err());
        assert!(encode(&PageImage { width: u32::MAX, height: u32::MAX, pixels: vec![0; 4] }).is_err());
    }
}
