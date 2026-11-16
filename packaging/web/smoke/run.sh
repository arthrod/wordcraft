#!/usr/bin/env bash
# Consumer smoke matrix for the wordcraft-web npm package (deferred-items plan 7.3a).
#
#   1. Pack the staged npm package: mirror $DIST/npm (or, if it is stale, a stage
#      rebuilt from dist/web or a fresh Trunk build) under smoke/stage/, then
#      `npm pack` it into smoke/tarball/wordcraft-web.tgz. Nothing is published.
#   2. `pnpm install` + `pnpm build` each bundler fixture (vite, webpack, esbuild)
#      which depends on the tarball via a `file:` dependency.
#   3. `node drive.mjs` serves the built bundle plus the unpacked package at
#      /wordcraft-web/ (the `assetBase` the fixtures pass to start()), drives
#      headless Chrome (puppeteer-core + system Chrome) and checks:
#        - start() resolves with no console errors after it does
#        - whether the wasm was found with zero bundler configuration
#        - one runtime locale (fr) fetched through assetBase
#        - one font pack (JetBrains Mono) fetched + installed through assetBase
#        - the proofing dedicated worker (?proof-worker) answering a check
#        - the createProofWorker export answering a ping via assetBase
#   4. Writes results/<bundler>.json and results/report.md.
#
# Usage: ./run.sh [--tarball <path>] [--build] [--only vite|webpack|esbuild]
#   --tarball  reuse a pre-packed .tgz (skips staging and npm pack)
#   --build    force `package.sh --npm` (full Trunk build) instead of reusing
#              dist/release/npm or de-hashing dist/web
#   --only     run a single fixture
#
# Needs: node, npm (pack only), pnpm, and a Chrome/Chromium for drive.mjs
# (CHROME_PATH, /Applications/Google Chrome.app, or a puppeteer cache download).

set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=../../env.sh
source "$HERE/../../env.sh"

SMOKE_DIST="$HERE/stage"
TARBALL_DIR="$HERE/tarball"
TARBALL="$TARBALL_DIR/wordcraft-web.tgz"
RESULTS="$HERE/results"
FIXTURES_DIR="$HERE/fixtures"
NPM_STAGE="$DIST/npm"
STATIC_DIST="$ROOT/dist/web"

USE_TARBALL=""
FORCE_BUILD=0
ONLY=""

for ((i = 1; i <= $#; i++)); do
  case "${!i}" in
    --tarball)
      i=$((i + 1))
      USE_TARBALL="${!i:-}"
      [[ -n "$USE_TARBALL" ]] || { echo "error: --tarball needs a path" >&2; exit 2; }
      ;;
    --build)
      FORCE_BUILD=1
      ;;
    --only)
      i=$((i + 1))
      ONLY="${!i:-}"
      [[ -n "$ONLY" ]] || { echo "error: --only needs a fixture name" >&2; exit 2; }
      ;;
    *)
      echo "usage: $0 [--tarball <path>] [--build] [--only vite|webpack|esbuild]" >&2
      exit 2
      ;;
  esac
done

command -v node >/dev/null || { echo "error: node not found" >&2; exit 1; }
command -v npm >/dev/null || { echo "error: npm not found (npm pack only; installs use pnpm)" >&2; exit 1; }
command -v pnpm >/dev/null || { echo "error: pnpm not found" >&2; exit 1; }

# The files the npm stage must hold for the smoke to mean anything. Mirrors the
# checks package.sh makes plus the runtime assets the fixtures fetch.
stage_complete() {
  local dir="$1" f
  for f in \
    index.js index.d.ts package.json \
    wordcraft-web.js wordcraft-web.d.ts wordcraft-web_bg.wasm \
    wordcraft-worker-proof.js wordcraft-worker-proof_loader.js wordcraft-worker-proof_bg.wasm \
    wordcraft-worker-codecs.js wordcraft-worker-codecs_loader.js wordcraft-worker-codecs_bg.wasm \
    wordcraft-worker-pdf.js wordcraft-worker-pdf_loader.js wordcraft-worker-pdf_bg.wasm \
    i18n/manifest.json fonts/manifest.json; do
    [[ -f "$dir/$f" ]] || return 1
  done
  return 0
}

# Copy the hashed Trunk static build into the stage under the un-hashed names a
# `--filehash=false` build emits. The glue resolves `wordcraft-web_bg.wasm`
# (un-hashed) at runtime, so renaming is byte-safe.
seed_stage_from_static() {
  local src="$STATIC_DIST" dest="$SMOKE_DIST/npm"
  rm -rf "$dest"
  mkdir -p "$dest"
  local glue wasm dts
  glue="$(basename "$(ls "$src"/wordcraft-web-*.js | grep -v '\.d\.ts$' | head -n1)")"
  wasm="$(basename "$(ls "$src"/wordcraft-web-*_bg.wasm | head -n1)")"
  dts="$(basename "$(ls "$src"/wordcraft-web-*.d.ts | head -n1)")"
  /bin/cp -f "$src/$glue" "$dest/wordcraft-web.js"
  /bin/cp -f "$src/$wasm" "$dest/wordcraft-web_bg.wasm"
  /bin/cp -f "$src/$dts" "$dest/wordcraft-web.d.ts"
  for f in wordcraft-worker-proof.js wordcraft-worker-proof_loader.js wordcraft-worker-proof_bg.wasm \
      wordcraft-worker-codecs.js wordcraft-worker-codecs_loader.js wordcraft-worker-codecs_bg.wasm \
      wordcraft-worker-pdf.js wordcraft-worker-pdf_loader.js wordcraft-worker-pdf_bg.wasm; do
    [[ -f "$src/$f" ]] || { echo "error: $src/$f missing (static build predates the workers)" >&2; return 1; }
    /bin/cp -f "$src/$f" "$dest/"
  done
  for d in snippets i18n samples fonts; do
    [[ -d "$src/$d" ]] && /bin/cp -fR "$src/$d" "$dest/"
  done
  echo "seeded $dest from $src (un-hashed)"
}

