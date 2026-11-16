# WordCraft server-rendered viewer (Cloudflare Worker)

A tiny viewer for Word documents where the work is done on the server. The reader's browser gets
a small HTML page and PNG images of the pages: no WebAssembly, no script. The Worker is Rust
(`workers-rs`); it opens the `.docx` with WordCraft's view-only core
([`apps/wordcraft-viewer`](../../../../apps/wordcraft-viewer)), lays it out and draws each page.

It runs beside the static site (`dist/web`) as one deployment: the Worker answers `/view` and
`/view/*`, and everything else is served by the static asset tier without running it.

## Routes

| Route | Answer |
|---|---|
| `GET /view?doc=samples/welcome.docx&page=1&scale=1` | The viewer page for a document of this deployment: the page image (with its `width` and `height`), "Page N of M", previous and next links, a link to the site's own page for the document (`/?doc=…`, which has the editor's Edit button) and the upload form. `page` defaults to 1, `scale` to 1. |
| `GET /view/page.png?doc=…&page=N&scale=S` | One page as `image/png`. |
| `GET /view` (no `doc`) | The upload form: a file, an optional password, a submit button. |
| `POST /view` (`multipart/form-data`, fields `file` and `password`) | The uploaded document's pages, inline as `data:image/png` images. |

`doc` names a file of the deployment and is read **only through the `ASSETS` binding**; the
Worker never fetches a URL from a request. It must be a relative path under `samples/` ending
`.docx`, made of ASCII letters, digits, `.`, `_`, `-` and `/`, with no empty, `.` or `..` segment
and no segment starting with a dot, at most 128 bytes. The check runs on the decoded value, so a
percent-encoded `..` is refused too, and a doubly encoded one leaves a `%`, which is refused.

### Status codes

| Status | When |
|---|---|
| 200 | The page, image or upload result. |
| 400 | A bad `doc`, `page` or `scale`, a parameter given twice, or a malformed form (no file, two files, a password over 255 characters). |
| 404 | No such document in the deployment, a page past the end, or another address under `/view/`. |
| 405 | Another method (`Allow` says which). |
| 413 | An upload over 4 MiB, or a package whose parts would unpack to more than 24 MiB or that has more than 2,000 parts. |
| 415 | Not a Word document (neither a zip package nor an encrypted Office file), or an upload not sent as `multipart/form-data`. |
| 422 | A protected document without its password or with a wrong one (the form comes back asking for it; choose the file again, since nothing is kept), a zip that isn't a readable Word document, or a page that can't be drawn. 422 rather than 401: a 401 must carry a `WWW-Authenticate` challenge, and this is a form. |

Hostile input gets one of these, never a 500 and never a panic: the crate root denies `unwrap`,
`expect`, `panic!`, `unreachable!`, `todo!` and `unimplemented!`, and `unsafe` is forbidden.

## Limits

Every bound is in [`src/limits.rs`](src/limits.rs) with its reason.

