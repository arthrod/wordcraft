//! The canvas launch book: which `<canvas>` already has a WordCraft launch, shared by
//! `start(options)` and the automatic standalone start. Kept free of `web_sys`/`wasm_bindgen`
//! (the shell around it — canvases, promises, eframe — is wasm-only) so the policy runs in
//! `cargo test` on the host.
//!
//! One rule: the first launch of a canvas owns it; later `start`s for the same canvas resolve
//! to that launch's promise. That is why the automatic standalone launch never registers
//! during module init: the npm wrapper's `start(options)` awaits module init before it
//! forwards options, so an eager default launch would claim the default-id canvas with
//! default options first and the explicit `canvas`/`assetBase`/`host`/`sample` options would
//! never apply. The default launch is deferred past the init task instead and consults the
//! book when it runs: a canvas an explicit `start` registered meanwhile is left to it.

/// The canvases with a launch registered. `same` decides canvas identity everywhere (the shell
/// passes `Object::is` on the elements; tests compare keys).
pub(crate) struct LaunchBook<K, P> {
    entries: Vec<(K, P)>,
}

impl<K, P: Clone> LaunchBook<K, P> {
    pub(crate) const fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// The promise handed to a repeat `start` of an already-launched canvas, if any.
    pub(crate) fn find(&self, canvas: &K, same: impl Fn(&K, &K) -> bool) -> Option<P> {
        self.entries.iter().find(|(k, _)| same(k, canvas)).map(|(_, p)| p.clone())
    }

    /// Record `canvas`'s launch; the first launch of a canvas owns it.
    pub(crate) fn register(&mut self, canvas: K, promise: P) {
        self.entries.push((canvas, promise));
    }

    /// Free a canvas whose launch failed, so a later start can try again.
    pub(crate) fn remove(&mut self, canvas: &K, same: impl Fn(&K, &K) -> bool) {
        self.entries.retain(|(k, _)| !same(k, canvas));
    }

    /// Whether the deferred automatic launch may claim `canvas`: only one no explicit
    /// `start(options)` registered while it was deferred — the default launch must not get
    /// there first and shadow a programmatic start's options.
    pub(crate) fn default_launch_due(&self, canvas: &K, same: impl Fn(&K, &K) -> bool) -> bool {
        self.find(canvas, same).is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same(a: &u32, b: &u32) -> bool {
        a == b
    }

    /// A page that imports the module and calls `start(options)` on the default-id canvas —
    /// the npm wrapper's sequence, which awaits module init (wasm `main`, where the default
    /// launch is only deferred) before forwarding options. The explicit start registers the
    /// canvas with its own `canvas`, `assetBase`, `host` and `sample` options; when the
    /// deferred default launch then fires it must stand down, and the caller keeps the
    /// explicit launch's promise.
    #[test]
    fn explicit_start_options_win_over_the_deferred_default_launch() {
        let mut book = LaunchBook::new();
        let canvas = 0; // the default-id canvas
        let explicit = 11; // the promise start(options) resolved with
        book.register(canvas, explicit);
        assert!(!book.default_launch_due(&canvas, same), "the default launch must not override the explicit options");
        assert_eq!(book.find(&canvas, same), Some(explicit));
    }

    /// A standalone page (`#wordcraft_canvas`, no programmatic start): the deferred default
    /// launch finds nothing registered and runs; a later `start` of that canvas reuses it.
    #[test]
    fn standalone_default_launch_runs_when_no_explicit_start_came() {
        let mut book = LaunchBook::new();
        let canvas = 0;
        assert!(book.default_launch_due(&canvas, same));
        book.register(canvas, 5);
        assert!(!book.default_launch_due(&canvas, same));
        assert_eq!(book.find(&canvas, same), Some(5));
    }

    /// A launch that failed frees the canvas, so another start can try again.
    #[test]
    fn a_failed_launch_frees_the_canvas() {
        let mut book = LaunchBook::new();
        book.register(1, 3);
        book.remove(&1, same);
        assert_eq!(book.find(&1, same), None);
        assert!(book.default_launch_due(&1, same));
    }
}
