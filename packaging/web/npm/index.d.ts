export type HostMode = "none" | "parent" | "page";

export interface StartOptions {
  /** Target canvas element or element id (defaults to `"wordcraft_canvas"`). */
  canvas?: string | HTMLCanvasElement;
  /** Override URL, Request, Response, bytes, or compiled Module for `wordcraft-web_bg.wasm`. */
  wasmUrl?: RequestInfo | URL | Response | BufferSource | WebAssembly.Module;
  /** Base URL for external runtime assets such as `i18n/` and `samples/`. */
  assetBase?: string;
  /** Optional host integration mode (`"none"`, `"parent"` iframe, or `"page"` DOM events). */
  host?: HostMode;
  /** Author name used for comments and tracked changes in hosted sessions. */
  author?: string;
  /** Publish this instance onto `window.wordcraft` as well as returning it (default `false`). */
  installGlobal?: boolean;
  /** Open the built-in sample document instead of a blank document on startup. */
  sample?: boolean;
}

export interface CommandResult<T = unknown> {
  [key: string]: T;
}

export interface WordCraftApi {
  /** Dispatch any raw control-protocol method (`docs/control-protocol.md`). */
  request<T = unknown>(method: string, params?: Record<string, unknown>): Promise<T>;
  /** Run any registered WordCraft command by id (`wordcraft-cli commands`). */
  execute<T = CommandResult>(command: string, params?: Record<string, unknown>): Promise<T>;
  /** List all registered commands, labels, shortcuts, and parameter docs. */
  commands(): Promise<unknown[]>;
  /** Inspect document text, structure, selection, styles, or metadata. */
  docInspect(params?: Record<string, unknown>): Promise<Record<string, unknown>>;
  /** Inspect UI ribbons, dialogs, tabs, and accessibility state. */
  uiInspect(params?: Record<string, unknown>): Promise<Record<string, unknown>>;
  /** Capture a PNG screenshot of the current document canvas. */
  screenshot(params?: Record<string, unknown>): Promise<string>;
  /** Export the current document as Markdown text. */
  getMarkdown(): Promise<string>;
  /** Replace the current document content from Markdown text. */
  applyMarkdown(markdown: string): Promise<Record<string, unknown>>;
  /** Serialize the current document to `.docx` bytes. */
  exportDocx(): Promise<Uint8Array>;
  /** Load a `.docx` file from bytes or a base64 string (with optional password). */
  loadDocx(data: Uint8Array | ArrayBuffer | string, password?: string): Promise<Record<string, unknown>>;
  /**
   * Download the unbundled pack faces of `families` from `<assetBase>/fonts/` (all pack faces
   * when omitted) and resolve `{installed, failed}` (font file names) once no pack fetch is in
   * flight. Await it before measuring layout-sensitive output — a bold face arriving can move
   * line and page breaks.
   */
  installFonts(families?: string[]): Promise<{ installed: string[]; failed: string[] }>;
  /**
   * Convert between a `.docx` package and a format the build leaves out (RTF, ODT, HTML,
   * LaTeX, legacy `.doc`, flat OPC XML; also `.docx` flavour changes) on the codec dedicated
   * worker, started lazily by this call. Exactly one of `from`/`to` is `"docx"`; resolves to
   * the converted bytes as a `Uint8Array`. `password` decrypts an encrypted input and encrypts
   * the output where the format supports it.
   */
  convertDocument(options: { from: string; to: string; data: Uint8Array | ArrayBuffer | string; password?: string }): Promise<Uint8Array>;
  /**
   * Export the document as PDF on the dedicated worker, started lazily by this call. The
   * document crosses as the same `.docx` package `exportDocx` writes, with the non-bundled
   * font faces (user fonts, packs, document-embedded) as copies so the worker's layout matches
   * the page's; resolves to the PDF bytes as a `Uint8Array`. A password-protected session
   * exports an encrypted package the worker cannot read (it rejects, saying so).
   */
  exportPdf(options?: { markup?: boolean }): Promise<Uint8Array>;
}

/** Incoming CustomEvent names emitted by `<canvas data-host="page">`. */
export interface WordCraftPageEvents {
  "wordcraft:ready": CustomEvent<{ version: string; appVersion?: string; file?: boolean }>;
  "wordcraft:save": CustomEvent<{ name: string; bytes: Uint8Array; id?: string; saveAs?: boolean }>;
  "wordcraft:dirty": CustomEvent<{ dirty: boolean }>;
  "wordcraft:failed": CustomEvent<{ error: string; id?: string }>;
  "wordcraft:handoff": CustomEvent<{ name: string; bytes?: Uint8Array }>;
}

/** Outgoing message types accepted via `window.postMessage` or canvas `CustomEvent`. */
export type WordCraftHostMessages =
  | { type: "wordcraft:host"; version: string; handoff?: boolean }
  | { type: "wordcraft:open"; name: string; bytes?: Uint8Array | ArrayBuffer; file?: File | Blob; size?: number }
  | { type: "wordcraft:saved"; id: string }
  | { type: "wordcraft:handoff-done"; name: string; app: string };

/**
 * Load and initialize the `wordcraft-web` WebAssembly module explicitly without
 * mounting a canvas yet.
 */
export function init(wasmUrl?: StartOptions["wasmUrl"]): Promise<unknown>;

/** Synchronously instantiate a pre-compiled `WebAssembly.Module` or buffer. */
export function initSync(
  module: { module: BufferSource | WebAssembly.Module } | BufferSource | WebAssembly.Module
): unknown;

/**
 * Initialize `wordcraft-web` (if needed), mount onto the target `<canvas>`, and
 * resolve to its `WordCraftApi` instance. Calling repeatedly for the same canvas
 * element resolves to its existing instance.
 */
export function start(options?: StartOptions): Promise<WordCraftApi>;

export interface ProofWorkerOptions {
  /**
   * Base URL the worker loader is served from (same semantics as
   * `StartOptions.assetBase`); `wordcraft-worker-proof_loader.js` is appended.
   */
  assetBase?: string;
  /** Exact URL of `wordcraft-worker-proof_loader.js`; takes precedence over `assetBase`. */
  loaderUrl?: string;
}

/**
 * Construct the proofing dedicated worker bundled with this package as a module
 * worker. With no options it resolves `./wordcraft-worker-proof_loader.js` against
 * the package; pass `assetBase` or `loaderUrl` for deployments that serve the
 * worker files separately. It speaks the `wordcraft-worker-proto` wire contract.
 */
export function createProofWorker(options?: ProofWorkerOptions): Worker;
