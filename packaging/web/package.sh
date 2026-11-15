#!/usr/bin/env bash
# Build browser artifacts:
#   - Default: both static zip ($DIST/wordcraft-web-<version>.zip) and npm dir ($DIST/npm)
#   - --static: only build the hashed static site zip ($DIST/wordcraft-web-<version>.zip)
#   - --npm: only build the unhashed npm package directory ($DIST/npm)
#   - --skip-build: reuse existing Trunk output without invoking `trunk build`
#   - --check: validate $DIST/npm (Node SSR import safety, checkJs, tsc bundler + nodenext, npm pack --dry-run)
#
# Needs: trunk (cargo install trunk --locked) and the wasm32-unknown-unknown target.

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=../env.sh
source "$HERE/../env.sh"

BUILD_STATIC=1
BUILD_NPM=1
SKIP_BUILD=0
RUN_CHECK=0

for arg in "$@"; do
  case "$arg" in
    --static)
      BUILD_STATIC=1
      BUILD_NPM=0
      ;;
    --npm)
      BUILD_STATIC=0
      BUILD_NPM=1
      ;;
    --skip-build)
      SKIP_BUILD=1
      ;;
    --check)
      RUN_CHECK=1
      ;;
    *)
      echo "usage: $0 [--static|--npm] [--skip-build] [--check]" >&2
      exit 2
      ;;
  esac
done

NAME="wordcraft-web-$VERSION"
NPM_STAGE="$DIST/npm"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

if [[ "$SKIP_BUILD" -eq 0 ]]; then
  command -v trunk >/dev/null || { echo "error: trunk not found (cargo install trunk --locked)" >&2; exit 1; }
fi


# Rebuild the dedicated workers WITHOUT `-C target-feature=+simd128` and overwrite the artifacts
# Trunk emitted for them (§4.2's flag lives in .cargo/config.toml and applies to every wasm32
# build, workers included). Measured 2026-11-15: the main module keeps the flag (layout 43%
# faster, module 8.5% smaller), but the PDF worker built with it never answers a renderPdf —
# the same bytes render in 264 ms natively and in the browser once the worker drops the flag —
# so the workers ship scalar until that miscompile is understood. The loader shims Trunk wrote
# are static and stay; the glue and wasm are byte-replaced before anything digests them (the
# sidecars and SRI below run after this).
rebuild_workers_scalar() {
  local dist="$1" w tmp
  # Trunk's own pinned copy first: the CLI version must match the crates' wasm-bindgen exactly,
  # and another wasm-bindgen on PATH (0.2.100, say) fails against a 0.2.129 build. Resolve the
  # version Trunk.toml pins, then accept only an executable that reports that version —
  # Trunk's versioned cache dir or a matching binary on PATH; never glob-order luck.
  local wb_ver
  wb_ver="$(sed -n 's/^[[:space:]]*wasm_bindgen[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$ROOT/apps/wordcraft-web/Trunk.toml" | head -n1)"
  local wb="" cand
  for cand in \
      "$HOME/Library/Caches/dev.trunkrs.trunk/wasm-bindgen-$wb_ver/wasm-bindgen" \
      "$HOME/.cache/trunk/wasm-bindgen-$wb_ver/wasm-bindgen" \
      "$(command -v wasm-bindgen || true)"; do
    [[ -n "$cand" && -x "$cand" ]] || continue
    if [[ -z "$wb_ver" || "$wb_ver" == "latest" ]] || "$cand" --version 2>/dev/null | grep -qF "wasm-bindgen $wb_ver"; then
      wb="$cand"
      break
    fi
  done
  [[ -n "$wb" ]] || { echo "error: wasm-bindgen $wb_ver not found for the scalar worker rebuild" >&2; exit 1; }
  for w in wordcraft-worker-proof wordcraft-worker-codecs wordcraft-worker-pdf; do
    tmp="$(mktemp -d)"
    (cd "$ROOT" && CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS="-C debuginfo=0" \
      cargo build -q -p "$w" --target wasm32-unknown-unknown --profile wasm-release) || { echo "error: scalar build of $w failed" >&2; exit 1; }
    "$wb" --target web --out-dir "$tmp" "$ROOT/target/wasm32-unknown-unknown/wasm-release/$w.wasm" >/dev/null \
      || { echo "error: wasm-bindgen failed for $w" >&2; exit 1; }
    wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals --enable-simd \
      "$tmp/${w}_bg.wasm" -o "$dist/${w}_bg.wasm" \
      || { echo "error: wasm-opt failed for $w" >&2; exit 1; }
    /bin/cp -f "$tmp/$w.js" "$dist/$w.js"
    rm -rf "$tmp"
    echo "rebuilt $w without simd"
  done
}

