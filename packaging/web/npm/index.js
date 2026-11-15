import initWasm, { initSync, start as wasmStart } from "./wordcraft-web.js";

/** @type {Promise<unknown> | null} */
let initPromise = null;

/**
 * Initialize the underlying WebAssembly module once.
 *
 * Safe to call repeatedly; concurrent calls share the same initialization
 * promise and transient failures clear the cached promise so retries work.
 *
 * @param {import("./index.d.ts").StartOptions["wasmUrl"]} [wasmUrl]
 * @returns {Promise<unknown>}
 */
export function init(wasmUrl) {
  if (!initPromise) {
    const module_or_path = wasmUrl ?? new URL("./wordcraft-web_bg.wasm", import.meta.url);
    initPromise = Promise.resolve(initWasm({ module_or_path })).catch((err) => {
      initPromise = null;
      throw err instanceof Error ? err : new Error(String(err));
    });
  }
  return initPromise;
}

export { initSync };

/**
 * Mount WordCraft into a browser `<canvas>` element and resolve to its
 * `WordCraftApi` promise interface.
 *
 * Importing this module touches no DOM globals, so SSR/Node imports succeed
 * cleanly until `start(...)` is called in a browser context.
 *
 * @param {import("./index.d.ts").StartOptions} [options]
 * @returns {Promise<import("./index.d.ts").WordCraftApi>}
 */
export async function start(options = {}) {
  if (options !== null && options !== undefined && typeof options !== "object") {
    throw new TypeError("wordcraft-web start(options) expects an options object");
  }
  const resolved = options ?? {};
  const { wasmUrl, ...rustOptions } = resolved;
  await init(wasmUrl);
  const assetBase = rustOptions.assetBase ?? new URL("./", import.meta.url).href;
  const installGlobal = rustOptions.installGlobal ?? false;
  return wasmStart({
    ...rustOptions,
    assetBase,
    installGlobal,
  });
}

/**
 * Construct the proofing dedicated worker bundled with this package, as a module
 * worker (the same-origin Trunk loader shim — no `blob:` URL, so
 * `worker-src 'self'` suffices). By default it resolves against the package's own
 * files; pass `assetBase` (the same runtime base `start` uses) or `loaderUrl` when
 * the files are served elsewhere.
 *
 * The worker speaks the `wordcraft-worker-proto` wire contract
 * (`[headerJson, payloadArrayBuffer]` messages). Consumers that embed WordCraft
 * normally let the app spawn its own worker; this export exists for hosts that
 * need the worker under their own control or CSP.
 *
 * @param {import("./index.d.ts").ProofWorkerOptions} [options]
 * @returns {Worker}
 */
export function createProofWorker(options = {}) {
  if (options !== null && options !== undefined && typeof options !== "object") {
    throw new TypeError("wordcraft-web createProofWorker(options) expects an options object");
  }
  const resolved = options ?? {};
  const { assetBase, loaderUrl } = resolved;
  if (loaderUrl !== undefined) {
    if (typeof loaderUrl !== "string") throw new TypeError("createProofWorker loaderUrl must be a string");
    return new Worker(loaderUrl, { type: "module" });
  }
  if (assetBase !== undefined) {
    if (typeof assetBase !== "string") throw new TypeError("createProofWorker assetBase must be a string");
    return new Worker(`${joinAssetBase(assetBase)}wordcraft-worker-proof_loader.js`, { type: "module" });
  }
  return workerFrom("./wordcraft-worker-proof_loader.js");
}

// The URL is resolved in one statement and the Worker constructed in the next: bundlers
// rewrite only the literal `new Worker(new URL(..., import.meta.url), ...)` expression
// (Vite/rolldown re-bundle it as an iife worker, whose format rejects the Trunk loader's
// top-level `await init()`), and a plain string URL stays a same-origin module worker that
// the browser loads straight from the package's files.
/**
 * @param {string} specifier
 */
function workerFrom(specifier) {
  const url = new URL(specifier, import.meta.url);
  return new Worker(url.href, { type: "module" });
}

/**
 * @param {string} assetBase
 */
function joinAssetBase(assetBase) {
  const trimmed = assetBase.trim();
  if (trimmed === "") return "./";
  return trimmed.endsWith("/") ? trimmed : `${trimmed}/`;
}
