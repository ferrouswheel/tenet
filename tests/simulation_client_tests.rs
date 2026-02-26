//! Integration tests for `SimulationClient` public API and mesh protocol
//! `MetaMessage` serialisation.  The four-phase mesh protocol is exercised at
//! the unit-test level in `src/client.rs`; these tests cover the parts of the
//! API that are visible from outside the crate.

use tenet::client::{MessageHandler, SimulationClient};
use tenet::protocol::{
    build_meta_payload, decode_meta_payload, Envelope, MessageKind, MetaMessage,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A `MessageHandler` that records every `MetaMessage` it receives so tests
/// can assert on the mesh-protocol messages delivered via the handler path.
#[derive(Default)]
struct CapturingHandler {
    pub meta_messages: Vec<MetaMessage>,
}

impl MessageHandler for CapturingHandler {
    fn on_meta(&mut self, meta: &MetaMessage) -> Vec<Envelope> {
        self.meta_messages.push(meta.clone());
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Basic SimulationClient construction
// ---------------------------------------------------------------------------

#[test]
fn test_simulation_client_creation() {
    let schedule = vec![true, false, true];
    let client = SimulationClient::new("peer-a", schedule, None);
    // The only public state we can inspect from outside the crate is the cache
    // and peer list.
    assert!(client.public_message_cache().is_empty());
    assert!(client.list_peers().is_empty());
}

#[test]
fn test_simulation_client_event_based_creation() {
    let client = SimulationClient::new_event_based("peer-b", None);
    assert!(client.public_message_cache().is_empty());
}

#[test]
fn test_simulation_client_set_online_does_not_panic() {
    // set_online() is the public setter for the event-based online flag.
    let mut client = SimulationClient::new_event_based("peer-c", None);
    client.set_online(true);
    client.set_online(false);
}

#[test]
fn test_simulation_client_peer_management_via_public_api() {
    let mut client = SimulationClient::new("peer-d", vec![true; 3], None);
    assert!(client.list_peers().is_empty());

    client.add_peer("alice".to_string(), "alice_signing_key".to_string());
    assert_eq!(client.list_peers().len(), 1);
    assert!(client.get_peer("alice").is_some());

    client.remove_peer("alice");
    assert!(client.list_peers().is_empty());
}

#[test]
fn test_simulation_client_handler_can_be_set_and_cleared() {
    let mut client = SimulationClient::new("peer-e", vec![true; 3], None);
    client.set_handler(Box::new(CapturingHandler::default()));
    client.clear_handler();
    // No panic — just verifying the API is usable.
}

// ---------------------------------------------------------------------------
// Mesh MetaMessage payload round-trips
//
// All four mesh-protocol MetaMessage variants must survive a
// build_meta_payload → decode_meta_payload round-trip with no data loss.
// ---------------------------------------------------------------------------

#[test]
fn test_mesh_message_request_roundtrip() {
    let meta = MetaMessage::MessageRequest {
        peer_id: "alice".to_string(),
        since_timestamp: 1_700_000_000,
    };
    let payload = build_meta_payload(&meta).expect("build payload");
    match decode_meta_payload(&payload).expect("decode payload") {
        MetaMessage::MessageRequest {
            peer_id,
            since_timestamp,
        } => {
            assert_eq!(peer_id, "alice");
            assert_eq!(since_timestamp, 1_700_000_000);
        }
        other => panic!("expected MessageRequest, got {other:?}"),
    }
}

#[test]
fn test_mesh_available_roundtrip() {
    let meta = MetaMessage::MeshAvailable {
        peer_id: "bob".to_string(),
        message_ids: vec!["id1".to_string(), "id2".to_string()],
        since_timestamp: 42,
    };
    let payload = build_meta_payload(&meta).expect("build payload");
    match decode_meta_payload(&payload).expect("decode payload") {
        MetaMessage::MeshAvailable {
            peer_id,
            message_ids,
            since_timestamp,
        } => {
            assert_eq!(peer_id, "bob");
            assert_eq!(message_ids, vec!["id1", "id2"]);
            assert_eq!(since_timestamp, 42);
        }
        other => panic!("expected MeshAvailable, got {other:?}"),
    }
}

#[test]
fn test_mesh_request_roundtrip() {
    let meta = MetaMessage::MeshRequest {
        peer_id: "carol".to_string(),
        message_ids: vec!["id-x".to_string()],
    };
    let payload = build_meta_payload(&meta).expect("build payload");
    match decode_meta_payload(&payload).expect("decode payload") {
        MetaMessage::MeshRequest {
            peer_id,
            message_ids,
        } => {
            assert_eq!(peer_id, "carol");
            assert_eq!(message_ids, vec!["id-x"]);
        }
        other => panic!("expected MeshRequest, got {other:?}"),
    }
}

#[test]
fn test_mesh_delivery_roundtrip() {
    use tenet::crypto::generate_keypair;
    use tenet::protocol::build_plaintext_envelope;

    let kp = generate_keypair();
    let env = build_plaintext_envelope(
        &kp.id,
        "*",
        None,
        None,
        100u64,
        3600,
        MessageKind::Public,
        None,
        None,
        "hello mesh",
        &[0u8; 16],
        &kp.signing_private_key_hex,
    )
    .expect("build envelope");

    let env_val = serde_json::to_value(&env).expect("serialize envelope");
    let mut sender_keys = std::collections::HashMap::new();
    sender_keys.insert(kp.id.clone(), kp.signing_public_key_hex.clone());

    let meta = MetaMessage::MeshDelivery {
        peer_id: "dave".to_string(),
        envelopes: vec![env_val],
        sender_keys,
    };
    let payload = build_meta_payload(&meta).expect("build payload");
    match decode_meta_payload(&payload).expect("decode payload") {
        MetaMessage::MeshDelivery {
            peer_id,
            envelopes,
            sender_keys,
        } => {
            assert_eq!(peer_id, "dave");
            assert_eq!(envelopes.len(), 1);
            let delivered: Envelope =
                serde_json::from_value(envelopes[0].clone()).expect("deserialize envelope");
            assert_eq!(delivered.header.sender_id, kp.id);
            assert_eq!(delivered.header.message_kind, MessageKind::Public);
            assert!(sender_keys.contains_key(&kp.id));
        }
        other => panic!("expected MeshDelivery, got {other:?}"),
    }
}
