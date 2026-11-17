use super::*;

#[test]
fn a_declared_length_past_the_document_limit_is_refused_before_the_body() {
    let limit = MAX_VIEW_DOC_BYTES as u64;
    assert!(declared_too_large(Some(&(limit + 1).to_string())));
    assert!(declared_too_large(Some(" 104857600 ")), "100 MiB");
    // More digits than any integer holds: still a length, and past the limit.
    assert!(declared_too_large(Some("99999999999999999999999999")));
}

#[test]
fn a_length_within_the_limit_or_none_reads_the_body() {
    let limit = MAX_VIEW_DOC_BYTES as u64;
    assert!(!declared_too_large(Some(&limit.to_string())));
    assert!(!declared_too_large(Some("0")));
    assert!(!declared_too_large(Some("12345")));
    // Absent or not a length: read, and judged by the bytes that arrive.
    for unusable in [None, Some(""), Some("lots"), Some("-5"), Some("1e9"), Some("12 34")] {
        assert!(!declared_too_large(unusable), "{unusable:?}");
    }
}
