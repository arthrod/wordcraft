//! A bounded `multipart/form-data` reader (RFC 7578) for the upload form: a `file` part and an
//! optional `password` part. The body is already capped ([`crate::limits::MAX_BODY_BYTES`]); here
//! the number of parts, each header block and the password are bounded too, and the delimiter
//! search is linear in the body whatever its bytes are. Nothing is copied but the password.

use crate::limits::{MAX_FORM_PARTS, MAX_PART_HEADER_BYTES};

/// The longest password accepted, in characters: what Word accepts
/// (`craft_offcrypto::MAX_PASSWORD_CHARS`).
pub const MAX_PASSWORD_CHARS: usize = craft_offcrypto::MAX_PASSWORD_CHARS;

/// The longest boundary RFC 2046 allows.
const MAX_BOUNDARY_BYTES: usize = 70;

/// What the form carried.
#[derive(Debug, PartialEq, Eq)]
pub struct Upload<'a> {
    pub file: &'a [u8],
    /// The name the browser gave the file, when it is printable and short (shown in the result
    /// page's title, escaped).
    pub file_name: Option<String>,
    /// `None` when the field was absent or empty.
    pub password: Option<String>,
}

/// Why a form was refused: always a 4xx.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormError {
    /// The Content-Type isn't `multipart/form-data` (415).
    NotMultipart,
    /// No usable boundary parameter (400).
    BadBoundary,
    /// The body doesn't follow the multipart framing (400).
    Malformed,
    /// More than [`MAX_FORM_PARTS`] parts (400).
    TooManyParts,
    /// A field given twice (400).
    Repeated(&'static str),
    /// No `file` part, or an empty one (400).
    NoFile,
    /// The password isn't UTF-8 or is longer than [`MAX_PASSWORD_CHARS`] (400).
    BadPassword,
}

impl std::fmt::Display for FormError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormError::NotMultipart => write!(f, "the upload must be sent as multipart/form-data"),
            FormError::BadBoundary => write!(f, "the form data has no usable boundary"),
            FormError::Malformed => write!(f, "the form data is malformed"),
            FormError::TooManyParts => write!(f, "the form has more than {MAX_FORM_PARTS} fields"),
            FormError::Repeated(name) => write!(f, "the form has more than one {name} field"),
            FormError::NoFile => write!(f, "choose a .docx file to upload"),
            FormError::BadPassword => write!(f, "a password is at most {MAX_PASSWORD_CHARS} characters of text"),
        }
    }
}

/// The boundary of a `multipart/form-data` Content-Type header value.
pub fn boundary(content_type: &str) -> Result<String, FormError> {
    let mut params = header_params(content_type).into_iter();
    let Some((media_type, None)) = params.next() else { return Err(FormError::NotMultipart) };
    if !media_type.eq_ignore_ascii_case("multipart/form-data") {
        return Err(FormError::NotMultipart);
    }
    let value = params.find_map(|(k, v)| if k.eq_ignore_ascii_case("boundary") { v } else { None }).ok_or(FormError::BadBoundary)?;
    // RFC 2046 §5.1.1: 1 to 70 characters, printable ASCII (space allowed, not last).
    if value.is_empty() || value.len() > MAX_BOUNDARY_BYTES || !value.bytes().all(|b| (0x20..0x7f).contains(&b)) || value.ends_with(' ') {
        return Err(FormError::BadBoundary);
    }
    Ok(value)
}

/// `value; key=value; key="quoted;value"` split into its first item and its parameters, quotes
/// honoured. Keys are trimmed; a bare item has no value.
fn header_params(header: &str) -> Vec<(String, Option<String>)> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    // No backslash escapes: browsers send file names with `"` as `%22` and backslashes as they
    // are (WHATWG form-data encoding), so a Windows path stays a path.
    for c in header.chars() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => items.push(std::mem::take(&mut current)),
            c => current.push(c),
        }
    }
    items.push(current);
    items
        .into_iter()
        .map(|item| match item.split_once('=') {
            Some((k, v)) => (k.trim().to_string(), Some(v.trim().to_string())),
            None => (item.trim().to_string(), None),
        })
        .collect()
}

