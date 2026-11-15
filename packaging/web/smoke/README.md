# Consumer smoke matrix

Deferred-items plan **7.3a**: pack the staged `wordcraft-web` npm package,
install the tarball into minimal bundler fixtures, build each, and drive
headless Chrome to confirm the package works for real consumers — including
the runtime files bundlers do *not* copy.

## Run it

```sh
packaging/web/smoke/run.sh                # full matrix
packaging/web/smoke/run.sh --only vite    # one bundler
packaging/web/smoke/run.sh --tarball path/to/wordcraft-web.tgz   # reuse a tarball
packaging/web/smoke/run.sh --build        # force `package.sh --npm` (full Trunk build)
```

Needs `node`, `pnpm`, `npm` (used only for `npm pack` — installs are pnpm
only), and a Chrome: `CHROME_PATH`, `/Applications/Google Chrome.app`, or a
Chrome already in `~/.cache/puppeteer`.

## What it does

1. **Stage + pack.** Mirrors the npm package under `smoke/stage/npm`, then
   `npm pack`s it into `smoke/tarball/wordcraft-web.tgz`. The stage comes from,
   in order: the real `$DIST/npm` stage when complete, the hashed `dist/web`
   output de-hashed to the names a `--filehash=false` build emits (the glue
   resolves `wordcraft-web_bg.wasm` un-hashed at runtime, so renaming is
   byte-safe), or a fresh `package.sh --npm` Trunk build with `--build`.
   Nothing is published.
2. **Fixtures.** `fixtures/{vite,webpack,esbuild}` each depend on the tarball
   through a `file:` dependency, import `index.js`, and call
   `start({ canvas, assetBase: "/wordcraft-web/", host: "none" })` from the
   shared `fixtures/main.shared.js`. Each builds with its own bundler —
   deliberately zero bundler-specific configuration, since the matrix exists to
   record what works without it.
3. **Drive.** `drive.mjs` serves the fixture's `dist/` at `/` and the unpacked
   package at `/wordcraft-web/` — the `assetBase` the fixtures pass, which is
   how a consumer serves the runtime files — then loads `/?proof-worker` in
   headless Chrome (puppeteer-core + system Chrome).
4. **Report.** Writes `results/<bundler>.json` and `results/report.md`.

## What each check proves

| Check | How it's exercised | What it catches |
|---|---|---|
| `start()` resolves | `await start(...)` on `<canvas id="smoke_canvas">` (not `wordcraft_canvas`, so the module's `start_default_if_present` auto-start can't race the explicit call) | bundler broke the glue or wasm |
| **wasm found (no config)** | first `start()` uses the package default `new URL("./wordcraft-web_bg.wasm", import.meta.url)` | bundlers that leave the wasm URL unresolved; on a miss the page retries with an explicit `wasmUrl` so the rest still runs |
| **locale loaded** | `navigator.language(s)` is `fr-FR` (+ stored `{"language":"fr"}`), so the shell fetches `i18n/fr/common.tsv` and `i18n/fr/wordcraft.tsv` through `assetBase` | runtime catalogs consumers must self-serve |
| **font pack loaded** | `api.installFonts(["JetBrains Mono"])` → `fonts/manifest.json` + `fonts/JetBrainsMono-*.ttf.br` fetched, Brotli-decoded, installed | the assetBase runtime files bundlers never copy |
| **worker started** | `?proof-worker` spawns `<assetBase>wordcraft-worker-proof_loader.js` as a module worker and runs one `check`; success is the `proofing worker: N issue(s)` console line | worker loader/glue/wasm all resolve same-origin |
| **exported proof worker** | the fixture calls `createProofWorker({ assetBase: "/wordcraft-web/" })`, waits for `ready`, and completes one `ping`/`pong` | the npm export resolves the loader from the documented deployment base in a bundled consumer |
| **malformed messages** | a second worker instance is posted `null`, a string, a bad header/payload pair and a non-string header, then one valid `ping`; every malformed message must get a typed `error`/`badRequest` reply and the ping a `pong` | `unpack` throwing out of `onmessage` (dropped messages), wrong-type payloads slipping through |
| console errors | any `console.error`/`pageerror` after the deliberate zero-config wasm attempt | uncaught failures |

## JavaScript exception

The repository's "Rust only" rule covers shipped product code (the sole npm
exception is `packaging/web/npm/index.js` + `index.d.ts`). The fixture entries
and `drive.mjs` are **test/tooling scripts under `packaging/web/smoke/`** —
the same category as `package.sh`'s inline Node checks — not product code.
Plan note Q3 flags this stretch of the D1 exception for review.

## Results

Last local run (2026-11-14, Chrome 157, `wordcraft-web` 0.18.1, package staged from the
three-worker build and packed as `wordcraft-web.tgz`):

| Bundler | wasm found (no config) | start() | locale (fr) | font pack | proof worker | console errors |
|---|---|---|---|---|---|---|
| vite | yes | yes | yes | yes | yes | none |
| webpack | yes | yes | yes | yes | yes | none |
| esbuild | no (explicit `wasmUrl` fallback) | yes | yes | yes | yes | none |

`run.sh` overwrites `results/report.md` with the measured table — update this
one from it.
