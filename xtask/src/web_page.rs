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

/// `html` with every `<!-- ... -->` comment removed (an unterminated comment runs to the end).
pub(crate) fn without_comments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("<!--") {
        out.push_str(rest.get(..start).unwrap_or_default());
        let after = rest.get(start..).unwrap_or_default();
        rest = after.find("-->").and_then(|end| after.get(end.saturating_add(3)..)).unwrap_or_default();
    }
    out.push_str(rest);
    out
}

/// The glue module the page's inline bootstrap imports (`import init, * as bindings from
/// './NAME.js'`), as a path relative to the page. Comments are skipped: the page's own notes
/// quote the editor's `import init from './wordcraft-web.js'`, which is not the bootstrap. The
/// `from` clause is read as JavaScript reads it, not as Trunk happens to print it: any
/// whitespace or none before the quote (`from"./x.js"`, as a minifier writes it), either quote.
fn main_glue(html: &str) -> Option<String> {
    let live = without_comments(html);
    let at = live.find("import init")?;
    let rest = live.get(at..)?;
    let quoted = rest.match_indices("from").find_map(|(i, _)| {
        let before = rest.get(..i)?.chars().next_back();
        let after = rest.get(i.saturating_add("from".len())..)?.trim_start();
        let keyword = !before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$');
        (keyword && after.starts_with(['\'', '"'])).then_some(after)
    })?;
    let quote = quoted.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let inner = quoted.get(1..)?;
    let end = inner.find(quote)?;
    inner.get(..end).map(|p| p.trim_start_matches("./").to_string())
}

