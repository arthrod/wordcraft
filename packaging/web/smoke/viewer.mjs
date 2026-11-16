#!/usr/bin/env node
// Headless-Chrome checks for the viewer-first page (plan 10.3) against a Trunk build.
//
//   node viewer.mjs --dist ../../../dist/web --core <dir> [--font <file.ttf>] [--out results/viewer.json]
//
// `--core` is a directory holding the view-only core built for Node
// (`wasm-bindgen --target nodejs` output of `wordcraft-viewer`): the reference the
// painted canvas is compared with, byte for byte. Without it that one check is skipped
// and reported as such. `--font` is a TrueType file for the installFont check; without it a
// common system font is used, and with none found that check is skipped.
//
// Checks (the acceptance criteria of plan step 10.3):
//   AC1  a first visit with no input fetches the page module and the viewer worker,
//        and nothing of the editor or of the proofing, codecs or PDF workers
//   AC2  page 0 is painted, is not blank, and its canvas holds exactly the RGBA the
//        view-only core returns for that page at that scale
//   AC3  after the edit action the editor is fetched, shows the same document and
//        accepts typing; the viewer is gone
//   AC4  transfer sizes (raw, gzip -9, Brotli q11) of the first load and of the edit step
//   AC5  without WebGL, with the editor's glue or wasm missing, or with its download stalled
//        past the wait (editorWaitMs / data-editor-wait-ms), the viewer stays, a notice
//        shows, and nothing is thrown uncaught
//   AC6  served with the deployed Content-Security-Policy (`_headers`, plus the inline
//        bootstrap's hash as `package.sh` adds it), nothing is refused
//   and the three modes a host can choose (view, viewer-first, edit), an edit action while
//   the document still downloads, a document declared past the size limit, the viewer failing
//   for good (no viewer worker, not a .docx, a failed download) with Edit still offered, a
//   protected document (the password asked in the page), a
//   font installed just before another document opens, and the editor's parent bridge
//   (?host=parent).
//
// Tooling-script JavaScript, like drive.mjs (see README.md).

import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { brotliCompressSync, constants, gzipSync } from "node:zlib";
import puppeteer from "puppeteer-core";
import { findChrome, startServer } from "./drive.mjs";

const args = {};
for (let i = 2; i < process.argv.length; i += 2) args[process.argv[i].slice(2)] = process.argv[i + 1];
const dist = resolve(args.dist ?? "../../../dist/web");
// The editor's files: its glue, loader and module, and its wasm-bindgen snippets (eframe's).
const EDITOR = /\/wordcraft-web(_bg\.wasm|\.js|_loader\.js)$|\/snippets\/eframe-/;
const OTHER_WORKERS = /\/wordcraft-worker-(proof|codecs|pdf)/;
const SAMPLE_TEXT = "Welcome to WordCraftWeb";

const failures = [];
const report = {};
const violations = (page) => page.evaluate(() => window.__cspViolations ?? ["the violation recorder did not run"]);
function check(name, ok, detail = "") {
  console.log(`${ok ? "ok  " : "FAIL"} ${name}${detail ? `: ${detail}` : ""}`);
  if (!ok) failures.push(name);
  return ok;
}

// A page with its network log, console errors and uncaught errors recorded. `blocked`
// answers matching URLs with 404; `stalled` never answers them (a download that hangs);
// `noWebGl` makes every canvas refuse a WebGL context; `rootAttrs` are set on
// #wordcraft_viewer as the parser inserts it, before the page module starts. `hooks` lets a case
// hold the workers' replies (`window.__hold`, delivered in order by `window.__release()`) and make
// `postMessage` throw (`window.__failPost`); `holdFromStart` holds from the first one, `ready`.
async function open(browser, base, path, { blocked = null, stalled = null, noWebGl = false, rootAttrs = null, hooks = false, holdFromStart = false, width = 1280, height = 800 } = {}) {
  const page = await browser.newPage();
  await page.setViewport({ width, height, deviceScaleFactor: 1 });
  const log = { requests: [], consoleErrors: [], uncaught: [] };
  const held = [];
  // Every Content-Security-Policy violation on the page (AC6).
  await page.evaluateOnNewDocument(() => {
    window.__cspViolations = [];
    addEventListener("securitypolicyviolation", (e) => window.__cspViolations.push(`${e.effectiveDirective} ${e.blockedURI}`));
    // The viewer must never block on a browser dialog: record any call.
    window.__prompts = [];
    window.prompt = (...a) => (window.__prompts.push(a.join(" ")), null);
  });
  if (rootAttrs) {
    await page.evaluateOnNewDocument((attrs) => {
      new MutationObserver((_, observer) => {
        const root = document.getElementById("wordcraft_viewer");
        if (!root) return;
        for (const [k, v] of Object.entries(attrs)) root.setAttribute(k, v);
        observer.disconnect();
      }).observe(document, { childList: true, subtree: true });
    }, rootAttrs);
  }
  page.on("request", (r) => log.requests.push(new URL(r.url()).pathname));
  page.on("console", (m) => m.type() === "error" && log.consoleErrors.push(m.text()));
  page.on("pageerror", (e) => log.uncaught.push(String(e?.message ?? e)));
  if (blocked || stalled) {
    await page.setRequestInterception(true);
    page.on("request", (r) => {
      const path = new URL(r.url()).pathname;
      if (blocked?.test(path)) r.respond({ status: 404, body: "not found" });
      else if (!stalled?.test(path)) r.continue();
      else held.push(r); // unanswered until the case releases it

    });
  }
  if (hooks) {
    await page.evaluateOnNewDocument((hold) => {
      window.__hold = hold;
      window.__held = [];
      window.__failPost = false;
      window.__release = () => {
        window.__hold = false;
        for (const deliver of window.__held.splice(0)) deliver();
      };
      const handler = Object.getOwnPropertyDescriptor(Worker.prototype, "onmessage");
      Object.defineProperty(Worker.prototype, "onmessage", {
        configurable: true,
        get() {
          return handler.get.call(this);
        },
        set(fn) {
          const worker = this;
          handler.set.call(this, fn && ((e) => (window.__hold ? window.__held.push(() => fn.call(worker, e)) : fn.call(worker, e))));
        },
      });
      const post = Worker.prototype.postMessage;
      Worker.prototype.postMessage = function (...a) {
        if (window.__failPost) throw new DOMException("refused by the test", "DataCloneError");
        return post.apply(this, a);
      };
    }, holdFromStart);
  }
  if (noWebGl) {
    await page.evaluateOnNewDocument(() => {
      const real = HTMLCanvasElement.prototype.getContext;
      HTMLCanvasElement.prototype.getContext = function (kind, ...rest) {
        return /webgl/i.test(kind) ? null : real.call(this, kind, ...rest);
      };
    });
  }
  await page.goto(`${base}${path}`, { waitUntil: "load" });
  return { page, log, held };
}

