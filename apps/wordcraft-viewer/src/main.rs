//! The view-only module as a wasm module (plan Phase 8): the `wasm-bindgen` handle over the
//! `wordcraft_viewer` library. Built like the dedicated workers, as a bin on
//! `wasm32-unknown-unknown`; it has nothing to run, the host calls its exports.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(target_arch = "wasm32")]
mod wasm;

fn main() {
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("wordcraft-viewer is a wasm32 module; build it with --target wasm32-unknown-unknown");
}
