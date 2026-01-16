use tenet::protocol::{
    AttachmentRef, ContentId, Envelope, Header, HeaderError, Payload, ProtocolVersion,
};

#[test]
fn envelope_roundtrips_via_json() {
    let payload = Payload {
        id: ContentId::from_bytes(b"payload-body"),
        content_type: "application/octet-stream".to_string(),
        body: "deadbeef".to_string(),
        attachments: vec![AttachmentRef {
            content_id: ContentId::from_bytes(b"attachment-1"),
            content_type: "image/png".to_string(),
            size: 128,
        }],
    };

    let version = ProtocolVersion::V1;
    let mut header = Header {
        sender_id: "sender-id".to_string(),
        recipient_id: "recipient-id".to_string(),
        timestamp: 1_700_000_000,
        message_id: ContentId::from_bytes(b"message-1"),
        ttl_seconds: 3_600,
        payload_size: payload.body.len() as u64,
        signature: None,
    };
    header.signature = Some(
        header
            .expected_signature(version)
            .expect("signature generation"),
    );

    let envelope = Envelope {
        version,
        header,
        payload,
    };

    let json = serde_json::to_string(&envelope).expect("serialize envelope");
    let decoded: Envelope = serde_json::from_str(&json).expect("deserialize envelope");

    assert_eq!(decoded, envelope);
}

#[test]
fn header_signature_fails_with_invalid_signature() {
    let version = ProtocolVersion::V1;
    let header = Header {
        sender_id: "sender-id".to_string(),
        recipient_id: "recipient-id".to_string(),
        timestamp: 1_700_000_001,
        message_id: ContentId::from_bytes(b"message-2"),
        ttl_seconds: 120,
        payload_size: 42,
        signature: Some("bad-signature".to_string()),
    };

    let err = header.verify_signature(version).expect_err("invalid signature");
    assert_eq!(err, HeaderError::InvalidSignature);
}