| Limit | Value | Why |
|---|---|---|
| Upload body, and a document read from `ASSETS` | 4 MiB | An isolate has 128 MB for everything: the body, a decrypted copy, the unpacked parts, the document, its layout and a page raster. The same cap as the older [`cloudflare-workers`](../cloudflare-workers) example. Checked against `Content-Length` and again while the body streams in. |
| Unpacked size of a package | 24 MiB (sum of the sizes its zip entries declare) | The `.docx` reader's own limit is 512 MiB, sized for a desktop. The zip directory is read first and the declared sizes added up; the zip reader refuses an entry that inflates past its declared size, so a zip bomb is refused before anything is inflated. A protected file is decrypted first and its package checked the same way. |
| Parts in a package | 2,000 | A real `.docx` has tens. |
| Form parts / header block / password | 8 / 2 KiB / 255 characters | The form has two fields; 255 is the longest password Word takes. The multipart reader's delimiter search is linear whatever the body holds. |
| Pages inline in an upload result | 12 | The page says how many were left out and why. |
| Upload result size | 3 MiB of HTML | A page whose image would pass it is left out, and the page says so. |
| `page` | 1 to 100,000, digits only | Past the document's end is a 404. |
| `scale` | 0.5 to 2.0 pixels per point, digits and one `.` only | Out of range, `NaN`, `inf`, signs and exponents are a 400, not clamped. |
| Pixels per page image | 4 million | A Letter page at 2.0 is 1.9 million. A larger page (a poster) is drawn at a smaller scale to fit instead of being refused. The core's own cap (32 million) is never reached. |
| Font manifest / pack file | 64 KiB / the font crate's file cap | Only file names in `fonts/` ending `.ttf.br` are read; each pack is decoded to exactly the size the manifest declares. |
| CPU per request | 5,000 ms (`[limits] cpu_ms`) | A protected file's key derivation and a long document's layout are the expensive parts; the runtime ends a request that runs longer. |

## What a reader gets

- **Fonts.** The Worker carries the four faces the minimal wasm build bundles
  (`wordcraft-fonts`, `minimal-wasm-fonts`). When a document asks for one of the nine faces the site
  ships as font packs (`dist/web/fonts/*.ttf.br`, listed in `fonts/manifest.json`), the Worker reads
  that pack through `ASSETS`, decodes it with the Brotli decoder the font crate already links, and
  installs it for the life of the isolate, then lays the document out again. Measured: page 1 of
  `samples/welcome.docx` from `wrangler dev` is byte for byte the image a host build with all
  thirteen faces draws. Other fonts (Calibri, Aptos, Times New Roman…) are drawn with
  WordCraft's metric-compatible substitutes, as in the browser app; a document set in a font with
  no substitute shows in a fallback face, so treat those pages as approximate.
- **Fonts a document embeds.** A deployed document's embedded fonts are loaded, as the app does
  when it opens a file, and stay for the life of the isolate: they are the deployment's, like the
  documents. An **upload's are ignored**: the font database is the isolate's, shared by every
  request and never pruned, so a font one visitor uploaded would draw later visitors' documents
  and repeated uploads would grow the isolate without end. An upload is drawn with the bundled
  faces and the site's packs, as if nothing were embedded.
- **Images.** A page with only grey ink is a greyscale PNG and a page with up to 256 colours a
  palette PNG, both exact. A page with coloured text has more colours (anti-aliased edges blend
  each ink into the paper), so the lowest bits of its red, green and blue are dropped until 256
  colours remain (as few bits as that takes, four at most; white stays white) and it is written as
  a palette: each channel is then off by at most 2^bits − 1. For the sample that is 2 bits, so at
  most 3 of 255, which can't be seen, and it halves the file. A page that still has too many
  colours (a photograph) is written exactly as RGB.
- **No editing, no text selection, no search.** The pages are pictures. The link on the viewer
  page opens the same document in the browser app, which can edit it.

## Sizes (measured 2026-11-16)

Page 1 of `samples/welcome.docx` (Letter, 2 pages, coloured headings), with the PNG encoder at its
`High` setting:

| Scale | Pixels | This viewer's PNG | Exact RGB | Plain RGBA (encoder default) |
|---|---|---:|---:|---:|
| 0.75 | 459×594 | 31,862 B | 59,695 B | 64,062 B |
| **1.0 (default)** | 612×792 | **49,112 B** | 89,489 B | 96,900 B |
| 1.25 | 765×990 | 66,633 B | 121,926 B | 130,780 B |
| 1.5 | 918×1188 | 84,883 B | 154,488 B | 163,713 B |
| 2.0 | 1224×1584 | 124,016 B | 221,116 B | 235,606 B |

At 1.0, 11-point body text is 11 pixels high: readable, and the smallest file that is. Dropping 3
or 4 bits instead of 2 saves only 9% or 20% more (44,937 B, 39,316 B); `Balanced` compression
instead of `High` is 1.3% larger and no faster here (the palette search dominates).

