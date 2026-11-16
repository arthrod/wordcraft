#!/usr/bin/env node
// Lazy-loading checks for the standalone web app (implementation plan, Phase 10).
//
//   node lazy.mjs --site ../../../dist/web [--out results/lazy.json] [--only idle,edit,…]
//
// Serves a built site (the Trunk output: index.html beside the wasm, worker and
// runtime files), drives headless Chrome, and reads the server's own request
// log to say which files each use of the app fetched:
//
//   idle     load the page (blank, then `?sample`) and wait with no input: no
//            `wordcraft-worker-*` file is requested (10.1 AC1). Records the
//            first-load request list with sizes.
//   edit     `?sample&proof-worker`: nothing before typing; after typing a
//            misspelt word with real key events the proofing worker's files
//            are requested and a document paragraph comes back with an issue
//            (10.1 AC2).
//   open     drop an .rtf, an .html and an .odt file on the app (the queue the
//            file picker's answer and a drop share): each shows its text, and
//            the codecs worker's files are requested on the first of them and
//            not before (10.2 AC1).
//   pdf      File › Export's command for PDF: the download starts with `%PDF-`
//            and the PDF worker's files are requested then and not before
//            (10.2 AC2).
//   saveas   Save As .rtf and .odt: each download, opened again, reads back to
//            the same text (10.2 AC3).
//   missing  the codecs and PDF worker loaders answer 404: the app says so in
//            plain words, nothing throws, the document's text is unchanged
//            (10.2 AC4).
//
// Fixture text is written here (our own words); nothing is read from disk but
// the built site. Tooling-script JavaScript, as drive.mjs (see README.md).

import { existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { gzipSync } from "node:zlib";
import puppeteer from "puppeteer-core";
import { findChrome, startServer } from "./drive.mjs";

const RTF_TEXT = "Lazy rich text fixture for the codecs worker";
const HTML_TEXT = "Lazy web page fixture for the codecs worker";
const ODT_TEXT = "Lazy open document fixture for the codecs worker";
const SAVED_TEXT = "Round trip text written by the lazy smoke";
const KEPT_TEXT = "This text must survive a missing worker";

const enc = (s) => [...new TextEncoder().encode(s)];
const RTF = enc(`{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0 Arial;}}\\f0\\fs24 ${RTF_TEXT}\\par}`);
const HTML = enc(`<!DOCTYPE html><html><head><meta charset="utf-8"><title>Fixture</title></head><body><p>${HTML_TEXT}</p></body></html>`);

function parseArgs(argv) {
  const args = { site: null, out: null, only: null };
  for (let i = 2; i < argv.length; i++) args[argv[i].slice(2)] = argv[++i];
  return args;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function until(what, fn, timeoutMs = 90_000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await sleep(250);
  }
}

// One page on a fresh server log. `miss` makes matching paths 404. A setup that
// fails closes its page and server before throwing: the caller never got the
// `close`, and a listening server would keep Node running after the report.
async function open(browser, siteDir, query, miss) {
  const log = [];
  const { server, port } = await startServer(siteDir, null, { log, miss });
  let context = null;
  try {
    context = await browser.createBrowserContext();
    return await setUp(context, server, port, log, query);
  } catch (error) {
    await context?.close().catch(() => {});
    server.close();
    throw error;
  }
}

async function setUp(context, server, port, log, query) {
  const page = await context.newPage();
  await page.setCacheEnabled(false);
  const consoleLog = [];
  const pageErrors = [];
  page.on("console", (m) => consoleLog.push({ type: m.type(), text: m.text() }));
  page.on("pageerror", (e) => pageErrors.push(String(e)));
  await page.evaluateOnNewDocument(() => {
    // No save picker in a headless run: saves take the anchor-download path,
    // and the bytes each download carries are kept for the checks to read.
    window.showSaveFilePicker = undefined;
    window.__DOWNLOADS = [];
    const click = HTMLAnchorElement.prototype.click;
    HTMLAnchorElement.prototype.click = function () {
      if (this.download && this.href.startsWith("blob:")) {
        const name = this.download;
        fetch(this.href)
          .then((r) => r.arrayBuffer())
          .then((b) => window.__DOWNLOADS.push({ name, bytes: [...new Uint8Array(b)] }));
        return undefined;
      }
      return click.call(this);
    };
  });
  await page.setViewport({ width: 1400, height: 900 });
  await page.goto(`http://127.0.0.1:${port}/${query}`, { waitUntil: "domcontentloaded", timeout: 60_000 });
  await page.waitForFunction(() => typeof globalThis.wordcraft?.execute === "function", { timeout: 240_000, polling: 250 });
  const asked = (part) => log.filter((r) => r.path.includes(part)).map((r) => `${r.status} ${r.path}`);
  const text = () => page.evaluate(() => globalThis.wordcraft.getMarkdown());
  const said = (re) => consoleLog.find((m) => re.test(m.text))?.text ?? null;
  const close = async () => {
    await context.close().catch(() => {});
    server.close();
  };
  return { page, log, consoleLog, pageErrors, asked, text, said, close };
}

// Hand the app a file the way a drop does: the shell reads its bytes and queues
// it where the file picker's answer is queued too.
async function drop(page, name, bytes) {
  await page.evaluate(
    (name, bytes) => {
      const canvas = document.querySelector("canvas");
      const data = new DataTransfer();
      data.items.add(new File([new Uint8Array(bytes)], name));
      canvas.dispatchEvent(new DragEvent("drop", { bubbles: true, cancelable: true, dataTransfer: data }));
    },
    name,
    bytes,
  );
}

const download = (page, suffix) =>
  until(`a download ending ${suffix}`, () => page.evaluate((s) => globalThis.__DOWNLOADS.find((d) => d.name.endsWith(s)) ?? null, suffix));

function sizes(siteDir, log) {
  return log
    .filter((r) => r.status === 200)
    .map((r) => {
      const file = join(siteDir, r.path);
      const raw = existsSync(file) && statSync(file).isFile() ? readFileSync(file) : Buffer.alloc(0);
      return { path: r.path, raw: raw.length, gzip: gzipSync(raw, { level: 9 }).length };
    });
}

async function missingWasm(browser, siteDir) {
  const s = await open(browser, siteDir, "?sample", (path) => /wordcraft-worker-codecs_bg\.wasm$/.test(path));
  try {
    await drop(s.page, "fixture.rtf", RTF);
    return await until("the failed open's message", () => s.said(/Couldn't open fixture\.rtf: /), 60_000).catch(() => null);
  } finally {
    await s.close();
  }
}

const flows = {
  // 10.1 AC1: a page that is loaded and left alone fetches no worker.
  async idle(browser, siteDir) {
    const out = { pass: true, pages: {} };
    for (const query of ["", "?sample"]) {
      const s = await open(browser, siteDir, query);
      try {
        await sleep(8_000);
        const workers = s.asked("wordcraft-worker-");
        const list = sizes(siteDir, s.log);
        out.pages[query || "(blank)"] = {
          workerRequests: workers,
          requests: list,
          total: list.reduce((a, r) => ({ raw: a.raw + r.raw, gzip: a.gzip + r.gzip }), { raw: 0, gzip: 0 }),
          pageErrors: s.pageErrors,
        };
        if (workers.length > 0 || s.pageErrors.length > 0) out.pass = false;
      } finally {
        await s.close();
      }
    }
    return out;
  },

  // 10.1 AC2: the first typed word starts the proofing worker, and squiggles follow.
  async edit(browser, siteDir) {
    const s = await open(browser, siteDir, "?sample&proof-worker");
    try {
      await sleep(5_000);
      const before = s.asked("wordcraft-worker-proof");
      // Real key events: click into the page, then type a misspelt sentence.
      await s.page.mouse.click(700, 520);
      await s.page.keyboard.type(" Ths is a tst. ", { delay: 30 });
      const typed = await until("the typed text", async () => (await s.text()).includes("Ths is a tst."), 20_000).catch(() => false);
      const loader = await until("the proofing worker loader", () => s.asked("wordcraft-worker-proof_loader.js").length > 0, 30_000).catch(() => false);
      const wasm = await until("the proofing worker wasm", () => s.asked("wordcraft-worker-proof_bg.wasm").length > 0, 30_000).catch(() => false);
      const smokeLine = await until("the smoke check line", () => s.said(/proofing worker: [1-9]\d* issue\(s\) in the smoke check/), 90_000).catch(() => null);
      const squiggleLine = await until("an issue in a document paragraph", () => s.said(/proofing squiggles: [1-9]\d* issue\(s\) in a document paragraph/), 90_000).catch(
        () => null,
      );
      return {
        pass: before.length === 0 && typed === true && loader === true && wasm === true && smokeLine !== null && squiggleLine !== null && s.pageErrors.length === 0,
        proofRequestsBeforeTyping: before,
        typed,
        proofRequestsAfterTyping: s.asked("wordcraft-worker-proof"),
        smokeLine,
        squiggleLine,
        pageErrors: s.pageErrors,
      };
    } finally {
      await s.close();
    }
  },

  // 10.2 AC1: the app's Open reads the extra formats through the codecs worker,
  // fetched on the first such open and not before.
  async open(browser, siteDir) {
    const s = await open(browser, siteDir, "?sample");
    try {
      await sleep(4_000);
      const before = s.asked("wordcraft-worker-codecs");
      const shown = {};
      await drop(s.page, "fixture.rtf", RTF);
      shown.rtf = await until("the .rtf text", async () => (await s.text()).includes(RTF_TEXT), 120_000).catch(() => false);
      const afterFirst = s.asked("wordcraft-worker-codecs");
      await drop(s.page, "fixture.html", HTML);
      shown.html = await until("the .html text", async () => (await s.text()).includes(HTML_TEXT), 60_000).catch(() => false);
      // An .odt of our own text: written by the app's Save As, opened again by the drop.
      await s.page.evaluate((t) => globalThis.wordcraft.execute("document.setText", { text: t }), ODT_TEXT);
      await s.page.evaluate(() => globalThis.wordcraft.execute("file.saveAs", { path: "fixture.odt" }));
      const odt = await download(s.page, "fixture.odt");
      await s.page.evaluate(() => globalThis.wordcraft.execute("document.setText", { text: "cleared" }));
      await drop(s.page, "fixture.odt", odt.bytes);
      shown.odt = await until("the .odt text", async () => (await s.text()).includes(ODT_TEXT), 60_000).catch(() => false);
      // Opening and saving these formats is the codecs worker's job alone: the PDF worker stays unfetched.
      const pdfRequests = s.asked("wordcraft-worker-pdf");
      return {
        pass:
          before.length === 0 &&
          shown.rtf === true &&
          shown.html === true &&
          shown.odt === true &&
          afterFirst.length >= 3 &&
          pdfRequests.length === 0 &&
          s.pageErrors.length === 0,
        codecRequestsBeforeOpen: before,
        codecRequestsAfterFirstOpen: afterFirst,
        shown,
        said: s.consoleLog.filter((m) => /Opening |Opened |Couldn't /.test(m.text)).map((m) => m.text),
        pdfRequests,
        pageErrors: s.pageErrors,
      };
    } finally {
      await s.close();
    }
  },

  // 10.2 AC2: Export PDF goes through the PDF worker, fetched on that first export.
  async pdf(browser, siteDir) {
    const s = await open(browser, siteDir, "?sample");
    try {
      await sleep(4_000);
      const before = s.asked("wordcraft-worker-pdf");
      const answer = await s.page.evaluate(() => globalThis.wordcraft.execute("file.exportPdf", {}));
      const file = await download(s.page, ".pdf");
      const head = String.fromCharCode(...file.bytes.slice(0, 5));
      // The PDF worker reads the Word package itself: the codecs worker stays unfetched.
      const codecRequests = s.asked("wordcraft-worker-codecs");
      return {
        pass:
          before.length === 0 &&
          head === "%PDF-" &&
          file.bytes.length > 2_000 &&
          s.asked("wordcraft-worker-pdf").length >= 3 &&
          codecRequests.length === 0 &&
          s.pageErrors.length === 0,
        pdfRequestsBeforeExport: before,
        pdfRequestsAfterExport: s.asked("wordcraft-worker-pdf"),
        codecRequests,
        commandAnswer: answer,
        name: file.name,
        head,
        bytes: file.bytes.length,
        said: s.consoleLog.filter((m) => /Exporting |Exported |Couldn't /.test(m.text)).map((m) => m.text),
        pageErrors: s.pageErrors,
      };
    } finally {
      await s.close();
    }
  },

  // 10.2 AC3: Save As to a worker's format writes bytes that read back to the same text.
  async saveas(browser, siteDir) {
    const s = await open(browser, siteDir, "?sample");
    try {
      const out = { pass: true, formats: {}, pageErrors: s.pageErrors };
      for (const ext of ["rtf", "odt"]) {
        const text = `${SAVED_TEXT} (${ext})`;
        await s.page.evaluate((t) => globalThis.wordcraft.execute("document.setText", { text: t }), text);
        await s.page.evaluate((e) => globalThis.wordcraft.execute("file.saveAs", { path: `saved.${e}` }), ext);
        const file = await download(s.page, `saved.${ext}`);
        await s.page.evaluate(() => globalThis.wordcraft.execute("document.setText", { text: "cleared" }));
        await drop(s.page, `saved.${ext}`, file.bytes);
        const back = await until(`the saved .${ext} text`, async () => (await s.text()).includes(text), 60_000).catch(() => false);
        out.formats[ext] = { bytes: file.bytes.length, head: String.fromCharCode(...file.bytes.slice(0, 6)), readBack: back };
        if (back !== true || file.bytes.length === 0) out.pass = false;
      }
      if (s.pageErrors.length > 0) out.pass = false;
      return out;
    } finally {
      await s.close();
    }
  },

  // 10.2 AC4: with a worker's loader missing, the app says so and the document is unharmed.
  async missing(browser, siteDir) {
    const s = await open(browser, siteDir, "?sample", (path) => /wordcraft-worker-(codecs|pdf)_loader\.js$/.test(path));
    try {
      await s.page.evaluate((t) => globalThis.wordcraft.execute("document.setText", { text: t }), KEPT_TEXT);
      const before = await s.text();
      await drop(s.page, "fixture.rtf", RTF);
      const openLine = await until("the failed open's message", () => s.said(/Couldn't open fixture\.rtf: /), 60_000).catch(() => null);
      await s.page.evaluate(() => globalThis.wordcraft.execute("file.exportPdf", {}));
      const pdfLine = await until("the failed export's message", () => s.said(/Couldn't export .*\.pdf: /), 60_000).catch(() => null);
      await s.page.evaluate(() => globalThis.wordcraft.execute("file.saveAs", { path: "saved.rtf" }));
      const saveLine = await until("the failed save's message", () => s.said(/Couldn't export saved\.rtf: /), 60_000).catch(() => null);
      await sleep(1_000);
      const after = await s.text();
      const panics = s.consoleLog.filter((m) => /panicked|unreachable|RuntimeError/.test(m.text)).map((m) => m.text);
      const downloads = await s.page.evaluate(() => globalThis.__DOWNLOADS.map((d) => d.name));
      // The loader arrives but the worker's wasm does not: the same plain answer.
      const wasmLine = await missingWasm(browser, siteDir);
      return {
        pass:
          openLine !== null &&
          pdfLine !== null &&
          saveLine !== null &&
          wasmLine !== null &&
          after === before &&
          after.includes(KEPT_TEXT) &&
          panics.length === 0 &&
          s.pageErrors.length === 0 &&
          downloads.length === 0,
        wasmLine,
        openLine,
        pdfLine,
        saveLine,
        textUnchanged: after === before,
        downloads,
        panics,
        pageErrors: s.pageErrors,
        loaderRequests: s.asked("_loader.js"),
      };
    } finally {
      await s.close();
    }
  },
};

async function main() {
  const args = parseArgs(process.argv);
  if (!args.site) throw new Error("usage: node lazy.mjs --site <built site dir> [--out file.json] [--only a,b]");
  const siteDir = resolve(args.site);
  if (!existsSync(join(siteDir, "index.html"))) throw new Error(`${siteDir} has no index.html — build the site first (trunk build --release)`);
  const wanted = args.only ? args.only.split(",") : Object.keys(flows);
  const browser = await puppeteer.launch({ executablePath: findChrome(), headless: true, args: ["--hide-scrollbars", "--mute-audio", "--window-size=1400,900"] });
  const report = { site: siteDir, chrome: await browser.version(), flows: {} };
  try {
    for (const name of wanted) {
      if (!flows[name]) throw new Error(`no flow \`${name}\` (have: ${Object.keys(flows).join(", ")})`);
      try {
        report.flows[name] = await flows[name](browser, siteDir);
      } catch (error) {
        report.flows[name] = { pass: false, error: String((error && error.stack) || error) };
      }
    }
  } finally {
    await browser.close().catch(() => {});
  }
  report.pass = Object.values(report.flows).every((f) => f.pass === true);
  if (args.out) {
    mkdirSync(resolve(args.out, ".."), { recursive: true });
    writeFileSync(args.out, JSON.stringify(report, null, 2) + "\n");
  }
  console.log(JSON.stringify(report, null, 2));
  for (const [name, f] of Object.entries(report.flows)) console.error(`${f.pass ? "PASS" : "FAIL"} ${name}`);
  if (!report.pass) process.exitCode = 1;
}

await main();
