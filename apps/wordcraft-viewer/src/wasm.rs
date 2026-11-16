//! The `wasm-bindgen` handle over the pure functions in the library. A thin layer: every
//! check is made underneath, where the host tests reach it.
//!
//! ```js
//! const viewer = Viewer.open(bytes, password);        // throws an Error with a `code`
//! for (let i = 0; i < viewer.pageCount(); i++) {
//!   const page = viewer.render(i, devicePixelRatio);  // one page a call, never all of them
//!   const { width, height } = page;
//!   const rgba = page.pixels();                       // Uint8Array; the page handle is used up
//! }
//! ```
//!
//! Only `wasm-bindgen` is linked: no `web-sys`, and no `js-sys` either, so the module needs no
//! DOM and runs wherever its glue does. Two things follow. Bytes cross the boundary by copy,
//! and the copy of an argument is made before any Rust runs: a host that does not trust its
//! input checks `bytes.length` against [`Viewer::max_document_bytes`] first. And a rejection
//! is built on the host's own `Error` constructor, imported below, with `name` and `code` set.

use wasm_bindgen::prelude::*;

use wordcraft_viewer::ViewerError;

#[wasm_bindgen]
extern "C" {
    /// The host's `Error`.
    #[wasm_bindgen(js_name = Error)]
    type HostError;

    #[wasm_bindgen(constructor, js_class = "Error")]
    fn new(message: &str) -> HostError;

    #[wasm_bindgen(method, setter, js_class = "Error")]
    fn set_name(this: &HostError, name: &str);

    #[wasm_bindgen(method, setter, js_class = "Error")]
    fn set_code(this: &HostError, code: &str);
}

/// An `Error` object named `ViewerError`, its `code` the error's [`ViewerError::code`].
fn thrown(e: ViewerError) -> JsValue {
    let err = HostError::new(&e.to_string());
    err.set_name("ViewerError");
    err.set_code(e.code());
    err.into()
}

/// An opened document. Free it (`viewer.free()`) to release the document and its layout.
#[wasm_bindgen]
pub struct Viewer {
    session: wordcraft_viewer::Session,
}

#[wasm_bindgen]
impl Viewer {
    /// Open the `.docx` in `bytes`, with `password` if it is encrypted.
    pub fn open(bytes: &[u8], password: Option<String>) -> Result<Viewer, JsValue> {
        wordcraft_viewer::open(bytes, password.as_deref()).map(|session| Viewer { session }).map_err(thrown)
    }

    /// How many pages the document has.
    #[wasm_bindgen(js_name = pageCount)]
    pub fn page_count(&self) -> u32 {
        u32::try_from(self.session.page_count()).unwrap_or(u32::MAX)
    }

    /// The size of page `page` (0-based) in points.
    #[wasm_bindgen(js_name = pageSize)]
    pub fn page_size(&self, page: u32) -> Result<PageSize, JsValue> {
        self.session.page_size(page).map(|s| PageSize { width: s.width, height: s.height }).map_err(thrown)
    }

    /// Page `page` (0-based) at `scale` pixels per point.
    pub fn render(&self, page: u32, scale: f32) -> Result<PageImage, JsValue> {
        self.session.render(page, scale).map(|image| PageImage { image }).map_err(thrown)
    }

    /// Make the font file `bytes` available as `name` and lay the document out again with it.
    #[wasm_bindgen(js_name = installFont)]
    pub fn install_font(&mut self, name: &str, bytes: &[u8]) -> Result<(), JsValue> {
        self.session.install_font(name, bytes).map_err(thrown)
    }

    /// The fonts the document asks for that aren't available.
    #[wasm_bindgen(js_name = missingFonts)]
    pub fn missing_fonts(&self) -> Vec<String> {
        self.session.missing_fonts()
    }

    /// Lay the document out again (after a font was installed through another viewer).
    pub fn relayout(&mut self) {
        self.session.relayout();
    }

    /// Largest document `open` accepts, in bytes.
    #[wasm_bindgen(js_name = maxDocumentBytes)]
    pub fn max_document_bytes() -> u32 {
        u32::try_from(wordcraft_viewer::MAX_DOCUMENT_BYTES).unwrap_or(u32::MAX)
    }

    /// Largest font file `installFont` accepts, in bytes.
    #[wasm_bindgen(js_name = maxFontBytes)]
    pub fn max_font_bytes() -> u32 {
        u32::try_from(wordcraft_viewer::MAX_FONT_BYTES).unwrap_or(u32::MAX)
    }

    /// Largest scale `render` accepts, in pixels per point.
    #[wasm_bindgen(js_name = maxScale)]
    pub fn max_scale() -> f32 {
        wordcraft_viewer::MAX_SCALE
    }

    /// Most pixels one `render` call returns.
    #[wasm_bindgen(js_name = maxRenderPixels)]
    pub fn max_render_pixels() -> u32 {
        u32::try_from(wordcraft_viewer::MAX_RENDER_PIXELS).unwrap_or(u32::MAX)
    }
}

/// A page's size in points.
#[wasm_bindgen]
pub struct PageSize {
    pub width: f32,
    pub height: f32,
}

/// One rendered page.
#[wasm_bindgen]
pub struct PageImage {
    image: wordcraft_viewer::PageImage,
}

#[wasm_bindgen]
impl PageImage {
    /// Width in pixels.
    #[wasm_bindgen(getter)]
    pub fn width(&self) -> u32 {
        self.image.width
    }

    /// Height in pixels.
    #[wasm_bindgen(getter)]
    pub fn height(&self) -> u32 {
        self.image.height
    }

    /// The pixels: `width * height * 4` bytes of RGBA, not premultiplied, rows top to bottom
    /// (what `ImageData` takes). Uses this page up: read `width` and `height` first.
    pub fn pixels(self) -> Vec<u8> {
        self.image.pixels
    }
}
