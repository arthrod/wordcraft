# WordCraftWeb on Cloudflare Pages (100% Browser Mode)

Serves the static `dist/web` Trunk bundle (`wordcraft-web_bg.wasm`, hashed `.js`, `snippets/`, `i18n/`, `samples/`, and `_headers`) directly from Cloudflare Pages with zero backend code.

## Prerequisites

1. Build the hashed static site and generate `dist/web/_headers`:

   ```sh
   ./packaging/web/package.sh --static
   ```

2. Ensure [Wrangler](https://developers.cloudflare.com/workers/wrangler/) is installed and authenticated.

## Deploy

```sh
npx wrangler pages deploy dist/web --project-name wordcraft-web-pages
```

Or connect your fork in the Cloudflare Pages dashboard with:

- **Build command**: `./packaging/web/package.sh --static`
- **Build output directory**: `dist/web`

## Notes

- All document editing, layout, Markdown conversion, and `.docx` import/export run locally in the browser (`100% Browser WASM`).
- If you also want the optional Hybrid Edge `/api/convert` endpoint, use [`../cloudflare-workers/`](../cloudflare-workers/README.md) instead.

