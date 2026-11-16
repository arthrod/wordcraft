//! `cargo xtask web-page [<staging dir>]`: a Trunk `post_build` step for the viewer-first page
//! (plan 10.3). It runs on Trunk's staging directory (`TRUNK_STAGING_DIR`, or the argument)
//! after the page's `index.html` is written and before the build is published.
//!
//! Why it exists: Trunk runs `wasm-bindgen` for every Rust target of a page into one shared
//! output directory (`<target dir>/wasm-bindgen/release`) and gives the page's main target a
//! `<link rel="modulepreload">` for every file under its `snippets/` there. With the editor built
//! as a secondary target beside the page module, the editor's `eframe` snippet is in that
//! directory whenever the editor's `wasm-bindgen` ran first (this build or an earlier one), so
//! the page's first load would fetch a file of the editor — which a reader must never download.
//! The step drops each snippet preload that the main module's glue does not import. It changes
//! nothing else: the snippet files stay (the editor imports them when it loads), and the
//! integrity hashes of the links that remain are untouched.

use std::path::{Path, PathBuf};

/// The glue module the page's inline bootstrap imports (`import init, * as bindings from
/// './NAME.js'`), as a path relative to the page.
fn main_glue(html: &str) -> Option<&str> {
    let at = html.find("import init")?;
    let rest = html.get(at..)?;
    let from = rest.find(" from ")?;
    let quoted = rest.get(from + " from ".len()..)?.trim_start();
    let quote = quoted.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let inner = quoted.get(1..)?;
    let end = inner.find(quote)?;
    inner.get(..end).map(|p| p.trim_start_matches("./"))
}

/// `html` without the `<link rel="modulepreload" href="./snippets/…">` tags whose file the main
/// glue (`glue`, its text) does not import, and the hrefs dropped.
pub fn drop_foreign_snippet_preloads(html: &str, glue: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(html.len());
    let mut dropped = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<link") {
        let (before, tag_on) = rest.split_at(start);
        out.push_str(before);
        let Some(end) = tag_on.find('>') else {
            out.push_str(tag_on);
            return (out, dropped);
        };
        let (tag, after) = tag_on.split_at(end + 1);
        let href = attr(tag, "href").map(|h| h.trim_start_matches("./"));
        let foreign = tag.contains("rel=\"modulepreload\"") && href.is_some_and(|h| h.starts_with("snippets/") && !glue.contains(h));
        if foreign {
            dropped.extend(href.map(str::to_string));
        } else {
            out.push_str(tag);
        }
        rest = after;
    }
    out.push_str(rest);
    (out, dropped)
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    let at = tag.find(&needle)? + needle.len();
    let value = tag.get(at..)?;
    value.get(..value.find('"')?)
}

/// Apply [`drop_foreign_snippet_preloads`] to `<dir>/index.html`.
pub fn run(args: &[&str]) -> Result<(), String> {
    let dir = args
        .first()
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("TRUNK_STAGING_DIR").map(PathBuf::from))
        .ok_or("usage: cargo xtask web-page <staging dir> (or run it as a Trunk hook, which sets TRUNK_STAGING_DIR)")?;
    fix_dir(&dir)
}

