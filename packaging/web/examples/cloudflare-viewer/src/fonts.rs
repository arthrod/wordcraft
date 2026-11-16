//! Font packs from the deployment's `fonts/` folder.
//!
//! The Worker carries the four faces the minimal wasm build bundles. The site also ships nine
//! more as Brotli-compressed packs (`fonts/<stem>.<hash8>.ttf.br`, listed in
//! `fonts/manifest.json`). When laying out a document asks for one of their families, the Worker
//! reads the matching pack through `ASSETS` (never a URL from the request), decodes it with the
//! decoder the font crate already links ([`wordcraft_fonts::decode_pack_bytes`]) and installs it
//! for the isolate's lifetime; the document is then laid out again. This module is the pure half:
//! reading the manifest, and choosing which files a document's wants need.

use std::collections::BTreeSet;

use serde::Deserialize;
use wordcraft_fonts::WantedPackFace;
use wordcraft_fonts::packs::PackFaceLike;

use crate::limits::{MAX_MANIFEST_BYTES, MAX_MANIFEST_FACES};

/// One face in `fonts/manifest.json`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Face {
    pub family: String,
    pub weight: u16,
    pub italic: bool,
    /// The pack file's name in `fonts/`, checked by [`parse_manifest`].
    pub file: String,
    /// Its size decoded, which [`wordcraft_fonts::decode_pack_bytes`] holds the stream to.
    pub bytes: usize,
    /// Its size as shipped.
    #[serde(rename = "compressedBytes")]
    pub compressed_bytes: usize,
}

impl PackFaceLike for Face {
    fn family(&self) -> &str {
        &self.family
    }
    fn weight(&self) -> u16 {
        self.weight
    }
    fn italic(&self) -> bool {
        self.italic
    }
}

/// Read `fonts/manifest.json`. Bounded: at most [`MAX_MANIFEST_BYTES`] and
/// [`MAX_MANIFEST_FACES`]; a face whose file name isn't a plain `*.ttf.br` name in the folder, or
/// whose sizes are past the font cap, is dropped.
pub fn parse_manifest(json: &[u8]) -> Result<Vec<Face>, String> {
    #[derive(Deserialize)]
    struct Manifest {
        faces: Vec<serde_json::Value>,
    }
    if json.len() > MAX_MANIFEST_BYTES {
        return Err(format!("the font manifest is over {MAX_MANIFEST_BYTES} bytes"));
    }
    let manifest: Manifest = serde_json::from_slice(json).map_err(|e| format!("the font manifest is not valid: {e}"))?;
    let limit = wordcraft_fonts::MAX_FONT_FILE_BYTES;
    Ok(manifest
        .faces
        .into_iter()
        .filter_map(|v| serde_json::from_value::<Face>(v).ok())
        .filter(|f| plain_pack_name(&f.file) && (1..=limit).contains(&f.bytes) && (1..=limit).contains(&f.compressed_bytes))
        .take(MAX_MANIFEST_FACES)
        .collect())
}

/// A file name in `fonts/` itself: letters, digits, `.`, `_`, `-`, not starting with a dot,
/// ending `.ttf.br`. No path, no scheme, no escape.
fn plain_pack_name(file: &str) -> bool {
    file.len() <= 128
        && !file.starts_with('.')
        && file.ends_with(".ttf.br")
        && file.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The manifest entries (indices into `faces`) to fetch for `wanted`, each once, skipping files
/// already `tried`.
pub fn files_for(faces: &[Face], wanted: &[WantedPackFace], tried: &BTreeSet<String>) -> Vec<usize> {
    let mut picked: Vec<usize> = Vec::new();
    for w in wanted {
        if let Some(i) = wordcraft_fonts::pick_pack_face(faces, w)
            && !picked.contains(&i)
            && faces.get(i).is_some_and(|f| !tried.contains(&f.file))
        {
            picked.push(i);
        }
    }
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{"faces":[
        {"bytes":419744,"compressedBytes":161435,"family":"Inter","file":"Inter-SemiBold.78a843fa.ttf.br","italic":false,"weight":600},
        {"bytes":428176,"compressedBytes":155108,"family":"Source Sans 3","file":"SourceSans3-Bold.9214b9d9.ttf.br","italic":false,"weight":700},
        {"bytes":315812,"compressedBytes":126115,"family":"Source Sans 3","file":"SourceSans3-It.192afd78.ttf.br","italic":true,"weight":400}
    ]}"#;

    #[test]
    fn reads_the_manifest() {
        let faces = parse_manifest(MANIFEST.as_bytes()).unwrap();
        assert_eq!(faces.len(), 3);
        assert_eq!(faces[1].file, "SourceSans3-Bold.9214b9d9.ttf.br");
        assert_eq!(faces[1].bytes, 428_176);
    }

    #[test]
    fn hostile_manifests_are_bounded() {
        assert!(parse_manifest(b"not json").is_err());
        assert!(parse_manifest(&vec![b' '; MAX_MANIFEST_BYTES + 1]).is_err());
        let face = |file: &str, bytes: usize| {
            format!(r#"{{"bytes":{bytes},"compressedBytes":10,"family":"Inter","file":"{file}","italic":false,"weight":600}}"#)
        };
        // Paths, URLs and odd names are dropped; so are impossible sizes.
        let json = format!(
            r#"{{"faces":[{},{},{},{},{},{},{}]}}"#,
            face("../secret.ttf.br", 10),
            face("https://evil.example/x.ttf.br", 10),
            face("sub/x.ttf.br", 10),
            face("x.ttf", 10),
            face(r"x\\y.ttf.br", 10), // a backslash, escaped for JSON
            face("ok.ttf.br", usize::MAX),
            face("ok.ttf.br", 10),
        );
        let faces = parse_manifest(json.as_bytes()).unwrap();
        assert_eq!(faces.iter().map(|f| f.file.as_str()).collect::<Vec<_>>(), ["ok.ttf.br"]);
        // Too many faces: only the first MAX_MANIFEST_FACES are kept.
        let many: Vec<String> = (0..MAX_MANIFEST_FACES + 5).map(|i| face(&format!("f{i}.ttf.br"), 10)).collect();
        let json = format!(r#"{{"faces":[{}]}}"#, many.join(","));
        assert_eq!(parse_manifest(json.as_bytes()).unwrap().len(), MAX_MANIFEST_FACES);
    }

    #[test]
    fn picks_each_needed_file_once() {
        let faces = parse_manifest(MANIFEST.as_bytes()).unwrap();
        let want = |family: &str, bold: bool, italic: bool| WantedPackFace { family: family.to_string(), bold, italic, available: true };
        let wanted = [
            want("Source Sans 3", true, false),
            want("Source Sans 3", true, false),
            want("Source Sans 3", false, true),
            want("Calibri", false, false),
        ];
        assert_eq!(files_for(&faces, &wanted, &BTreeSet::new()), [1, 2]);
        let tried: BTreeSet<String> = ["SourceSans3-Bold.9214b9d9.ttf.br".to_string()].into();
        assert_eq!(files_for(&faces, &wanted, &tried), [2]);
        assert!(files_for(&faces, &[], &BTreeSet::new()).is_empty());
    }
}
