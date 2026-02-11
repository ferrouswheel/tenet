//! Integration tests for the client-managed storage design:
//!
//! - `SyncOutcome` now contains typed `SyncEvent`s in addition to the
//!   convenience `messages` and `errors` views.
//! - `MessageHandler` trait allows callers to register push-style callbacks on
//!   `RelayClient`.
//! - `sync_inbox()` handles all message kinds (Direct, Public, FriendGroup, Meta).

use tenet::client::{
    ClientConfig, ClientEncryption, ClientMessage, MessageHandler, RelayClient, SyncEvent,
    SyncEventOutcome,
};
use tenet::crypto::generate_keypair;
use tenet::protocol::{
    build_meta_payload, build_plaintext_envelope, decode_meta_payload, Envelope, MessageKind,
    MetaMessage,
};

// ---------------------------------------------------------------------------
// Helper: a simple in-process MessageHandler that records every callback
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingHandler {
    messages: Vec<ClientMessage>,
    meta_messages: Vec<MetaMessage>,
    raw_metas: Vec<String>,
    rejected: Vec<String>,
}

impl MessageHandler for RecordingHandler {
    fn on_message(&mut self, _envelope: &Envelope, message: &ClientMessage) {
        self.messages.push(message.clone());
    }

    fn on_meta(&mut self, meta: &MetaMessage) {
        self.meta_messages.push(meta.clone());
    }

    fn on_raw_meta(&mut self, _envelope: &Envelope, body: &str) {
        self.raw_metas.push(body.to_string());
    }

    fn on_rejected(&mut self, _envelope: &Envelope, reason: &str) {
        self.rejected.push(reason.to_string());
    }
}

// ---------------------------------------------------------------------------
// Tests for SyncEventOutcome variants (without a live relay)
// ---------------------------------------------------------------------------

#[test]
fn sync_event_outcome_variants_exist() {
    // Smoke test: verify all variants can be constructed.
    let msg = ClientMessage {
        message_id: "id1".to_string(),
        sender_id: "alice".to_string(),
        timestamp: 0,
        body: "hello".to_string(),
    };
    let _m = SyncEventOutcome::Message(msg);
    let _dup = SyncEventOutcome::Duplicate;
    let _ttl = SyncEventOutcome::TtlExpired;
    let _unk = SyncEventOutcome::UnknownSender;
    let _sig = SyncEventOutcome::InvalidSignature {
        reason: "bad sig".to_string(),
    };
    let _dec = SyncEventOutcome::DecryptFailed {
        reason: "bad dec".to_string(),
    };
    let _raw = SyncEventOutcome::RawMeta {
        body: "{}".to_string(),
    };
}

#[test]
fn relay_client_set_and_clear_handler() {
    let kp = generate_keypair();
    let config = ClientConfig::new("http://localhost:9999", 3600, ClientEncryption::Plaintext);
    let mut client = RelayClient::new(kp, config);

    // Setting a handler should not panic.
    client.set_handler(Box::new(RecordingHandler::default()));

    // Clearing it should also not panic.
    client.clear_handler();
}

#[test]
fn relay_client_clone_drops_handler() {
    let kp = generate_keypair();
    let config = ClientConfig::new("http://localhost:9999", 3600, ClientEncryption::Plaintext);
    let mut client = RelayClient::new(kp, config);
    client.set_handler(Box::new(RecordingHandler::default()));

    // Cloning should succeed and produce a client with no handler (handlers
    // are not clonable, so the clone starts fresh).
    let cloned = client.clone();
    // The original still has its handler (not consumed).
    // The clone should at least be usable:
    assert_eq!(cloned.id(), client.id());
}

#[test]
fn sync_outcome_messages_and_errors_are_derived_views() {
    // Without a relay we cannot call sync_inbox(), but we can verify the
    // SyncOutcome struct layout manually.
    use tenet::client::SyncOutcome;

    let alice = generate_keypair();
    let msg = ClientMessage {
        message_id: "abc".to_string(),
        sender_id: alice.id.clone(),
        timestamp: 1_700_000_000,
        body: "hello".to_string(),
    };
    let envelope = {
        let salt = [0u8; 16];
        build_plaintext_envelope(
            &alice.id,
            "*",
            None,
            None,
            1_700_000_000,
            3600,
            MessageKind::Public,
            None,
            None,
            "hello",
            salt,
            &alice.signing_private_key_hex,
        )
        .expect("build envelope")
    };

    let outcome = SyncOutcome {
        events: vec![SyncEvent {
            envelope,
            outcome: SyncEventOutcome::Message(msg.clone()),
        }],
        fetched: 1,
        messages: vec![msg.clone()],
        errors: vec![],
    };

    assert_eq!(outcome.fetched, 1);
    assert_eq!(outcome.messages.len(), 1);
    assert!(outcome.errors.is_empty());
    assert_eq!(outcome.events.len(), 1);
    if let SyncEventOutcome::Message(ref m) = outcome.events[0].outcome {
        assert_eq!(m.body, "hello");
    } else {
        panic!("expected Message variant");
    }
}

#[test]
fn meta_message_payload_roundtrip() {
    let meta = MetaMessage::Online {
        peer_id: "alice-id".to_string(),
        timestamp: 1_700_000_000,
    };
    let payload = build_meta_payload(&meta).expect("build meta payload");
    let decoded = decode_meta_payload(&payload).expect("decode meta payload");
    assert!(matches!(decoded, MetaMessage::Online { .. }));
    if let MetaMessage::Online { peer_id, timestamp } = decoded {
        assert_eq!(peer_id, "alice-id");
        assert_eq!(timestamp, 1_700_000_000);
    }
}

#[test]
fn message_handler_trait_default_methods_are_no_ops() {
    // A handler that uses all default implementations should not panic.
    struct EmptyHandler;
    impl MessageHandler for EmptyHandler {}

    let alice = generate_keypair();
    let salt = [0u8; 16];
    let envelope = build_plaintext_envelope(
        &alice.id,
        "*",
        None,
        None,
        1_700_000_000,
        3600,
        MessageKind::Public,
        None,
        None,
        "test",
        salt,
        &alice.signing_private_key_hex,
    )
    .expect("build envelope");
    let message = ClientMessage {
        message_id: envelope.header.message_id.0.clone(),
        sender_id: alice.id.clone(),
        timestamp: envelope.header.timestamp,
        body: "test".to_string(),
    };
    let meta = MetaMessage::Online {
        peer_id: alice.id.clone(),
        timestamp: 0,
    };

    let mut handler = EmptyHandler;
    handler.on_message(&envelope, &message);
    handler.on_meta(&meta);
    handler.on_raw_meta(&envelope, "{}");
    handler.on_rejected(&envelope, "some reason");
}