fn fix_dir(dir: &Path) -> Result<(), String> {
    let index = dir.join("index.html");
    let html = std::fs::read_to_string(&index).map_err(|e| format!("{}: {e}", index.display()))?;
    let glue_name = main_glue(&html).ok_or_else(|| format!("{}: no `import init … from './….js'` bootstrap", index.display()))?;
    let glue_path = dir.join(glue_name);
    let glue = std::fs::read_to_string(&glue_path).map_err(|e| format!("{}: {e}", glue_path.display()))?;
    let (fixed, dropped) = drop_foreign_snippet_preloads(&html, &glue);
    if dropped.is_empty() {
        println!("web-page: every preload in {} is the main module's", index.display());
        return Ok(());
    }
    std::fs::write(&index, fixed).map_err(|e| format!("{}: {e}", index.display()))?;
    println!("web-page: dropped preloads of files {glue_name} does not import: {}", dropped.join(", "));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = concat!(
        "<head><script type=\"module\">\nimport init, * as bindings from './wordcraft-viewer-web-94897d8b7e46a69.js';\n</script>\n",
        "<link rel=\"modulepreload\" href=\"./wordcraft-viewer-web-94897d8b7e46a69.js\" crossorigin=\"anonymous\" integrity=\"sha384-a\">",
        "<link rel=\"modulepreload\" href=\"./snippets/eframe-d31a8450ea12dfc1/inline0.js\" crossorigin=\"anonymous\" integrity=\"sha384-b\">",
        "<link rel=\"modulepreload\" href=\"./snippets/wordcraft-viewer-web-1/inline0.js\" crossorigin=\"anonymous\" integrity=\"sha384-c\">",
        "<link rel=\"preload\" href=\"./wordcraft-viewer-web-609ed8fe88ab3216_bg.wasm\" as=\"fetch\" type=\"application/wasm\">",
        "<link data-x href=\"./snippets/eframe-d31a8450ea12dfc1/inline0.js\" rel=\"stylesheet\"></head>"
    );

    #[test]
    fn the_bootstrap_names_the_main_glue() {
        assert_eq!(main_glue(PAGE), Some("wordcraft-viewer-web-94897d8b7e46a69.js"));
        assert_eq!(main_glue("<script>import init from \"./a.js\";</script>"), Some("a.js"));
        assert_eq!(main_glue("<html></html>"), None);
        assert_eq!(main_glue("import init from x"), None);
    }

    #[test]
    fn only_snippet_preloads_the_main_glue_does_not_import_are_dropped() {
        let glue = "import { x } from './snippets/wordcraft-viewer-web-1/inline0.js';";
        let (fixed, dropped) = drop_foreign_snippet_preloads(PAGE, glue);
        assert_eq!(dropped, vec!["snippets/eframe-d31a8450ea12dfc1/inline0.js"]);
        assert!(!fixed.contains("sha384-b"), "the editor's snippet preload is gone");
        for kept in ["sha384-a", "sha384-c", "_bg.wasm", "rel=\"stylesheet\"", "<script type=\"module\">", "</head>"] {
            assert!(fixed.contains(kept), "{kept} stays");
        }
        assert_eq!(fixed.len(), PAGE.len() - "<link rel=\"modulepreload\" href=\"./snippets/eframe-d31a8450ea12dfc1/inline0.js\" crossorigin=\"anonymous\" integrity=\"sha384-b\">".len());
        // A glue that imports both keeps both; a page with nothing to drop is unchanged.
        let both = format!("{glue} './snippets/eframe-d31a8450ea12dfc1/inline0.js'");
        assert_eq!(drop_foreign_snippet_preloads(PAGE, &both), (PAGE.to_string(), Vec::new()));
    }

    #[test]
    fn unterminated_tags_and_junk_never_panic() {
        for junk in ["<link", "<link rel=\"modulepreload\" href=\"./snippets/a", "<<link>>", "", "<link href=\"\">"] {
            let (fixed, _) = drop_foreign_snippet_preloads(junk, "");
            assert!(fixed.len() <= junk.len());
        }
    }

    #[test]
    fn the_step_rewrites_a_staging_dir() {
        let dir = std::env::temp_dir().join(format!("wc_web_page_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), PAGE).unwrap();
        std::fs::write(dir.join("wordcraft-viewer-web-94897d8b7e46a69.js"), "export default 1;").unwrap();
        fix_dir(&dir).unwrap();
        let fixed = std::fs::read_to_string(dir.join("index.html")).unwrap();
        assert!(!fixed.contains("modulepreload\" href=\"./snippets/"), "{fixed}");
        // Run again: nothing left to drop.
        fix_dir(&dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("index.html")).unwrap(), fixed);
        std::fs::remove_file(dir.join("wordcraft-viewer-web-94897d8b7e46a69.js")).unwrap();
        assert!(fix_dir(&dir).is_err(), "a missing glue file is an error, not a silent pass");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
