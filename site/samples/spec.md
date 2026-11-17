# WordCraftWeb Architecture & Browser Capabilities

WordCraftWeb runs the clean-room Rust **WordCraft** document engine, layout engine, OOXML codec, and GPU canvas UI completely inside WebAssembly.

## Key Highlights

- **Zero-copy local editing**: `.docx`, `.md`, and `.txt` files are parsed and saved client-side in your browser without uploading document contents.
- **Microsoft Word parity**: full paragraph styles, tables, headers/footers, tracked changes, comments, and math typesetting.
- **Scriptable Promise API**: control the live document from JavaScript via `start(options)` or `window.wordcraft`.

> Edit this Markdown live in the Demo Bar split-pane and click **Apply Markdown** to update the formatted canvas immediately.