/// A Knuth–Morris–Pratt search for one needle: linear in the haystack whatever its bytes, so a
/// body full of near-misses of the delimiter costs no more than any other.
struct Finder {
    needle: Vec<u8>,
    /// `fail[i]`: the length of the longest proper prefix of `needle[..=i]` that is also a suffix.
    fail: Vec<usize>,
}

impl Finder {
    fn new(needle: Vec<u8>) -> Finder {
        let mut fail = vec![0; needle.len()];
        let mut k = 0;
        for i in 1..needle.len() {
            while k > 0 && needle.get(i) != needle.get(k) {
                k = fail.get(k - 1).copied().unwrap_or(0);
            }
            if needle.get(i) == needle.get(k) {
                k += 1;
            }
            if let Some(f) = fail.get_mut(i) {
                *f = k;
            }
        }
        Finder { needle, fail }
    }

    /// The first match at or after `from`.
    fn find(&self, haystack: &[u8], from: usize) -> Option<usize> {
        if self.needle.is_empty() {
            return None;
        }
        let mut k = 0;
        for (i, b) in haystack.iter().enumerate().skip(from) {
            while k > 0 && self.needle.get(k) != Some(b) {
                k = self.fail.get(k - 1).copied().unwrap_or(0);
            }
            if self.needle.get(k) == Some(b) {
                k += 1;
            }
            if k == self.needle.len() {
                return Some(i + 1 - k);
            }
        }
        None
    }
}

/// Read the form in `body`, framed by `boundary`.
pub fn parse<'a>(body: &'a [u8], boundary: &str) -> Result<Upload<'a>, FormError> {
    // Every delimiter but the first is preceded by CRLF; the first may open the body.
    let dash_boundary = format!("--{boundary}").into_bytes();
    let delimiter = Finder::new([b"\r\n".as_slice(), &dash_boundary].concat());
    let mut at = if body.starts_with(&dash_boundary) {
        dash_boundary.len()
    } else {
        delimiter.find(body, 0).ok_or(FormError::Malformed)?.saturating_add(2 + dash_boundary.len())
    };
    let header_end = Finder::new(b"\r\n\r\n".to_vec());
    let (mut file, mut file_name, mut password) = (None, None, None);
    let mut parts = 0usize;
    loop {
        let rest = body.get(at..).ok_or(FormError::Malformed)?;
        if rest.starts_with(b"--") {
            break; // The close delimiter.
        }
        // Transport padding (spaces and tabs) may follow a delimiter before its CRLF.
        let pad = rest.iter().take_while(|b| matches!(b, b' ' | b'\t')).count();
        if !rest.get(pad..).is_some_and(|r| r.starts_with(b"\r\n")) {
            return Err(FormError::Malformed);
        }
        parts += 1;
        if parts > MAX_FORM_PARTS {
            return Err(FormError::TooManyParts);
        }
        let part_start = at.saturating_add(pad + 2);
        // Headers end at the first blank line, within the header cap.
        let window_end = part_start.saturating_add(MAX_PART_HEADER_BYTES + 4).min(body.len());
        let window = body.get(..window_end).ok_or(FormError::Malformed)?;
        let headers_end = header_end.find(window, part_start).ok_or(FormError::Malformed)?;
        let headers = std::str::from_utf8(body.get(part_start..headers_end).ok_or(FormError::Malformed)?).map_err(|_| FormError::Malformed)?;
        let data_start = headers_end.saturating_add(4);
        let data_end = delimiter.find(body, data_start).ok_or(FormError::Malformed)?;
        let data = body.get(data_start..data_end).ok_or(FormError::Malformed)?;
        at = data_end.saturating_add(2 + dash_boundary.len());

        let (name, part_file_name) = disposition(headers).ok_or(FormError::Malformed)?;
        match name.as_str() {
            "file" => {
                if file.replace(data).is_some() {
                    return Err(FormError::Repeated("file"));
                }
                file_name = part_file_name.as_deref().and_then(shown_file_name);
            }
            "password" => {
                let text = std::str::from_utf8(data).map_err(|_| FormError::BadPassword)?;
                if text.chars().count() > MAX_PASSWORD_CHARS {
                    return Err(FormError::BadPassword);
                }
                if password.replace(text).is_some() {
                    return Err(FormError::Repeated("password"));
                }
            }
            _ => {}
        }
    }
    let file = file.filter(|f| !f.is_empty()).ok_or(FormError::NoFile)?;
    Ok(Upload { file, file_name, password: password.filter(|p| !p.is_empty()).map(str::to_string) })
}

