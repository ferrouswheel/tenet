use tenet_crypto::protocol::{
    AttachmentRef, ContentId, Envelope, MessageHeader, Payload, RecipientKey,
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

    let header = MessageHeader {
        sender_id: "sender-id".to_string(),
        timestamp: 1_700_000_000,
        payload_id: payload.id.clone(),
        recipients: vec![RecipientKey {
            recipient_id: "recipient-id".to_string(),
            key_scheme: "hpke-x25519".to_string(),
            encrypted_key: "base64-key".to_string(),
        }],
        signature: Some("sig".to_string()),
    };

    let envelope = Envelope {
        id: ContentId::from_value(&header).expect("content id"),
        header,
        payload,
    };

    let json = serde_json::to_string(&envelope).expect("serialize envelope");
    let decoded: Envelope = serde_json::from_str(&json).expect("deserialize envelope");

    assert_eq!(decoded, envelope);
}
