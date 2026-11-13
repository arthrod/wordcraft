//! `fonts/manifest.json` parsing and face matching, kept free of `web_sys`/`wasm_bindgen` so the
//! checks run in `cargo test` on the host. The fetch/install side is `web::font_packs` (wasm only).

/// Largest font file a pack entry may decode to; mirrors `wordcraft_fonts::MAX_FONT_FILE_BYTES`
/// (this module is compiled on the host for tests, where that crate is not linked).
const MAX_PACK_BYTES: u64 = 50 << 20;

/// Most entries a manifest may list (there are nine pack faces today).
const MAX_MANIFEST_ENTRIES: usize = 64;

/// One face of `fonts/manifest.json` (staged by `cargo xtask web-assets`).
#[derive(Clone, Debug)]
pub(crate) struct ManifestEntry {
    pub family: String,
    pub weight: u16,
    pub italic: bool,
    /// `fonts/<file>`, e.g. `SourceSans3-Bold.9f27c1d0.ttf.br`.
    pub file: String,
    /// Decoded TTF size; the decode is capped at this and must match it exactly.
    pub bytes: usize,
}

/// Parse `fonts/manifest.json` into bounded entries, dropping any face whose fields are missing,
/// unreasonable, or whose file name could leave `fonts/` (`file` is appended to `assetBase`).
pub(crate) fn parse_manifest(text: &str) -> Option<Vec<ManifestEntry>> {
    let json: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut out = Vec::new();
    for e in json.get("faces")?.as_array()?.iter().take(MAX_MANIFEST_ENTRIES) {
        let family = e.get("family")?.as_str()?;
        let file = e.get("file")?.as_str()?;
        if family.is_empty()
            || family.len() > 256
            || file.is_empty()
            || file.len() > 128
            || !file.ends_with(".ttf.br")
            || file.chars().any(|c| matches!(c, '/' | '\\' | '?' | '#' | '%') || c.is_control())
        {
            continue;
        }
        let weight = e.get("weight")?.as_u64()?;
        let bytes = e.get("bytes")?.as_u64()?;
        if bytes == 0 || bytes > MAX_PACK_BYTES {
            continue;
        }
        out.push(ManifestEntry {
            family: family.to_string(),
            weight: u16::try_from(weight).ok()?,
            italic: e.get("italic")?.as_bool()?,
            file: file.to_string(),
            bytes: usize::try_from(bytes).ok()?,
        });
    }
    Some(out)
}

/// The manifest entry best matching `(family, bold, italic)` — the same rule
/// `wordcraft_fonts`'s `packs::pick_pack_face` applies: `italic` must match exactly (a roman face
/// can't satisfy an italic want), a bold want needs a face at least semibold (a lighter face
/// would still fake the bold — bold italic sans has no pack face and stays synthesized), then
/// closest weight to the target (`700` bold, else `400`), ties to the heavier weight.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn pick_entry<'a>(manifest: &'a [ManifestEntry], family: &str, bold: bool, italic: bool) -> Option<&'a ManifestEntry> {
    let target: i32 = if bold { 700 } else { 400 };
    manifest
        .iter()
        .filter(|e| e.family.eq_ignore_ascii_case(family) && e.italic == italic && if bold { e.weight >= 600 } else { e.weight <= 500 })
        .min_by(|a, b| {
            let da = (i32::from(a.weight) - target).abs();
            let db = (i32::from(b.weight) - target).abs();
            da.cmp(&db).then(b.weight.cmp(&a.weight))
        })
}

/// Every manifest face of `family` (manual installs fetch the family's whole pack set).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn entries_for_family<'a>(manifest: &'a [ManifestEntry], family: &str) -> Vec<&'a ManifestEntry> {
    manifest.iter().filter(|e| e.family.eq_ignore_ascii_case(family)).collect()
}

/// The (weight, italic) bucket an installed face's style name means, used to tell whether the
/// face a manifest entry names is already loaded. Style names come from the font's `name` table
/// ("Regular", "Italic", "Semibold", "Bold Italic", …).
pub(crate) fn style_bucket(style: &str) -> (u16, bool) {
    let s = style.to_ascii_lowercase();
    let italic = s.contains("italic") || s.split_whitespace().any(|w| w == "it");
    let weight = if s.contains("semibold") || s.contains("demibold") {
        600
    } else if s.contains("bold") || s.contains("black") || s.contains("heavy") {
        700
    } else if s.contains("medium") {
        500
    } else if s.contains("light") {
        300
    } else {
        400
    };
    (weight, italic)
}

