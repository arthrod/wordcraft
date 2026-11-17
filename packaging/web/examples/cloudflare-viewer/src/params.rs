//! Query parameters: `doc`, `page` and `scale`, every one hostile until checked.
//!
//! `doc` names a document of this deployment, read through the `ASSETS` binding and never
//! fetched as a URL. It is checked against an allow-list rather than a deny-list: ASCII letters,
//! digits, `.`, `_`, `-` and `/` only, so a scheme (`:`), a backslash, a percent sign, a space or a
//! control character can't appear at all; then no empty, `.` or `..` segment and no segment that
//! starts with a dot; then the [`DOC_PREFIX`] folder and the `.docx` extension. The check runs on
//! the decoded value, so `%2e%2e` is `..` by then and refused, and a doubly encoded `%252e`
//! decodes to `%2e`, whose `%` is refused.

use crate::limits::{DOC_PREFIX, MAX_DOC_PARAM_BYTES, MAX_PAGE_NUMBER, MAX_SCALE, MIN_SCALE};

/// A `doc` value that passed [`validate_doc`]: a relative path under [`DOC_PREFIX`], ASCII, safe
/// to put in a URL query or an HTML attribute as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocPath(String);

impl DocPath {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The file name, for titles.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }
}

/// Why a `doc` value was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocRejection {
    Empty,
    TooLong,
    /// A byte outside the allow-list: a scheme's `:`, a backslash, `%`, a space, a control
    /// character, anything not ASCII.
    BadCharacter,
    /// An empty, `.` or `..` segment, or one starting with a dot.
    Traversal,
    /// Not under [`DOC_PREFIX`] (an absolute path included).
    NotUnderPrefix,
    NotDocx,
}

/// Why a query was refused: always a 400.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamError {
    BadDoc(DocRejection),
    BadPage,
    BadScale,
    /// A parameter given twice: which one counts would be a guess.
    Repeated(&'static str),
}

impl std::fmt::Display for ParamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParamError::BadDoc(DocRejection::Empty) => write!(f, "the doc parameter is empty"),
            ParamError::BadDoc(DocRejection::TooLong) => write!(f, "the doc parameter is longer than {MAX_DOC_PARAM_BYTES} bytes"),
            ParamError::BadDoc(DocRejection::BadCharacter) => {
                write!(f, "the doc parameter may hold only letters, digits, '.', '_', '-' and '/'")
            }
            ParamError::BadDoc(DocRejection::Traversal) => write!(f, "the doc parameter has an empty, '.' or '..' segment"),
            ParamError::BadDoc(DocRejection::NotUnderPrefix) => write!(f, "the doc parameter must name a file under {DOC_PREFIX}"),
            ParamError::BadDoc(DocRejection::NotDocx) => write!(f, "the doc parameter must name a .docx file"),
            ParamError::BadPage => write!(f, "page must be a whole number from 1 to {MAX_PAGE_NUMBER}"),
            ParamError::BadScale => write!(f, "scale must be a number from {MIN_SCALE} to {MAX_SCALE}"),
            ParamError::Repeated(name) => write!(f, "the {name} parameter is given more than once"),
        }
    }
}

/// A parsed `/view` or `/view/page.png` query. `page` is 1-based.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewQuery {
    pub doc: Option<DocPath>,
    pub page: u32,
    pub scale: f32,
}

/// Check a decoded `doc` value (see the module notes).
pub fn validate_doc(raw: &str) -> Result<DocPath, DocRejection> {
    if raw.is_empty() {
        return Err(DocRejection::Empty);
    }
    if raw.len() > MAX_DOC_PARAM_BYTES {
        return Err(DocRejection::TooLong);
    }
    if !raw.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/')) {
        return Err(DocRejection::BadCharacter);
    }
    // A leading `/` makes an empty first segment, so absolute and protocol-relative paths stop
    // here too.
    if raw.split('/').any(|segment| segment.is_empty() || segment.starts_with('.')) {
        return Err(DocRejection::Traversal);
    }
    if !raw.starts_with(DOC_PREFIX) {
        return Err(DocRejection::NotUnderPrefix);
    }
    if !raw.ends_with(".docx") {
        return Err(DocRejection::NotDocx);
    }
    Ok(DocPath(raw.to_string()))
}

/// A 1-based page number: ASCII digits only (no sign, no space, no exponent), 1 to
/// [`MAX_PAGE_NUMBER`].
pub fn parse_page(raw: &str) -> Result<u32, ParamError> {
    // Six digits cover MAX_PAGE_NUMBER; a longer string can't parse into range anyway.
    if raw.is_empty() || raw.len() > 6 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ParamError::BadPage);
    }
    match raw.parse::<u32>() {
        Ok(n) if (1..=MAX_PAGE_NUMBER).contains(&n) => Ok(n),
        _ => Err(ParamError::BadPage),
    }
}