const attr = (page, name) => page.evaluate((n) => document.documentElement.getAttribute(n), name);
const waitAttr = (page, name, value, timeout = 120000) =>
  page.waitForFunction((n, v) => document.documentElement.getAttribute(n) === v, { timeout }, name, value);
const idle = (page) => page.waitForNetworkIdle({ idleTime: 2000, timeout: 120000 });

function sizes(paths) {
  let total = { raw: 0, gzip: 0, brotli: 0 };
  const rows = [];
  for (const p of [...new Set(paths)].sort()) {
    const file = p === "/" ? "index.html" : p.slice(1);
    let bytes;
    try {
      bytes = readFileSync(join(dist, file));
    } catch {
      continue;
    }
    const row = {
      file,
      raw: bytes.length,
      gzip: gzipSync(bytes, { level: 9 }).length,
      brotli: brotliCompressSync(bytes, { params: { [constants.BROTLI_PARAM_QUALITY]: 11, [constants.BROTLI_PARAM_LGWIN]: 24, [constants.BROTLI_PARAM_SIZE_HINT]: bytes.length } }).length,
    };
    rows.push(row);
    for (const k of ["raw", "gzip", "brotli"]) total[k] += row[k];
  }
  return { rows, total };
}

function printSizes(title, { rows, total }) {
  console.log(`\n${title} (bytes: raw / gzip -9 / Brotli q11)`);
  for (const r of rows) console.log(`  ${r.file.padEnd(52)} ${String(r.raw).padStart(10)} ${String(r.gzip).padStart(10)} ${String(r.brotli).padStart(10)}`);
  console.log(`  ${"TOTAL".padEnd(52)} ${String(total.raw).padStart(10)} ${String(total.gzip).padStart(10)} ${String(total.brotli).padStart(10)}\n`);
}

// The canvas of page `index`: its size, the scale it says it was painted at, and its pixels.
async function readCanvas(page, index) {
  const got = await page.evaluate((i) => {
    const c = document.querySelector(`#wordcraft_viewer canvas[data-page="${i}"]`);
    if (!c || !c.hasAttribute("data-scale")) return null;
    const data = c.getContext("2d").getImageData(0, 0, c.width, c.height).data;
    let s = "";
    for (let at = 0; at < data.length; at += 0x8000) s += String.fromCharCode.apply(null, data.subarray(at, at + 0x8000));
    return { width: c.width, height: c.height, scale: Number(c.getAttribute("data-scale")), b64: btoa(s) };
  }, index);
  return got && { ...got, pixels: Buffer.from(got.b64, "base64") };
}

// The policy a deployment sends (AC6): `_headers`' Content-Security-Policy, plus the SHA-256 of
// Trunk's inline bootstrap, which `package.sh` adds the same way. Sent on every file, workers
// included, as the `/*` rule does.
function deployedCsp() {
  const headers = readFileSync(new URL("../_headers", import.meta.url), "utf8");
  const policy = headers.match(/Content-Security-Policy:\s*(.+)/)?.[1]?.trim();
  if (!policy) throw new Error("no Content-Security-Policy in packaging/web/_headers");
  const html = readFileSync(join(dist, "index.html"), "utf8");
  const hashes = [...html.matchAll(/<script\b[^>]*>([\s\S]*?)<\/script>/gi)]
    .map((m) => m[1])
    .filter((body) => body.trim().length > 0)
    .map((body) => `'sha256-${createHash("sha256").update(body, "utf8").digest("base64")}'`);
  return { policy: policy.replace("script-src 'self' 'wasm-unsafe-eval'", ["script-src 'self' 'wasm-unsafe-eval'", ...hashes].join(" ")), source: policy };
}
const csp = deployedCsp();
report.csp = csp;
console.log(`Content-Security-Policy served: ${csp.policy}`);