/// Does the style name of an installed face match the face `entry` names?
pub(crate) fn style_matches(style: &str, entry: &ManifestEntry) -> bool {
    let (weight, italic) = style_bucket(style);
    weight == entry.weight && italic == entry.italic
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parsing_is_bounded_and_filters_junk() {
        let good = parse_manifest(
            r#"{"faces":[{"family":"Inter","weight":600,"italic":false,"file":"Inter-SemiBold.abcd1234.ttf.br","bytes":1234,"compressedBytes":600}]}"#,
        )
        .unwrap();
        assert_eq!(good.len(), 1);
        assert_eq!(good[0].file, "Inter-SemiBold.abcd1234.ttf.br");
        assert_eq!(good[0].bytes, 1234);

        // A path or query in `file`, missing fields, absurd sizes, or a non-.br file is dropped;
        // `compressedBytes` is informational — the decode contract is `bytes` — so it isn't read.
        let junk = parse_manifest(
            r#"{"faces":[
              {"family":"X","weight":400,"italic":false,"file":"../index.html","bytes":10,"compressedBytes":1},
              {"family":"Y","weight":400,"italic":false,"file":"a.ttf.br?x=1","bytes":10,"compressedBytes":1},
              {"family":"Z","weight":400,"italic":false,"file":"z.ttf.br","bytes":999999999999,"compressedBytes":1},
              {"family":"W","weight":400,"italic":false,"file":"w.ttf.br"},
              {"family":"V","weight":400,"italic":false,"file":"v.ttf","bytes":10,"compressedBytes":1},
              {"family":"U","weight":400,"italic":false,"file":"u.ttf.br","bytes":0,"compressedBytes":1}
            ]}"#,
        )
        .unwrap();
        assert!(junk.is_empty(), "{junk:?}");

        assert!(parse_manifest("not json").is_none());
        assert!(parse_manifest("{}").is_none());
        // Over-long manifests are cut at the cap rather than read whole.
        let big = format!(
            "{{\"faces\":[{}]}}",
            std::iter::repeat_n(
                r#"{"family":"F","weight":400,"italic":false,"file":"f.ttf.br","bytes":1,"compressedBytes":1}"#,
                MAX_MANIFEST_ENTRIES + 10
            )
            .collect::<Vec<_>>()
            .join(",")
        );
        assert_eq!(parse_manifest(&big).unwrap().len(), MAX_MANIFEST_ENTRIES);
    }

    #[test]
    fn pick_entry_matches_family_weight_italic() {
        let faces = parse_manifest(
            r#"{"faces":[
              {"family":"Inter","weight":600,"italic":false,"file":"inter-sb.ttf.br","bytes":1,"compressedBytes":1},
              {"family":"JetBrains Mono","weight":400,"italic":false,"file":"jb.ttf.br","bytes":1,"compressedBytes":1},
              {"family":"Source Sans 3","weight":700,"italic":false,"file":"ss3-b.ttf.br","bytes":1,"compressedBytes":1},
              {"family":"Source Sans 3","weight":400,"italic":true,"file":"ss3-i.ttf.br","bytes":1,"compressedBytes":1},
              {"family":"Source Sans 3","weight":600,"italic":false,"file":"ss3-sb.ttf.br","bytes":1,"compressedBytes":1},
              {"family":"Source Serif 4","weight":700,"italic":false,"file":"sf4-b.ttf.br","bytes":1,"compressedBytes":1},
              {"family":"Source Serif 4","weight":700,"italic":true,"file":"sf4-bi.ttf.br","bytes":1,"compressedBytes":1},
              {"family":"Source Serif 4","weight":400,"italic":true,"file":"sf4-i.ttf.br","bytes":1,"compressedBytes":1},
              {"family":"Source Serif 4","weight":600,"italic":false,"file":"sf4-sb.ttf.br","bytes":1,"compressedBytes":1}
            ]}"#,
        )
        .unwrap();
        // The acceptance case: Calibri bold substitutes to Source Sans 3 → SourceSans3-Bold.
        assert_eq!(pick_entry(&faces, "Source Sans 3", true, false).map(|e| e.file.as_str()), Some("ss3-b.ttf.br"));
        assert_eq!(pick_entry(&faces, "Inter", true, false).map(|e| e.file.as_str()), Some("inter-sb.ttf.br"));
        assert_eq!(pick_entry(&faces, "Source Serif 4", true, true).map(|e| e.file.as_str()), Some("sf4-bi.ttf.br"));
        assert_eq!(pick_entry(&faces, "Source Serif 4", false, true).map(|e| e.file.as_str()), Some("sf4-i.ttf.br"));
        assert_eq!(pick_entry(&faces, "JetBrains Mono", false, false).map(|e| e.file.as_str()), Some("jb.ttf.br"));
        // Bold italic sans has no pack face: it stays synthesized.
        assert!(pick_entry(&faces, "Source Sans 3", true, true).is_none());
        assert!(pick_entry(&faces, "Inter", false, true).is_none());
        // Case-insensitive family.
        assert_eq!(pick_entry(&faces, "jetbrains mono", false, false).map(|e| e.file.as_str()), Some("jb.ttf.br"));
    }

    #[test]
    fn style_bucket_reads_standard_style_names() {
        assert_eq!(style_bucket("Regular"), (400, false));
        assert_eq!(style_bucket("Italic"), (400, true));
        assert_eq!(style_bucket("Bold"), (700, false));
        assert_eq!(style_bucket("Bold Italic"), (700, true));
        assert_eq!(style_bucket("Semibold"), (600, false));
        assert_eq!(style_bucket("Medium"), (500, false));
        let entry = ManifestEntry { family: "F".into(), weight: 700, italic: false, file: "f.ttf.br".into(), bytes: 1 };
        assert!(style_matches("Bold", &entry));
        assert!(!style_matches("Regular", &entry));
        assert!(!style_matches("Bold Italic", &entry));
    }
}
