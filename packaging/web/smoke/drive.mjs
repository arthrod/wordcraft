#!/usr/bin/env node
// Headless-Chrome driver for one built bundler fixture (deferred-items plan §7.3a).
//
//   node drive.mjs --fixture vite --dist fixtures/vite/dist --pkg stage/npm --out results/vite.json
//   node drive.mjs --report --results results/
//
// Serves the fixture's bundler output at / and the unpacked wordcraft-web
// package at /wordcraft-web/ (the `assetBase` the fixture pages pass to
// start()), loads `/?proof-worker` in headless Chrome, and records:
//   - whether start() resolved, and whether the wasm was found with zero
//     bundler configuration (the fixture falls back to an explicit wasmUrl so
//     the rest of the matrix still runs)
//   - one runtime locale (i18n/fr/*.tsv) fetched through assetBase
//   - one font pack (JetBrains Mono .ttf.br) fetched + installed through assetBase
//   - the proofing dedicated worker answering a `check` (?proof-worker smoke)
//   - the package's createProofWorker export answering a ping via assetBase
//   - console errors after the deliberate zero-config wasm attempt
//
// Tooling-script JavaScript: the repository's "Rust only" rule covers shipped
// product code; test/tooling scripts are the same exception package.sh already
// uses for its inline Node checks (see README.md).

