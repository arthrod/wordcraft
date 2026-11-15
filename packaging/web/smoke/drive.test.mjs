// Unit tests for drive.mjs helpers (node --test drive.test.mjs).

import test from "node:test";
import assert from "node:assert/strict";
import { isExpectedWasmMiss } from "./drive.mjs";

// Shape of window.__SMOKE after the deliberate zero-config wasm attempt missed
// and the explicit-wasmUrl retry resolved (fixtures/main.shared.js).
const smokeMiss = { wasmAuto: false, startResolvedAt: 10_000, autoFailedAt: 9_900 };

test("suppresses errors that identify the failed wasm URL", () => {
  // The real esbuild miss: Chrome logs the failed fetch against the wasm URL.
  assert.equal(
    isExpectedWasmMiss(smokeMiss, {
      t: 10_100,
      url: "http://127.0.0.1:65269/wordcraft-web_bg.wasm",
      text: "Failed to load resource: the server responded with a status of 404 (Not Found)",
    }),
    true,
  );
  // Bundler variants may only name the wasm in the message text.
  assert.equal(
    isExpectedWasmMiss(smokeMiss, { t: 10_100, url: "http://127.0.0.1:65269/", text: "error fetching wordcraft-web_bg.wasm" }),
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

test("does not suppress when wasmAuto is not false", () => {
  const wasmErr = { t: 10_100, url: "http://127.0.0.1:65269/wordcraft-web_bg.wasm", text: "" };
  assert.equal(isExpectedWasmMiss({ wasmAuto: true, startResolvedAt: 10_000 }, wasmErr), false);
  assert.equal(isExpectedWasmMiss({ wasmAuto: null, startResolvedAt: 10_000 }, wasmErr), false);
});

test("suppresses a wasm miss however late console delivery arrives", () => {
  // Regression: the old predicate bounded e.t (Node-side receipt time) by the
  // page-side startResolvedAt + 500ms, so a delayed console event for the
  // expected miss counted as a real error.
  const wasmErr = { t: 60_000, url: "http://127.0.0.1:65269/wordcraft-web_bg.wasm", text: "" };
  assert.equal(isExpectedWasmMiss(smokeMiss, wasmErr), true);
});
