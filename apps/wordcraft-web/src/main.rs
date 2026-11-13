//! WordCraft in the browser.
//!
//! Runs the same [`wordcraft_ui_egui::WordApp`] as the desktop app through eframe's web runner
//! (wgpu: WebGPU where available, WebGL2 otherwise). Build with `trunk build --release` from this
//! directory (output in `dist/web`).
//!
//! Differences from the desktop app: no TCP control channel; Open and Insert › Pictures use the
//! browser file picker (bytes arrive through `Services::inbox`); Save and Export download.
//! Preferences (theme, interface size, editing options…) are kept in `localStorage` (`prefs`).
//! URL flags: `?webgl` forces WebGL2; `?sample` opens the sample document.
//! `?host=parent` (inside an `<iframe>`) talks to a same-origin parent page instead: it sends the
//! document to open and stores what WordCraft saves (`host`). A page can also host WordCraft in
//! its own document (no iframe) with `data-host="page"` on the canvas, talking over DOM
//! `CustomEvent`s on the canvas. `&author=` names the user for comments and tracked changes in
//! either host mode.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(any(target_arch = "wasm32", test))]
mod prefs;
#[cfg(any(target_arch = "wasm32", test))]
mod save_fallback;

#[cfg(target_arch = "wasm32")]
mod api;
#[cfg(target_arch = "wasm32")]
mod demo;
#[cfg(target_arch = "wasm32")]
mod host;
#[cfg(target_arch = "wasm32")]
mod web;

#[cfg(any(target_arch = "wasm32", test))]
mod font_pack_manifest;

#[cfg(test)]
const DECLARED_API_METHODS: &[&str] = &[
    "request",
    "execute",
    "commands",
    "docInspect",
    "uiInspect",
    "screenshot",
    "getMarkdown",
    "applyMarkdown",
    "exportDocx",
    "loadDocx",
    "installFonts",
];

#[cfg(target_arch = "wasm32")]
fn main() {
    web::start_default_if_present();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("wordcraft-web only runs in the browser: build it with `trunk build --release` in apps/wordcraft-web");
}

#[cfg(test)]
mod api_contract_tests {
    #[test]
    fn dts_declares_every_wordcraft_api_method() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let dts = std::fs::read_to_string(root.join("packaging/web/npm/index.d.ts")).expect("packaging/web/npm/index.d.ts");
        for method in super::DECLARED_API_METHODS {
            assert!(dts.contains(&format!("{method}(")) || dts.contains(&format!("{method}<")), "index.d.ts missing method {method}");
        }
        assert!(dts.contains("export function start("), "index.d.ts must export start(options)");
        assert!(dts.contains("export interface StartOptions"), "index.d.ts must export StartOptions");
        assert!(dts.contains("export interface WordCraftApi"), "index.d.ts must export WordCraftApi");

        let index_js = std::fs::read_to_string(root.join("packaging/web/npm/index.js")).expect("packaging/web/npm/index.js");
        assert!(!index_js.contains("export default"), "index.js must not use a default export (7.0b)");

        let api_rs = std::fs::read_to_string(root.join("apps/wordcraft-web/src/api.rs")).expect("apps/wordcraft-web/src/api.rs");
        assert!(!api_rs.contains("Err(JsValue::from_str"), "api.rs must reject with Error objects instead of bare strings (7.2a)");
        assert!(!api_rs.contains("ok_or_else(|| JsValue::from_str"), "api.rs helpers must reject with Error objects instead of bare strings (7.2a)");
    }
}