prepare_tarball() {
  mkdir -p "$TARBALL_DIR"
  if [[ -n "$USE_TARBALL" ]]; then
    [[ -f "$USE_TARBALL" ]] || { echo "error: tarball $USE_TARBALL not found" >&2; exit 1; }
    # --tarball may already name our own tarball path; copying a file onto itself fails,
    # so skip the copy when both resolve to the same file.
    local src_abs dst_abs
    src_abs="$(cd "$(dirname "$USE_TARBALL")" && pwd)/$(basename "$USE_TARBALL")"
    dst_abs="$(cd "$TARBALL_DIR" && pwd)/$(basename "$TARBALL")"
    if [[ "$src_abs" != "$dst_abs" ]]; then
      /bin/cp -f "$USE_TARBALL" "$TARBALL"
    fi
    echo "reusing $USE_TARBALL"
    # Still need an unpacked stage to serve at /wordcraft-web/ as assetBase.
    rm -rf "$SMOKE_DIST/npm"
    mkdir -p "$SMOKE_DIST/npm"
    tar -xzf "$TARBALL" -C "$SMOKE_DIST/npm" --strip-components=1
    return 0
  fi

  if [[ "$FORCE_BUILD" -eq 1 ]]; then
    # Canonical path, output redirected under smoke/ via $DIST.
    DIST="$SMOKE_DIST" "$ROOT/packaging/web/package.sh" --npm
  elif stage_complete "$NPM_STAGE"; then
    # Re-stage the real artifact under smoke/ so package.sh refreshes the
    # wrapper files and package.json without touching dist/release.
    rm -rf "$SMOKE_DIST/npm"
    mkdir -p "$SMOKE_DIST/npm"
    /bin/cp -fR "$NPM_STAGE/"* "$SMOKE_DIST/npm/"
    DIST="$SMOKE_DIST" "$ROOT/packaging/web/package.sh" --npm --skip-build
    echo "staged from $NPM_STAGE"
  else
    echo "note: $NPM_STAGE is stale or incomplete; de-hashing $STATIC_DIST instead" >&2
    seed_stage_from_static
    DIST="$SMOKE_DIST" "$ROOT/packaging/web/package.sh" --npm --skip-build
  fi

  stage_complete "$SMOKE_DIST/npm" || {
    echo "error: stage under $SMOKE_DIST/npm is incomplete" >&2
    exit 1
  }

  rm -f "$TARBALL" "$TARBALL_DIR"/wordcraft-web-*.tgz
  (cd "$SMOKE_DIST/npm" && npm pack --pack-destination "$TARBALL_DIR" >/dev/null)
  /bin/mv -f "$TARBALL_DIR"/wordcraft-web-*.tgz "$TARBALL"
  echo "packed $TARBALL"
}

build_fixture() {
  local name="$1" dir="$FIXTURES_DIR/$name"
  echo "=== fixture: $name"
  # --allow-build: pnpm 12 makes unapproved dependency build scripts a hard error; esbuild's
  # postinstall is the only one the matrix pulls (its platform binary; the settings files in
  # fixtures/esbuild record the same approval for interactive installs).
  local install_args=()
  [[ "$name" == "esbuild" ]] && install_args+=(--allow-build=esbuild)
  (cd "$dir" && pnpm install --reporter=append-only "${install_args[@]}" && pnpm run build)
  [[ -f "$dir/dist/index.html" ]] || { echo "error: $name build produced no dist/index.html" >&2; return 1; }
}

main() {
  mkdir -p "$RESULTS"
  prepare_tarball

  [[ -d "$HERE/node_modules/puppeteer-core" ]] || (cd "$HERE" && pnpm install --reporter=append-only)

  local fixtures=(vite webpack esbuild)
  if [[ -n "$ONLY" ]]; then
    fixtures=("$ONLY")
  fi

  local name status=0
  for name in "${fixtures[@]}"; do
    [[ -d "$FIXTURES_DIR/$name" ]] || { echo "error: no fixture $FIXTURES_DIR/$name" >&2; exit 2; }
    build_fixture "$name"
    if ! node "$HERE/drive.mjs" \
      --fixture "$name" \
      --dist "$FIXTURES_DIR/$name/dist" \
      --pkg "$SMOKE_DIST/npm" \
      --out "$RESULTS/$name.json"; then
      status=1
    fi
  done

  node "$HERE/drive.mjs" --report --results "$RESULTS" | tee "$RESULTS/report.md"

  # What a visitor's page fetches, and when (README: lazy-loading checks). Needs the
  # standalone site, which a --tarball or npm-stage run may not have built.
  if [[ -z "$ONLY" && -f "$STATIC_DIST/index.html" ]]; then
    echo "=== lazy-loading checks: $STATIC_DIST"
    node "$HERE/lazy.mjs" --site "$STATIC_DIST" --out "$RESULTS/lazy.json" >/dev/null || status=1
  elif [[ -z "$ONLY" ]]; then
    echo "note: $STATIC_DIST has no index.html; lazy-loading checks skipped (trunk build --release, then lazy.mjs)" >&2
  fi
  echo "results in $RESULTS"
  return "$status"
}

main "$@"
