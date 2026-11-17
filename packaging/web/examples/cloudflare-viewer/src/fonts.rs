//! Font packs from the deployment's `fonts/` folder.
//!
//! The Worker carries the four faces the minimal wasm build bundles. The site also ships nine
//! more as Brotli-compressed packs (`fonts/<stem>.<hash8>.ttf.br`, listed in
//! `fonts/manifest.json`). When laying out a document asks for one of their families, the Worker
//! reads the matching pack through `ASSETS` (never a URL from the request), decodes it with the
//! decoder the font crate already links ([`wordcraft_fonts::decode_pack_bytes`]) and installs it
//! for the isolate's lifetime; the document is then laid out again. This module is the half that
//! doesn't touch the runtime: reading the manifest, what the isolate knows about packs across
//! requests ([`PackState`]), and one request's part in bringing them in ([`settle`]).
//!
//! Wants are not a request's own. The font database records a want when a family is resolved, and
//! resolutions are remembered until a font is installed: a second document asking for the same
//! family records nothing. So every want taken from the database goes into the isolate's
//! [`PackState`] and stays there until its pack is installed, and any request settles all of them.
//! A request's layout is final when nothing it could still want is outstanding and no font arrived
//! since it was laid out; only then is its answer cached publicly.

use std::cell::RefCell;
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

/// Most wants kept outstanding. Wants name pack families only (four, each bold or not, italic or
/// not, with or without a face), so a real isolate never gets near it; the cap is a backstop.
const MAX_PENDING_WANTS: usize = 128;

/// What the isolate knows about the site's packs, across requests. On the Worker it lives in a
/// `thread_local!`: a Workers isolate runs its wasm on one thread, so that is one per isolate.
#[derive(Debug, Default)]
pub struct PackState {
    /// Wants taken from the font database and not yet met, whichever request's layout made them.
    pending: BTreeSet<WantedPackFace>,
    /// Pack files installed.
    installed: BTreeSet<String>,
    /// Pack files read whole that can't be installed (not the size the manifest says, not a
    /// font, missing from the deployment): the deployment's, so the same in every isolate and
    /// never tried again. A read that failed is not here: it is tried again by a later request.
    unusable: BTreeSet<String>,
}

impl PackState {
    /// Keep `wants` until they are met.
    pub fn note(&mut self, wants: impl IntoIterator<Item = WantedPackFace>) {
        for w in wants {
            if self.pending.len() < MAX_PENDING_WANTS {
                self.pending.insert(w);
            }
        }
    }

    /// The manifest entries (indices into `faces`, in manifest order) still to read for the
    /// outstanding wants, each once. Wants that are met, or that the manifest can't meet (no face for them, or only an
    /// unusable file), are dropped.
    pub fn outstanding_files(&mut self, faces: &[Face]) -> Vec<usize> {
        let mut picked: Vec<usize> = Vec::new();
        let (installed, unusable) = (&self.installed, &self.unusable);
        self.pending.retain(|w| {
            let Some(i) = wordcraft_fonts::pick_pack_face(faces, w) else { return false };
            let Some(face) = faces.get(i) else { return false };
            if installed.contains(&face.file) || unusable.contains(&face.file) {
                return false;
            }
            if !picked.contains(&i) {
                picked.push(i);
            }
            true
        });
        picked.sort_unstable();
        picked
    }

    pub fn is_installed(&self, file: &str) -> bool {
        self.installed.contains(file)
    }

    pub fn installed(&mut self, file: &str) {
        self.installed.insert(file.to_string());
    }

    pub fn unusable(&mut self, file: &str) {
        self.unusable.insert(file.to_string());
    }