/// The `name` and `filename` of a part's `Content-Disposition: form-data` header.
fn disposition(headers: &str) -> Option<(String, Option<String>)> {
    let value = headers.split("\r\n").find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case("content-disposition").then_some(value)
    })?;
    let mut params = header_params(value).into_iter();
    let (kind, _) = params.next()?;
    if !kind.eq_ignore_ascii_case("form-data") {
        return None;
    }
    let (mut name, mut file_name) = (None, None);
    for (k, v) in params {
        if k.eq_ignore_ascii_case("name") {
            name = v;
        } else if k.eq_ignore_ascii_case("filename") {
            file_name = v;
        }
    }
    Some((name?, file_name))
}

/// The last path component of a browser-supplied file name, when it is short and printable: no
/// control character and no bidirectional formatting character, which would show the name's
/// characters in another order than they are.
fn shown_file_name(raw: &str) -> Option<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw).trim();
    (!base.is_empty() && base.chars().count() <= 120 && !base.chars().any(|c| c.is_control() || is_bidi_control(c))).then(|| base.to_string())
}

/// The bidirectional formatting characters (Unicode Standard Annex #9, table 4): embeddings and
/// overrides (U+202A..U+202E), isolates (U+2066..U+2069) and the implicit marks (U+200E, U+200F,
/// U+061C).
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(boundary: &str, parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, file_name, data) in parts {
            out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match file_name {
                Some(f) => out.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\nContent-Type: application/octet-stream\r\n\r\n")
                        .as_bytes(),
                ),
                None => out.extend_from_slice(format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes()),
            }
            out.extend_from_slice(data);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        out
    }

    #[test]
    fn boundary_from_content_type() {
        assert_eq!(boundary("multipart/form-data; boundary=abc123").as_deref(), Ok("abc123"));
        assert_eq!(boundary("Multipart/Form-Data;boundary=\"a b;c\"").as_deref(), Ok("a b;c"));
        assert_eq!(boundary("multipart/form-data; charset=utf-8; boundary=----x").as_deref(), Ok("----x"));
        assert_eq!(boundary("application/x-www-form-urlencoded"), Err(FormError::NotMultipart));
        assert_eq!(boundary("text/plain"), Err(FormError::NotMultipart));
        assert_eq!(boundary("multipart/form-data"), Err(FormError::BadBoundary));
        assert_eq!(boundary("multipart/form-data; boundary="), Err(FormError::BadBoundary));
        assert_eq!(boundary(&format!("multipart/form-data; boundary={}", "x".repeat(71))), Err(FormError::BadBoundary));
        assert_eq!(boundary("multipart/form-data; boundary=a\u{1}b"), Err(FormError::BadBoundary));
    }

    #[test]
    fn reads_file_and_password() {
        let body = form("XyZ", &[("file", Some("report.docx"), b"PK\x03\x04data\r\n--Xy"), ("password", None, b"s3cret")]);
        let up = parse(&body, "XyZ").unwrap();
        assert_eq!(up.file, b"PK\x03\x04data\r\n--Xy");
        assert_eq!(up.file_name.as_deref(), Some("report.docx"));
        assert_eq!(up.password.as_deref(), Some("s3cret"));
        // An empty password is no password; unknown fields are ignored.
        let body = form("b", &[("password", None, b""), ("note", None, b"hi"), ("file", Some("a.docx"), b"x")]);
        let up = parse(&body, "b").unwrap();
        assert_eq!((up.file, up.password), (&b"x"[..], None));
    }

    #[test]
    fn refuses_bad_forms() {
        assert_eq!(parse(b"", "b"), Err(FormError::Malformed));
        assert_eq!(parse(b"garbage with no delimiter", "b"), Err(FormError::Malformed));
        // A part whose header block never ends.
        let mut endless = b"--b\r\nContent-Disposition: form-data; name=\"file\"".to_vec();
        endless.extend(std::iter::repeat_n(b'x', MAX_PART_HEADER_BYTES + 10));
        assert_eq!(parse(&endless, "b"), Err(FormError::Malformed));
        // No closing delimiter.
        assert_eq!(parse(b"--b\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\nabc", "b"), Err(FormError::Malformed));
        // No file, an empty file, two files.
        assert_eq!(parse(&form("b", &[("password", None, b"p")]), "b"), Err(FormError::NoFile));
        assert_eq!(parse(&form("b", &[("file", Some("a.docx"), b"")]), "b"), Err(FormError::NoFile));
        assert_eq!(parse(&form("b", &[("file", Some("a"), b"1"), ("file", Some("b"), b"2")]), "b"), Err(FormError::Repeated("file")));
        assert_eq!(
            parse(&form("b", &[("file", Some("a"), b"1"), ("password", None, b"x"), ("password", None, b"y")]), "b"),
            Err(FormError::Repeated("password"))
        );
        // Too many parts.
        let many: Vec<(&str, Option<&str>, &[u8])> = (0..=MAX_FORM_PARTS).map(|_| ("x", None, &b"1"[..])).collect();
        assert_eq!(parse(&form("b", &many), "b"), Err(FormError::TooManyParts));
        // A password too long, or not text.
        let long = "p".repeat(MAX_PASSWORD_CHARS + 1);
        assert_eq!(parse(&form("b", &[("file", Some("a"), b"1"), ("password", None, long.as_bytes())]), "b"), Err(FormError::BadPassword));
        assert_eq!(parse(&form("b", &[("file", Some("a"), b"1"), ("password", None, b"\xff\xfe")]), "b"), Err(FormError::BadPassword));
        // A part with no Content-Disposition name.
        assert_eq!(parse(b"--b\r\nContent-Type: text/plain\r\n\r\nx\r\n--b--\r\n", "b"), Err(FormError::Malformed));
    }

    #[test]
    fn a_hostile_body_is_read_in_linear_time() {
        // Near-misses of the delimiter everywhere: a quadratic search would take minutes.
        let boundary = "a".repeat(70);
        let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\n").into_bytes();
        let near = format!("\r\n--{}", "a".repeat(69));
        while body.len() < 4 << 20 {
            body.extend_from_slice(near.as_bytes());
        }
        let t = std::time::Instant::now();
        assert_eq!(parse(&body, &boundary), Err(FormError::Malformed));
        assert!(t.elapsed() < std::time::Duration::from_secs(2), "{:?}", t.elapsed());
    }

    #[test]
    fn file_names_are_kept_only_when_printable_and_short() {
        let body = form("b", &[("file", Some("a\u{7}b.docx"), b"1")]);
        assert_eq!(parse(&body, "b").unwrap().file_name, None);
        let long = "n".repeat(300);
        let body = form("b", &[("file", Some(&long), b"1")]);
        assert_eq!(parse(&body, "b").unwrap().file_name, None);
        let body = form("b", &[("file", Some("C:\\Users\\x\\report.docx"), b"1")]);
        assert_eq!(parse(&body, "b").unwrap().file_name.as_deref(), Some("report.docx"));
    }

    /// A name that would show its characters in another order ("Trojan Source"): every bidi
    /// embedding, override, isolate and mark falls back to the generic label, as a control
    /// character does. Right-to-left letters themselves are kept.
    #[test]
    fn file_names_with_bidi_controls_are_not_shown() {
        for c in [
            '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}', '\u{200E}', '\u{200F}',
            '\u{061C}',
        ] {
            let name = format!("invoice{c}xcod.exe.docx");
            let body = form("b", &[("file", Some(&name), b"1")]);
            assert_eq!(parse(&body, "b").unwrap().file_name, None, "U+{:04X}", u32::from(c));
        }
        let body = form("b", &[("file", Some("\u{5D3}\u{5D5}\u{5D7}.docx"), b"1")]);
        assert_eq!(parse(&body, "b").unwrap().file_name.as_deref(), Some("\u{5D3}\u{5D5}\u{5D7}.docx"));
    }
}
