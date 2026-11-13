# WordCraftWeb on Cloudflare Workers (Static Assets + Optional Hybrid Edge)

This example serves `dist/web` (with `_headers`) on Cloudflare Workers Static Assets and attaches a pure-Rust (`workers-rs`) edge endpoint (`run_worker_first = ["/api/*"]`) for optional **Hybrid Cloudflare Edge** conversions (`?demo=1`).

## Prerequisites

1. Build the hashed static site and generate `dist/web/_headers`:

   ```sh
   ./packaging/web/package.sh --static
   ```

2. Install [Wrangler](https://developers.cloudflare.com/workers/wrangler/) and `worker-build`:

   ```sh
   cargo install worker-build --locked
   ```

## Local Development & Deploy

From `packaging/web/examples/cloudflare-workers/`:

```sh
npx wrangler dev
npx wrangler deploy
```

Notice that static routes (`/`, `/*.wasm`, `/*.js`, `/i18n/*`, `/samples/*`) are served directly by Cloudflare's static asset tier without invoking the Worker isolate; only `/api/*` enters Rust Worker code.

## Feasibility Gate & Limits (`5.2a` Benchmark)

Measured on release `wordcraft-cli convert`:

| Document Size | Conversion | Wall Time | CPU Time | Peak RSS | Workers Eligibility |
|---|---|---:|---:|---:|---|
| 1 page (`3.4 KB` `.md`) | `.md` -> `.docx` | `0.67 s` | `10 ms` | `14.5 MiB` | Free / Paid (within 128 MB) |
| 1 page (`6.6 KB` `.docx`) | `.docx` -> `.pdf` | `0.31 s` | `120 ms` | `33.1 MiB` | Paid CPU (`>10 ms`) |
| 50 pages (`170 KB` `.md`) | `.md` -> `.docx` | `0.02 s` | `<10 ms` | `16.4 MiB` | Free / Paid (within 128 MB) |
| 50 pages (`8.4 KB` `.docx`) | `.docx` -> `.pdf` | `0.10 s` | `90 ms` | `53.0 MiB` | Paid CPU (`>10 ms`) |
| 500 pages (`1.70 MB` `.md`) | `.md` -> `.docx` | `0.05 s` | `40 ms` | `38.0 MiB` | Paid CPU (`>10 ms`) |
| 500 pages (`24 KB` `.docx`) | `.docx` -> `.pdf` | `0.62 s` | `600 ms` | `233.4 MiB` | Exceeds 128 MB isolate RAM |

Because PDF pagination/font-subsetting on large documents exceeds Cloudflare's 128 MB isolate memory cap, `/api/convert` limits payloads to `4 MiB` (`413 Payload Too Large`) and scopes edge conversions to lightweight format codecs (`md`, `html`, `txt`, `docx`), while client-side WebAssembly handles arbitrary full-document editing locally without uploads.

## Privacy & Rate Limiting

- **Default Local Mode**: In `100% Browser (Local WASM)` mode, no document bytes leave the browser.
- **Opt-In Hybrid Mode**: Selecting `Hybrid Cloudflare Edge` in the `?demo=1` bar notifies the user before uploading to `/api/convert`. Responses carry `Cache-Control: no-store`, and the Worker never logs or persists payloads. Configure Cloudflare WAF Rate Limiting rules on `/api/convert` for public production deployments.

