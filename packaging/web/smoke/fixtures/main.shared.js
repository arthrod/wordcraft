// Shared entry body for every bundler fixture (deferred-items plan §7.3a). Each
// fixture's own `src/main.js` imports `wordcraft-web` and calls `run(start)` from here;
// `packaging/web/smoke/drive.mjs` reads `window.__SMOKE` after the run.
//
// The `wordcraft-web` import lives in the fixture's `src/main.js`, not here: a bare
// specifier resolves from the importing file's directory, and this shared file sits one
// level above all three fixtures, where no node_modules holds the package.
//
// The flow deliberately tries the zero-configuration path first — the package
// resolves `./wordcraft-web_bg.wasm` against `import.meta.url` — so the report
// records whether each bundler emitted the wasm as an asset on its own. On a
// miss it retries with an explicit `wasmUrl` under `assetBase`, so locale, font
// pack and worker checks still run for bundlers that cannot copy it.

// The unpacked package is served at /wordcraft-web/ by drive.mjs — the runtime
// files (i18n/, fonts/, worker loader) live there because bundlers don't copy
// them; pointing assetBase at them is exactly what a consumer does.
const ASSET_BASE = "/wordcraft-web/";

const result = {
  phase: "boot",
  wasmAuto: null,
  autoError: null,
  autoFailedAt: null,
  startOk: false,
  startResolvedAt: null,
  fonts: null,
  exportWorker: null,
  fatal: null,
};
window.__SMOKE = result;

function options(extra) {
  return { canvas: "smoke_canvas", assetBase: ASSET_BASE, host: "none", installGlobal: false, ...extra };
}

// The package's createProofWorker export, pointed at the same /wordcraft-web/
// deployment base start() uses: ready, then one ping/pong on the worker proto.
function proofWorkerExport(createProofWorker) {
  return new Promise((resolve, reject) => {
    let worker;
    try {
      worker = createProofWorker({ assetBase: ASSET_BASE });
    } catch (error) {
      reject(error);
      return;
    }
    const finish = (fn, value) => {
      clearTimeout(timer);
      worker.terminate();
      fn(value);
    };
    const timer = setTimeout(() => finish(reject, new Error("proof worker export did not answer ping")), 30_000);
    worker.onmessage = (event) => {
      let msg;
      try {
        msg = JSON.parse(event.data?.[0]);
      } catch {
        return;
      }
      if (msg.kind === "ready") {
        worker.postMessage(['{"v":1,"id":1,"kind":"ping","revision":0}', new Uint8Array(0)]);
      } else if (msg.kind === "pong" && msg.id === 1) {
        finish(resolve, true);
      } else if (msg.kind === "error") {
        finish(reject, new Error(`proof worker export error: ${event.data?.[0]}`));
      }
    };
    worker.onerror = (event) => finish(reject, new Error(`proof worker export failed: ${event.message ?? "worker error"}`));
  });
}

export async function run(start, createProofWorker) {
  try {
    let api;
    try {
      api = await start(options());
      result.wasmAuto = true;
    } catch (autoError) {
      result.wasmAuto = false;
      result.autoError = String(autoError);
      result.autoFailedAt = Date.now();
      api = await start(options({ wasmUrl: `${ASSET_BASE}wordcraft-web_bg.wasm` }));
    }
    result.startOk = true;
    result.startResolvedAt = Date.now();
    // One font pack through assetBase: resolves once the manifest and the
    // family's .ttf.br files are fetched, decoded and installed.
    result.fonts = await api.installFonts(["JetBrains Mono"]);
    // The proofing worker starts on the first edit, not on page load (implementation
    // plan 10.1): make one, so the `?proof-worker` known-bad check drive.mjs waits
    // for has a worker to run in.
    // The page starts on File › Home with no document open, so it opens a blank one first.
    await api.execute("file.new", {});
    await api.execute("text.insert", { text: "Ths is a tst. " });
    result.exportWorker = await proofWorkerExport(createProofWorker);
    result.phase = "done";
  } catch (error) {
    result.phase = "failed";
    result.fatal = String((error && error.stack) || error);
  }
}
