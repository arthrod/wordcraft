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