import { createServer } from "node:http";
import { createReadStream, existsSync, mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { extname, join, normalize, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import puppeteer from "puppeteer-core";

const HERE = fileURLToPath(new URL(".", import.meta.url));

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".map": "application/json; charset=utf-8",
  ".wasm": "application/wasm",
  ".tsv": "text/tab-separated-values; charset=utf-8",
  ".br": "application/octet-stream",
  ".ttf": "font/ttf",
  ".png": "image/png",
  ".svg": "image/svg+xml",
  ".ico": "image/x-icon",
  ".txt": "text/plain; charset=utf-8",
  ".md": "text/markdown; charset=utf-8",
};

function serveFile(res, path, headers = {}) {
  res.writeHead(200, { ...headers, "Content-Type": MIME[extname(path)] ?? "application/octet-stream" });
  createReadStream(path).pipe(res);
}

// Static server: `/wordcraft-web/*` → the unpacked npm package (assetBase),
// everything else → the fixture's dist. Mirrors how a consumer serves the
// runtime files bundlers don't copy.
//
// `opts.log` (an array) receives one `{ path, served, status, bytes }` per
// request — `path` as the page requested it, `served` the file sent for it (null
// for none) — and `opts.miss(path)` answering true makes that request a 404 —
// lazy.mjs uses both to measure what a page fetches and to simulate a missing
// worker file.
// `opts.headers` are added to every file served (viewer.mjs sends the deployed
// Content-Security-Policy this way).
export function startServer(distDir, pkgDir, opts = {}) {
  const server = createServer((req, res) => {
    const url = new URL(req.url ?? "/", "http://localhost");
    const requested = decodeURIComponent(url.pathname);
    let pathname = requested;
    const note = (status, bytes, served = null) => opts.log?.push({ path: requested, served, status, bytes });
    if (opts.miss?.(pathname)) {
      note(404, 0);
      res.writeHead(404).end("not found");
      return;
    }
    let root = distDir;
    if (pathname === "/favicon.ico") {
      res.writeHead(204).end();
      return;
    }
    if (pkgDir && pathname.startsWith("/wordcraft-web/")) {
      pathname = pathname.slice("/wordcraft-web".length);
      root = pkgDir;
    }
    const path = normalize(join(root, pathname));
    if (!path.startsWith(resolve(root) + sep) && path !== resolve(root)) {
      res.writeHead(403).end();
      return;
    }
    const file = existsSync(path) && statSync(path).isDirectory() ? join(path, "index.html") : path;
    if (existsSync(file) && statSync(file).isFile()) {
      note(200, statSync(file).size, file);
      serveFile(res, file, opts.headers);
    } else {
      note(404, 0);
      res.writeHead(404).end("not found");
    }
  });
  return new Promise((resolvePromise) => {
    server.listen(0, "127.0.0.1", () => resolvePromise({ server, port: server.address().port }));
  });
}

// The executable basenames a puppeteer cache install can hold: `chrome` and
// `chrome-headless-shell` on Linux, and `Google Chrome for Testing` inside the
// macOS app bundle (`…/Google Chrome for Testing.app/Contents/MacOS/`).
export function isChromeExecutable(path) {
  return /(?:^|\/)(?:chrome|chrome-headless-shell|Google Chrome for Testing)$/.test(path);
}

export function findChrome() {
  const candidates = [process.env.CHROME_PATH];
  if (process.platform === "darwin") {
    candidates.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
    candidates.push("/Applications/Chromium.app/Contents/MacOS/Chromium");
    candidates.push("/Applications/Brave Browser.app/Contents/MacOS/Brave Browser");
  }
  candidates.push("/usr/bin/google-chrome", "/usr/bin/chromium", "/usr/bin/chromium-browser");
  for (const c of candidates) {
    if (c && existsSync(c)) return c;
  }
  // A Chrome already downloaded into the puppeteer cache also works.
  const cache = join(process.env.HOME ?? "", ".cache", "puppeteer", "chrome");
  if (existsSync(cache)) {
    const installs = readdirSync(cache).sort().reverse();
    for (const dir of installs) {
      const base = join(cache, dir);
      const found = readdirSync(base, { recursive: true })
        .map((f) => join(base, f.toString()))
        .find((f) => isChromeExecutable(f.toString()));
      if (found && existsSync(found)) return found;
    }
  }
  throw new Error("no Chrome found: set CHROME_PATH, install Google Chrome, or let puppeteer download one");
}

function parseArgs(argv) {
  const args = { fixture: null, dist: null, pkg: null, out: null, report: false, results: null };
  for (let i = 2; i < argv.length; i++) {
    const key = argv[i];
    if (key === "--report") {
      args.report = true;
    } else {
      args[key.slice(2)] = argv[++i];
    }
  }
  return args;
}

// The exact main wasm URL the deliberate zero-config attempt tried, as the page
// recorded it in `autoError` ("… fetching 'http://…/wordcraft-web_bg.wasm'"),
// or null when the attempt's error names no URL. The main wasm's name is
// distinct from the worker modules' (`wordcraft-worker-*_bg.wasm`), which a
// substring match on "_bg.wasm" would wrongly swallow.
export function autoWasmUrl(smoke) {
  const m = /https?:\/\/[^\s'"]*wordcraft-web_bg\.wasm/.exec(smoke?.autoError ?? "");
  return m ? m[0] : null;
}

// Errors caused by the deliberate zero-config wasm attempt are expected
// exactly when wasmAuto is false; everything else counts. Suppression requires
// the error to identify that attempt's exact wasm URL (its `url`, or error
// text naming it) — worker-module wasm misses and unrelated resource failures
// stay real errors. There is deliberately no time bound: console delivery to
// Node can lag the page-side attempt, and a bound would turn a delayed
// expected miss into a false error.
export function isExpectedWasmMiss(smoke, e) {
  const url = autoWasmUrl(smoke);
  return smoke?.wasmAuto === false && url !== null && (e.url === url || namesUrl(e.text ?? "", url));
}

// Whether `text` names `url` as a whole: some occurrence is not followed by a
// character that would make it a longer URL (`….wasm.map`, `….wasm/x`,
// `….wasm?v=2`). A dot counts as continuing only when a URL character follows
// it, so a sentence that ends "….wasm." still names the URL.
function namesUrl(text, url) {
  for (let at = text.indexOf(url); at !== -1; at = text.indexOf(url, at + 1)) {
    if (!/^(?:[\w~%/?#&=+@:-]|\.[\w~%-])/.test(text.slice(at + url.length))) return true;
  }
  return false;
}

async function waitForConsole(logs, match, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const hit = logs.find(match);
    if (hit) return hit;
    await new Promise((r) => setTimeout(r, 250));
  }
  return null;
}

async function drive({ fixture, dist, pkg, out }) {
  const distDir = resolve(dist);
  const pkgDir = resolve(pkg);
  if (!existsSync(join(distDir, "index.html"))) throw new Error(`${distDir} has no index.html — build the fixture first`);
  if (!existsSync(join(pkgDir, "index.js"))) throw new Error(`${pkgDir} is not an unpacked wordcraft-web package`);

  const { server, port } = await startServer(distDir, pkgDir);
  const executablePath = findChrome();
  const consoleLog = [];
  const pageErrors = [];
  const responses = new Map();
  const requestFailures = [];

  const browser = await puppeteer.launch({
    executablePath,
    headless: true,
    args: ["--hide-scrollbars", "--mute-audio", "--window-size=1400,900"],
  });
  try {
    const page = await browser.newPage();
    page.on("console", (msg) => {
      consoleLog.push({ type: msg.type(), text: msg.text(), url: msg.location()?.url ?? "", t: Date.now() });
    });
    page.on("pageerror", (err) => {
      pageErrors.push({ text: String(err), t: Date.now() });
    });
    page.on("response", (r) => {
      responses.set(r.url(), r.status());
    });
    page.on("requestfailed", (r) => {
      requestFailures.push({ url: r.url(), error: r.failure()?.errorText ?? "?", t: Date.now() });
    });
    // A French interface makes the shell fetch i18n/fr/*.tsv through assetBase;
    // both the real-world trigger (navigator languages) and a stored
    // preference are set, since either path proves the runtime file resolves.
    await page.evaluateOnNewDocument(() => {
      try {
        Object.defineProperty(navigator, "language", { get: () => "fr-FR" });
        Object.defineProperty(navigator, "languages", { get: () => ["fr-FR", "fr", "en"] });
        localStorage.setItem("wordcraft.ui", JSON.stringify({ language: "fr" }));
      } catch {
        /* storage or navigator overrides unavailable */
      }
    });
    // `?proof-worker` makes the shell run one known-bad `check` through the
    // dedicated proofing worker when it starts — on the first edit, which the
    // fixture page makes (fixtures/main.shared.js).
    await page.goto(`http://127.0.0.1:${port}/?proof-worker`, { waitUntil: "domcontentloaded", timeout: 60_000 });

    await page.waitForFunction(() => globalThis.__SMOKE && (globalThis.__SMOKE.phase === "done" || globalThis.__SMOKE.phase === "failed"), {
      timeout: 240_000,
      polling: 500,
    });
    const smoke = await page.evaluate(() => globalThis.__SMOKE);

    // The worker's smoke check runs after the fixture's first edit; give it time.
    const workerLine = await waitForConsole(consoleLog, (m) => /proofing worker: \d+ issue\(s\) in the smoke check/.test(m.text), 90_000);

    // Malformed messages must get a typed `error` reply, never silence or a throw out of
    // onmessage (each worker's `unpack`): post junk straight at every dedicated worker's
    // loader, plus one well-formed ping to prove the worker still answers.
    const malformed = await page.evaluate(async (assetBase) => {
      const probe = async (file) => {
        const worker = new Worker(`${assetBase}${file}`, { type: "module" });
        try {
          const replies = [];
          const errors = [];
          worker.onmessage = (e) => replies.push(e.data);
          worker.onerror = (e) => errors.push(String(e.message ?? e));
          const kindOf = (d) => {
            try {
              return JSON.parse(d[0]).kind;
            } catch {
              return null;
            }
          };
          const codeOf = (d) => {
            try {
              return JSON.parse(new TextDecoder().decode(d[1] ?? new ArrayBuffer(0))).code;
            } catch {
              return null;
            }
          };
          // The wasm binds asynchronously: `ready` is posted only once `onmessage` is set, so
          // messages sent before it would be dispatched to nothing and dropped.
          const bootDeadline = Date.now() + 30_000;
          while (Date.now() < bootDeadline && !replies.some((d) => kindOf(d) === "ready")) {
            await new Promise((r) => setTimeout(r, 100));
          }
          worker.postMessage(null);
          worker.postMessage("not-an-array");
          worker.postMessage([null, new ArrayBuffer(8)]);
          worker.postMessage([{ v: 1, id: 7, kind: "ping", revision: 0 }, "not-bytes"]);
          worker.postMessage(['{"v":1,"id":9,"kind":"ping","revision":0}', new Uint8Array(0)]);
          const deadline = Date.now() + 30_000;
          const badRequests = () => replies.filter((d) => kindOf(d) === "error" && codeOf(d) === "badRequest").length;
          while (Date.now() < deadline && (badRequests() < 4 || !replies.some((d) => kindOf(d) === "pong"))) {
            await new Promise((r) => setTimeout(r, 100));
          }
          return {
            badRequests: badRequests(),
            pong: replies.some((d) => kindOf(d) === "pong" && JSON.parse(d[0]).id === 9),
            kinds: replies.map(kindOf),
            errors,
          };
        } finally {
          worker.terminate();
        }
      };
      return {
        proof: await probe("wordcraft-worker-proof_loader.js"),
        codecs: await probe("wordcraft-worker-codecs_loader.js"),
        pdf: await probe("wordcraft-worker-pdf_loader.js"),
      };
    }, "/wordcraft-web/");
    const malformedOk = (m) => m !== null && m.badRequests >= 4 && m.pong === true;

    const status = (substr) => {
      for (const [url, code] of responses) {
        if (url.includes(substr)) return code;
      }
      return null;
    };
    const got = (substr) => status(substr) === 200;

    const consoleErrors = [
      ...consoleLog.filter((m) => (m.type === "error" || m.type === "assert") && !isExpectedWasmMiss(smoke, m)).map((m) => m.text),
      ...pageErrors.map((e) => e.text),
    ];

    const workerFetchOk = got("/wordcraft-web/wordcraft-worker-proof_loader.js") && got("/wordcraft-web/wordcraft-worker-proof_bg.wasm");
    const report = {
      fixture,
      chrome: executablePath,
      version: await browser.version(),
      startOk: smoke?.startOk === true,
      phase: smoke?.phase ?? "missing",
      wasmAuto: smoke?.wasmAuto === true,
      autoError: smoke?.autoError ?? null,
      fatal: smoke?.fatal ?? null,
      locale: got("/wordcraft-web/i18n/fr/common.tsv") && got("/wordcraft-web/i18n/fr/wordcraft.tsv"),
      fontManifest: got("/wordcraft-web/fonts/manifest.json"),
      fontPack: Array.isArray(smoke?.fonts?.installed) && smoke.fonts.installed.some((f) => f.includes("JetBrainsMono")),
      fontPackFailed: smoke?.fonts?.failed ?? [],
      workerFetch: workerFetchOk,
      workerReady: workerLine !== null,
      workerLine: workerLine?.text ?? null,
      workerMalformed: malformed,
      workerMalformedOk: malformedOk(malformed?.proof) && malformedOk(malformed?.codecs) && malformedOk(malformed?.pdf),
      exportWorker: smoke?.exportWorker === true,
      workerFailed: consoleLog.some((m) => /proofing worker.*(fail|error)/i.test(m.text)),
      consoleErrors,
      httpMisses: [...responses.entries()].filter(([url, code]) => code >= 400 && !url.includes("favicon")).map(([url, code]) => `${code} ${url}`),
      requestFailures: requestFailures.map((r) => `${r.error} ${r.url}`),
    };
    report.pass =
      report.startOk && report.locale && report.fontPack && report.fontPackFailed.length === 0 && report.workerReady && report.exportWorker && report.workerMalformedOk && !report.workerFailed && report.consoleErrors.length === 0;

    if (out) {
      mkdirSync(resolve(out, ".."), { recursive: true });
      writeFileSync(out, JSON.stringify(report, null, 2) + "\n");
    }
    console.log(JSON.stringify(report, null, 2));
    if (!report.pass) process.exitCode = 1;
  } finally {
    await browser.close().catch(() => {});
    server.close();
  }
}

function report(resultsDir) {
  const rows = [];
  for (const name of ["vite", "webpack", "esbuild"]) {
    const file = join(resultsDir, `${name}.json`);
    if (!existsSync(file)) {
      rows.push({ fixture: name, missing: true });
      continue;
    }
    rows.push(JSON.parse(readFileSync(file, "utf8")));
  }
  const yes = (v) => (v === true ? "yes" : v === false ? "no" : "—");
  const lines = [
    "| Bundler | wasm found (no config) | start() | locale (fr) | font pack | proof worker | console errors |",
    "|---|---|---|---|---|---|---|",
    ...rows.map((r) =>
      r.missing
        ? `| ${r.fixture} | — | — | — | — | — | not run |`
        : `| ${r.fixture} | ${yes(r.wasmAuto)} | ${yes(r.startOk)} | ${yes(r.locale)} | ${yes(r.fontPack)} | ${yes(r.workerReady)} | ${r.consoleErrors.length === 0 ? "none" : r.consoleErrors.length} |`,
    ),
  ];
  return lines.join("\n") + "\n";
}

// Run only when invoked as a script, so drive.test.mjs can import the helpers.
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const args = parseArgs(process.argv);
  if (args.report) {
    process.stdout.write(report(resolve(args.results ?? join(HERE, "results"))));
  } else {
    await drive(args);
  }
}