STATIC_DIST="$ROOT/dist/web"

if [[ "$BUILD_STATIC" -eq 1 ]]; then
  if [[ "$SKIP_BUILD" -eq 0 ]]; then
    (cd "$ROOT/apps/wordcraft-web" && trunk build --release)
    rebuild_workers_scalar "$STATIC_DIST"
  fi
  [[ -f "$STATIC_DIST/index.html" ]] || { echo "error: missing $STATIC_DIST/index.html (build static site first)" >&2; exit 1; }
  cat > "$STATIC_DIST/.assetsignore" << 'EOF'
*.wasm.gz
*.d.ts
.trunk-lock
EOF
  # Precompressed Brotli wasm sidecars (plan §2.2, S2): hosts that negotiate Content-Encoding
  # serve `<name>.wasm.br` instead of compressing the wasm on every miss. Written into
  # $STATIC_DIST itself — the Workers example deploys this directory (wrangler [assets]
  # directory = dist/web) and its Worker reads the sidecars through the ASSETS binding — so
  # the zip copy below carries them too. Only sidecars are written: the .wasm bytes (and their
  # SRI hashes baked into index.html) are untouched, and `.assetsignore` above deliberately
  # does not list `*.wasm.br`.
  for wasm_file in "$STATIC_DIST/"*.wasm; do
    [[ -f "$wasm_file" ]] || continue
    [[ -f "$wasm_file.br" ]] || (cd "$ROOT" && cargo xtask brotli "$wasm_file" "$wasm_file.br")
  done
  mkdir -p "$WORK/$NAME"
  /bin/cp -fR "$STATIC_DIST/"* "$WORK/$NAME/"
  rm -f "$WORK/$NAME/.trunk-lock" "$WORK/$NAME/_headers" "$WORK/$NAME/"*.wasm.gz "$WORK/$NAME/"*.d.ts
  /bin/cp -f "$HERE/.htaccess" "$WORK/$NAME/"
  if command -v node >/dev/null; then
    node --input-type=module -e "
      import { createHash } from 'node:crypto';
      import { readFileSync, writeFileSync } from 'node:fs';
      const html = readFileSync('$STATIC_DIST/index.html', 'utf8');
      const hashes = [];
      for (const m of html.matchAll(/<script\b[^>]*>([\s\S]*?)<\/script>/gi)) {
        const body = m[1];
        if (body.trim().length > 0) {
          hashes.push(\`'sha256-\${createHash('sha256').update(body, 'utf8').digest('base64')}'\`);
        }
      }
      let headers = readFileSync('$HERE/_headers', 'utf8');
      if (hashes.length > 0) {
        headers = headers.replace(
          \"script-src 'self' 'wasm-unsafe-eval'\",
          \"script-src 'self' 'wasm-unsafe-eval' \" + hashes.join(' ')
        );
      }
      writeFileSync('$STATIC_DIST/_headers', headers);
      writeFileSync('$WORK/$NAME/_headers', headers);
    "
  else
    echo "error: node is required to hash the inline bootstrap script into _headers" >&2
    echo "       (without the sha256 entries the Content-Security-Policy blocks the app from starting)" >&2
    exit 1
  fi
  copy_docs "$WORK/$NAME"
  rm -f "$DIST/$NAME.zip"
  (cd "$WORK" && zip -qr9 "$DIST/$NAME.zip" "$NAME")
  echo "wrote $DIST/$NAME.zip"
  ls -lh "$DIST/$NAME.zip"
fi

if [[ "$BUILD_NPM" -eq 1 ]]; then
  NPM_TRUNK_OUT="$WORK/trunk-npm"
  if [[ "$SKIP_BUILD" -eq 0 ]]; then
    mkdir -p "$NPM_TRUNK_OUT"
    (cd "$ROOT/apps/wordcraft-web" && trunk build --release --filehash=false --dist "$NPM_TRUNK_OUT")
    rebuild_workers_scalar "$NPM_TRUNK_OUT"
  else
    NPM_TRUNK_OUT="$NPM_STAGE"
  fi

  [[ -f "$NPM_TRUNK_OUT/wordcraft-web.js" ]] || { echo "error: missing $NPM_TRUNK_OUT/wordcraft-web.js" >&2; exit 1; }
  [[ -f "$NPM_TRUNK_OUT/wordcraft-web.d.ts" ]] || { echo "error: missing $NPM_TRUNK_OUT/wordcraft-web.d.ts (ensure data-typescript is on index.html)" >&2; exit 1; }
  [[ -f "$NPM_TRUNK_OUT/wordcraft-web_bg.wasm" ]] || { echo "error: missing $NPM_TRUNK_OUT/wordcraft-web_bg.wasm" >&2; exit 1; }
  # The dedicated workers (plans §3.2 and §3.3; the data-type="worker" links in index.html emit
  # these files with stable un-hashed names).
  for f in wordcraft-worker-proof.js wordcraft-worker-proof_loader.js wordcraft-worker-proof_bg.wasm \
      wordcraft-worker-codecs.js wordcraft-worker-codecs_loader.js wordcraft-worker-codecs_bg.wasm \
      wordcraft-worker-pdf.js wordcraft-worker-pdf_loader.js wordcraft-worker-pdf_bg.wasm; do
    [[ -f "$NPM_TRUNK_OUT/$f" ]] || { echo "error: missing $NPM_TRUNK_OUT/$f (worker link in index.html)" >&2; exit 1; }
  done

  if [[ "$SKIP_BUILD" -eq 0 ]]; then
    rm -rf "$NPM_STAGE"
    mkdir -p "$NPM_STAGE"
    /bin/cp -f "$NPM_TRUNK_OUT/wordcraft-web.js" "$NPM_STAGE/"
    /bin/cp -f "$NPM_TRUNK_OUT/wordcraft-web.d.ts" "$NPM_STAGE/"
    /bin/cp -f "$NPM_TRUNK_OUT/wordcraft-web_bg.wasm" "$NPM_STAGE/"
    for w in wordcraft-worker-proof wordcraft-worker-codecs wordcraft-worker-pdf; do
      /bin/cp -f "$NPM_TRUNK_OUT/$w.js" "$NPM_STAGE/"
      /bin/cp -f "$NPM_TRUNK_OUT/${w}_loader.js" "$NPM_STAGE/"
      /bin/cp -f "$NPM_TRUNK_OUT/${w}_bg.wasm" "$NPM_STAGE/"
    done
    if [[ -d "$NPM_TRUNK_OUT/snippets" ]]; then
      /bin/cp -fR "$NPM_TRUNK_OUT/snippets" "$NPM_STAGE/"
    fi
    if [[ -d "$NPM_TRUNK_OUT/i18n" ]]; then
      /bin/cp -fR "$NPM_TRUNK_OUT/i18n" "$NPM_STAGE/"
    fi
    if [[ -d "$NPM_TRUNK_OUT/samples" ]]; then
      /bin/cp -fR "$NPM_TRUNK_OUT/samples" "$NPM_STAGE/"
    fi
    if [[ -d "$NPM_TRUNK_OUT/fonts" ]]; then
      /bin/cp -fR "$NPM_TRUNK_OUT/fonts" "$NPM_STAGE/"
    fi
  fi

  /bin/cp -f "$HERE/npm/index.js" "$HERE/npm/index.d.ts" "$NPM_STAGE/"
  sed "s/@VERSION@/$VERSION/g" "$HERE/npm/package.json.in" > "$NPM_STAGE/package.json"
  copy_docs "$NPM_STAGE"
  /bin/cp -f "$HERE/README.md" "$NPM_STAGE/README.md"
  echo "prepared $NPM_STAGE"
fi

if [[ "$RUN_CHECK" -eq 1 ]]; then
  [[ -f "$NPM_STAGE/index.js" && -f "$NPM_STAGE/wordcraft-web_bg.wasm" && -f "$NPM_STAGE/wordcraft-web.d.ts" ]] || {
    echo "error: $NPM_STAGE is incomplete; run without --skip-build" >&2
    exit 1
  }
  command -v node >/dev/null || { echo "error: node not found for --check" >&2; exit 1; }
  command -v npm >/dev/null || { echo "error: npm not found for --check" >&2; exit 1; }

  echo "checking Node ESM / SSR import safety and Error rejections..."
  node --input-type=module -e "
    import * as wc from '$NPM_STAGE/index.js';
    if ('default' in wc) throw new Error('index.js must not have a default export');
    if (typeof wc.start !== 'function' || typeof wc.init !== 'function' || typeof wc.initSync !== 'function') {
      throw new Error('wordcraft-web missing expected named exports');
    }
    let rejected = false;
    try {
      await wc.start(42);
    } catch (err) {
      rejected = true;
      if (!(err instanceof Error)) throw new Error('start(42) must reject with an Error instance');
    }
    if (!rejected) throw new Error('start(42) should reject');
  "

  if command -v tsc >/dev/null; then
    echo "checking wrapper JS types (--checkJs)..."
    tsc --noEmit --strict --allowJs --checkJs --target es2022 --module esnext --moduleResolution bundler "$NPM_STAGE/index.js"

    for mode in bundler nodenext; do
      echo "checking TypeScript consumer under --moduleResolution $mode..."
      TS_DIR="$WORK/ts-$mode"
      mkdir -p "$TS_DIR/node_modules"
      ln -s "$NPM_STAGE" "$TS_DIR/node_modules/wordcraft-web"
      cat > "$TS_DIR/consumer.ts" <<'EOF'
import { init, initSync, start, type StartOptions, type WordCraftApi } from "wordcraft-web";
import rawInit, { start as rawStart } from "wordcraft-web/glue";

async function check(canvas: HTMLCanvasElement, precompiled: WebAssembly.Module): Promise<void> {
  const opts: StartOptions = { canvas, host: "page", installGlobal: false };
  const api: WordCraftApi = await start(opts);
  const md: string = await api.getMarkdown();
  await api.applyMarkdown(md);
  const bytes: Uint8Array = await api.exportDocx();
  await api.loadDocx(bytes);
  const fonts: { installed: string[]; failed: string[] } = await api.installFonts(["Inter"]);
  const rtf = new Uint8Array(await api.convertDocument({ from: "docx", to: "rtf", data: bytes })); void rtf;
  const pdf = new Uint8Array(await api.exportPdf()); void pdf;
  await init();
  initSync(precompiled);
  await rawInit();
  await rawStart(opts);
}
void check;
EOF
      if [[ "$mode" == "nodenext" ]]; then
        MODULE_FLAG="nodenext"
      else
        MODULE_FLAG="esnext"
      fi
      tsc --noEmit --strict --target es2022 --module "$MODULE_FLAG" --moduleResolution "$mode" "$TS_DIR/consumer.ts"
    done
  else
    echo "warning: tsc not found, skipping TypeScript consumer check" >&2
  fi

  echo "checking npm pack --dry-run --json..."
  PACK_JSON="$(cd "$NPM_STAGE" && npm pack --dry-run --json)"
  node --input-type=module -e "
    const [pkg] = JSON.parse(process.argv[1]);
    if (pkg.name !== 'wordcraft-web') throw new Error('unexpected package name: ' + pkg.name);
    if (pkg.version !== '$VERSION') throw new Error('version mismatch: ' + pkg.version);
    const paths = new Set(pkg.files.map((f) => f.path));
    for (const req of ['index.js', 'index.d.ts', 'wordcraft-web.js', 'wordcraft-web.d.ts', 'wordcraft-web_bg.wasm', 'wordcraft-worker-proof.js', 'wordcraft-worker-proof_loader.js', 'wordcraft-worker-proof_bg.wasm', 'wordcraft-worker-codecs.js', 'wordcraft-worker-codecs_loader.js', 'wordcraft-worker-codecs_bg.wasm', 'wordcraft-worker-pdf.js', 'wordcraft-worker-pdf_loader.js', 'wordcraft-worker-pdf_bg.wasm', 'i18n/manifest.json', 'fonts/manifest.json', 'LICENSE-MIT', 'LICENSE-APACHE', 'NOTICE', 'ATTRIBUTION.md', 'README.md']) {
      if (!paths.has(req)) throw new Error('npm tarball missing required file: ' + req);
    }
    for (const p of paths) {
      if (p.endsWith('.wasm.gz')) throw new Error('npm tarball must not contain gzipped wasm: ' + p);
    }
    const jsLine = await (await import('node:fs/promises')).readFile('$NPM_STAGE/wordcraft-web.js', 'utf8');
    if (jsLine.includes('./snippets/') && !pkg.files.some((f) => f.path.startsWith('snippets/'))) {
      throw new Error('wordcraft-web.js imports snippets/ but snippets/ was not packed');
    }
    console.log('npm pack check passed:', pkg.files.length, 'files, packed size', pkg.size, 'bytes');
  " "$PACK_JSON"
fi