const { server, port } = await startServer(dist, dist, { headers: { "Content-Security-Policy": csp.policy } });
const base = `http://127.0.0.1:${port}`;
// Fixture routes the static server does not have, by path; anything else is served as before.
const fixtures = new Map();
const unfinished = [];
{
  const [serveStatic] = server.listeners("request");
  server.removeAllListeners("request");
  server.on("request", (req, res) => {
    const route = fixtures.get(new URL(req.url ?? "/", "http://localhost").pathname);
    if (route) route(res);
    else serveStatic(req, res);
  });
}
const browser = await puppeteer.launch({ executablePath: findChrome(), headless: true, args: ["--hide-scrollbars", "--mute-audio", "--window-size=1400,900"] });
try {
  // ---- AC1: first visit, no input ----
  const first = await open(browser, base, "/");
  await waitAttr(first.page, "data-wordcraft-painted", "true");
  await idle(first.page);
  const firstLoad = [...first.log.requests];
  report.firstLoadRequests = firstLoad;
  console.log("first-visit network log:\n  " + firstLoad.join("\n  "));
  check("AC1 page module fetched", firstLoad.some((p) => /\/wordcraft-viewer-web-[0-9a-f]+_bg\.wasm$/.test(p)) && firstLoad.some((p) => /\/wordcraft-viewer-web-[0-9a-f]+\.js$/.test(p)));
  check("AC1 viewer worker fetched", ["_loader.js", ".js", "_bg.wasm"].every((s) => firstLoad.includes(`/wordcraft-worker-viewer${s}`)));
  check("AC1 no editor file fetched", !firstLoad.some((p) => EDITOR.test(p)), firstLoad.filter((p) => EDITOR.test(p)).join(" "));
  check("AC1 no proof, codecs or pdf worker fetched", !firstLoad.some((p) => OTHER_WORKERS.test(p)));
  check("AC1 the password form takes no space until a document asks for one", await first.page.evaluate(() => document.querySelector("#wordcraft_viewer_password")?.getClientRects().length === 0));
  check("AC1 phase is viewing", (await attr(first.page, "data-wordcraft-phase")) === "viewing");
  check("AC1 no console or uncaught errors", first.log.consoleErrors.length + first.log.uncaught.length === 0, [...first.log.consoleErrors, ...first.log.uncaught].join(" | "));
  const firstViolations = await violations(first.page);
  check("AC6 first visit: no Content-Security-Policy violation", firstViolations.length === 0, firstViolations.join(" | "));

  // ---- AC2: the painted page equals the core's RGBA ----
  const shown = await readCanvas(first.page, 0);
  check("AC2 page 0 is painted", !!shown);
  if (shown) {
    let ink = 0;
    for (let i = 0; i < shown.pixels.length; i += 4) if (shown.pixels[i] !== 255 || shown.pixels[i + 1] !== 255 || shown.pixels[i + 2] !== 255) ink++;
    report.page0 = { width: shown.width, height: shown.height, scale: shown.scale, inkPixels: ink };
    check("AC2 page 0 is not blank", ink > 1000, `${shown.width}x${shown.height} at scale ${shown.scale}, ${ink} pixels are not white`);
    if (args.core) {
      const { Viewer } = createRequire(import.meta.url)(resolve(args.core, "wordcraft-viewer.js"));
      const viewer = Viewer.open(readFileSync(join(dist, "samples/welcome.docx")), undefined);
      const image = viewer.render(0, shown.scale);
      const [w, h] = [image.width, image.height];
      const want = Buffer.from(image.pixels());
      let differing = 0;
      if (want.length === shown.pixels.length) for (let i = 0; i < want.length; i++) if (want[i] !== shown.pixels[i]) differing++;
      report.page0.core = { width: w, height: h, bytes: want.length, differingBytes: differing };
      check("AC2 canvas size equals the core's raster", w === shown.width && h === shown.height, `core ${w}x${h}, canvas ${shown.width}x${shown.height}`);
      check("AC2 canvas bytes equal the core's RGBA, byte for byte", want.length === shown.pixels.length && differing === 0, `${want.length} bytes compared, ${differing} differ`);
      report.pageCount = viewer.pageCount();
    } else {
      console.log("skip AC2 byte comparison: no --core given");
    }
  }
  const canvases = await first.page.evaluate(() => [...document.querySelectorAll("#wordcraft_viewer canvas")].map((c) => [Number(c.dataset.page), c.hasAttribute("data-scale")]));
  const pages = Number(await first.page.evaluate(() => document.querySelector("#wordcraft_viewer").dataset.pages));
  report.canvases = canvases;
  console.log(`pages in the document: ${pages}; canvases mounted [page, painted]: ${JSON.stringify(canvases)}`);

  // ---- AC4 (first load) ----
  report.firstLoad = sizes(firstLoad);
  printSizes("AC4 first load", report.firstLoad);

  // ---- AC3: the edit action ----
  await first.page.click("#wordcraft_viewer_edit");
  const loadingSeen = await first.page.evaluate(() => [document.documentElement.getAttribute("data-wordcraft-phase"), !!document.querySelector("#wordcraft_viewer canvas[data-scale]")]);
  check("AC3 viewer stays while the editor loads", loadingSeen[0] !== "editing" && loadingSeen[1], JSON.stringify(loadingSeen));
  await waitAttr(first.page, "data-wordcraft-phase", "editing");
  await idle(first.page);
  const editStep = first.log.requests.slice(firstLoad.length);
  report.editStepRequests = editStep;
  console.log("edit-step network log:\n  " + editStep.join("\n  "));
  check("AC3 editor module fetched on the edit action", ["_loader.js", ".js", "_bg.wasm"].every((s) => editStep.includes(`/wordcraft-web${s}`)));
  // The proofing worker starts on the first edit, not when the editor starts.
  check("AC3 edit step fetches no proof worker before typing", !editStep.some((p) => /\/wordcraft-worker-proof/.test(p)), editStep.filter((p) => /proof/.test(p)).join(" "));
  check("AC3 viewer removed", await first.page.evaluate(() => !document.querySelector("#wordcraft_viewer") && !document.querySelector("#wordcraft_viewer_loading")));
  const before = await first.page.evaluate(() => window.wordcraft.getMarkdown());
  check("AC3 editor shows the same document", before.includes(SAMPLE_TEXT), JSON.stringify(before.slice(0, 80)));
  await first.page.screenshot({ path: join(dirname(resolve(args.out ?? "results/viewer.json")), "viewer-editor.png") }).catch(() => {});
  await first.page.mouse.click(640, 520);
  await first.page.keyboard.type("Zebra42");
  await new Promise((r) => setTimeout(r, 800));
  const after = await first.page.evaluate(() => window.wordcraft.getMarkdown());
  check("AC3 editor accepts typing", after.includes("Zebra42") && !before.includes("Zebra42"));
  const typingStep = first.log.requests.slice(firstLoad.length + editStep.length);
  report.typingStepRequests = typingStep;
  console.log("after typing, network log:\n  " + typingStep.join("\n  "));
  check("AC3 no uncaught errors", first.log.uncaught.length === 0, first.log.uncaught.join(" | "));
  const editViolations = await violations(first.page);
  check("AC6 after the edit step: no Content-Security-Policy violation", editViolations.length === 0, editViolations.join(" | "));
  report.editStep = sizes(editStep);
  printSizes("AC4 edit step", report.editStep);
  await first.page.close();

  // ---- AC5: the editor cannot start ----
  const noGl = await open(browser, base, "/", { noWebGl: true });
  await waitAttr(noGl.page, "data-wordcraft-painted", "true");
  await noGl.page.click("#wordcraft_viewer_edit");
  await waitAttr(noGl.page, "data-wordcraft-phase", "viewer-only", 20000);
  await idle(noGl.page);
  const noGlState = await noGl.page.evaluate(() => ({
    notice: document.querySelector("#wordcraft_viewer_notice")?.innerText ?? "",
    hidden: document.querySelector("#wordcraft_viewer_notice")?.hidden,
    reason: document.documentElement.getAttribute("data-wordcraft-notice"),
    painted: document.querySelectorAll("#wordcraft_viewer canvas[data-scale]").length,
  }));
  console.log("without WebGL:", JSON.stringify(noGlState));
  check("AC5 without WebGL the viewer stays", noGlState.painted > 0);
  check("AC5 without WebGL the notice appears", noGlState.hidden === false && noGlState.reason === "no-webgl" && noGlState.notice.length > 0);
  check("AC5 without WebGL nothing of the editor is fetched", !noGl.log.requests.some((p) => EDITOR.test(p)));
  check("AC5 without WebGL no console or uncaught error", noGl.log.consoleErrors.length + noGl.log.uncaught.length === 0, [...noGl.log.consoleErrors, ...noGl.log.uncaught].join(" | "));
  await noGl.page.close();

  for (const [what, blocked, reason] of [
    ["editor glue 404", /\/wordcraft-web\.js$/, "load-failed"],
    ["editor wasm 404", /\/wordcraft-web_bg\.wasm$/, "start-failed"],
  ]) {
    const miss = await open(browser, base, "/", { blocked });
    await waitAttr(miss.page, "data-wordcraft-painted", "true");
    await miss.page.click("#wordcraft_viewer_edit");
    await waitAttr(miss.page, "data-wordcraft-phase", "viewer-only", 60000).catch(() => {});
    const state = await miss.page.evaluate(() => ({
      phase: document.documentElement.getAttribute("data-wordcraft-phase"),
      reason: document.documentElement.getAttribute("data-wordcraft-notice"),
      notice: document.querySelector("#wordcraft_viewer_notice")?.innerText ?? "",
      painted: document.querySelectorAll("#wordcraft_viewer canvas[data-scale]").length,
    }));
    console.log(`${what}:`, JSON.stringify(state), "console:", JSON.stringify(miss.log.consoleErrors), "uncaught:", JSON.stringify(miss.log.uncaught));
    check(`AC5 ${what}: the viewer stays with a notice`, state.phase === "viewer-only" && state.reason === reason && state.painted > 0 && state.notice.length > 0);
    check(`AC5 ${what}: no uncaught error`, miss.log.uncaught.length === 0, miss.log.uncaught.join(" | "));
    await miss.page.close();
  }

  // The editor's download never finishes: the bounded wait ends in the notice.
  {
    const waitMs = 3000;
    const stall = await open(browser, base, "/", { stalled: /\/wordcraft-web_bg\.wasm$/, rootAttrs: { "data-editor-wait-ms": String(waitMs) } });
    await waitAttr(stall.page, "data-wordcraft-painted", "true");
    const t0 = Date.now();
    await stall.page.click("#wordcraft_viewer_edit");
    await waitAttr(stall.page, "data-wordcraft-phase", "viewer-only", 30000).catch(() => {});
    const waited = Date.now() - t0;
    const state = await stall.page.evaluate(() => ({
      phase: document.documentElement.getAttribute("data-wordcraft-phase"),
      reason: document.documentElement.getAttribute("data-wordcraft-notice"),
      notice: document.querySelector("#wordcraft_viewer_notice")?.innerText ?? "",
      painted: document.querySelectorAll("#wordcraft_viewer canvas[data-scale]").length,
    }));
    console.log(`editor wasm stalled (wait ${waitMs} ms, gave up after ${waited} ms):`, JSON.stringify(state));
    check("AC5 editor download stalls: the wait ends in the viewer with a notice", state.phase === "viewer-only" && state.reason === "timed-out" && state.painted > 0 && state.notice.length > 0);
    check("AC5 editor download stalls: not before the wait", waited >= waitMs - 100, `${waited} ms`);
    // The download then finishes: the late editor takes over instead of running unseen.
    for (const r of stall.held.splice(0)) await r.continue();
    await waitAttr(stall.page, "data-wordcraft-phase", "editing", 120000).catch(() => {});
    const late = await stall.page.evaluate(async () => ({
      phase: document.documentElement.getAttribute("data-wordcraft-phase"),
      notice: document.documentElement.getAttribute("data-wordcraft-notice"),
      viewer: !!document.querySelector("#wordcraft_viewer"),
      // An editor running hidden with no document rejects: report that, not a crash.
      markdown: await Promise.resolve(window.wordcraft?.getMarkdown?.()).catch((e) => `rejected: ${e?.message ?? e}`),
    }));
    console.log("editor download answered late:", JSON.stringify({ ...late, markdown: String(late.markdown).slice(0, 40) }));
    check("AC5 editor download answered late: the editor takes over, notice cleared, viewer gone", late.phase === "editing" && late.notice === null && !late.viewer, JSON.stringify({ ...late, markdown: undefined }));
    check("AC5 editor download answered late: it shows the same document", typeof late.markdown === "string" && late.markdown.includes(SAMPLE_TEXT));
    check("AC5 editor download stalls: no uncaught error", stall.log.uncaught.length === 0, stall.log.uncaught.join(" | "));
    await stall.page.close();
  }

  // ---- an edit action while the document is still downloading ----
  {
    const slow = await open(browser, base, "/?doc=samples/welcome.docx", { stalled: /\/samples\/welcome\.docx$/ });
    await slow.page.waitForFunction(() => window.wordcraftViewer, { timeout: 30000 }).catch(() => {});
    const before = slow.log.requests.length;
    await slow.page.evaluate(() => window.wordcraftViewer?.edit());
    await new Promise((r) => setTimeout(r, 1500));
    const waiting = await slow.page.evaluate(() => ({
      phase: document.documentElement.getAttribute("data-wordcraft-phase"),
      edit: (document.querySelector("#wordcraft_viewer_edit")?.getClientRects().length ?? 0) > 0,
    }));
    check("slow document: no edit action before it is open", waiting.phase === "viewing" && !waiting.edit && !slow.log.requests.slice(before).some((p) => EDITOR.test(p)), JSON.stringify(waiting));
    for (const r of slow.held.splice(0)) await r.continue();
    await waitAttr(slow.page, "data-wordcraft-painted", "true", 30000).catch(() => {});
    const edit = await slow.page.evaluate(() => (document.querySelector("#wordcraft_viewer_edit")?.getClientRects().length ?? 0) > 0);
    check("slow document: Edit is offered once it is open", edit);
    await slow.page.click("#wordcraft_viewer_edit").catch(() => {});
    await waitAttr(slow.page, "data-wordcraft-phase", "editing", 120000).catch(() => {});
    const handed = await slow.page.evaluate(() => Promise.resolve(window.wordcraft?.getMarkdown?.()).catch(() => null));
    check("slow document: the editor shows that document", typeof handed === "string" && handed.includes(SAMPLE_TEXT), JSON.stringify(String(handed).slice(0, 60)));
    check("slow document: no uncaught errors", slow.log.uncaught.length === 0, slow.log.uncaught.join(" | "));
    await slow.page.close();
  }

  // ---- a document whose declared length is past the viewer's limit ----
  {
    // 100 MiB declared, 1 KiB sent, and the rest never: refused before the body is read, so
    // the notice comes at once instead of after (or never after) a full download.
    fixtures.set("/__fixture/oversized.docx", (res) => {
      unfinished.push(res);
      res.writeHead(200, { "Content-Type": "application/octet-stream", "Content-Length": String(100 * 1024 * 1024) });
      res.write(Buffer.alloc(1024));
    });
    const big = await open(browser, base, "/?doc=/__fixture/oversized.docx");
    await waitAttr(big.page, "data-wordcraft-error", "document-failed", 15000).catch(() => {});
    const state = await big.page.evaluate(() => ({
      error: document.documentElement.getAttribute("data-wordcraft-error"),
      notice: document.querySelector("#wordcraft_viewer_notice")?.hidden === false ? document.querySelector("#wordcraft_viewer_notice").innerText : null,
    }));
    check("oversized Content-Length: refused before the body with the too-large notice", state.error === "document-failed" && /too large/i.test(state.notice ?? ""), JSON.stringify(state));
    check("oversized Content-Length: no uncaught errors", big.log.uncaught.length === 0, big.log.uncaught.join(" | "));
    await big.page.close();
  }

  // ---- the viewer fails for good: the editor is still offered and fetches the document ----
  {
    const editVisible = (p) => p.evaluate(() => (document.querySelector("#wordcraft_viewer_edit")?.getClientRects().length ?? 0) > 0);
    const noWorker = await open(browser, base, "/", { blocked: /\/wordcraft-worker-viewer_loader\.js$/ });
    await waitAttr(noWorker.page, "data-wordcraft-error", "viewer-failed", 30000).catch(() => {});
    await noWorker.page.waitForFunction(() => (document.querySelector("#wordcraft_viewer_edit")?.getClientRects().length ?? 0) > 0, { timeout: 10000 }).catch(() => {});
    check("viewer worker missing: Edit is offered", await editVisible(noWorker.page));
    await noWorker.page.click("#wordcraft_viewer_edit").catch(() => {});
    await waitAttr(noWorker.page, "data-wordcraft-phase", "editing", 120000).catch(() => {});
    const handed = await noWorker.page.evaluate(() => Promise.resolve(window.wordcraft?.getMarkdown?.()).catch(() => null));
    check("viewer worker missing: the editor shows the document", typeof handed === "string" && handed.includes(SAMPLE_TEXT), JSON.stringify(String(handed).slice(0, 60)));
    check("viewer worker missing: no uncaught errors", noWorker.log.uncaught.length === 0, noWorker.log.uncaught.join(" | "));
    await noWorker.page.close();
    for (const [what, doc] of [
      ["not a .docx", "i18n/manifest.json"],
      ["download failed", "missing-document.docx"],
    ]) {
      const bad = await open(browser, base, `/?doc=${doc}`);
      await waitAttr(bad.page, "data-wordcraft-error", "document-failed", 30000).catch(() => {});
      await new Promise((r) => setTimeout(r, 500));
      check(`${what}: Edit is offered`, await editVisible(bad.page));
      await bad.page.close();
    }
  }

  // ---- a protected document: the password is asked for in the page ----
  {
    // The fixture: the sample, encrypted by the editor itself (file.encrypt, then exportDocx).
    const PASSWORD = "open sesame";
    const maker = await open(browser, base, "/?mode=edit");
    await waitAttr(maker.page, "data-wordcraft-phase", "editing");
    const locked = await maker.page.evaluate(async (password) => {
      const sample = new Uint8Array(await (await fetch("samples/welcome.docx")).arrayBuffer());
      await window.wordcraft.execute("file.new", {});
      await window.wordcraft.loadDocx(sample);
      await window.wordcraft.execute("file.encrypt", { password });
      return Array.from(await window.wordcraft.exportDocx());
    }, PASSWORD);
    await maker.page.close();
    check("password fixture: the editor wrote an encrypted package", locked.length > 0 && locked[0] === 0xd0 && locked[1] === 0xcf, `${locked.length} bytes`);

    // Edit pressed before the worker answers that the new document needs a password: refused,
    // so the editor is never handed the protected bytes without it.
    {
      const race = await open(browser, base, "/");
      await waitAttr(race.page, "data-wordcraft-painted", "true");
      const before = race.log.requests.length;
      await race.page.evaluate((bytes) => {
        window.wordcraftViewer.open(new Uint8Array(bytes)).catch(() => {});
        window.wordcraftViewer.edit();
      }, locked);
      await race.page.waitForFunction(() => (document.querySelector("#wordcraft_viewer_password")?.getClientRects().length ?? 0) > 0, { timeout: 20000 }).catch(() => {});
      await new Promise((r) => setTimeout(r, 1000));
      const state = await race.page.evaluate(() => ({
        phase: document.documentElement.getAttribute("data-wordcraft-phase"),
        form: (document.querySelector("#wordcraft_viewer_password")?.getClientRects().length ?? 0) > 0,
      }));
      check("password: Edit before the worker answers loads no editor", state.phase === "viewing" && state.form && !race.log.requests.slice(before).some((p) => EDITOR.test(p)), JSON.stringify(state));
      await race.page.close();
      // And once an edit action is taken, no other document is opened under it.
      const busy = await open(browser, base, "/");
      await waitAttr(busy.page, "data-wordcraft-painted", "true");
      const during = await busy.page.evaluate(async (bytes) => {
        window.wordcraftViewer.edit();
        return window.wordcraftViewer.open(new Uint8Array(bytes)).then(() => "resolved", (e) => `rejected: ${e.message}`);
      }, locked);
      check("open() while the editor takes over is refused", during.startsWith("rejected:"), during);
      await busy.page.close();
    }

    const prot = await open(browser, base, "/");
    await waitAttr(prot.page, "data-wordcraft-painted", "true");
    const opened = await prot.page.evaluate(async (bytes) => {
      try {
        await window.wordcraftViewer.open(new Uint8Array(bytes));
        return "resolved";
      } catch (e) {
        return `rejected: ${e.message}`;
      }
    }, locked);
    const ask = () =>
      prot.page.evaluate(() => ({
        edit: (document.querySelector("#wordcraft_viewer_edit")?.getClientRects().length ?? 0) > 0,
        notice: document.querySelector("#wordcraft_viewer_notice")?.hidden === false ? document.querySelector("#wordcraft_viewer_notice").innerText : null,
        error: document.documentElement.getAttribute("data-wordcraft-error"),
        // Shown, not merely without the attribute: the form must take no space when hidden.
        form: (document.querySelector("#wordcraft_viewer_password")?.getClientRects().length ?? 0) > 0,
        focused: document.activeElement?.id ?? "",
        painted: document.querySelectorAll("#wordcraft_viewer canvas[data-scale]").length,
      }));
    const first = await ask();
    console.log("protected document:", opened, JSON.stringify(first));
    check("password: open() of a protected document rejects", opened.startsWith("rejected:"), opened);
    check("password: the in-page form is shown and focused", first.error === "password-required" && first.form && first.focused === "wordcraft_viewer_password_input", JSON.stringify(first));
    // No edit action while the password is asked for: Edit is gone, and a double click,
    // Enter, F2 and edit() do nothing (no editor file is fetched).
    const before = prot.log.requests.length;
    await prot.page.click("#wordcraft_viewer [role=document]", { clickCount: 2 });
    await prot.page.focus("#wordcraft_viewer [role=document]");
    await prot.page.keyboard.press("Enter");
    await prot.page.keyboard.press("F2");
    await prot.page.evaluate(() => window.wordcraftViewer.edit());
    await new Promise((r) => setTimeout(r, 1500));
    const lockedState = await ask();
    check("password: no edit action while it is asked for", !lockedState.edit && (await attr(prot.page, "data-wordcraft-phase")) === "viewing" && !prot.log.requests.slice(before).some((p) => EDITOR.test(p)), JSON.stringify(lockedState));
    // Cancel: the form goes, the notice says the document is protected, editing stays withdrawn.
    await prot.page.click("#wordcraft_viewer_password_cancel");
    const cancelled = await ask();
    check("password: Cancel leaves a 'protected' notice and no Edit", !cancelled.form && /protected/i.test(cancelled.notice ?? "") && !cancelled.edit, JSON.stringify(cancelled));
    // Opened again, it asks again.
    await prot.page.evaluate((bytes) => window.wordcraftViewer.open(new Uint8Array(bytes)).catch(() => {}), locked);
    await prot.page.waitForFunction(() => (document.querySelector("#wordcraft_viewer_password")?.getClientRects().length ?? 0) > 0, { timeout: 20000 }).catch(() => {});
    await prot.page.type("#wordcraft_viewer_password_input", "guess");
    await prot.page.keyboard.press("Enter");
    await waitAttr(prot.page, "data-wordcraft-error", "wrong-password", 20000).catch(() => {});
    const wrong = await ask();
    check("password: a wrong password asks again", wrong.error === "wrong-password" && wrong.form, JSON.stringify(wrong));
    await prot.page.type("#wordcraft_viewer_password_input", PASSWORD);
    await prot.page.keyboard.press("Enter");
    await prot.page
      .waitForFunction(() => !document.documentElement.hasAttribute("data-wordcraft-error") && document.querySelectorAll("#wordcraft_viewer canvas[data-scale]").length > 0, { timeout: 30000 })
      .catch(() => {});
    const right = await ask();
    check("password: the right password shows the document and hides the form", right.error === null && !right.form && right.painted > 0, JSON.stringify(right));
    check("password: Edit comes back once the document is open", right.edit, JSON.stringify(right));
    check("password: no browser dialog was used", (await prot.page.evaluate(() => window.__prompts.length)) === 0);
    // The editor gets the document with its password.
    await prot.page.click("#wordcraft_viewer_edit");
    await waitAttr(prot.page, "data-wordcraft-phase", "editing", 120000).catch(() => {});
    const handed = await prot.page.evaluate(() => window.wordcraft?.getMarkdown?.() ?? null);
    const phase = await attr(prot.page, "data-wordcraft-phase");
    check("password: the editor opens the protected document", phase === "editing" && typeof handed === "string" && handed.includes(SAMPLE_TEXT), `${phase} ${JSON.stringify(String(handed).slice(0, 60))}`);
    check("password: no uncaught errors", prot.log.uncaught.length === 0, prot.log.uncaught.join(" | "));
    await prot.page.close();
  }

  // ---- a font installed, then another document opened at once: the newer document wins ----
  {
    // A TrueType file the view-only core can read: `--font`, else a common system one.
    const fontPath = [args.font, "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", "/usr/share/fonts/TTF/DejaVuSans.ttf", "/System/Library/Fonts/Supplemental/Georgia.ttf"].find((p) => p && existsSync(p));
    if (!fontPath) {
      console.log("skip font-then-open: no font file found (pass --font <file.ttf>)");
    } else {
      fixtures.set("/__fixture/font.ttf", (res) => res.writeHead(200, { "Content-Type": "font/ttf" }).end(readFileSync(fontPath)));
      // The other document: blank, with page breaks, so its page count is not the sample's.
      const maker = await open(browser, base, "/?mode=edit");
      await waitAttr(maker.page, "data-wordcraft-phase", "editing");
      const other = await maker.page.evaluate(async () => {
        await window.wordcraft.execute("file.new", {});
        for (let i = 0; i < 5; i++) await window.wordcraft.execute("insert.pageBreak", {});
        return Array.from(await window.wordcraft.exportDocx());
      });
      await maker.page.close();
      const race = await open(browser, base, "/");
      await waitAttr(race.page, "data-wordcraft-painted", "true");
      // installFont's relayout of the shown document is answered first; the open after it must
      // still land, and its promise must say the new document's page count.
      const got = await race.page.evaluate(async (doc) => {
        const viewer = window.wordcraftViewer;
        const shown = viewer.pageCount();
        const font = new Uint8Array(await (await fetch("/__fixture/font.ttf")).arrayBuffer());
        const installed = viewer.installFont("Fixture Font", font).then(() => "ok", (e) => `rejected: ${e.message}`);
        const opened = viewer.open(new Uint8Array(doc)).then((a) => a.pageCount, (e) => `rejected: ${e.message}`);
        return { shown, installed: await installed, opened: await opened };
      }, other);
      await new Promise((r) => setTimeout(r, 1000));
      const after = await race.page.evaluate(() => ({ count: window.wordcraftViewer.pageCount(), pages: Number(document.querySelector("#wordcraft_viewer").dataset.pages) }));
      const detail = JSON.stringify({ font: fontPath, ...got, after });
      check("font then open: the open resolves with the new document", got.installed === "ok" && typeof got.opened === "number" && got.opened !== got.shown, detail);
      check("font then open: the page shows the new document", after.count === got.opened && after.pages === got.opened, detail);
      check("font then open: no uncaught errors", race.log.uncaught.length === 0, race.log.uncaught.join(" | "));
      await race.page.close();
    }
  }

  // ---- open() against the startup download, another open(), and the worker's replies ----
  {
    // Two blank documents with page breaks: 6 pages, and 41 (long enough to scroll away).
    const maker = await open(browser, base, "/?mode=edit");
    await waitAttr(maker.page, "data-wordcraft-phase", "editing");
    const [six, long] = await maker.page.evaluate(async () => {
      const make = async (breaks) => {
        await window.wordcraft.execute("file.new", {});
        for (let i = 0; i < breaks; i++) await window.wordcraft.execute("insert.pageBreak", {});
        return Array.from(await window.wordcraft.exportDocx());
      };
      return [await make(5), await make(40)];
    });
    await maker.page.close();
    const shown = (p) => p.evaluate(() => ({ count: window.wordcraftViewer.pageCount(), pages: Number(document.querySelector("#wordcraft_viewer").dataset.pages) }));

    // Pages answered after the reader scrolled away (their canvases gone) are painted again when
    // scrolled back to, not left blank.
    const away = await open(browser, base, "/", { hooks: true });
    await waitAttr(away.page, "data-wordcraft-painted", "true");
    await away.page.evaluate((doc) => window.wordcraftViewer.open(new Uint8Array(doc)), long);
    await new Promise((r) => setTimeout(r, 1500));
    const back = await away.page.evaluate(async () => {
      const frames = (n) => new Promise((done) => (function step(k) { k ? requestAnimationFrame(() => step(k - 1)) : done(); })(n));
      const scroller = document.querySelector("#wordcraft_viewer [role=document]");
      const at = Math.round((scroller.scrollHeight * 20) / 41);
      window.__hold = true;
      scroller.scrollTop = at;
      await frames(3);
      const asked = [...document.querySelectorAll("#wordcraft_viewer canvas:not([data-scale])")].map((c) => Number(c.dataset.page));
      scroller.scrollTop = scroller.scrollHeight;
      await frames(3);
      const gone = asked.every((i) => !document.querySelector(`#wordcraft_viewer canvas[data-page="${i}"]`));
      window.__release();
      await new Promise((r) => setTimeout(r, 1500));
      scroller.scrollTop = at;
      await new Promise((r) => setTimeout(r, 3000));
      const painted = asked.map((i) => document.querySelector(`#wordcraft_viewer canvas[data-page="${i}"]`)?.hasAttribute("data-scale") ?? false);
      return { asked, gone, painted };
    });
    check("a page answered after its canvas went is painted when shown again", back.asked.length > 0 && back.gone && back.painted.every(Boolean), JSON.stringify(back));
    check("scrolled away and back: no uncaught errors", away.log.uncaught.length === 0, away.log.uncaught.join(" | "));
    await away.page.close();
  }

  // ---- the modes a host can choose ----
  const viewOnly = await open(browser, base, "/?mode=view");
  await waitAttr(viewOnly.page, "data-wordcraft-painted", "true");
  await viewOnly.page.evaluate(() => window.wordcraftViewer.edit());
  await idle(viewOnly.page);
  check("mode=view offers no Edit control and never loads the editor", (await viewOnly.page.evaluate(() => document.querySelector("#wordcraft_viewer_edit").hidden)) && !viewOnly.log.requests.some((p) => EDITOR.test(p)) && (await attr(viewOnly.page, "data-wordcraft-phase")) === "viewing");
  await viewOnly.page.close();

  const editorFirst = await open(browser, base, "/?mode=edit&sample");
  await waitAttr(editorFirst.page, "data-wordcraft-phase", "editing");
  await idle(editorFirst.page);
  check("mode=edit loads the editor at once and no viewer worker", editorFirst.log.requests.some((p) => EDITOR.test(p)) && !editorFirst.log.requests.some((p) => p.includes("wordcraft-worker-viewer")), editorFirst.log.requests.filter((p) => p.includes("worker")).join(" "));
  check("mode=edit no uncaught errors", editorFirst.log.uncaught.length === 0, editorFirst.log.uncaught.join(" | "));
  await editorFirst.page.close();

  const editorFirstNoGl = await open(browser, base, "/?mode=edit", { noWebGl: true });
  await waitAttr(editorFirstNoGl.page, "data-wordcraft-painted", "true");
  check("mode=edit without WebGL mounts the viewer with the notice", (await attr(editorFirstNoGl.page, "data-wordcraft-notice")) === "no-webgl" && !editorFirstNoGl.log.requests.some((p) => EDITOR.test(p)) && editorFirstNoGl.log.uncaught.length === 0);
  await editorFirstNoGl.page.close();

  // ---- the editor's parent bridge (?host=parent) is the editor's page ----
  // A same-origin parent frames the app. As before the viewer existed: with WebGL the editor
  // announces itself and owns the frame; without it the editor is still fetched and tells the
  // parent it failed. Either way no sample is opened.
  const frameApp = (page) =>
    page.evaluate(
      () =>
        new Promise((done) => {
          const seen = [];
          addEventListener("message", (e) => {
            seen.push(e.data?.type ?? String(e.data));
            if (/^wordcraft:(failed|ready)$/.test(e.data?.type ?? "")) done(seen);
          });
          const frame = document.createElement("iframe");
          frame.src = "/?host=parent";
          frame.style.cssText = "width:1000px;height:700px;border:0";
          document.body.append(frame);
          setTimeout(() => done(seen), 60000);
        }),
    );
  {
    const parent = await open(browser, base, "/i18n/manifest.json");
    const heard = await frameApp(parent.page);
    const frame = parent.page.frames().find((f) => f !== parent.page.mainFrame());
    await frame?.waitForFunction(() => document.documentElement.getAttribute("data-wordcraft-phase") === "editing", { timeout: 60000 }).catch(() => {});
    const phase = await frame?.evaluate(() => document.documentElement.getAttribute("data-wordcraft-phase"));
    await idle(parent.page);
    const reqs = parent.log.requests;
    console.log("?host=parent, the parent heard:", JSON.stringify(heard), "frame phase:", phase);
    check("host=parent: the editor announces itself and owns the frame", heard.includes("wordcraft:ready") && phase === "editing", `${JSON.stringify(heard)} ${phase}`);
    check("host=parent: no viewer worker and no sample", !reqs.some((p) => p.includes("wordcraft-worker-viewer") || p.includes("/samples/")));
    check("host=parent: no uncaught errors", parent.log.uncaught.length === 0, parent.log.uncaught.join(" | "));
    await parent.page.close();
  }
  {
    const parent = await open(browser, base, "/i18n/manifest.json", { noWebGl: true });
    const heard = await frameApp(parent.page);
    await idle(parent.page);
    const reqs = parent.log.requests;
    console.log("?host=parent without WebGL, the parent heard:", JSON.stringify(heard));
    check("host=parent without WebGL: the editor tells the parent it failed", heard.includes("wordcraft:failed"), JSON.stringify(heard));
    check("host=parent: the editor is fetched and the viewer is not", reqs.some((p) => EDITOR.test(p)) && !reqs.some((p) => p.includes("wordcraft-worker-viewer")), reqs.filter((p) => /wordcraft-(web|worker-viewer)/.test(p)).join(" "));
    check("host=parent: no sample is opened", !reqs.some((p) => p.includes("/samples/")));
    check("host=parent without WebGL: no uncaught errors", parent.log.uncaught.length === 0, parent.log.uncaught.join(" | "));
    await parent.page.close();
  }
} finally {
  for (const res of unfinished) res.destroy();
  await browser.close();
  server.close();
}

report.failures = failures;
if (args.out) {
  mkdirSync(dirname(resolve(args.out)), { recursive: true });
  writeFileSync(resolve(args.out), JSON.stringify(report, null, 2));
}
console.log(failures.length === 0 ? "viewer-first checks: all passed" : `viewer-first checks: ${failures.length} FAILED: ${failures.join("; ")}`);
process.exit(failures.length === 0 ? 0 : 1);