    /// Whether any want is outstanding, before the manifest is known.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// A pack file read from the deployment.
pub enum PackRead {
    Bytes(Vec<u8>),
    /// The deployment doesn't have it, or it is over the font cap: not tried again.
    Unusable,
    /// The store failed: tried again by a later request.
    Failed,
}

/// What [`settle`] needs from the runtime and the request.
#[allow(async_fn_in_trait)] // used in this crate only, on one thread
pub trait PackHost {
    /// Drain the font database's record of wanted pack faces.
    fn take_wants(&mut self) -> Vec<WantedPackFace>;
    /// Read `fonts/<file>` from the deployment.
    async fn read(&mut self, file: &str) -> PackRead;
    /// Decode and install `face`'s pack; true when its faces are available afterwards (also when
    /// another request installed them first).
    fn install(&mut self, face: &Face, packed: &[u8]) -> bool;
    /// The font database's generation, which changes whenever a font is installed.
    fn generation(&self) -> u64;
    /// Lay the request's document out again.
    fn relayout(&mut self);
}

/// Install the packs the isolate's outstanding wants need, then lay the request's document out
/// again when any font arrived (from this request or another) since it was laid out, at
/// generation `laid_out_at`. Bounded by [`crate::limits::MAX_PACK_ROUNDS`]. `state` is borrowed
/// only between awaits. Returns whether the layout is final: every want the manifest can meet is
/// met and the document was laid out after the last font arrived. A pack that failed to read
/// leaves it not final, and is read again by a later request.
pub async fn settle(state: &RefCell<PackState>, faces: &[Face], host: &mut impl PackHost, mut laid_out_at: u64) -> bool {
    for _ in 0..crate::limits::MAX_PACK_ROUNDS {
        let wants = host.take_wants();
        state.borrow_mut().note(wants);
        let picks = state.borrow_mut().outstanding_files(faces);
        for i in picks {
            let Some(face) = faces.get(i) else { continue };
            // Another request may have installed it while this one waited.
            if state.borrow().is_installed(&face.file) {
                continue;
            }
            match host.read(&face.file).await {
                PackRead::Bytes(packed) if host.install(face, &packed) => state.borrow_mut().installed(&face.file),
                PackRead::Bytes(_) | PackRead::Unusable => state.borrow_mut().unusable(&face.file),
                PackRead::Failed => {}
            }
        }
        let now = host.generation();
        if now == laid_out_at {
            break;
        }
        laid_out_at = now;
        host.relayout();
    }
    let wants = host.take_wants();
    state.borrow_mut().note(wants);
    host.generation() == laid_out_at && state.borrow_mut().outstanding_files(faces).is_empty()
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

    fn want(family: &str, bold: bool, italic: bool) -> WantedPackFace {
        WantedPackFace { family: family.to_string(), bold, italic, available: true }
    }

    const BOLD: &str = "SourceSans3-Bold.9214b9d9.ttf.br";
    const ITALIC: &str = "SourceSans3-It.192afd78.ttf.br";

    #[test]
    fn picks_each_needed_file_once() {
        let faces = parse_manifest(MANIFEST.as_bytes()).unwrap();
        let mut state = PackState::default();
        state.note([
            want("Source Sans 3", true, false),
            want("Source Sans 3", true, false),
            want("Source Sans 3", false, true),
            want("Calibri", false, false),
        ]);
        assert_eq!(state.outstanding_files(&faces), [1, 2]);
        // Calibri has no pack: dropped, not outstanding.
        assert_eq!(state.pending.len(), 2);
        state.installed(BOLD);
        assert_eq!(state.outstanding_files(&faces), [2]);
        state.unusable(ITALIC);
        assert!(state.outstanding_files(&faces).is_empty());
        assert!(!state.has_pending());
    }

    /// The font database and the deployment, as [`settle`] sees them, shared by the requests of
    /// one test like an isolate's.
    #[derive(Default)]
    struct Isolate {
        generation: std::cell::Cell<u64>,
        /// Files whose next read fails, once each.
        failing: RefCell<BTreeSet<String>>,
        /// Files read, in order.
        reads: RefCell<Vec<String>>,
        /// Files installed, in order.
        installs: RefCell<Vec<String>>,
    }

    /// One request: its layout's wants, and how many times it was laid out again.
    struct Request<'a> {
        isolate: &'a Isolate,
        wants: Vec<WantedPackFace>,
        relayouts: usize,
        /// Reads that wait once before answering, as a fetch does.
        slow: bool,
    }

    impl<'a> Request<'a> {
        fn new(isolate: &'a Isolate, wants: Vec<WantedPackFace>) -> Self {
            Request { isolate, wants, relayouts: 0, slow: false }
        }
    }

