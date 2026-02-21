use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use base64::Engine as _;
use futures_util::StreamExt;
use hpke::kem::X25519HkdfSha256;
use hpke::Kem as _;
use hpke::Serializable;
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite;

use tenet::crypto::{
    decrypt_payload, encrypt_payload, generate_keypair, make_relay_auth_token, unwrap_content_key,
    wrap_content_key,
};
use tenet::relay::{app, RelayConfig, RelayState};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Header {
    sender_id: String,
    recipient_id: String,
    timestamp: u64,
    content_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WrappedKeyData {
    enc_hex: String,
    ciphertext_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EncryptedPayload {
    nonce_hex: String,
    ciphertext_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Envelope {
    message_id: String,
    header: Header,
    wrapped_key: WrappedKeyData,
    payload: EncryptedPayload,
}

async fn start_relay(config: RelayConfig) -> (String, oneshot::Sender<()>) {
    let state = RelayState::new(config);
    let app: Router = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind relay");
    let addr = listener.local_addr().expect("relay addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = shutdown_rx.await;
    });
    tokio::spawn(async move {
        let _ = server.await;
    });

    (format!("http://{}", addr), shutdown_tx)
}

fn build_message_id(header: &Header, payload: &EncryptedPayload) -> String {
    let mut bytes = serde_json::to_vec(header).expect("header bytes");
    bytes.extend_from_slice(payload.nonce_hex.as_bytes());
    bytes.extend_from_slice(payload.ciphertext_hex.as_bytes());
    let digest = Sha256::digest(bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn post_envelope(base_url: &str, envelope: &Envelope) {
    let body = serde_json::to_string(envelope).expect("serialize envelope");
    let response = ureq::post(&format!("{}/envelopes", base_url))
        .set("Content-Type", "application/json")
        .send_string(&body)
        .expect("post envelope");
    assert!(response.status() < 400, "relay rejected envelope");
}

fn fetch_inbox(base_url: &str, recipient_id: &str, auth_token: &str) -> Vec<Envelope> {
    let response = ureq::get(&format!("{}/inbox/{}", base_url, recipient_id))
        .set("Authorization", &format!("Bearer {}", auth_token))
        .call()
        .expect("fetch inbox");
    let body = response.into_string().expect("inbox body");
    serde_json::from_str(&body).expect("deserialize inbox")
}

#[tokio::test]
async fn relay_expires_messages_after_ttl() {
    let (base_url, shutdown_tx) = start_relay(RelayConfig {
        ttl: Duration::from_millis(50),
        max_messages: 10,
        max_bytes: 1024 * 1024,
        retry_backoff: Vec::new(),
        peer_log_window: Duration::from_secs(60),
        peer_log_interval: Duration::from_secs(30),
        log_sink: None,
        pause_flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        qos: tenet::relay::RelayQosConfig::default(),
    })
    .await;

    let recipient_kp = generate_keypair();
    let recipient_id = recipient_kp.id.clone();

    let envelope = Envelope {
        message_id: "expire-test".to_string(),
        header: Header {
            sender_id: "sender".to_string(),
            recipient_id: recipient_id.clone(),
            timestamp: 1,
            content_type: "text/plain".to_string(),
        },
        wrapped_key: WrappedKeyData {
            enc_hex: "00".to_string(),
            ciphertext_hex: "11".to_string(),
        },
        payload: EncryptedPayload {
            nonce_hex: "22".to_string(),
            ciphertext_hex: "33".to_string(),
        },
    };

    tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        let envelope = envelope.clone();
        move || post_envelope(&base_url, &envelope)
    })
    .await
    .expect("post task");

    tokio::time::sleep(Duration::from_millis(80)).await;

    let inbox = tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        let signing_priv = recipient_kp.signing_private_key_hex.clone();
        move || {
            let token = make_relay_auth_token(&signing_priv, &recipient_id).expect("token");
            fetch_inbox(&base_url, &recipient_id, &token)
        }
    })
    .await
    .expect("fetch task");

    shutdown_tx.send(()).ok();

    assert!(inbox.is_empty(), "expected expired inbox");
}

