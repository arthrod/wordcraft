#!/usr/bin/env node
// Headless-Chrome checks for the viewer-first page (plan 10.3) against a Trunk build.
//
//   node viewer.mjs --dist ../../../dist/web --core <dir> [--out results/viewer.json]
//
// `--core` is a directory holding the view-only core built for Node
// (`wasm-bindgen --target nodejs` output of `wordcraft-viewer`): the reference the
// painted canvas is compared with, byte for byte. Without it that one check is skipped
// and reported as such.
//
// Checks (the acceptance criteria of plan step 10.3):
//   AC1  a first visit with no input fetches the page module and the viewer worker,
//        and nothing of the editor or of the proofing, codecs or PDF workers
//   AC2  page 0 is painted, is not blank, and its canvas holds exactly the RGBA the
//        view-only core returns for that page at that scale
//   AC3  after the edit action the editor is fetched, shows the same document and
//        accepts typing; the viewer is gone
//   AC4  transfer sizes (raw, gzip -9, Brotli q11) of the first load and of the edit step
//   AC5  without WebGL, or with the editor's glue missing, the viewer stays, a notice
//        shows, and nothing is thrown uncaught
//   AC6  served with the deployed Content-Security-Policy (`_headers`, plus the inline
//        bootstrap's hash as `package.sh` adds it), nothing is refused
//   and the three modes a host can choose (view, viewer-first, edit).
//
// Tooling-script JavaScript, like drive.mjs (see README.md).

import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
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
// answers matching URLs with 404; `noWebGl` makes every canvas refuse a WebGL context.
async function open(browser, base, path, { blocked = null, noWebGl = false, width = 1280, height = 800 } = {}) {
  const page = await browser.newPage();
  await page.setViewport({ width, height, deviceScaleFactor: 1 });
  const log = { requests: [], consoleErrors: [], uncaught: [] };
  // Every Content-Security-Policy violation on the page (AC6).
  await page.evaluateOnNewDocument(() => {
    window.__cspViolations = [];
    addEventListener("securitypolicyviolation", (e) => window.__cspViolations.push(`${e.effectiveDirective} ${e.blockedURI}`));
  });
  page.on("request", (r) => log.requests.push(new URL(r.url()).pathname));
  page.on("console", (m) => m.type() === "error" && log.consoleErrors.push(m.text()));
  page.on("pageerror", (e) => log.uncaught.push(String(e?.message ?? e)));
  if (blocked) {
    await page.setRequestInterception(true);
    page.on("request", (r) => (blocked.test(new URL(r.url()).pathname) ? r.respond({ status: 404, body: "not found" }) : r.continue()));
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
  return { page, log };
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
  check("AC3 viewer removed", await first.page.evaluate(() => !document.querySelector("#wordcraft_viewer") && !document.querySelector("#wordcraft_viewer_loading")));
  const before = await first.page.evaluate(() => window.wordcraft.getMarkdown());
  check("AC3 editor shows the same document", before.includes(SAMPLE_TEXT), JSON.stringify(before.slice(0, 80)));
  await first.page.screenshot({ path: join(dirname(resolve(args.out ?? "results/viewer.json")), "viewer-editor.png") }).catch(() => {});
  await first.page.mouse.click(640, 520);
  await first.page.keyboard.type("Zebra42");
  await new Promise((r) => setTimeout(r, 800));
  const after = await first.page.evaluate(() => window.wordcraft.getMarkdown());
  check("AC3 editor accepts typing", after.includes("Zebra42") && !before.includes("Zebra42"));
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
} finally {
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
