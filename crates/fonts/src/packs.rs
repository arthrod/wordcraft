//! Downloadable font packs: the faces the minimal web build doesn't bundle.
//!
//! `minimal-wasm-fonts` builds embed only four faces (`CORE_WASM_FONTS` in `build.rs`) to stay
//! inside the web size budget; the other nine ship as Brotli-compressed pack files the browser
//! downloads on demand. Resolution notes which pack faces a document would use
//! ([`FontDb::take_wanted_packs`]); the host decodes a fetched file with [`decode_pack_bytes`]
//! and installs it with [`FontDb::install_pack_font`], which bumps the font generation so cached
//! resolutions re-run and the text re-layouts in the real face.

use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};

use crate::fontdb::{FontDb, MAX_FONT_FILE_BYTES, UserFontError, decode_font_file};

/// A face shipped as a downloadable pack rather than bundled. The stem is the file name in
/// `assets/fonts/`; the shipped pack is `<stem>.<hash8>.ttf.br` with an entry in
/// `fonts/manifest.json`, both written by `cargo xtask web-assets`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackFace {
    /// Typographic family the face reports ("Source Sans 3").
    pub family: &'static str,
    /// usWeightClass: 400 Regular, 600 Semibold, 700 Bold.
    pub weight: u16,
    pub italic: bool,
    /// File stem ("SourceSans3-Bold").
    pub stem: &'static str,
}

/// What [`pick_pack_face`] reads from a face: this crate's own [`PackFace`] implements it, and
/// so can the host's manifest type, so the web shell never has to leak its owned family names
/// into `&'static str`s just to match against them.
pub trait PackFaceLike {
    fn family(&self) -> &str;
    fn weight(&self) -> u16;
    fn italic(&self) -> bool;
}

impl PackFaceLike for PackFace {
    fn family(&self) -> &str {
        self.family
    }
    fn weight(&self) -> u16 {
        self.weight
    }
    fn italic(&self) -> bool {
        self.italic
    }
}

/// The faces that ship as packs: everything in `assets/fonts/` beyond the four `CORE_WASM_FONTS`
/// embedded by `crates/fonts/build.rs`. This table must agree with that list (the bundled four)
/// and with `PACK_FACES` in `xtask/src/web_assets.rs`, which writes the `fonts/manifest.json`
/// the host reads — a face here but not there never downloads, and one there but not here
/// installs under a name resolution never asks for. There is no Source Sans 3 Bold Italic face,
/// so bold italic sans stays synthesized (see [`pick_pack_face`]).
pub const PACK_FACES: &[PackFace] = &[
    PackFace { family: "Inter", weight: 600, italic: false, stem: "Inter-SemiBold" },
    PackFace { family: "JetBrains Mono", weight: 400, italic: false, stem: "JetBrainsMono-Regular" },
    PackFace { family: "Source Sans 3", weight: 700, italic: false, stem: "SourceSans3-Bold" },
    PackFace { family: "Source Sans 3", weight: 400, italic: true, stem: "SourceSans3-It" },
    PackFace { family: "Source Sans 3", weight: 600, italic: false, stem: "SourceSans3-Semibold" },
    PackFace { family: "Source Serif 4", weight: 700, italic: false, stem: "SourceSerif4-Bold" },
    PackFace { family: "Source Serif 4", weight: 700, italic: true, stem: "SourceSerif4-BoldIt" },
    PackFace { family: "Source Serif 4", weight: 400, italic: true, stem: "SourceSerif4-It" },
    PackFace { family: "Source Serif 4", weight: 600, italic: false, stem: "SourceSerif4-Semibold" },
];

/// `family`'s pack family, in [`PACK_FACES`]'s own spelling (case-insensitive): the canonical
/// name the wanted record keeps, so a document's "SOURCE SANS 3" and "Source Sans 3" are one
/// want rather than two entries a fetch would serve twice.
fn pack_family(family: &str) -> Option<&'static str> {
    PACK_FACES.iter().find(|f| f.family.eq_ignore_ascii_case(family)).map(|f| f.family)
}

/// A pack face a document would use if it were loaded. Resolution records these
/// (`word::resolve`) and the host drains them once per frame after layout
/// ([`FontDb::take_wanted_packs`]) to decide which `fonts/manifest.json` entry to fetch
/// ([`pick_pack_face`]).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WantedPackFace {
    /// A [`PACK_FACES`] family, in the table's spelling (bounded by it: the names are short, so
    /// no length cap is needed on top).
    pub family: String,
    pub bold: bool,
    pub italic: bool,
}