#[tokio::test]
async fn relay_deduplicates_by_message_id() {
    let (base_url, shutdown_tx) = start_relay(RelayConfig {
        ttl: Duration::from_secs(5),
        max_messages: 10,
        max_bytes: 1024 * 1024,
        retry_backoff: Vec::new(),
        peer_log_window: Duration::from_secs(60),
        peer_log_interval: Duration::from_secs(30),
        log_sink: None,
        pause_flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        qos: tenet::relay::RelayQosConfig::default(),
    })
    .await;

    let recipient_kp = generate_keypair();
    let recipient_id = recipient_kp.id.clone();

    let envelope = Envelope {
        message_id: "dedupe-test".to_string(),
        header: Header {
            sender_id: "sender".to_string(),
            recipient_id: recipient_id.clone(),
            timestamp: 1,
            content_type: "text/plain".to_string(),
        },
        wrapped_key: WrappedKeyData {
            enc_hex: "aa".to_string(),
            ciphertext_hex: "bb".to_string(),
        },
        payload: EncryptedPayload {
            nonce_hex: "cc".to_string(),
            ciphertext_hex: "dd".to_string(),
        },
    };

    tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        let envelope = envelope.clone();
        move || {
            post_envelope(&base_url, &envelope);
            post_envelope(&base_url, &envelope);
        }
    })
    .await
    .expect("post task");

    let inbox = tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        let signing_priv = recipient_kp.signing_private_key_hex.clone();
        move || {
            let token = make_relay_auth_token(&signing_priv, &recipient_id).expect("token");
            fetch_inbox(&base_url, &recipient_id, &token)
        }
    })
    .await
    .expect("fetch task");

    shutdown_tx.send(()).ok();

    assert_eq!(inbox.len(), 1);
}

#[tokio::test]
async fn node_can_send_and_receive_through_relay() {
    let (base_url, shutdown_tx) = start_relay(RelayConfig {
        ttl: Duration::from_secs(5),
        max_messages: 10,
        max_bytes: 1024 * 1024,
        retry_backoff: Vec::new(),
        peer_log_window: Duration::from_secs(60),
        peer_log_interval: Duration::from_secs(30),
        log_sink: None,
        pause_flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        qos: tenet::relay::RelayQosConfig::default(),
    })
    .await;

    let mut rng = ChaCha20Rng::from_seed([9u8; 32]);
    let (recipient_private, recipient_public) = X25519HkdfSha256::gen_keypair(&mut rng);
    let recipient_kp = generate_keypair();
    let recipient_id = recipient_kp.id.clone();

    let header = Header {
        sender_id: "sender-node".to_string(),
        recipient_id: recipient_id.clone(),
        timestamp: 123,
        content_type: "text/plain".to_string(),
    };
    let aad = serde_json::to_vec(&header).expect("aad");
    let content_key = [11u8; 32];
    let plaintext = b"hello relay";

    let (nonce, ciphertext) =
        encrypt_payload(&content_key, plaintext, &aad, None).expect("encrypt payload");
    let wrapped = wrap_content_key(
        &recipient_public.to_bytes(),
        &content_key,
        b"tenet-cli",
        Some([2u8; 32]),
    )
    .expect("wrap content key");

    let payload = EncryptedPayload {
        nonce_hex: hex::encode(nonce),
        ciphertext_hex: hex::encode(ciphertext),
    };
    let message_id = build_message_id(&header, &payload);
    let envelope = Envelope {
        message_id,
        header,
        wrapped_key: WrappedKeyData {
            enc_hex: hex::encode(wrapped.enc),
            ciphertext_hex: hex::encode(wrapped.ciphertext),
        },
        payload,
    };

    tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        let envelope = envelope.clone();
        move || post_envelope(&base_url, &envelope)
    })
    .await
    .expect("post task");

    let inbox = tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        let signing_priv = recipient_kp.signing_private_key_hex.clone();
        move || {
            let token = make_relay_auth_token(&signing_priv, &recipient_id).expect("token");
            fetch_inbox(&base_url, &recipient_id, &token)
        }
    })
    .await
    .expect("fetch task");

    shutdown_tx.send(()).ok();

    let fetched = inbox.first().expect("fetched envelope");
    let wrapped = tenet::crypto::WrappedKey {
        enc: hex::decode(&fetched.wrapped_key.enc_hex).expect("enc bytes"),
        ciphertext: hex::decode(&fetched.wrapped_key.ciphertext_hex).expect("cipher bytes"),
    };
    let content_key = unwrap_content_key(&recipient_private.to_bytes(), &wrapped, b"tenet-cli")
        .expect("unwrap content key");
    let nonce = hex::decode(&fetched.payload.nonce_hex).expect("nonce bytes");
    let ciphertext = hex::decode(&fetched.payload.ciphertext_hex).expect("cipher bytes");

    let aad = serde_json::to_vec(&fetched.header).expect("aad");
    let decrypted =
        decrypt_payload(&content_key, &nonce, &ciphertext, &aad).expect("decrypt payload");

    assert_eq!(decrypted, plaintext);
}