/// A scale: digits, optionally a `.` and more digits (`1`, `1.25`, `.75`), finite, from
/// [`MIN_SCALE`] to [`MAX_SCALE`]. No sign, exponent, `NaN` or `inf` can get through the
/// character check, and the range check refuses whatever else the float parser makes of it.
pub fn parse_scale(raw: &str) -> Result<f32, ParamError> {
    let (whole, frac) = match raw.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (raw, None),
    };
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    let shaped = raw.len() <= 8
        && digits(whole)
        && match frac {
            Some(f) => !f.is_empty() && digits(f),
            None => !whole.is_empty(),
        };
    if !shaped {
        return Err(ParamError::BadScale);
    }
    match raw.parse::<f32>() {
        Ok(s) if s.is_finite() && (MIN_SCALE..=MAX_SCALE).contains(&s) => Ok(s),
        _ => Err(ParamError::BadScale),
    }
}

/// Parse a raw (still percent-encoded) query string. Unknown parameters are ignored; a known one
/// given twice is refused. `page` defaults to 1 and `scale` to `default_scale`.
pub fn parse_view_query(query: Option<&str>, default_scale: f32) -> Result<ViewQuery, ParamError> {
    let (mut doc, mut page, mut scale) = (None, None, None);
    for (key, value) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        let (slot, name) = match key.as_ref() {
            "doc" => (&mut doc, "doc"),
            "page" => (&mut page, "page"),
            "scale" => (&mut scale, "scale"),
            _ => continue,
        };
        if slot.replace(value).is_some() {
            return Err(ParamError::Repeated(name));
        }
    }
    Ok(ViewQuery {
        doc: doc.map(|d| validate_doc(&d)).transpose().map_err(ParamError::BadDoc)?,
        page: page.map(|p| parse_page(&p)).transpose()?.unwrap_or(1),
        scale: scale.map(|s| parse_scale(&s)).transpose()?.unwrap_or(default_scale),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sample_path_is_accepted() {
        assert_eq!(validate_doc("samples/welcome.docx").map(|d| d.as_str().to_string()), Ok("samples/welcome.docx".to_string()));
        assert_eq!(validate_doc("samples/sub-dir/My_File.2.docx").map(|d| d.file_name().to_string()), Ok("My_File.2.docx".to_string()));
    }

    #[test]
    fn every_rejection_class_of_doc() {
        let cases: &[(&str, DocRejection)] = &[
            ("", DocRejection::Empty),
            // Traversal, plain and hidden behind the prefix.
            ("../../etc/passwd", DocRejection::Traversal),
            ("samples/../../etc/passwd.docx", DocRejection::Traversal),
            ("samples/./welcome.docx", DocRejection::Traversal),
            ("samples//welcome.docx", DocRejection::Traversal),
            ("samples/.hidden.docx", DocRejection::Traversal),
            // Absolute URL and scheme: the ':' is not in the allow-list.
            ("https://example.com/x.docx", DocRejection::BadCharacter),
            ("http://127.0.0.1/samples/welcome.docx", DocRejection::BadCharacter),
            ("file:samples/welcome.docx", DocRejection::BadCharacter),
            ("data:application/x,samples/a.docx", DocRejection::BadCharacter),
            // Protocol-relative and absolute paths.
            ("//example.com/samples/x.docx", DocRejection::Traversal),
            ("/samples/welcome.docx", DocRejection::Traversal),
            ("/etc/passwd.docx", DocRejection::Traversal),
            // Backslash.
            ("samples\\..\\secret.docx", DocRejection::BadCharacter),
            ("samples\\welcome.docx", DocRejection::BadCharacter),
            // Wrong extension, or none.
            ("samples/welcome.md", DocRejection::NotDocx),
            ("samples/welcome.docx.exe", DocRejection::NotDocx),
            ("samples/welcome.DOCX", DocRejection::NotDocx),
            ("samples/welcome", DocRejection::NotDocx),
            // Outside the folder.
            ("index.docx", DocRejection::NotUnderPrefix),
            ("fonts/x.docx", DocRejection::NotUnderPrefix),
            ("samplesx/welcome.docx", DocRejection::NotUnderPrefix),
            // Control characters, a NUL, a space, non-ASCII.
            ("samples/wel\ncome.docx", DocRejection::BadCharacter),
            ("samples/welcome.docx\0.png", DocRejection::BadCharacter),
            ("samples/wel\tcome.docx", DocRejection::BadCharacter),
            ("samples/wel\u{7f}come.docx", DocRejection::BadCharacter),
            ("samples/wel come.docx", DocRejection::BadCharacter),
            ("samples/wélcome.docx", DocRejection::BadCharacter),
            // A percent sign left after decoding (a doubly encoded value).
            ("samples/%2e%2e/x.docx", DocRejection::BadCharacter),
        ];
        for (raw, want) in cases {
            assert_eq!(validate_doc(raw), Err(*want), "doc {raw:?}");
        }
        let long = format!("samples/{}.docx", "a".repeat(MAX_DOC_PARAM_BYTES));
        assert_eq!(validate_doc(&long), Err(DocRejection::TooLong));
        let just_fits = format!("samples/{}.docx", "a".repeat(MAX_DOC_PARAM_BYTES - "samples/.docx".len()));
        assert!(validate_doc(&just_fits).is_ok());
    }

    #[test]
    fn percent_encoded_traversal_is_refused_after_decoding() {
        for q in [
            "doc=samples%2F..%2F..%2Fetc%2Fpasswd.docx",
            "doc=samples/%2e%2e/%2e%2e/etc/passwd.docx",
            "doc=samples/%2E%2E/secret.docx",
            "doc=%2e%2e/%2e%2e/etc/passwd",
        ] {
            assert_eq!(parse_view_query(Some(q), 1.0), Err(ParamError::BadDoc(DocRejection::Traversal)), "query {q:?}");
        }
        // Doubly encoded: one decode leaves a '%', which is refused.
        assert_eq!(parse_view_query(Some("doc=samples/%252e%252e/x.docx"), 1.0), Err(ParamError::BadDoc(DocRejection::BadCharacter)));
        // An encoded scheme, backslash or NUL.
        assert_eq!(parse_view_query(Some("doc=https%3A%2F%2Fexample.com%2Fx.docx"), 1.0), Err(ParamError::BadDoc(DocRejection::BadCharacter)));
        assert_eq!(parse_view_query(Some("doc=samples%5Cwelcome.docx"), 1.0), Err(ParamError::BadDoc(DocRejection::BadCharacter)));
        assert_eq!(parse_view_query(Some("doc=samples/a%00.docx"), 1.0), Err(ParamError::BadDoc(DocRejection::BadCharacter)));
        // '+' decodes to a space in a query.
        assert_eq!(parse_view_query(Some("doc=samples/a+b.docx"), 1.0), Err(ParamError::BadDoc(DocRejection::BadCharacter)));
    }

    #[test]
    fn page_junk_is_refused() {
        for raw in ["0", "-1", "-0", "+1", " 1", "1 ", "1.0", "1e3", "NaN", "inf", "", "abc", "0x10", "99999999999999999999", "100001", "١"] {
            assert_eq!(parse_page(raw), Err(ParamError::BadPage), "page {raw:?}");
        }
        assert_eq!(parse_page("1"), Ok(1));
        assert_eq!(parse_page("007"), Ok(7));
        assert_eq!(parse_page("100000"), Ok(MAX_PAGE_NUMBER));
    }

    #[test]
    fn scale_junk_is_refused() {
        for raw in [
            "0",
            "0.0",
            "-1",
            "-0.5",
            "0.49",
            "2.01",
            "3",
            "1e0",
            "1e309",
            "NaN",
            "nan",
            "inf",
            "Infinity",
            "-inf",
            "",
            ".",
            "1.",
            "1..5",
            "1.5.0",
            "+1",
            " 1",
            "abc",
            "0x1",
            "1,5",
            "99999999999999999999999999999999999999999",
        ] {
            assert_eq!(parse_scale(raw), Err(ParamError::BadScale), "scale {raw:?}");
        }
        assert_eq!(parse_scale("1"), Ok(1.0));
        assert_eq!(parse_scale("0.5"), Ok(0.5));
        assert_eq!(parse_scale("2"), Ok(2.0));
        assert_eq!(parse_scale("1.25"), Ok(1.25));
        assert_eq!(parse_scale(".75"), Ok(0.75));
    }

    #[test]
    fn query_defaults_repeats_and_unknowns() {
        assert_eq!(parse_view_query(None, 1.0), Ok(ViewQuery { doc: None, page: 1, scale: 1.0 }));
        assert_eq!(parse_view_query(Some(""), 1.25), Ok(ViewQuery { doc: None, page: 1, scale: 1.25 }));
        let q = parse_view_query(Some("doc=samples/welcome.docx&page=3&scale=1.5&utm=x"), 1.0);
        assert_eq!(q, Ok(ViewQuery { doc: validate_doc("samples/welcome.docx").ok(), page: 3, scale: 1.5 }));
        assert_eq!(parse_view_query(Some("doc=samples/a.docx&doc=samples/b.docx"), 1.0), Err(ParamError::Repeated("doc")));
        assert_eq!(parse_view_query(Some("page=1&page=2"), 1.0), Err(ParamError::Repeated("page")));
        assert_eq!(parse_view_query(Some("scale=1&scale=1"), 1.0), Err(ParamError::Repeated("scale")));
        assert_eq!(parse_view_query(Some("doc=samples/a.docx&page=0"), 1.0), Err(ParamError::BadPage));
        assert_eq!(parse_view_query(Some("doc=samples/a.docx&scale=NaN"), 1.0), Err(ParamError::BadScale));
        // An empty `doc=` is a bad doc, not "no doc".
        assert_eq!(parse_view_query(Some("doc="), 1.0), Err(ParamError::BadDoc(DocRejection::Empty)));
        // Hostile but harmless: a huge number of unknown parameters is just ignored.
        let many = "x=1&".repeat(10_000);
        assert_eq!(parse_view_query(Some(&many), 1.0).map(|q| q.page), Ok(1));
        // Every error has a message.
        assert!(!ParamError::BadDoc(DocRejection::NotDocx).to_string().is_empty());
    }
}
