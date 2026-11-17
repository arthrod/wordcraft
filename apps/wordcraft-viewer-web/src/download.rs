//! A document download, judged by what its response declares before the body is read.

use wordcraft_worker_proto::MAX_VIEW_DOC_BYTES;

/// Whether a response whose `Content-Length` header is `content_length` declares a body past
/// the largest document the viewer opens, so the body need not be read at all. A response with
/// no usable length is read as before and judged by its size once it has arrived (there is no
/// streaming read with a running cap).
pub fn declared_too_large(content_length: Option<&str>) -> bool {
    let Some(value) = content_length.map(str::trim).filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())) else {
        return false;
    };
    // All digits and still not a `u64`: longer than any limit.
    value.parse::<u64>().map_or(true, |declared| declared > MAX_VIEW_DOC_BYTES as u64)
}

#[cfg(test)]
mod tests;