fn test_relay_config() -> RelayConfig {
    RelayConfig {
        ttl: Duration::from_secs(5),
        max_messages: 100,
        max_bytes: 1024 * 1024,
        retry_backoff: Vec::new(),
        peer_log_window: Duration::from_secs(60),
        peer_log_interval: Duration::ZERO,
        log_sink: None,
        pause_flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        qos: tenet::relay::RelayQosConfig::default(),
    }
}

fn ws_url(http_url: &str, recipient_id: &str, auth_token: &str) -> String {
    let ws_base = http_url.replacen("http://", "ws://", 1);
    format!("{ws_base}/ws/{recipient_id}?token={auth_token}")
}

fn make_test_envelope(sender: &str, recipient: &str, msg_id: &str) -> Envelope {
    Envelope {
        message_id: msg_id.to_string(),
        header: Header {
            sender_id: sender.to_string(),
            recipient_id: recipient.to_string(),
            timestamp: 1000,
            content_type: "text/plain".to_string(),
        },
        wrapped_key: WrappedKeyData {
            enc_hex: "aa".to_string(),
            ciphertext_hex: "bb".to_string(),
        },
        payload: EncryptedPayload {
            nonce_hex: "cc".to_string(),
            ciphertext_hex: "dd".to_string(),
        },
    }
}