    impl PackHost for Request<'_> {
        fn take_wants(&mut self) -> Vec<WantedPackFace> {
            std::mem::take(&mut self.wants)
        }
        async fn read(&mut self, file: &str) -> PackRead {
            if self.slow {
                YieldOnce(false).await;
            }
            self.isolate.reads.borrow_mut().push(file.to_string());
            if self.isolate.failing.borrow_mut().remove(file) { PackRead::Failed } else { PackRead::Bytes(b"pack".to_vec()) }
        }
        fn install(&mut self, face: &Face, _packed: &[u8]) -> bool {
            let mut installs = self.isolate.installs.borrow_mut();
            if !installs.contains(&face.file) {
                installs.push(face.file.clone());
                self.isolate.generation.set(self.isolate.generation.get() + 1);
            }
            true
        }
        fn generation(&self) -> u64 {
            self.isolate.generation.get()
        }
        fn relayout(&mut self) {
            self.relayouts += 1;
        }
    }

    /// Pending once, then ready: an await that lets another request run.
    struct YieldOnce(bool);

    impl std::future::Future for YieldOnce {
        type Output = ();
        fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
            if self.0 {
                std::task::Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        let mut f = std::pin::pin!(f);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        loop {
            if let std::task::Poll::Ready(out) = f.as_mut().poll(&mut cx) {
                return out;
            }
        }
    }

    #[test]
    fn a_pack_that_failed_to_read_is_read_again_by_a_later_request() {
        let faces = parse_manifest(MANIFEST.as_bytes()).unwrap();
        let state = RefCell::new(PackState::default());
        let isolate = Isolate::default();
        isolate.failing.borrow_mut().insert(BOLD.to_string());
        // The read fails: the layout isn't final (so not cached publicly), and the want stays.
        let mut first = Request::new(&isolate, vec![want("Source Sans 3", true, false)]);
        assert!(!run(settle(&state, &faces, &mut first, 0)));
        assert_eq!(first.relayouts, 0);
        // A later request for the same document records no want (the resolution is remembered),
        // yet the pack is read again, installed, and that request's document laid out with it.
        let mut later = Request::new(&isolate, Vec::new());
        assert!(run(settle(&state, &faces, &mut later, 0)));
        assert_eq!(*isolate.reads.borrow(), [BOLD, BOLD]);
        assert_eq!(*isolate.installs.borrow(), [BOLD]);
        assert_eq!(later.relayouts, 1);
        // Then nothing is outstanding: a third request reads nothing and is final as laid out.
        let mut third = Request::new(&isolate, Vec::new());
        assert!(run(settle(&state, &faces, &mut third, 1)));
        assert_eq!(isolate.reads.borrow().len(), 2);
        assert_eq!(third.relayouts, 0);
    }

    #[test]
    fn requests_that_interleave_each_get_the_packs_their_documents_want() {
        let faces = parse_manifest(MANIFEST.as_bytes()).unwrap();
        let state = RefCell::new(PackState::default());
        let isolate = Isolate::default();
        // A lays out first and takes the want; its read waits.
        let mut a = Request::new(&isolate, vec![want("Source Sans 3", true, false)]);
        a.slow = true;
        {
            let mut a_settles = std::pin::pin!(settle(&state, &faces, &mut a, 0));
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(a_settles.as_mut().poll(&mut cx).is_pending());
            // B lays out the same document meanwhile: the database records no want for it.
            let mut b = Request::new(&isolate, Vec::new());
            assert!(run(settle(&state, &faces, &mut b, 0)), "B is final only with the pack in");
            assert_eq!(b.relayouts, 1, "B is laid out again with the pack");
            // A finishes: the pack is already in, and A is laid out again with it too.
            assert!(run(a_settles));
        }
        assert_eq!(a.relayouts, 1);
        assert_eq!(*isolate.installs.borrow(), [BOLD]);
    }

    #[test]
    fn a_pack_the_deployment_cannot_give_is_settled_and_not_read_again() {
        let faces = parse_manifest(MANIFEST.as_bytes()).unwrap();
        let state = RefCell::new(PackState::default());
        let isolate = Isolate::default();
        struct Broken<'a>(Request<'a>);
        impl PackHost for Broken<'_> {
            fn take_wants(&mut self) -> Vec<WantedPackFace> {
                self.0.take_wants()
            }
            async fn read(&mut self, file: &str) -> PackRead {
                self.0.isolate.reads.borrow_mut().push(file.to_string());
                PackRead::Unusable
            }
            fn install(&mut self, face: &Face, packed: &[u8]) -> bool {
                self.0.install(face, packed)
            }
            fn generation(&self) -> u64 {
                self.0.generation()
            }
            fn relayout(&mut self) {
                self.0.relayout();
            }
        }
        // Calibri has no pack at all; the bold face's file is missing from the deployment. Both
        // are the deployment's, the same everywhere: final, with substitutes, and read once.
        let wants = vec![want("Calibri", false, false), want("Source Sans 3", true, false)];
        assert!(run(settle(&state, &faces, &mut Broken(Request::new(&isolate, wants)), 0)));
        assert!(run(settle(&state, &faces, &mut Broken(Request::new(&isolate, vec![want("Source Sans 3", true, false)])), 0)));
        assert_eq!(*isolate.reads.borrow(), [BOLD]);
        assert!(isolate.installs.borrow().is_empty());
    }

    #[test]
    fn a_font_that_arrived_after_the_layout_makes_it_stale() {
        let faces = parse_manifest(MANIFEST.as_bytes()).unwrap();
        let state = RefCell::new(PackState::default());
        let isolate = Isolate::default();
        isolate.generation.set(5);
        // Laid out at generation 4, nothing wanted now: laid out again, then final.
        let mut r = Request::new(&isolate, Vec::new());
        assert!(run(settle(&state, &faces, &mut r, 4)));
        assert_eq!(r.relayouts, 1);
    }
}
