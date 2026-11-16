# Hosting and embedding WordCraftWeb (`wordcraft-web`)

`packaging/web/package.sh` builds two release layouts from the same compiled WebAssembly engine:

1. **NPM package (`wordcraft-web`)** via `packaging/web/package.sh --npm`: an unbundled ES module (`index.js` + `index.d.ts`, TypeScript types, `wordcraft-web.js` glue, `wordcraft-web_bg.wasm`, `snippets/`, `i18n/`, and `samples/`).
2. **Static Cloudflare / web site zip (`wordcraft-web-<version>.zip`)** via `packaging/web/package.sh --static`: a hashed Trunk bundle ready for Cloudflare Pages, Cloudflare Workers static assets, or any static host:

| File | What it is |
|---|---|
| `index.html` | Progressive loading shell (HTML/CSS skeleton + the page module's bootstrap). |
| `wordcraft-viewer-web-<hash>.js`, `wordcraft-viewer-web-<hash>_bg.wasm` | The page module, the first thing a visit runs (~0.21 MB raw, ~0.09 MB gzip; see [Viewer first](#viewer-first-what-a-visit-loads)). Content-hashed. |
| `wordcraft-worker-viewer{.js,_loader.js,_bg.wasm}` | The view-only worker that lays out and paints pages for reading (~5.85 MB raw, ~2.78 MB gzip). |
| `wordcraft-web.js`, `wordcraft-web_loader.js`, `wordcraft-web_bg.wasm` | The editor: `wasm-bindgen` glue, its loader and the compiled engine (~17.70 MB raw, ~7.21 MB gzip; 2026-11-16 build, capped under 25 MiB for Cloudflare). Fetched only on an edit action, so its names are not hashed. |
| `i18n/` | Externalized locale TSV catalogs + `manifest.json` fetched on demand. |
| `samples/` | Pre-bundled showcase documents (`welcome.md`, `spec.md`, `letter.md`, and `welcome.docx`, which the viewer opens on a first visit). |
| `_headers`, `.htaccess` | Sample header rules for Cloudflare Pages/Workers static assets, Netlify, and Apache. |


## Viewer first: what a visit loads

The static site opens documents for reading first and fetches the editor only when someone wants
to edit (plan 10.3). A first visit downloads `index.html`, the page module
(`wordcraft-viewer-web-<hash>`), the view-only worker (`wordcraft-worker-viewer`) and the
document; nothing of the editor. The pages are painted on 2D canvases, so reading needs no
WebGL.

- **Edit action:** the **Edit** button, a double click on a page, or Enter or F2 with the pages
  focused. The page module then adds the editor's script (`wordcraft-web_loader.js`), keeps the
  pages on screen while it loads, gives it the same `.docx` bytes (and password) through the
  editor's page API, and removes the viewer once the editor has painted.
- **When the editor can't take over,** the pages stay and a notice says why
  (`data-wordcraft-notice` on `<html>`): `no-webgl` (nothing of the editor is fetched),
  `load-failed` (its files didn't download), `start-failed`, `hand-over-failed`, or `timed-out`
  (it did not start within the wait, 120 s by default).
- **Modes:** `?mode=view` (reading only, no Edit), `?mode=viewer-first` (the default) or
  `?mode=edit` (the editor at once, as before the viewer existed). A page that uses the editor's
  bridge (`?host=parent`, see below) gets the editor at once and never the viewer, so the editor
  itself reports `wordcraft:failed` to the parent and no sample is opened.
- **Protected documents:** the password is asked for in the page (a form under the notice line),
  never with a browser dialog.
- **Embedding the viewer:** the module exports `start(options)`, which resolves to the viewer's
  API (`phase()`, `pageCount()`, `missingFonts()`, `open(bytes, password?)`, `installFont`,
  `edit()`; also `window.wordcraftViewer` with `installGlobal: true`). Options: `container`,
  `mode`, `document` (a `Uint8Array` or `ArrayBuffer`) or `url` (same origin), `password`,
  `assetBase`, `installGlobal`, and `editorWaitMs` (1 s to 10 min; also `data-editor-wait-ms`
  on the container). The page also reports its state on `<html>`: `data-wordcraft-phase`
  (`viewing`, `loading-editor`, `handing-over`, `editing`, `viewer-only`),
  `data-wordcraft-error` and `data-wordcraft-painted`.

The npm package ships the editor only (`start()` as before); the viewer files are not in it yet.

## Browser floor (WebAssembly SIMD)

The shipped modules are built with `-C target-feature=+simd128` (`.cargo/config.toml`) and
`wasm-opt --enable-simd` (`apps/wordcraft-web/index.html`), adopted after measuring (2026-11-14,
plan §4.2): laying out a 235 KB Markdown document went from 204 ms to 115 ms and the module got
8.5% smaller — 16,256,770 B raw against 17,758,276 B scalar (gzip 6,671,547 vs 6,977,543;
Brotli q11 4,862,005 vs 5,021,646); page raster was unchanged. Every engine with WebAssembly
SIMD support compiles it: Chrome 91+, Safari 16.4+, Firefox 89+. A browser without SIMD cannot
compile the module, and there is no fallback build.

## Quick start via npm (`wordcraft-web`)

```bash
npm install wordcraft-web
```

`wordcraft-web` exports named `start(options)`, `init(wasmUrl?)`, and `initSync` (no default export). Importing the package does not access DOM globals, so SSR/Node imports succeed cleanly until `start(...)` runs in a browser:

```js
import { start } from "wordcraft-web";

const canvas = document.getElementById("wordcraft_canvas");
const api = await start({
  canvas, // HTMLCanvasElement or element id (defaults to "wordcraft_canvas")
  // installGlobal: true, // optional: also expose window.wordcraft
  // sample: true,        // optional: load welcome sample document
});

const md = await api.getMarkdown();
await api.execute("format.bold");
const docxBytes = await api.exportDocx();
```

### Bundler recipes (`Vite`, `Webpack`, `esbuild`)

- **Vite:** exclude `"wordcraft-web"` from dependency pre-bundling (`optimizeDeps: { exclude: ["wordcraft-web"] }`) so `new URL("./wordcraft-web_bg.wasm", import.meta.url)` and `./i18n/` / `./samples/` relative URLs remain intact.
- **CDN / custom asset paths:** serve `wordcraft-web_bg.wasm`, `i18n/`, `samples/`, `fonts/`, and all of the `wordcraft-worker-*` files (the `wordcraft-worker-{proof,codecs,pdf}_loader.js` workers and the `wordcraft-worker-*.js` / `wordcraft-worker-*_bg.wasm` files they load) from your public path and pass `{ wasmUrl, assetBase }`. Hosts that spawn the proofing worker themselves use the same base: `createProofWorker({ assetBase })` (or `createProofWorker({ loaderUrl })` for an exact loader URL):

```js
const api = await start({
  canvas: "wordcraft_canvas",
  wasmUrl: "/wordcraft/wordcraft-web_bg.wasm",
  assetBase: "/wordcraft/",
});
```

- **When each worker is fetched:** none on page load. The proofing worker on the first edit (or proofing command); the codecs worker the first time a file in one of its formats is opened or saved (`.rtf`, `.odt`, `.html`, legacy `.doc`, …; from the app's Open, a drop, Save As, Export, or `convertDocument`); the PDF worker on the first PDF export (File › Export, or `exportPdf`). A file opened this way opens as a Word document (`report.odt` becomes `report.docx`), beside a document that has work in it, never in its place. A zip package is routed by its content before its name (an OpenDocument text under any name goes to the codecs worker, a Word package under any name stays with the app). If a worker's files cannot be fetched the status bar says so and the open document is left as it was, so all three must still be served. `packaging/web/smoke/lazy.mjs` measures this.
- **Worker jobs, as they behave now** (limits, not promises):
  - *Outcome of a scripted save.* `file.saveAs` / `file.exportPdf` to a worker's format answers `{"exporting": name, "pending": true}` at once; the file is written later. How it ended is said on the status bar and in the console (`Exported name (N bytes)` or `Couldn't export name: …`) and is not returned to the caller. A page that needs the bytes or the error calls `convertDocument` / `exportPdf`, whose promises settle with either. The document keeps its unsaved changes: such a save is a copy.
  - *Delivery.* The finished file is an anchor download (into the browser's downloads), never the Save As picker: the worker answers after the click's user activation has expired, and the browser refuses the picker then. The leave-page warning covers the export from the request until it is delivered. In host mode the file goes to the host as `wordcraft:save` with `saveAs: false` (a copy).
  - *Failures reach the status bar only.* A failed open says `Couldn't open name: …` on the status bar and in the console, as a failed open of a `.docx` does; the host page that sent the file gets no message (the host contract has none for a failed open). The user's own file is untouched and can be opened again.
  - *Deadlines.* A conversion may take 2 minutes plus 10 s per MiB of input, a PDF export 3 minutes plus 15 s per MiB, each at most 15 minutes, counted after the jobs queued ahead of it. A worker past its deadline is stopped (its waiting jobs fail) and the next job starts a fresh one. Workers send no progress, so a job is not told apart from a stuck one before then.
  - *Passwords.* A password-protected Word package (`.docx`, or one saved under `.doc`) opens with the app's password prompt. A password-protected Word 97–2003 binary `.doc` and an encrypted `.odt` cannot be opened in the browser: the codecs worker needs the password and has no way to ask (the status bar says the file is protected).
  - *Names.* The converted document is named `<stem>.docx`; its next Save writes a `.docx`.
- **CSP requirements:** allow `'wasm-unsafe-eval'` in `script-src` alongside `'self'` (plus any SHA-256 hash of Trunk's inline bootstrap `<script>` when using `dist/web`), `worker-src 'self'`, and `connect-src 'self'` (same-origin fetches for `.wasm`, `i18n/*.tsv`, and `samples/*.md`). No `COOP` / `COEP` headers are required.
- **ESM wrapper scope (House Rule 4):** `packaging/web/npm/index.js` (with `index.d.ts`) is the thin ESM entrypoint wrapper over Rust's `#[wasm_bindgen] pub fn start(options)` export, and `packaging/web/smoke/` is the consumer smoke-test harness (see its README). These are the only approved handwritten JavaScript/TypeScript in the web packaging tree. Application logic and the demo bar (`apps/wordcraft-web/src/demo.rs`) stay pure Rust.

## Cloudflare deployments (static & hybrid)

Two ready-to-deploy examples live in this directory (with `.assetsignore` excluding `.wasm.gz`, `.d.ts`, and `.trunk-lock` from static uploads so Cloudflare serves edge-compressed `.wasm` with a clean `Content-Type: application/wasm`):

- **Static deployment (Cloudflare Pages):** <https://wordcraft-web-pages.pages.dev/?sample&demo=1> ([`examples/cloudflare-pages/`](examples/cloudflare-pages/README.md)) — zero backend, serves `dist/web/` directly (`6.39 MB` Brotli / `6.69 MB` Gzip wasm transfer, measured live 2026-11-15).
- **Hybrid Edge deployment (Cloudflare Workers + `workers-rs`):** <https://wordcraft-web-worker.cicero-im.workers.dev/?sample&demo=1> ([`examples/cloudflare-workers/`](examples/cloudflare-workers/README.md)) — serves the same browser-first static assets via `ASSETS` (`4.87 MB` Brotli sidecar / `6.35 MB` Zstd / `6.69 MB` Gzip wasm transfer, measured live 2026-11-15) plus `GET /api/health` and an unlogged `POST /api/convert` endpoint bounded to `4 MiB` (`Cache-Control: no-store`).

### Headless conversion benchmark (`wordcraft-cli`, Step 5.2a)

Measured locally via `cargo build --release -p wordcraft-cli` with `/usr/bin/time -l` to size Cloudflare Workers' 128 MB isolate RAM budget:

| Document | Pipeline | Real / CPU Time | Peak RSS | Fits 128 MB Isolate? |
|---|---|---|---|---|
| 1-page (`3.4 KB`) | `.md` → `.docx` | `0.67s` (`0.00u/0.01s`) | `14.5 MiB` | Yes |
| 1-page (`6.6 KB`) | `.docx` → `.pdf` | `0.31s` (`0.01u/0.11s`) | `33.1 MiB` | Yes |
| 50-page (`170 KB`) | `.md` → `.docx` | `0.02s` (`0.00u/0.00s`) | `16.4 MiB` | Yes |
| 50-page (`8.4 KB`) | `.docx` → `.pdf` | `0.10s` (`0.07u/0.02s`) | `53.0 MiB` | Yes |
| 500-page (`1.70 MB`) | `.md` → `.docx` | `0.05s` (`0.04u/0.00s`) | `38.0 MiB` | Yes |
| 500-page (`24 KB`) | `.docx` → `.pdf` | `0.62s` (`0.57u/0.03s`) | `233.4 MiB` | **No** (exceeds 128 MB) |

Memory safety inside the isolate is bounded by the decompression budgets and DOCX part/total limits the Worker passes to the reader, not by payload size alone; `/api/convert` still enforces a `4 MiB` upload bound and rejects oversized requests with HTTP `413` (`use 100% Browser WASM mode instead`).

## Any path works

All URLs in `index.html` are relative (`public_url = "./"` in `apps/wordcraft-web/Trunk.toml`),
so the static site works at a domain root (`https://example.com/`), under a prefix
(`https://example.com/tools/wordcraft/`) and from a CDN bucket. The page module
(`wordcraft-viewer-web-<hash>`) carries a content hash (`immutable`), while
`index.html`, `snippets/`, `i18n/`, `samples/`, the editor (`wordcraft-web.js`, `wordcraft-web_loader.js`,
`wordcraft-web_bg.wasm`) and the workers (`wordcraft-worker-*`) revalidate on fetch: their names
are not hashed, because the page module names them at run time. A host that pins un-hashed names
for a year keeps serving an old editor after every deploy. Never deploy the unhashed npm package
filenames under the `immutable` wildcard rule either.

## Required server settings

- **MIME type:** serve `.wasm` as `application/wasm`. Browsers refuse to stream-compile it under
  any other type, and the app then loads slowly or not at all. Serve `.js` as `text/javascript`.
  Most hosts already do both. For nginx, check that `mime.types` has `application/wasm wasm;`.
- **Compression:** turn on gzip or Brotli for `.wasm`, `.js` and `.html`. Gzip takes the release
  editor's `.wasm` from ~17.70 MB raw to ~7.21 MB (2026-11-16 build), and the view-only worker's
  from ~5.85 MB to ~2.78 MB.
- **Caching:** `Cache-Control: public, max-age=31536000, immutable` on the content-hashed page
  module (`wordcraft-viewer-web-<hash>`) only, and `no-cache` on `index.html`, the editor
  (`wordcraft-web{.js,_loader.js,_bg.wasm}`), the workers (`wordcraft-worker-*`) and `snippets/`
  (a snippet's directory name comes from its crate, not from the file's content). `i18n/` and
  `samples/` are cached for an hour and then revalidated. `_headers` and `.htaccess` already say so; the editor
  costs one revalidation per file on a visit that edits (a 304 when unchanged).
- **HTTPS:** Clipboard integration works best in a secure context (`https://` or `http://localhost`).
- **No special isolation headers:** WordCraft doesn't use `SharedArrayBuffer`, so it doesn't
  need `Cross-Origin-Opener-Policy` or `Cross-Origin-Embedder-Policy`. If your site already sends
  COEP `require-corp`, also send `Cross-Origin-Resource-Policy: same-origin` (or `cross-origin`
  when the files live on a CDN) on the app's files.

nginx example:

```nginx
location /wordcraft/ {
    types { application/wasm wasm; text/javascript js; text/html html; }
    gzip on;
    gzip_types application/wasm text/javascript text/html;
    # Un-hashed worker files revalidate like index.html (same exception as _headers/.htaccess):
    # a year-pinned worker older than the hashed main module fails the protocol version check.
    location ~* /wordcraft-worker-.*\.(wasm|js)$ { add_header Cache-Control "no-cache"; }
    # The editor's names are un-hashed too (fetched on an edit action): revalidate, never pin.
    location ~* /wordcraft-web(\.js|_loader\.js|_bg\.wasm)$ { add_header Cache-Control "no-cache"; }
    location ~* \.(wasm|js)$ { add_header Cache-Control "public, max-age=31536000, immutable"; }
    location ~* index\.html$ { add_header Cache-Control "no-cache"; }
}
```

Local test: `python3 -m http.server 8765` inside the folder, then open http://localhost:8765/.

## Embedding in a page (iframe)

```html
<iframe
  src="https://example.com/wordcraft/"
  title="WordCraft word processor"
  style="width: 100%; height: 720px; border: 0;"
  allow="fullscreen; clipboard-read; clipboard-write"
  allowfullscreen>
</iframe>
```

- The app fills the iframe and follows its size, so size the iframe and not the app.
- Keyboard shortcuts go to the iframe after the user clicks into it, as with any embedded app.
- **Cross-origin embeds** work. Preferences are kept in the iframe's `localStorage`. Browsers
  that partition or block third-party storage may forget them between visits, and the app
  then starts with defaults.
- **Sandboxed iframes** need at least
  `sandbox="allow-scripts allow-same-origin allow-downloads allow-popups"`. Without
  `allow-same-origin` there's no storage. Without `allow-downloads`, Save and Export (browser
  downloads) are blocked.
- Don't send `X-Frame-Options: DENY` or a `frame-ancestors` CSP that excludes the embedding page.

## The parent bridge (`?host=parent`)

A page framing WordCraft in a **same-origin** iframe can add `?host=parent` to the iframe's URL
(`https://example.com/wordcraft/?host=parent`) and have WordCraft hand its files over instead of
downloading them. The two sides talk `postMessage` with plain window messages that carry a `type`:

| Message | Direction | Fields |
|---|---|---|
| `wordcraft:ready` | app → page | `version`, `appVersion`: the app is running and listening; send a file now |
| `wordcraft:open` | page → app | `name`, `bytes` (an `ArrayBuffer` or `Uint8Array`): open like File › Open |
| `wordcraft:save` | app → page | `name`, `saveAs`, `bytes` (a transferred `ArrayBuffer`): every file the app would otherwise download (Save, Save As, Export as PDF, Export Page as PNG). The page stores it and reports failures itself. `saveAs` says whether the file becomes the document's file: `false` after Save (the page's own file, written under its name) and for exports that are only copies (PDF, Markdown, plain text); `true` after Save As and for exports to a format that keeps everything (.docx, .odt, .rtf), after which the next Save carries that name with `false` |
| `wordcraft:dirty` | app → page | `dirty`: unsaved changes appeared or went away |
| `wordcraft:failed` | app → page | `error`: the app couldn't start (no WebGPU or WebGL2, for example) |

In host mode the app opens no sample document (and the viewer-first page loads the editor at once,
without the viewer), so the framing page should send the file to edit
and guard leaving the page while changes are unsaved. `&author=` on the iframe's URL names the
user for the comments and tracked changes the app creates. Opening `?host=parent` outside an
iframe gives the normal app with downloads.

## Same-document host mode (no iframe)

A host application can also run WordCraft from its own document: it loads the wasm-bindgen module
itself and gives WordCraft one of its own canvases, so there is no iframe and no `?host=parent`.
It opts in with `data-host="page"` on that canvas — the exact value `page` — and the canvas keeps
the id the app looks up (`id="wordcraft_canvas"`).

```html
<canvas id="wordcraft_canvas" data-host="page" style="width: 100%; height: 720px; display: block;"></canvas>
```

The page and the app then talk with DOM `CustomEvent`s on the canvas instead of `postMessage`.
The app dispatches the same messages it would post to a parent — `wordcraft:ready`,
`wordcraft:save`, `wordcraft:dirty`, `wordcraft:failed` — with the message fields (without the
`type`) as the event's `detail`, and it opens a document when the page dispatches a
`wordcraft:open` event with `detail: { name, bytes }`. Events do not bubble, so listen on the
canvas itself (two editors on one page can't hear each other).

Add the listeners before `init`: the app announces itself with `wordcraft:ready` while `init` is
still running, and a file sent in reply would otherwise be missed.

```js
import init from "./wordcraft-web.js"; // the editor's wasm-bindgen glue shipped with the app

const canvas = document.getElementById("wordcraft_canvas");

canvas.addEventListener("wordcraft:ready", async () => {
  // The app is listening; hand it a document now, if you have one.
  const bytes = await documentBytes(); // an ArrayBuffer or Uint8Array
  canvas.dispatchEvent(new CustomEvent("wordcraft:open", {
    detail: { name: "Report.docx", bytes },
  }));
});

// Every file WordCraft would otherwise download (Save, Save As, Export as PDF) arrives here.
canvas.addEventListener("wordcraft:save", (event) => {
  const { name, saveAs, bytes } = event.detail; // bytes is an ArrayBuffer
  // Store the file under `name`; `saveAs` says whether it becomes the document's file.
  // Report your own errors.
  storeFile(name, bytes, saveAs);
});
canvas.addEventListener("wordcraft:dirty", (event) => {
  unsaved = event.detail.dirty; // warn before leaving, as you like
});
canvas.addEventListener("wordcraft:failed", (event) => {
  showError(event.detail.error); // the app couldn't start
});

await init({ module_or_path: "./wordcraft-web_bg.wasm" });
```

- Saves are not downloads in this mode: the `bytes` in `wordcraft:save` hold the saved document,
  and the host decides where the file goes.
- `?author=` on the page's URL still names the user for new comments and tracked changes.
- An unframed page that uses `?host=parent` keeps the normal app with downloads.
  `data-host="page"` turns this mode on only when `?host=parent` is absent: if `?host=parent` is
  present it takes precedence, so a same-origin framed page uses the parent bridge, while an
  unframed or cross-origin page gets no bridge and keeps normal downloads.

The messages and both transports are the shared contract of [`craft-web-host`](https://github.com/storytold/craft-libs/tree/main/crates/web-host) (storytold/craft-libs#420), the same for every Craft app: its README lists every message, including `saved`, with the compatibility notes. In `wordcraft:ready`, `version` is the contract's (`"1"`) and `appVersion` is this app's release. WordCraft reads bytes, so a host sends `bytes`, not a `File`.

### Trust and limits

- **Who is trusted.** In page host mode the page and WordCraft share one document, so any script on the page can drive the app: open files in it and receive what it saves. Use this mode only for a page you control. The iframe mode (`?host=parent`) answers only the parent window, on the page's own origin.
- **Files from the host are untrusted input.** An opened file goes through the same reading as one picked with File ▸ Open or dropped on the window, with the same format limits; before it is copied into the app's memory, its size is checked against the host open limit (512 MiB) and a larger file is refused, and a name is cut to 255 characters.
- **The event and message names are the interface.** `wordcraft:ready`, `wordcraft:open`, `wordcraft:save`, `wordcraft:dirty` and `wordcraft:failed`, and the fields they carry (the same as the `?host=parent` messages), stay stable; changing one is a breaking change.

### Driving it via the Promise API (`api` or `window.wordcraft`)

`start(...)` resolves to a `WordCraftApi` instance (also published to `window.wordcraft` by the standalone static demo or when `installGlobal: true` is passed), built with [`craft-web-api`](https://github.com/storytold/craft-libs/tree/main/crates/web-api). `request(method, params)` calls any control-protocol method (`docs/control-protocol.md`), alongside convenience shortcuts `execute(command, params)`, `commands()`, `docInspect(params)`, `uiInspect(params)`, `screenshot(params)`, `getMarkdown()`, `applyMarkdown(md)`, `exportDocx()`, and `loadDocx(data, password?)`. Each returns a Promise that resolves with the result and rejects with an `Error` (calls time out after 30 seconds).

```js
const doc = await api.docInspect();                  // summary of the open document
await api.execute("text.insert", { text: "Hello" });
const pngBase64 = await api.screenshot();            // canvas as PNG (base64)
const md = await api.getMarkdown();                  // serialize current document as Markdown
```

The API works in every mode, with or without a host bridge.

## Renderer selection

By default, `wordcraft-web` compiles with **`glow` (WebGL2)** so `wgpu` and `naga` do not inflate the WASM binary. Optional WebGPU support can be compiled in with the `wgpu` Cargo feature on `apps/wordcraft-web`, in which case `?webgl` forces WebGL2 fallback.

A browser without WebGL2 (or WebGPU when enabled) receives an error message in place of the canvas.