#[tokio::test]
async fn websocket_receives_messages_in_realtime() {
    let (base_url, shutdown_tx) = start_relay(test_relay_config()).await;
    let recipient_kp = generate_keypair();
    let recipient = recipient_kp.id.clone();
    let token =
        make_relay_auth_token(&recipient_kp.signing_private_key_hex, &recipient).expect("ws token");

    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(&base_url, &recipient, &token))
        .await
        .expect("ws connect");

    // Small delay for the server to register the subscriber
    tokio::time::sleep(Duration::from_millis(50)).await;

    let envelope = make_test_envelope("sender-a", &recipient, "ws-msg-1");
    let base_url_clone = base_url.clone();
    let envelope_clone = envelope.clone();
    tokio::task::spawn_blocking(move || post_envelope(&base_url_clone, &envelope_clone))
        .await
        .expect("post");

    let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("timeout waiting for ws message")
        .expect("stream ended")
        .expect("ws read error");

    match msg {
        tungstenite::Message::Text(text) => {
            let received: serde_json::Value =
                serde_json::from_str(&text).expect("parse ws message");
            assert_eq!(
                received["header"]["sender_id"].as_str().unwrap(),
                "sender-a"
            );
            assert_eq!(received["message_id"].as_str().unwrap(), "ws-msg-1");
        }
        other => panic!("expected text message, got {:?}", other),
    }

    ws.close(None).await.ok();
    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn websocket_multiple_subscribers_same_recipient() {
    let (base_url, shutdown_tx) = start_relay(test_relay_config()).await;
    let recipient_kp = generate_keypair();
    let recipient = recipient_kp.id.clone();
    let token =
        make_relay_auth_token(&recipient_kp.signing_private_key_hex, &recipient).expect("ws token");

    let (mut ws1, _) = tokio_tungstenite::connect_async(ws_url(&base_url, &recipient, &token))
        .await
        .expect("ws1 connect");
    let (mut ws2, _) = tokio_tungstenite::connect_async(ws_url(&base_url, &recipient, &token))
        .await
        .expect("ws2 connect");

    tokio::time::sleep(Duration::from_millis(50)).await;

    let envelope = make_test_envelope("sender-b", &recipient, "ws-multi-msg");
    let base_url_clone = base_url.clone();
    tokio::task::spawn_blocking(move || post_envelope(&base_url_clone, &envelope))
        .await
        .expect("post");

    for (i, ws) in [&mut ws1, &mut ws2].iter_mut().enumerate() {
        let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .unwrap_or_else(|_| panic!("timeout on ws{}", i + 1))
            .unwrap_or_else(|| panic!("stream ended on ws{}", i + 1))
            .unwrap_or_else(|e| panic!("read error on ws{}: {}", i + 1, e));

        match msg {
            tungstenite::Message::Text(text) => {
                let received: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(received["message_id"].as_str().unwrap(), "ws-multi-msg");
            }
            other => panic!("expected text on ws{}, got {:?}", i + 1, other),
        }
    }

    ws1.close(None).await.ok();
    ws2.close(None).await.ok();
    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn websocket_does_not_receive_other_recipients_messages() {
    let (base_url, shutdown_tx) = start_relay(test_relay_config()).await;

    let alice_kp = generate_keypair();
    let alice_id = alice_kp.id.clone();
    let alice_token =
        make_relay_auth_token(&alice_kp.signing_private_key_hex, &alice_id).expect("alice token");

    let bob_kp = generate_keypair();
    let bob_id = bob_kp.id.clone();

    let (mut ws_alice, _) =
        tokio_tungstenite::connect_async(ws_url(&base_url, &alice_id, &alice_token))
            .await
            .expect("ws alice connect");

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send a message to bob, not alice
    let envelope = make_test_envelope("sender-c", &bob_id, "msg-for-bob");
    let base_url_clone = base_url.clone();
    tokio::task::spawn_blocking(move || post_envelope(&base_url_clone, &envelope))
        .await
        .expect("post");

    // Alice should NOT receive this message
    let result = tokio::time::timeout(Duration::from_millis(300), ws_alice.next()).await;
    assert!(result.is_err(), "alice should not receive bob's message");

    ws_alice.close(None).await.ok();
    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn websocket_and_polling_coexist() {
    let (base_url, shutdown_tx) = start_relay(test_relay_config()).await;
    let recipient_kp = generate_keypair();
    let recipient = recipient_kp.id.clone();
    let token =
        make_relay_auth_token(&recipient_kp.signing_private_key_hex, &recipient).expect("ws token");

    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(&base_url, &recipient, &token))
        .await
        .expect("ws connect");

    tokio::time::sleep(Duration::from_millis(50)).await;

    let envelope = make_test_envelope("sender-d", &recipient, "ws-poll-msg");
    let base_url_clone = base_url.clone();
    let envelope_clone = envelope.clone();
    tokio::task::spawn_blocking(move || post_envelope(&base_url_clone, &envelope_clone))
        .await
        .expect("post");

    // WebSocket should receive the message
    let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("ws timeout")
        .expect("ws stream ended")
        .expect("ws read error");

    match msg {
        tungstenite::Message::Text(text) => {
            let received: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(received["message_id"].as_str().unwrap(), "ws-poll-msg");
        }
        other => panic!("expected text, got {:?}", other),
    }

    // Polling should also still work (message was consumed from WS but still in queue)
    let base_url_clone = base_url.clone();
    let signing_priv = recipient_kp.signing_private_key_hex.clone();
    let inbox = tokio::task::spawn_blocking(move || {
        let poll_token = make_relay_auth_token(&signing_priv, &recipient).expect("poll token");
        fetch_inbox(&base_url_clone, &recipient, &poll_token)
    })
    .await
    .expect("fetch");
    // The message is still in the queue because WebSocket notification is separate from queue drain
    assert_eq!(
        inbox.len(),
        1,
        "polling should still return the queued message"
    );

    ws.close(None).await.ok();
    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn websocket_receives_multiple_messages_sequentially() {
    let (base_url, shutdown_tx) = start_relay(test_relay_config()).await;
    let recipient_kp = generate_keypair();
    let recipient = recipient_kp.id.clone();
    let token =
        make_relay_auth_token(&recipient_kp.signing_private_key_hex, &recipient).expect("ws token");

    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(&base_url, &recipient, &token))
        .await
        .expect("ws connect");

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send 3 messages
    for i in 0..3 {
        let envelope = make_test_envelope("sender-e", &recipient, &format!("ws-seq-msg-{}", i));
        let base_url_clone = base_url.clone();
        tokio::task::spawn_blocking(move || post_envelope(&base_url_clone, &envelope))
            .await
            .expect("post");
    }

    // Receive all 3
    let mut received_ids = Vec::new();
    for _ in 0..3 {
        let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("timeout")
            .expect("stream ended")
            .expect("read error");

        if let tungstenite::Message::Text(text) = msg {
            let val: serde_json::Value = serde_json::from_str(&text).unwrap();
            received_ids.push(val["message_id"].as_str().unwrap().to_string());
        }
    }

    assert_eq!(received_ids.len(), 3);
    for i in 0..3 {
        assert!(
            received_ids.contains(&format!("ws-seq-msg-{}", i)),
            "missing ws-seq-msg-{}",
            i
        );
    }

    ws.close(None).await.ok();
    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn websocket_batch_store_notifies_subscribers() {
    let (base_url, shutdown_tx) = start_relay(test_relay_config()).await;
    let recipient_kp = generate_keypair();
    let recipient = recipient_kp.id.clone();
    let token =
        make_relay_auth_token(&recipient_kp.signing_private_key_hex, &recipient).expect("ws token");

    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(&base_url, &recipient, &token))
        .await
        .expect("ws connect");

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Post a batch of 2 envelopes for the same recipient
    let envelopes = vec![
        make_test_envelope("sender-f", &recipient, "ws-batch-1"),
        make_test_envelope("sender-g", &recipient, "ws-batch-2"),
    ];
    let base_url_clone = base_url.clone();
    tokio::task::spawn_blocking(move || {
        let body = serde_json::to_string(&envelopes).expect("serialize batch");
        let response = ureq::post(&format!("{}/envelopes/batch", base_url_clone))
            .set("Content-Type", "application/json")
            .send_string(&body)
            .expect("post batch");
        assert!(response.status() < 400, "relay rejected batch");
    })
    .await
    .expect("post batch task");

    let mut received_ids = Vec::new();
    for _ in 0..2 {
        let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("timeout")
            .expect("stream ended")
            .expect("read error");

        if let tungstenite::Message::Text(text) = msg {
            let val: serde_json::Value = serde_json::from_str(&text).unwrap();
            received_ids.push(val["message_id"].as_str().unwrap().to_string());
        }
    }

    assert!(received_ids.contains(&"ws-batch-1".to_string()));
    assert!(received_ids.contains(&"ws-batch-2".to_string()));

    ws.close(None).await.ok();
    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn websocket_disconnect_does_not_break_relay() {
    let (base_url, shutdown_tx) = start_relay(test_relay_config()).await;
    let recipient_kp = generate_keypair();
    let recipient = recipient_kp.id.clone();
    let token =
        make_relay_auth_token(&recipient_kp.signing_private_key_hex, &recipient).expect("ws token");

    // Connect and immediately disconnect
    let (ws, _) = tokio_tungstenite::connect_async(ws_url(&base_url, &recipient, &token))
        .await
        .expect("ws connect");
    drop(ws);

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Posting a message should still work fine
    let envelope = make_test_envelope("sender-h", &recipient, "after-disconnect");
    let base_url_clone = base_url.clone();
    tokio::task::spawn_blocking(move || post_envelope(&base_url_clone, &envelope))
        .await
        .expect("post after disconnect");

    // Polling should still work
    let base_url_clone = base_url.clone();
    let signing_priv = recipient_kp.signing_private_key_hex.clone();
    let inbox = tokio::task::spawn_blocking(move || {
        let poll_token = make_relay_auth_token(&signing_priv, &recipient).expect("poll token");
        fetch_inbox(&base_url_clone, &recipient, &poll_token)
    })
    .await
    .expect("fetch");
    assert_eq!(inbox.len(), 1);

    shutdown_tx.send(()).ok();
}
