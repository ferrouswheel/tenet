use sha2::{Digest, Sha256};
use tenet::crypto::generate_keypair;
use tenet::protocol::{
    AttachmentRef, ContentId, Envelope, Header, HeaderError, MessageKind, Payload, ProtocolVersion,
};

#[test]
fn envelope_roundtrips_via_json() {
    let keypair = generate_keypair();
    let payload = Payload {
        id: ContentId::from_bytes(b"payload-body"),
        content_type: "application/octet-stream".to_string(),
        body: "deadbeef".to_string(),
        attachments: vec![AttachmentRef {
            content_id: ContentId::from_bytes(b"attachment-1"),
            content_type: "image/png".to_string(),
            size: 128,
            filename: None,
            data: None,
        }],
    };

    let version = ProtocolVersion::V2;
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
        payload_hash: Some(hex::encode(Sha256::digest(payload.body.as_bytes()))),
        reply_to: None,
        signature: None,
    };
    header.signature = Some(
        header
            .compute_signature(version, &keypair.signing_private_key_hex, &payload.body)
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
    let keypair = generate_keypair();
    let version = ProtocolVersion::V2;
    let payload_body = "test payload";
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
        payload_hash: Some(hex::encode(Sha256::digest(payload_body.as_bytes()))),
        reply_to: None,
        signature: Some("bad-signature".to_string()),
    };

    let err = header
        .verify_signature(version, &keypair.signing_public_key_hex, payload_body)
        .expect_err("invalid signature");
    assert_eq!(err, HeaderError::InvalidSignature);
}

#[test]
fn header_roundtrips_and_validates_message_kinds() {
    let keypair = generate_keypair();
    let kinds = vec![
        (MessageKind::Public, None, None, None),
        (MessageKind::Meta, None, None, None),
        (MessageKind::Direct, None, None, None),
        (MessageKind::FriendGroup, Some("group-42"), None, None),
        (
            MessageKind::StoreForPeer,
            None,
            Some("peer-b"),
            Some("peer-c"),
        ),
    ];

    for (kind, group_id, store_for, storage_peer_id) in kinds {
        let version = ProtocolVersion::V2;
        let payload_body = "test payload for message kinds";
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
            payload_hash: Some(hex::encode(Sha256::digest(payload_body.as_bytes()))),
            reply_to: None,
            signature: None,
        };
        header.signature = Some(
            header
                .compute_signature(version, &keypair.signing_private_key_hex, payload_body)
                .expect("signature generation"),
        );

        let json = serde_json::to_string(&header).expect("serialize header");
        let decoded: Header = serde_json::from_str(&json).expect("deserialize header");

        assert_eq!(decoded, header);
        decoded
            .verify_signature(version, &keypair.signing_public_key_hex, payload_body)
            .expect("signature validation");
    }
}

#[test]
fn header_rejects_invalid_message_kind_combinations() {
    let keypair = generate_keypair();
    let version = ProtocolVersion::V2;
    let payload_body = "test payload";
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
        payload_hash: Some(hex::encode(Sha256::digest(payload_body.as_bytes()))),
        reply_to: None,
        signature: None,
    };
    header.signature = Some(
        header
            .compute_signature(version, &keypair.signing_private_key_hex, payload_body)
            .expect("signature generation"),
    );

    let err = header
        .verify_signature(version, &keypair.signing_public_key_hex, payload_body)
        .expect_err("missing group id");
    assert!(matches!(err, HeaderError::InvalidMessageKind(_)));

    header.message_kind = MessageKind::Meta;
    header.group_id = Some("group-1".to_string());
    header.signature = Some(
        header
            .compute_signature(version, &keypair.signing_private_key_hex, payload_body)
            .expect("signature generation"),
    );

    let err = header
        .verify_signature(version, &keypair.signing_public_key_hex, payload_body)
        .expect_err("unexpected group id");
    assert!(matches!(err, HeaderError::InvalidMessageKind(_)));
}

#[test]
fn header_signature_changes_with_message_kind() {
    let keypair = generate_keypair();
    let version = ProtocolVersion::V2;
    let payload_body = "test payload";
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
        payload_hash: Some(hex::encode(Sha256::digest(payload_body.as_bytes()))),
        reply_to: None,
        signature: None,
    };
    header.signature = Some(
        header
            .compute_signature(version, &keypair.signing_private_key_hex, payload_body)
            .expect("signature generation"),
    );

    header.message_kind = MessageKind::Public;

    let err = header
        .verify_signature(version, &keypair.signing_public_key_hex, payload_body)
        .expect_err("kind mismatch");
    assert_eq!(err, HeaderError::InvalidSignature);
}

#[test]
fn v2_signature_fails_with_wrong_payload_body() {
    let keypair = generate_keypair();
    let version = ProtocolVersion::V2;
    let original_payload = "original payload";
    let tampered_payload = "tampered payload";

    let mut header = Header {
        sender_id: "sender-id".to_string(),
        recipient_id: "recipient-id".to_string(),
        store_for: None,
        storage_peer_id: None,
        timestamp: 1_700_000_400,
        message_id: ContentId::from_bytes(b"message-tampered"),
        message_kind: MessageKind::Public,
        group_id: None,
        ttl_seconds: 3_600,
        payload_size: original_payload.len() as u64,
        payload_hash: Some(hex::encode(Sha256::digest(original_payload.as_bytes()))),
        reply_to: None,
        signature: None,
    };

    header.signature = Some(
        header
            .compute_signature(version, &keypair.signing_private_key_hex, original_payload)
            .expect("signature generation"),
    );

    // Verify with original payload should succeed
    header
        .verify_signature(version, &keypair.signing_public_key_hex, original_payload)
        .expect("valid signature with correct payload");

    // Verify with tampered payload should fail with PayloadHashMismatch
    let err = header
        .verify_signature(version, &keypair.signing_public_key_hex, tampered_payload)
        .expect_err("payload hash mismatch");
    assert_eq!(err, HeaderError::PayloadHashMismatch);
}

#[test]
fn v2_signature_fails_with_missing_payload_hash() {
    let keypair = generate_keypair();
    let version = ProtocolVersion::V2;
    let payload_body = "test payload";

    let mut header = Header {
        sender_id: "sender-id".to_string(),
        recipient_id: "recipient-id".to_string(),
        store_for: None,
        storage_peer_id: None,
        timestamp: 1_700_000_500,
        message_id: ContentId::from_bytes(b"message-no-hash"),
        message_kind: MessageKind::Public,
        group_id: None,
        ttl_seconds: 3_600,
        payload_size: payload_body.len() as u64,
        payload_hash: None, // Missing payload hash
        reply_to: None,
        signature: None,
    };

    // Even with a valid signature, missing payload_hash should fail
    header.signature = Some(
        header
            .compute_signature(version, &keypair.signing_private_key_hex, payload_body)
            .expect("signature generation"),
    );

    let err = header
        .verify_signature(version, &keypair.signing_public_key_hex, payload_body)
        .expect_err("missing payload hash");
    assert_eq!(err, HeaderError::MissingPayloadHash);
}
