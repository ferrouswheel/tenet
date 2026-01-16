use tenet::protocol::{
    AttachmentRef, ContentId, Envelope, Header, HeaderError, MessageKind, Payload, ProtocolVersion,
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
        store_for: None,
        storage_peer_id: None,
        timestamp: 1_700_000_000,
        message_id: ContentId::from_bytes(b"message-1"),
        message_kind: MessageKind::Public,
        group_id: None,
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
        store_for: None,
        storage_peer_id: None,
        timestamp: 1_700_000_001,
        message_id: ContentId::from_bytes(b"message-2"),
        message_kind: MessageKind::Direct,
        group_id: None,
        ttl_seconds: 120,
        payload_size: 42,
        signature: Some("bad-signature".to_string()),
    };

    let err = header
        .verify_signature(version)
        .expect_err("invalid signature");
    assert_eq!(err, HeaderError::InvalidSignature);
}

#[test]
fn header_roundtrips_and_validates_message_kinds() {
    let kinds = vec![
        (MessageKind::Public, None, None, None),
        (MessageKind::Meta, None, None, None),
        (MessageKind::Direct, None, None, None),
        (
            MessageKind::FriendGroup,
            Some("group-42"),
            None,
            None,
        ),
        (
            MessageKind::StoreForPeer,
            None,
            Some("peer-b"),
            Some("peer-c"),
        ),
    ];

    for (kind, group_id, store_for, storage_peer_id) in kinds {
        let version = ProtocolVersion::V1;
        let mut header = Header {
            sender_id: "sender-id".to_string(),
            recipient_id: storage_peer_id
                .map(str::to_string)
                .unwrap_or_else(|| "recipient-id".to_string()),
            store_for: store_for.map(str::to_string),
            storage_peer_id: storage_peer_id.map(str::to_string),
            timestamp: 1_700_000_100,
            message_id: ContentId::from_bytes(b"message-kind"),
            message_kind: kind,
            group_id: group_id.map(str::to_string),
            ttl_seconds: 3_600,
            payload_size: 128,
            signature: None,
        };
        header.signature = Some(
            header
                .expected_signature(version)
                .expect("signature generation"),
        );

        let json = serde_json::to_string(&header).expect("serialize header");
        let decoded: Header = serde_json::from_str(&json).expect("deserialize header");

        assert_eq!(decoded, header);
        decoded
            .verify_signature(version)
            .expect("signature validation");
    }
}

#[test]
fn header_rejects_invalid_message_kind_combinations() {
    let version = ProtocolVersion::V1;
    let mut header = Header {
        sender_id: "sender-id".to_string(),
        recipient_id: "recipient-id".to_string(),
        store_for: None,
        storage_peer_id: None,
        timestamp: 1_700_000_200,
        message_id: ContentId::from_bytes(b"message-invalid"),
        message_kind: MessageKind::FriendGroup,
        group_id: None,
        ttl_seconds: 120,
        payload_size: 42,
        signature: None,
    };
    header.signature = Some(
        header
            .expected_signature(version)
            .expect("signature generation"),
    );

    let err = header
        .verify_signature(version)
        .expect_err("missing group id");
    assert!(matches!(err, HeaderError::InvalidMessageKind(_)));

    header.message_kind = MessageKind::Meta;
    header.group_id = Some("group-1".to_string());
    header.signature = Some(
        header
            .expected_signature(version)
            .expect("signature generation"),
    );

    let err = header
        .verify_signature(version)
        .expect_err("unexpected group id");
    assert!(matches!(err, HeaderError::InvalidMessageKind(_)));
}

#[test]
fn header_signature_changes_with_message_kind() {
    let version = ProtocolVersion::V1;
    let mut header = Header {
        sender_id: "sender-id".to_string(),
        recipient_id: "recipient-id".to_string(),
        store_for: None,
        storage_peer_id: None,
        timestamp: 1_700_000_300,
        message_id: ContentId::from_bytes(b"message-signature"),
        message_kind: MessageKind::Direct,
        group_id: None,
        ttl_seconds: 300,
        payload_size: 64,
        signature: None,
    };
    header.signature = Some(
        header
            .expected_signature(version)
            .expect("signature generation"),
    );

    header.message_kind = MessageKind::Public;

    let err = header.verify_signature(version).expect_err("kind mismatch");
    assert_eq!(err, HeaderError::InvalidSignature);
}