What a reader downloads to see page 1 of the sample: the viewer page, **777 B**, and the image,
**49,112 B**: **49,889 B** in all, uncompressed, with no other request. The browser app's first
visit downloads its page module (~90 KB gzip), the view-only worker (~2.78 MB gzip) and the
document.

Worker bundle (`worker-build --release`, `opt-level = "z"`, fat LTO, `wasm-opt -Oz`): **6,158,298
B** of wasm raw, **2,926,848 B** gzip (`gzip -6`), plus 23,473 B of generated glue. Cloudflare dropped the
compressed size limits (3 MB Free, 10 MB Paid) on 2026-09-04; a Worker is now checked only against
64 MiB uncompressed, on every plan, so this fits with room to spare. It would also have fit
under the old Free limit, by about 73 KB.

**CPU.** Drawing a page takes far more than the Free plan's 10 ms of CPU per request (the older
example measured 36 ms warm and 151 ms cold for one page), so this Worker needs the Paid plan.

## Build, run and deploy

From this directory:

```sh
# The static site, with samples and font packs (from the repository root):
./packaging/web/package.sh --static          # or copy an existing build into dist/web

# Host tests of the pure logic (parameters, HTML, form data, opening, PNG, font manifest):
cargo test
cargo clippy --all-targets -- -D warnings
cargo clippy --target wasm32-unknown-unknown -- -D warnings

# The Worker (installs worker-build if missing):
cargo install worker-build --locked
worker-build --release

# Run it locally, then check it from outside:
pnpm dlx wrangler dev --port 8787
./smoke.sh http://127.0.0.1:8787

# Deploy (the account's own decision):
pnpm dlx wrangler deploy
```

`smoke.sh` prints PASS or FAIL per check: the viewer page's status, type, size and lack of script;
the image's type and PNG signature; 404 past the end; 400 for traversal, an absolute URL and junk
scale; an upload of the sample (200, inline PNG, `no-store`); 413 for 5 MiB of zeros; 4xx for a
text file and a non-form body. Given a fixtures directory
(`FIXTURES=dir cargo test -- --ignored write_fixtures`, then `./smoke.sh URL SAMPLE dir`) it also
checks a protected upload (password asked again, wrong password refused, neither echoed) and the
inline page cap.

The repository builds wasm32 with SIMD (`.cargo/config.toml`); `Cargo.toml` tells `wasm-opt` to
accept it (`[package.metadata.wasm-pack.profile.release]`).

## Caching

A response made from a deployed document (viewer page, page image) and the empty upload form carry
`Cache-Control: public, max-age=600`: the document only changes when the site is deployed again,
and its name carries no content hash, so ten minutes keeps paging back and forth free without
hiding a new deployment for long. Everything made from an upload, and every error, is
`no-store`.

## Privacy

An uploaded document is read in memory for the one request and dropped: it is not stored,
cached or logged, and neither is its password. The password travels only in the POST body, never
in a URL, and is never written back into a page. The upload's file name is shown back in the
result page's title, escaped. Every HTML answer carries a Content-Security-Policy that allows no
script, only images from this origin and `data:`, the page's own stylesheet, and forms posted back
here.

## How it differs from the in-browser viewer

| | Server-rendered (this) | In-browser viewer (`/`) |
|---|---|---|
| Sent to the reader | HTML (under 1 KB) and one PNG per page | Page module, view-only worker (~2.8 MB gzip), the document |
| Where the document is opened | On the Worker | In the reader's browser; the document never leaves it |
| Script needed | None | WebAssembly and JavaScript glue |
| Text | Pictures: no selection, search or copy | Drawn on canvas from the layout |
| Editing | A link to the app | Edit button loads the editor |
| Uploaded documents | Sent to the Worker, rendered, not kept | Never uploaded |
