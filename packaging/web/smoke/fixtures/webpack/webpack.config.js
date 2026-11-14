// Minimal zero-config-flavoured webpack build: no loaders, no plugins — the
// point of the matrix is what the bundler does with the package on its own
// (`new URL('./wordcraft-web_bg.wasm', import.meta.url)` asset emission).
export default {
  entry: "./src/main.js",
  output: {
    path: new URL("./dist/", import.meta.url).pathname,
    filename: "bundle.js",
    clean: true,
  },
};
