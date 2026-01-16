use std::time::Duration;

use axum::Router;
use base64::Engine as _;
use hpke::kem::X25519HkdfSha256;
use hpke::Kem as _;
use hpke::Serializable;
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use tenet::crypto::{decrypt_payload, encrypt_payload, unwrap_content_key, wrap_content_key};
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

    let server = axum::serve(listener, app).with_graceful_shutdown(async {
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

fn fetch_inbox(base_url: &str, recipient_id: &str) -> Vec<Envelope> {
    let response = ureq::get(&format!("{}/inbox/{}", base_url, recipient_id))
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
    })
    .await;

    let envelope = Envelope {
        message_id: "expire-test".to_string(),
        header: Header {
            sender_id: "sender".to_string(),
            recipient_id: "recipient".to_string(),
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
        move || fetch_inbox(&base_url, "recipient")
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
    })
    .await;

    let envelope = Envelope {
        message_id: "dedupe-test".to_string(),
        header: Header {
            sender_id: "sender".to_string(),
            recipient_id: "recipient".to_string(),
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
        move || fetch_inbox(&base_url, "recipient")
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
    })
    .await;

    let mut rng = ChaCha20Rng::from_seed([9u8; 32]);
    let (recipient_private, recipient_public) = X25519HkdfSha256::gen_keypair(&mut rng);
    let recipient_id = "recipient-node";

    let header = Header {
        sender_id: "sender-node".to_string(),
        recipient_id: recipient_id.to_string(),
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
        move || fetch_inbox(&base_url, recipient_id)
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
