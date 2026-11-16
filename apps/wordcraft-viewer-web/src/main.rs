//! The viewer-first page module (plan 10.3): the first module a WordCraft page loads.
//!
//! It gets the document's bytes, starts the view-only worker (`wordcraft-worker-viewer`),
//! and paints the visible pages on plain 2D canvases. The editor (`wordcraft-web`) is fetched
//! only on an edit action; it is then handed the same `.docx` bytes through its page API and
//! takes the page over once it has painted. Where the editor cannot start, the viewer stays
//! and says so.
//!
//! Nothing of the document stack is linked here: the module is `web-sys` glue around four
//! pure pieces, which the host tests cover: [`mode`] (which of the three ways the page runs),
//! [`pages`] (where pages sit and which are visible), [`cache`] (which pages to ask for and
//! which to drop) and [`handover`] (viewer, loading the editor, editor or fallback).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(any(target_arch = "wasm32", test))]
mod cache;
#[cfg(any(target_arch = "wasm32", test))]
mod handover;
#[cfg(any(target_arch = "wasm32", test))]
mod mode;
#[cfg(any(target_arch = "wasm32", test))]
mod pages;

#[cfg(target_arch = "wasm32")]
mod page;
#[cfg(target_arch = "wasm32")]
mod worker;

fn main() {
    #[cfg(target_arch = "wasm32")]
    page::start_default_if_present();
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("wordcraft-viewer-web only runs in the browser: it is built by Trunk from apps/wordcraft-web/index.html");
}
