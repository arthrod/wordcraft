//! Answering every request still waiting on the viewer worker when it dies or is stopped.

use wordcraft_worker_proto as proto;

/// The `error` payload sent when the one made for the occasion could not be encoded: an
/// `internal` [`proto::ErrorReply`], written out so that sending it cannot fail.
const STOPPED: &[u8] = br#"{"code":"internal","message":"the viewer stopped"}"#;

/// Give each of `pending` the typed `error` payload `encoded`, so no caller waits for ever.
/// Should encoding it have failed, each still gets an `internal` error ([`STOPPED`]): the
/// callers waiting here (an `open()`, an `installFont()` promise) are settled whatever happens.
pub(crate) fn settle_all<S>(pending: Vec<(u32, S)>, encoded: Result<Vec<u8>, proto::ProtoError>, mut answer: impl FnMut(u32, S, &[u8])) {
    let payload = encoded.unwrap_or_else(|_| STOPPED.to_vec());
    for (id, sink) in pending {
        answer(id, sink, &payload);
    }
}

#[cfg(test)]
mod tests;
