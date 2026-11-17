use super::*;

#[test]
fn every_pending_request_is_answered_even_when_its_error_cannot_be_encoded() {
    let mut answered = Vec::new();
    settle_all(vec![(1, "open"), (2, "installFont")], Err(proto::ProtoError::Encode("refused by the test".into())), |id, what, payload| {
        answered.push((id, what, proto::decode_payload::<proto::ErrorReply>(payload).ok().map(|r| r.code)));
    });
    assert_eq!(answered, vec![(1, "open", Some(proto::ErrorCode::Internal)), (2, "installFont", Some(proto::ErrorCode::Internal))]);
}

#[test]
fn the_encoded_error_is_the_one_sent() {
    let reply = proto::ErrorReply { code: proto::ErrorCode::Internal, message: "stopped".into() };
    let mut answered = Vec::new();
    settle_all(vec![(7, ())], proto::encode_payload(&reply), |id, (), payload| {
        answered.push((id, proto::decode_payload::<proto::ErrorReply>(payload).ok().map(|r| r.message)));
    });
    assert_eq!(answered, vec![(7, Some("stopped".to_string()))]);
}
