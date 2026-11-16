//! The pages the Worker sends: small, static HTML. No script, no event attributes, no external
//! request: a stylesheet in the page and images from this origin or inline `data:` URIs. The
//! responses also carry a Content-Security-Policy ([`CONTENT_SECURITY_POLICY`]) that would block a
//! script even if one were added by mistake.

use crate::params::DocPath;

/// The policy every HTML response carries: nothing by default, images from this origin and
/// inline `data:` images, the one inline stylesheet, forms posted back here only.
pub const CONTENT_SECURITY_POLICY: &str =
    "default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";

/// One page image placed in an upload result.
pub struct InlinePage<'a> {
    pub number: usize,
    pub width: u32,
    pub height: u32,
    pub png: &'a [u8],
}

/// The page's only styling: grey desk, white pages, centred.
const STYLE: &str = "<style>body{margin:0;font:15px/1.4 system-ui,sans-serif;background:#e6e6e6;color:#222;text-align:center}\
nav,p,form{margin:.7em}a{color:#1546b0}img{display:block;margin:0 auto 1em;max-width:100%;height:auto;background:#fff;box-shadow:0 1px 4px #0005}</style>";

/// Escape text for HTML content and double-quoted attribute values.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// The start of every page: doctype, encoding, viewport, title and the stylesheet.
fn head(title: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title>{STYLE}",
        escape(title)
    )
}

/// A scale as a query value: the shortest form that reads back as the same number (`1`, `1.25`).
fn scale_value(scale: f32) -> String {
    format!("{scale}")
}

/// The viewer page for page `page` (1-based) of the deployed document `doc`, which has `pages`
/// pages; the image is `width`×`height` pixels at `scale`.
pub fn viewer_page(doc: &DocPath, page: u32, pages: usize, width: u32, height: u32, scale: f32) -> String {
    // `doc` is validated: letters, digits, `._-/` only, so it goes into URLs and attributes as is.
    let (d, s) = (doc.as_str(), scale_value(scale));
    let name = escape(doc.file_name());
    let link = |n: u32, label: &str| format!("<a href=\"/view?doc={d}&amp;page={n}&amp;scale={s}\">{label}</a>");
    let mut nav = String::new();
    if page > 1 {
        nav.push_str(&link(page - 1, "Previous"));
        nav.push_str(" · ");
    }
    nav.push_str(&format!("Page {page} of {pages}"));
    if usize::try_from(page).is_ok_and(|p| p < pages) {
        nav.push_str(" · ");
        nav.push_str(&link(page.saturating_add(1), "Next"));
    }
    format!(
        "{}<nav>{nav} · <a href=\"/?doc={d}\">Open in the editor</a> · <a href=\"/view\">Upload</a></nav>\
<img src=\"/view/page.png?doc={d}&amp;page={page}&amp;scale={s}\" width=\"{width}\" height=\"{height}\" alt=\"Page {page} of {name}\"></html>",
        head(&format!("{} – page {page} of {pages}", doc.file_name())),
    )
}

/// The upload form. `notice` is a sentence of the Worker's own (never anything from a request).
pub fn upload_form(notice: Option<&str>, ask_password: bool) -> String {
    let notice = notice.map(|n| format!("<p><strong>{}</strong></p>", escape(n))).unwrap_or_default();
    let password_hint = if ask_password { "Password" } else { "Password (only for a protected document)" };
    format!(
        "{}<h1>View a Word document</h1>{notice}<form method=\"post\" action=\"/view\" enctype=\"multipart/form-data\">\
<p><input type=\"file\" name=\"file\" accept=\".docx\" required></p>\
<p><label>{password_hint}: <input type=\"password\" name=\"password\" autocomplete=\"off\"></label></p>\
<p><button>View</button></p></form>\
<p>The document is drawn on the server and sent back as page images. It is not stored or logged.</p>\
<p><a href=\"/\">Open the editor</a></p></html>",
        head("View a Word document")
    )
}