/// `html` without the `<link rel="modulepreload" href="./snippets/…">` tags whose file the main
/// glue (`glue`, its text) does not import, and the hrefs dropped.
pub fn drop_foreign_snippet_preloads(html: &str, glue: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(html.len());
    let mut dropped = Vec::new();
    let mut rest = html;
    while let Some(start) = link_start(rest) {
        let (before, tag_on) = rest.split_at(start);
        out.push_str(before);
        let Some(end) = tag_on.find('>') else {
            out.push_str(tag_on);
            return (out, dropped);
        };
        let (tag, after) = tag_on.split_at(end + 1);
        let href = attr(tag, "href").map(|h| h.trim_start_matches("./"));
        let preload = attr(tag, "rel").is_some_and(|rel| rel.split_ascii_whitespace().any(|t| t.eq_ignore_ascii_case("modulepreload")));
        let foreign = preload && href.is_some_and(|h| h.starts_with("snippets/") && !glue.contains(h));
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

/// Where the first `<link` start tag in `html` begins: the tag name in any case, followed by
/// a space, `/` or `>` (so `<linkx>` is not one).
fn link_start(html: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(at) = html.get(from..)?.find('<') {
        let start = from.saturating_add(at);
        let after = html.get(start.saturating_add(1)..)?;
        if after.get(..4).is_some_and(|n| n.eq_ignore_ascii_case("link"))
            && after.get(4..).and_then(|r| r.chars().next()).is_none_or(|c| c.is_ascii_whitespace() || c == '/' || c == '>')
        {
            return Some(start);
        }
        from = start.saturating_add(1);
    }
    None
}

/// The value of attribute `name` in the start tag `tag` (`<link …>`), read as HTML reads it:
/// double-quoted, single-quoted or unquoted (as a minifier writes it), with space around `=`
/// allowed; an attribute with no value is `""`. The name is matched whole, in any case, so
/// `data-href` is not `href`. `None` when the tag has no such attribute or a quote never closes.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let inside = tag.strip_prefix('<').unwrap_or(tag);
    // Past the tag name.
    let mut rest = inside.trim_start_matches(|c: char| !c.is_ascii_whitespace() && c != '>' && c != '/');
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '/');
        if rest.is_empty() || rest.starts_with('>') {
            return None;
        }
        let end = rest.find(|c: char| c.is_ascii_whitespace() || c == '=' || c == '>' || c == '/').unwrap_or(rest.len());
        let (key, after) = rest.split_at(end);
        let (value, next) = match after.trim_start().strip_prefix('=') {
            Some(v) => {
                let v = v.trim_start();
                match v.chars().next() {
                    Some(quote @ ('"' | '\'')) => {
                        let body = v.get(1..)?;
                        let close = body.find(quote)?;
                        (body.get(..close)?, body.get(close.saturating_add(1)..)?)
                    }
                    _ => v.split_at(v.find(|c: char| c.is_ascii_whitespace() || c == '>').unwrap_or(v.len())),
                }
            }
            None => ("", after),
        };
        if key.eq_ignore_ascii_case(name) {
            return Some(value);
        }
        // Each turn consumes at least one character: a name, or the `=` of a nameless value.
        rest = next;
    }
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
    let glue_path = dir.join(&glue_name);
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
        assert_eq!(main_glue(PAGE).as_deref(), Some("wordcraft-viewer-web-94897d8b7e46a69.js"));
        assert_eq!(main_glue("<script>import init from \"./a.js\";</script>").as_deref(), Some("a.js"));
        assert_eq!(main_glue("<html></html>"), None);
        assert_eq!(main_glue("import init from x"), None);
    }

    #[test]
    fn the_bootstrap_is_found_however_the_from_clause_is_spaced() {
        // What a minifier or a hand edit may write: no space before the quote, either quote, any
        // whitespace, and a binding name that contains `from`.
        for (html, want) in [
            ("<script type=\"module\">import init,*as bindings from\"./page-1.js\";</script>", "page-1.js"),
            ("<script>import init from'./page-2.js'</script>", "page-2.js"),
            ("<script>import init from \"./page-3.js\"</script>", "page-3.js"),
            ("<script>import init, * as bindings\n\tfrom\n\t'./page-4.js';</script>", "page-4.js"),
            ("<script>import init, * as fromage from './page-5.js';</script>", "page-5.js"),
            ("<script>import init,{from as f}from\"./page-6.js\"</script>", "page-6.js"),
        ] {
            assert_eq!(main_glue(html).as_deref(), Some(want), "{html}");
        }
        // `from` with no quoted path after it is no bootstrap.
        for html in ["import init from", "import init from x", "import init, * as from_x", "import initfrom'./a.js'"] {
            assert_eq!(main_glue(html), None, "{html}");
        }
    }

    #[test]
    fn an_import_quoted_in_a_comment_is_not_the_bootstrap() {
        // The page's notes quote the editor's loader. Wherever that comment sits, the bootstrap
        // is the one in the script.
        let before = "<!-- the loader: `import init from './wordcraft-web.js'; await init();` -->\n<script type=\"module\">import init from './page-1.js';</script>";
        assert_eq!(main_glue(before).as_deref(), Some("page-1.js"));
        let only = "<!-- import init from './wordcraft-web.js' -->";
        assert_eq!(main_glue(only), None);
        let open = "<script type=\"module\">import init from './page-1.js';</script><!-- import init from './x.js'";
        assert_eq!(main_glue(open).as_deref(), Some("page-1.js"));
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
    fn preloads_are_read_however_their_attributes_are_quoted() {
        // HTML allows double quotes, single quotes or none (a minifier drops them); an attribute
        // name is matched whole and in any case, and `rel` is a list of tokens.
        let glue = "import * from './snippets/kept-1/inline0.js';";
        let foreign = "snippets/eframe-d31a8450ea12dfc1/inline0.js";
        for tag in [
            "<link rel=\"modulepreload\" href=\"./snippets/eframe-d31a8450ea12dfc1/inline0.js\" crossorigin=\"anonymous\">",
            "<link rel='modulepreload' href='./snippets/eframe-d31a8450ea12dfc1/inline0.js' crossorigin='anonymous'>",
            "<link crossorigin href=./snippets/eframe-d31a8450ea12dfc1/inline0.js integrity=sha384-Wn+k/q= rel=modulepreload>",
            "<LINK REL = \"preload modulepreload\" HREF =./snippets/eframe-d31a8450ea12dfc1/inline0.js />",
        ] {
            let page = format!("<head>{tag}<link rel=modulepreload href=./snippets/kept-1/inline0.js></head>");
            let (fixed, dropped) = drop_foreign_snippet_preloads(&page, glue);
            assert_eq!(dropped, vec![foreign.to_string()], "{tag}");
            assert_eq!(fixed, "<head><link rel=modulepreload href=./snippets/kept-1/inline0.js></head>", "{tag}");
        }
        // Another attribute ending in `href` or `rel` is not that attribute.
        let tag = "<link data-rel=modulepreload data-href=./snippets/eframe-d31a8450ea12dfc1/inline0.js rel=stylesheet>";
        assert_eq!(drop_foreign_snippet_preloads(tag, glue), (tag.to_string(), Vec::new()));
        assert_eq!(link_start("<a><linkx><Link rel=x>"), Some(10));
        assert_eq!(link_start("<a>é<link>"), Some(5));
        assert_eq!(link_start("<lin"), None);
        assert_eq!(attr("<link a=1 href='x' b>", "href"), Some("x"));
        assert_eq!(attr("<link href=>", "href"), Some(""));
        assert_eq!(attr("<link href>", "href"), Some(""));
        assert_eq!(attr("<link hrefx=y>", "href"), None);
        assert_eq!(attr("<link href=\"unterminated", "href"), None);
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
