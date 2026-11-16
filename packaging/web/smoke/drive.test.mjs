// Unit tests for drive.mjs helpers (node --test drive.test.mjs).

import test from "node:test";
import assert from "node:assert/strict";
import { autoWasmUrl, isChromeExecutable, isExpectedWasmMiss } from "./drive.mjs";

// Shape of window.__SMOKE after the deliberate zero-config wasm attempt missed
// and the explicit-wasmUrl retry resolved (fixtures/main.shared.js): the real
// esbuild miss records the exact attempted URL in autoError.
const AUTO_URL = "http://127.0.0.1:65269/wordcraft-web_bg.wasm";
const smokeMiss = {
  wasmAuto: false,
  autoError: `Error: failed to fetch Wasm: 404 Not Found fetching '${AUTO_URL}'`,
  startResolvedAt: 10_000,
  autoFailedAt: 9_900,
};

test("extracts the attempted wasm URL from the recorded autoError", () => {
  assert.equal(autoWasmUrl(smokeMiss), AUTO_URL);
  assert.equal(autoWasmUrl({ wasmAuto: false, autoError: null }), null);
  assert.equal(autoWasmUrl({ wasmAuto: false, autoError: "wasm init failed" }), null);
});

test("suppresses errors that identify the failed wasm URL", () => {
  // The real esbuild miss: Chrome logs the failed fetch against the wasm URL.
  assert.equal(
    isExpectedWasmMiss(smokeMiss, {
      t: 10_100,
      url: AUTO_URL,
      text: "Failed to load resource: the server responded with a status of 404 (Not Found)",
    }),
    true,
  );
  // Bundler variants may name the wasm URL only in the message text.
  assert.equal(
    isExpectedWasmMiss(smokeMiss, { t: 10_100, url: "http://127.0.0.1:65269/", text: `error fetching ${AUTO_URL}` }),
    true,
  );
});

test("retains an unrelated resource 404 in the same interval", () => {
  // Regression: the old predicate matched any error text containing "404",
  // which suppressed e.g. a missing stylesheet during the wasm-miss window.
  assert.equal(
    isExpectedWasmMiss(smokeMiss, {
      t: 10_100,
      url: "http://127.0.0.1:65269/missing.css",
      text: "Failed to load resource: the server responded with a status of 404 (Not Found)",
    }),
    false,
  );
});

test("retains a worker-module wasm miss", () => {
  // Regression: the old predicate matched any URL or text containing "_bg.wasm",
  // which suppressed e.g. the proofing dedicated worker's own wasm failing.
  const workerErr = {
    t: 10_100,
    url: "http://127.0.0.1:65269/wordcraft-web/wordcraft-worker-proof_bg.wasm",
    text: "Failed to load resource: the server responded with a status of 404 (Not Found)",
  };
  assert.equal(isExpectedWasmMiss(smokeMiss, workerErr), false);
});

test("retains text that names only the bare wasm filename", () => {
  // Without the full attempted URL there is no proof the error is the zero-config
  // attempt's, so it stays a real error rather than being guessed away.
  assert.equal(
    isExpectedWasmMiss(smokeMiss, { t: 10_100, url: "http://127.0.0.1:65269/", text: "error fetching wordcraft-web_bg.wasm" }),
    false,
  );
});

test("does not suppress when wasmAuto is not false", () => {
  const wasmErr = { t: 10_100, url: AUTO_URL, text: "" };
  assert.equal(isExpectedWasmMiss({ wasmAuto: true, startResolvedAt: 10_000 }, wasmErr), false);
  assert.equal(isExpectedWasmMiss({ wasmAuto: null, startResolvedAt: 10_000 }, wasmErr), false);
});

test("suppresses a wasm miss however late console delivery arrives", () => {
  // Regression: the old predicate bounded e.t (Node-side receipt time) by the
  // page-side startResolvedAt + 500ms, so a delayed console event for the
  // expected miss counted as a real error.
  const wasmErr = { t: 60_000, url: AUTO_URL, text: "" };
  assert.equal(isExpectedWasmMiss(smokeMiss, wasmErr), true);
});

test("recognizes every puppeteer-cache Chrome executable basename", () => {
  // Linux cache layout: <install>/chrome-linux64/chrome and chrome-headless-shell.
  assert.equal(isChromeExecutable("/home/user/.cache/puppeteer/chrome/chrome-131.0.0.0/chrome-linux64/chrome"), true);
  assert.equal(
    isChromeExecutable("/home/user/.cache/puppeteer/chrome/chrome-131.0.0.0/chrome-headless-shell-linux64/chrome-headless-shell"),
    true,
  );
  // Regression: the macOS cache layout keeps the executable inside the app bundle
  // under a name the old predicate missed.
  assert.equal(
    isChromeExecutable(
      "/Users/user/.cache/puppeteer/chrome/chrome-131.0.0.0/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
    ),
    true,
  );
  // Not executables: the app bundle directory, other bundle contents.
  assert.equal(
    isChromeExecutable("/Users/user/.cache/puppeteer/chrome/chrome-131.0.0.0/chrome-mac-arm64/Google Chrome for Testing.app"),
    false,
  );
  assert.equal(
    isChromeExecutable("/home/user/.cache/puppeteer/chrome/chrome-131.0.0.0/chrome-linux64/chrome_crashpad_handler"),
    false,
  );
});