/// The pages of an uploaded document: `pages` are those placed inline, of `total` in the
/// document; `left_out` explains why the rest are missing, when some are.
pub fn upload_result(file_name: Option<&str>, pages: &[InlinePage<'_>], total: usize, left_out: Option<&str>) -> String {
    use base64::Engine as _;
    let name = file_name.unwrap_or("Uploaded document");
    let mut out = head(name);
    out.push_str(&format!(
        "<nav>{} · {} of {total} page{} · <a href=\"/view\">View another</a> · <a href=\"/\">Open the editor</a></nav>",
        escape(name),
        pages.len(),
        if total == 1 { "" } else { "s" }
    ));
    if let Some(why) = left_out {
        out.push_str(&format!("<p><strong>{}</strong></p>", escape(why)));
    }
    for p in pages {
        out.push_str("<img src=\"data:image/png;base64,");
        base64::engine::general_purpose::STANDARD.encode_string(p.png, &mut out);
        out.push_str(&format!("\" width=\"{}\" height=\"{}\" alt=\"Page {}\">", p.width, p.height, p.number));
    }
    out.push_str("</html>");
    out
}

/// The bytes [`upload_result`] adds for one page, so pages can be counted against the response
/// cap before they are placed: the Base64 of the PNG and a generous bound for the tag around it.
pub fn inline_page_cost(png_len: usize) -> usize {
    png_len.div_ceil(3).saturating_mul(4).saturating_add(128)
}

/// Why an upload result shows `shown` of `total` pages: the inline page cap, or the response
/// size cap when `for_size`.
pub fn left_out_note(total: usize, shown: usize, for_size: bool) -> String {
    let missing = total.saturating_sub(shown);
    let what = if missing == 1 { "1 more page is not shown" } else { &format!("{missing} more pages are not shown") };
    if for_size {
        format!("{what}: their images would make this answer larger than {} MiB.", crate::limits::MAX_RESPONSE_BYTES >> 20)
    } else {
        format!("{what}: at most {} pages are shown inline.", crate::limits::MAX_INLINE_PAGES)
    }
}

/// A small error page. `detail` is the Worker's own wording (escaped all the same).
pub fn error_page(status: u16, title: &str, detail: &str) -> String {
    format!(
        "{}<h1>{status} {}</h1><p>{}</p><p><a href=\"/view\">View a document</a> · <a href=\"/\">Open the editor</a></p></html>",
        head(title),
        escape(title),
        escape(detail)
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::params::validate_doc;

    /// The checks AC1 asks of every page: no script element, no event-handler attribute, no
    /// `http:`/`https:` or protocol-relative URL.
    pub(crate) fn assert_inert(html: &str) {
        let lower = html.to_ascii_lowercase();
        assert!(!lower.contains("<script"), "a <script> in {html}");
        assert!(!lower.contains("javascript:"), "a javascript: URL in {html}");
        assert!(!lower.contains("http:") && !lower.contains("https:"), "an http(s) URL in {html}");
        assert!(!lower.contains("=\"//") && !lower.contains("='//"), "a protocol-relative URL in {html}");
        // ` on<letters>=` is an event handler attribute (onclick, onerror, onload...).
        for (at, _) in lower.match_indices(" on") {
            let rest = &lower[at + 3..];
            let name_len = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
            assert!(name_len == 0 || rest.as_bytes().get(name_len) != Some(&b'='), "an event attribute in {html}");
        }
    }

    fn sample() -> DocPath {
        validate_doc("samples/welcome.docx").unwrap()
    }

    #[test]
    fn escape_covers_the_five_characters() {
        assert_eq!(escape(r#"<a href="x">&'</a>"#), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;&lt;/a&gt;");
        assert_eq!(escape("plain"), "plain");
    }

    #[test]
    fn viewer_page_is_small_inert_and_complete() {
        let html = viewer_page(&sample(), 2, 3, 612, 792, 1.0);
        assert_inert(&html);
        assert!(html.len() < 2_000, "{} bytes", html.len());
        assert!(html.starts_with("<!doctype html>"));
        // The image, with its size so the layout doesn't shift, pointing at the PNG route.
        assert!(html.contains(r#"<img src="/view/page.png?doc=samples/welcome.docx&amp;page=2&amp;scale=1" width="612" height="792""#), "{html}");
        // Page number and count, previous and next, the editor.
        assert!(html.contains("Page 2 of 3"), "{html}");
        assert!(html.contains(r#"href="/view?doc=samples/welcome.docx&amp;page=1&amp;scale=1""#), "{html}");
        assert!(html.contains(r#"href="/view?doc=samples/welcome.docx&amp;page=3&amp;scale=1""#), "{html}");
        // The site's own page for the same document (viewer first, with its Edit button).
        assert!(html.contains(r#"href="/?doc=samples/welcome.docx""#), "{html}");
        // No link before the first page or past the last.
        let first = viewer_page(&sample(), 1, 1, 612, 792, 1.25);
        assert_inert(&first);
        assert!(!first.contains("page=0") && !first.contains("page=2"), "{first}");
        assert!(first.contains("scale=1.25"), "{first}");
    }

    #[test]
    fn forms_post_multipart_back_here() {
        for html in [upload_form(None, false), upload_form(Some("That file is not a .docx."), false), upload_form(Some("Wrong password."), true)] {
            assert_inert(&html);
            assert!(html.contains(r#"<form method="post" action="/view" enctype="multipart/form-data">"#), "{html}");
            assert!(html.contains(r#"<input type="file" name="file""#), "{html}");
            assert!(html.contains(r#"<input type="password" name="password""#), "{html}");
            assert!(html.len() < 2_000, "{} bytes", html.len());
        }
        assert!(upload_form(Some("x < y"), false).contains("x &lt; y"));
    }

    #[test]
    fn upload_result_embeds_data_pngs_and_says_what_was_left_out() {
        let png = [0x89u8, b'P', b'N', b'G'];
        let pages = [InlinePage { number: 1, width: 10, height: 20, png: &png }, InlinePage { number: 2, width: 10, height: 20, png: &png }];
        let html = upload_result(Some("<evil>.docx"), &pages, 5, Some("3 more pages are not shown: at most 2 pages are placed inline."));
        assert_inert(&html);
        assert_eq!(html.matches("<img src=\"data:image/png;base64,iVBORw==\" width=\"10\" height=\"20\"").count(), 2, "{html}");
        assert!(html.contains("&lt;evil&gt;.docx"), "{html}");
        assert!(html.contains("3 more pages are not shown"), "{html}");
        assert!(html.contains("2 of 5 pages"), "{html}");
        // The cost estimate covers what a page really adds.
        let none = upload_result(None, &[], 5, None);
        let one = upload_result(None, &pages[..1], 5, None);
        assert!(one.len() - none.len() <= inline_page_cost(png.len()), "{} > {}", one.len() - none.len(), inline_page_cost(png.len()));
    }

    #[test]
    fn left_out_notes_say_why() {
        assert_eq!(left_out_note(20, 12, false), "8 more pages are not shown: at most 12 pages are shown inline.");
        assert_eq!(left_out_note(13, 12, false), "1 more page is not shown: at most 12 pages are shown inline.");
        assert_eq!(left_out_note(5, 3, true), "2 more pages are not shown: their images would make this answer larger than 3 MiB.");
    }

    #[test]
    fn error_pages_are_inert() {
        let html = error_page(404, "No such page", "The document has 3 pages.");
        assert_inert(&html);
        assert!(html.contains("404") && html.contains("No such page") && html.contains("3 pages"));
        assert!(error_page(400, "<b>", "&").contains("&lt;b&gt;"));
    }
}