/// Most wants remembered between the host's polls. The real packs offer 9 faces × 4
/// (bold, italic) combinations; the bound keeps a hostile document (families resolve through
/// this) from growing the record without end — new wants past it are dropped, and draining
/// empties it every frame anyway.
const MAX_WANTED_PACKS: usize = 128;

fn wanted_record() -> &'static Mutex<BTreeSet<WantedPackFace>> {
    static WANTED: OnceLock<Mutex<BTreeSet<WantedPackFace>>> = OnceLock::new();
    WANTED.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Remember a want, applying the caps: the only writer to the record.
fn record_wanted(w: WantedPackFace) {
    let mut set = wanted_record().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if set.len() >= MAX_WANTED_PACKS && !set.contains(&w) {
        return;
    }
    set.insert(w);
}

/// Record that a run asked for `family` as bold/italic, if `family` is a pack family
/// (case-insensitive; anything else is ignored). Called from resolution — see the note helper
/// in `word::resolve` for the two rules that fire it.
pub(crate) fn note_wanted(family: &str, bold: bool, italic: bool) {
    if let Some(family) = pack_family(family) {
        record_wanted(WantedPackFace { family: family.to_string(), bold, italic });
    }
}

/// The pack faces installed in one [`FontDb`]: (face id, family, style), in install order. Pack
/// faces are checked and added like user fonts but are a separate origin (see
/// [`FontDb::install_pack_font`]): this record is the only place they are kept, so they never
/// persist, count against the user font cap, or answer to [`FontDb::remove_user_font`].
#[derive(Default)]
pub struct PackFonts {
    /// At most [`MAX_INSTALLED_PACK_FACES`]: the real packs are nine faces, and the bound keeps
    /// a host that installs arbitrary files from growing the record without end.
    installed: Vec<(u32, String, String)>,
}

/// Most pack faces one database records (see [`PackFonts`]).
const MAX_INSTALLED_PACK_FACES: usize = 1024;

impl FontDb {
    /// Install a font file the host downloaded as a pack (see [`PACK_FACES`]): checked and added
    /// like [`FontDb::install_font`], but a separate origin — not kept in the user font list or
    /// folder, not counted against the user font cap, not removable ([`FontDb::remove_user_font`]
    /// refuses it) and not listed by [`FontDb::user_fonts`]. Returns the faces added as
    /// (family, style). `name` names the pack in diagnostics only; it is never stored.
    pub fn install_pack_font(&self, name: &str, bytes: &[u8]) -> Result<Vec<(String, String)>, UserFontError> {
        let added = self.add_pack_faces(decode_font_file(bytes)?)?;
        {
            let mut packs = self.write_packs();
            if packs.installed.len() < MAX_INSTALLED_PACK_FACES {
                packs.installed.extend(added.iter().map(|f| (f.id(), f.family.clone(), f.style.clone())));
            }
        }
        log::debug!("installed the font pack {name:?}: {} face(s)", added.len());
        Ok(added.iter().map(|f| (f.family.clone(), f.style.clone())).collect())
    }

    /// The faces installed from font packs ([`FontDb::install_pack_font`]) as (family, style),
    /// in the order they were added.
    pub fn pack_fonts(&self) -> Vec<(String, String)> {
        self.read_packs().installed.iter().map(|(_, family, style)| (family.clone(), style.clone())).collect()
    }

    /// The pack faces resolution wants, draining the record. The host polls this once per frame
    /// after layout, fetches each want ([`pick_pack_face`] finds the manifest's face for it) and
    /// installs it with [`FontDb::install_pack_font`]. Draining is correct rather than lossy:
    /// installing bumps the font generation, so everything still wanted is resolved — and
    /// recorded — again.
    pub fn take_wanted_packs(&self) -> Vec<WantedPackFace> {
        std::mem::take(&mut *wanted_record().lock().unwrap_or_else(std::sync::PoisonError::into_inner)).into_iter().collect()
    }
}

/// The pack face to fetch for one want: the index into `faces` of the face that would draw the
/// wanted text without synthesis — same family (case-insensitive), same italic, and bold enough
/// (weight ≥ 600, where `word::resolve` stops synthesizing bold) when bold is wanted — the
/// closest to the wanted weight (700 for bold, else 400) with the heavier face winning ties.
/// `None` when no face of the family qualifies: fetching a face that still synthesizes (bold
/// italic Source Sans 3 has no bold italic face, only a 400-weight italic) would download bytes
/// without drawing the text any truer, so the text keeps its synthesized face instead.
pub fn pick_pack_face(faces: &[impl PackFaceLike], wanted: &WantedPackFace) -> Option<usize> {
    let target: i32 = if wanted.bold { 700 } else { 400 };
    faces
        .iter()
        .enumerate()
        .filter(|(_, f)| f.family().eq_ignore_ascii_case(&wanted.family) && f.italic() == wanted.italic && (!wanted.bold || f.weight() >= 600))
        .min_by(|(_, a), (_, b)| {
            (i32::from(a.weight()) - target).abs().cmp(&(i32::from(b.weight()) - target).abs()).then(b.weight().cmp(&a.weight()))
        })
        .map(|(i, _)| i)
}

