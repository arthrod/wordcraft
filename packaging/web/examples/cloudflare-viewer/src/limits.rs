//! Every bound the Worker applies, in one place, each with the reason for its number. The
//! README's "Limits" table repeats them.

/// Largest upload body, and largest document read from `ASSETS`: 4 MiB, the cap of the older
/// `cloudflare-workers` example. An isolate has 128 MB in all; the body, the decrypted copy of a
/// protected file, the inflated parts (at most [`MAX_INFLATED_BYTES`]), the document model, its
/// layout and one page raster have to fit together, with the wasm module itself.
pub const MAX_BODY_BYTES: usize = 4 << 20;

/// Most bytes all the parts of a package may declare once inflated: 24 MiB. The 4 MiB body bounds
/// the compressed package only; a zip bomb could otherwise inflate to the `.docx` reader's own
/// limit (512 MiB) and take the isolate down. The zip reader refuses an entry that inflates past
/// the size it declares, so the declared total checked here bounds what is really inflated.
pub const MAX_INFLATED_BYTES: u64 = 24 << 20;

/// Most entries a package may declare. A real `.docx` has tens; the reader's own bound is 20,000.
pub const MAX_ZIP_ENTRIES: u64 = 2_000;

/// Most pages one viewer page lists, each an image the browser fetches when the reader nears
/// it. About 150 bytes of HTML a page; past this the page links to the next ones.
pub const MAX_LISTED_PAGES: usize = 1_000;

/// Pages rendered inline in an upload result. Each one is a PNG in the HTML; past this the page
/// says how many were left out.
pub const MAX_INLINE_PAGES: usize = 12;

/// Most bytes of an upload result's HTML: 3 MiB, the page images' Base64 included. A page that
/// would push the response past it is left out, and the page says so.
pub const MAX_RESPONSE_BYTES: usize = 3 << 20;

/// The scale a page is drawn at when none is asked for, in pixels per point: 1.0 is 72 dpi, a
/// US Letter page 612×792. Chosen for size (see the README's measurements): body text at 11 pt is
/// 11 pixels high, readable, and the page PNG stays small.
pub const DEFAULT_SCALE: f32 = 1.0;

/// The range `scale` may take from a query: 0.5 (36 dpi, a thumbnail) to 2.0 (144 dpi, sharp on
/// a 2× display at 100%). Anything else is a 400, not clamped, so a link means what it says.
pub const MIN_SCALE: f32 = 0.5;
pub const MAX_SCALE: f32 = 2.0;

/// Most pixels in one page image: 4 million (16 MB of RGBA). A Letter page at 2.0 is 1.9 million
/// and A3 at 2.0 is 3.9 million; a larger page (a poster) is drawn at a smaller scale to fit
/// rather than refused.
pub const MAX_PAGE_PIXELS: u64 = 4_000_000;

/// Longest `doc` parameter, in bytes (it is ASCII once validated).
pub const MAX_DOC_PARAM_BYTES: usize = 128;

/// The only folder of the deployment a `doc` parameter may name.
pub const DOC_PREFIX: &str = "samples/";

/// Largest page number a query may carry (1-based). Larger is a 400; a number within it but
/// past the document's end is a 404.
pub const MAX_PAGE_NUMBER: u32 = 100_000;

/// Most bytes of `fonts/manifest.json` read from `ASSETS` (the real one is under 2 KB).
pub const MAX_MANIFEST_BYTES: usize = 64 << 10;

/// Most rounds of installing packs and laying the document out again in one request. One is the
/// usual case; a second covers a want that shows only once a pack is in. Past that the answer is
/// sent with what is installed, and not cached publicly.
pub const MAX_PACK_ROUNDS: usize = 3;

/// Most faces read from the manifest (the real one lists nine).
pub const MAX_MANIFEST_FACES: usize = 32;

/// Most parts in an upload's form data: a file and a password, with room for a browser's extras.
pub const MAX_FORM_PARTS: usize = 8;

/// Most bytes of one form part's header block.
pub const MAX_PART_HEADER_BYTES: usize = 2 << 10;

/// How long a browser may keep a page image or viewer page made from a deployed document: ten
/// minutes. The document is part of the deployment, so the response only changes when the site is
/// deployed again; its name carries no content hash, so the time is kept short enough that a new
/// deployment shows within minutes, and long enough that paging back and forth costs no new
/// render.
pub const ASSET_CACHE_CONTROL: &str = "public, max-age=600";