/// Why a font pack file couldn't be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackFontError {
    /// The compressed bytes are empty.
    Empty,
    /// The manifest's declared decoded size is impossible: zero, or past the font size cap.
    BadExpectedSize(usize),
    /// The compressed input is past the font size cap ([`MAX_FONT_FILE_BYTES`]).
    TooLarge { bytes: usize },
    /// The Brotli stream is damaged.
    Damaged(String),
    /// The stream decoded to a different number of bytes than the manifest declared
    /// (truncated, oversized or lying).
    SizeMismatch { got: usize, expected: usize },
}

impl std::fmt::Display for PackFontError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackFontError::Empty => write!(f, "the pack file is empty"),
            PackFontError::BadExpectedSize(expected) => {
                write!(f, "the manifest's decoded size is impossible ({expected} bytes; 1 to {MAX_FONT_FILE_BYTES})")
            }
            PackFontError::TooLarge { bytes } => {
                write!(f, "the pack file is too large ({} MB; at most {} MB)", bytes.div_ceil(1 << 20), MAX_FONT_FILE_BYTES >> 20)
            }
            PackFontError::Damaged(e) => write!(f, "the pack file's compression is damaged: {e}"),
            PackFontError::SizeMismatch { got, expected } => {
                write!(f, "the pack file decoded to {got} bytes, not the manifest's {expected}")
            }
        }
    }
}

impl std::error::Error for PackFontError {}

/// Decode a downloaded pack file (`.ttf.br`, Brotli) into the TTF bytes
/// [`FontDb::install_pack_font`] takes. `expected` is the manifest's declared decoded size
/// (`bytes`): both sizes are capped up front, the decode is bounded at one byte past `expected`
/// (mirroring the WOFF2 reader) so an oversized stream can't stream forever, and the result must
/// be exactly `expected` bytes — a truncated, oversized or lying manifest is refused here
/// rather than passed to the font parser.
pub fn decode_pack_bytes(compressed: &[u8], expected: usize) -> Result<Vec<u8>, PackFontError> {
    if compressed.is_empty() {
        return Err(PackFontError::Empty);
    }
    if compressed.len() > MAX_FONT_FILE_BYTES {
        return Err(PackFontError::TooLarge { bytes: compressed.len() });
    }
    if expected == 0 || expected > MAX_FONT_FILE_BYTES {
        return Err(PackFontError::BadExpectedSize(expected));
    }
    use std::io::Read;
    let mut out = Vec::with_capacity(expected);
    match brotli_decompressor::Decompressor::new(compressed, 4096).take(expected as u64 + 1).read_to_end(&mut out) {
        Err(e) => Err(PackFontError::Damaged(e.to_string())),
        Ok(_) if out.len() == expected => Ok(out),
        Ok(_) => Err(PackFontError::SizeMismatch { got: out.len(), expected }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manifest-matching rule the host applies: a face is worth fetching only when it
    /// stops the synthesis it's wanted for.
    #[test]
    fn pick_pack_face_prefers_the_face_that_stops_the_synthesis() {
        let want = |family: &str, bold: bool, italic: bool| WantedPackFace { family: family.to_string(), bold, italic };
        let stem = |w: &WantedPackFace| pick_pack_face(PACK_FACES, w).map(|i| PACK_FACES[i].stem);
        assert_eq!(stem(&want("Source Sans 3", true, false)), Some("SourceSans3-Bold"), "the bold face, not the semibold");
        assert_eq!(stem(&want("Inter", true, false)), Some("Inter-SemiBold"), "Inter's only bold-enough pack face");
        assert_eq!(stem(&want("Source Serif 4", true, true)), Some("SourceSerif4-BoldIt"));
        // No bold italic Source Sans 3 exists: its italic face is 400, too light to stop a bold
        // synthesis, so nothing is fetched and the text stays synthesized.
        assert_eq!(stem(&want("Source Sans 3", true, true)), None);
        assert_eq!(stem(&want("JetBrains Mono", false, false)), Some("JetBrainsMono-Regular"));
        // Families match case-insensitively; a family with no italic face, or no pack faces,
        // wants nothing.
        assert_eq!(stem(&want("source serif 4", false, true)), Some("SourceSerif4-It"));
        assert_eq!(stem(&want("Inter", false, true)), None);
        assert_eq!(stem(&want("Calibri", true, false)), None);
    }

    #[test]
    fn decode_pack_bytes_round_trips_and_rejects_bad_input() {
        let data = b"the quick brown font".repeat(64);
        // Quality 11, as the shipped packs are compressed.
        let params = brotli::enc::BrotliEncoderParams { quality: 11, lgwin: 22, ..Default::default() };
        let mut packed = Vec::new();
        brotli::BrotliCompress(&mut data.as_slice(), &mut packed, &params).unwrap();
        assert_eq!(decode_pack_bytes(&packed, data.len()).unwrap(), data);
        // A lying manifest, in either direction.
        for expected in [data.len() - 1, data.len() + 1] {
            assert_eq!(decode_pack_bytes(&packed, expected), Err(PackFontError::SizeMismatch { got: data.len(), expected }));
        }
        // A truncated stream never decodes to the declared size.
        assert!(decode_pack_bytes(&packed[..packed.len() / 2], data.len()).is_err());
        // Sizes are refused before anything is decoded or allocated.
        assert_eq!(decode_pack_bytes(&[], 8), Err(PackFontError::Empty));
        assert_eq!(decode_pack_bytes(&[1, 2, 3], 0), Err(PackFontError::BadExpectedSize(0)));
        let huge = MAX_FONT_FILE_BYTES + 1;
        assert_eq!(decode_pack_bytes(&[1, 2, 3], huge), Err(PackFontError::BadExpectedSize(huge)));
        let big = vec![0u8; MAX_FONT_FILE_BYTES + 1];
        assert_eq!(decode_pack_bytes(&big, 8), Err(PackFontError::TooLarge { bytes: big.len() }));
    }

    /// The record drains, keeps only pack families (once per case variant), and stays bounded.
    /// It drives the note helpers directly: the native test build bundles all 13 faces, so
    /// substitution-triggered wants never fire there, and the synth-triggered ones that still
    /// can (JetBrains Mono has no bold anywhere) pick no downloadable face — nothing to assert
    /// resolution-side without depending on a machine's installed fonts.
    #[test]
    fn wanted_packs_record_drains_filters_and_caps() {
        FontDb::global().take_wanted_packs();
        note_wanted("Source Serif 4", false, true);
        note_wanted("source sans 3", true, false);
        note_wanted("Source Sans 3", true, false);
        note_wanted("Calibri", true, false);
        note_wanted("JetBrains Mono", false, false);
        let has = |w: &[WantedPackFace], family: &str, bold: bool, italic: bool| {
            w.iter().any(|w| w.family.eq_ignore_ascii_case(family) && w.bold == bold && w.italic == italic)
        };
        let taken = FontDb::global().take_wanted_packs();
        assert!(
            has(&taken, "Source Serif 4", false, true) && has(&taken, "Source Sans 3", true, false) && has(&taken, "JetBrains Mono", false, false)
        );
        assert!(!taken.iter().any(|w| w.family.eq_ignore_ascii_case("Calibri")), "only pack families are recorded");
        assert_eq!(taken.iter().filter(|w| w.family.eq_ignore_ascii_case("Source Sans 3")).count(), 1, "case variants are one want");
        // Draining: the next take doesn't repeat them.
        let again = FontDb::global().take_wanted_packs();
        assert!(!has(&again, "Source Sans 3", true, false) && !has(&again, "JetBrains Mono", false, false));
        // Past the cap, new wants are dropped rather than growing the record.
        for i in 0..(MAX_WANTED_PACKS + 10) {
            record_wanted(WantedPackFace { family: format!("Fam {i}"), bold: i % 2 == 0, italic: i % 3 == 0 });
        }
        assert_eq!(FontDb::global().take_wanted_packs().len(), MAX_WANTED_PACKS);
    }

    /// The pack table is exactly the other side of the bundled set: the web build bundles
    /// `CORE_WASM_FONTS` and packs the rest, native builds bundle all 13. A face in both tables
    /// would be wanted yet always available; a bundled-but-unlisted face would never download.
    #[test]
    fn pack_faces_are_exactly_the_unbundled_ones() {
        for p in PACK_FACES {
            assert_eq!(crate::bundled_by_stem(p.stem).is_some(), !cfg!(feature = "minimal-wasm-fonts"), "{}", p.stem);
        }
        for stem in ["SourceSans3-Regular", "SourceSerif4-Regular", "Inter-Regular", "Inter-Medium"] {
            assert!(!PACK_FACES.iter().any(|p| p.stem == stem), "{stem} is bundled, not a pack");
        }
        if !cfg!(feature = "minimal-wasm-fonts") {
            assert_eq!(crate::bundled().len(), PACK_FACES.len() + 4, "the 13 faces are the four bundled plus the packs");
        }
    }
}
