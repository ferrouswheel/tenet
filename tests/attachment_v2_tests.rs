//! Integration tests for the Attachment v2 Option D blob endpoints.
//!
//! Tests cover:
//! - Upload a chunk (`POST /blobs`)
//! - Download a chunk (`GET /blobs/:hash`)
//! - Conflict on duplicate upload
//! - Integrity rejection (mismatched hash)
//! - Chunk-size quota enforcement
//! - Delete a chunk (`DELETE /blobs/:hash`)
//! - `AttachmentTransport` serialisation round-trip

use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use tenet::protocol::{AttachmentRef, AttachmentTransport, ContentId};
use tenet::relay::{app, RelayConfig, RelayQosConfig, RelayState};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_config(blob_max_chunk_bytes: usize, blob_daily_quota_bytes: u64) -> RelayConfig {
    RelayConfig {
        ttl: Duration::from_secs(3600),
        max_messages: 100,
        max_bytes: 10 * 1024 * 1024,
        retry_backoff: Vec::new(),
        peer_log_window: Duration::from_secs(60),
        peer_log_interval: Duration::ZERO,
        log_sink: Some(Arc::new(|_| {})),
        pause_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        qos: RelayQosConfig::default(),
        blob_max_chunk_bytes,
        blob_daily_quota_bytes,
    }
}

async fn start_relay(config: RelayConfig) -> (String, oneshot::Sender<()>) {
    let state = RelayState::new(config);
    let router: Router = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind relay");
    let addr: SocketAddr = listener.local_addr().expect("local addr");
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        })
        .await
        .ok();
    });
    (format!("http://{addr}"), shutdown_tx)
}

/// Prepare a minimal valid upload body: encrypt some plaintext, compute hash.
///
/// Returns `(chunk_hash_hex, data_b64, raw_encrypted_bytes)`.
fn make_chunk(plaintext: &[u8]) -> (String, String, Vec<u8>) {
    // For tests we treat the plaintext as "already encrypted" (no actual AEAD).
    // What matters is that hash(raw) == declared chunk_hash.
    let raw = plaintext.to_vec();
    let hash = hex::encode(Sha256::digest(&raw));
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&raw);
    (hash, b64, raw)
}

fn post_blob(base_url: &str, chunk_hash: &str, data_b64: &str, sender_id: Option<&str>) -> u16 {
    let mut body = serde_json::json!({
        "chunk_hash": chunk_hash,
        "data_b64": data_b64,
    });
    if let Some(sid) = sender_id {
        body["sender_id"] = serde_json::Value::String(sid.to_string());
    }
    let url = format!("{base_url}/blobs");
    match ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
    {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("request failed: {e}"),
    }
}

fn get_blob(base_url: &str, chunk_hash: &str) -> (u16, Vec<u8>) {
    let url = format!("{base_url}/blobs/{chunk_hash}");
    match ureq::get(&url).call() {
        Ok(r) => {
            let status = r.status();
            let mut buf = Vec::new();
            r.into_reader().read_to_end(&mut buf).unwrap_or(0);
            (status, buf)
        }
        Err(ureq::Error::Status(code, r)) => {
            let mut buf = Vec::new();
            r.into_reader().read_to_end(&mut buf).unwrap_or(0);
            (code, buf)
        }
        Err(e) => panic!("request failed: {e}"),
    }
}

fn delete_blob(base_url: &str, chunk_hash: &str) -> u16 {
    let url = format!("{base_url}/blobs/{chunk_hash}");
    match ureq::delete(&url).call() {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("request failed: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn blob_upload_and_download_roundtrip() {
    let (base_url, shutdown_tx) = start_relay(make_config(512 * 1024, 500 * 1024 * 1024)).await;

    let plaintext = b"hello relay blob world!";
    let (hash, b64, raw) = make_chunk(plaintext);

    // Upload
    let status = tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        let hash = hash.clone();
        let b64 = b64.clone();
        move || post_blob(&base_url, &hash, &b64, None)
    })
    .await
    .unwrap();
    assert_eq!(status, 201, "expected 201 Created");

    // Download
    let (dl_status, dl_data) = tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        let hash = hash.clone();
        move || get_blob(&base_url, &hash)
    })
    .await
    .unwrap();
    assert_eq!(dl_status, 200, "expected 200 OK");
    assert_eq!(dl_data, raw, "downloaded data should match uploaded data");

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn blob_upload_conflict_returns_409() {
    let (base_url, shutdown_tx) = start_relay(make_config(512 * 1024, 500 * 1024 * 1024)).await;

    let (hash, b64, _) = make_chunk(b"duplicate chunk");

    let upload = {
        let base_url = base_url.clone();
        let hash = hash.clone();
        let b64 = b64.clone();
        move || post_blob(&base_url, &hash, &b64, None)
    };

    let s1 = tokio::task::spawn_blocking(upload.clone()).await.unwrap();
    assert_eq!(s1, 201, "first upload should succeed");

    let s2 = tokio::task::spawn_blocking(upload).await.unwrap();
    assert_eq!(s2, 409, "second upload should return 409 Conflict");

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn blob_mismatched_hash_rejected() {
    let (base_url, shutdown_tx) = start_relay(make_config(512 * 1024, 500 * 1024 * 1024)).await;

    let (_, b64, _) = make_chunk(b"some data");
    // Deliberate wrong hash (all zeros).
    let wrong_hash = "0".repeat(64);

    let status = tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        move || post_blob(&base_url, &wrong_hash, &b64, None)
    })
    .await
    .unwrap();
    assert_eq!(status, 400, "mismatched hash should return 400");

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn blob_invalid_hash_format_rejected() {
    let (base_url, shutdown_tx) = start_relay(make_config(512 * 1024, 500 * 1024 * 1024)).await;

    let (_, b64, _) = make_chunk(b"data");
    let bad_hash = "not-a-hex-hash";

    let status = tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        move || post_blob(&base_url, bad_hash, &b64, None)
    })
    .await
    .unwrap();
    assert_eq!(status, 400, "invalid hash format should return 400");

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn blob_download_not_found_returns_404() {
    let (base_url, shutdown_tx) = start_relay(make_config(512 * 1024, 500 * 1024 * 1024)).await;

    let missing_hash = "a".repeat(64);
    let (status, _) = tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        move || get_blob(&base_url, &missing_hash)
    })
    .await
    .unwrap();
    assert_eq!(status, 404, "missing chunk should return 404");

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn blob_exceeds_max_chunk_size_rejected() {
    // Set a tiny max chunk size of 4 bytes.
    let (base_url, shutdown_tx) = start_relay(make_config(4, 500 * 1024 * 1024)).await;

    let (hash, b64, _) = make_chunk(b"this is longer than 4 bytes");

    let status = tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        move || post_blob(&base_url, &hash, &b64, None)
    })
    .await
    .unwrap();
    assert_eq!(status, 413, "oversized chunk should return 413");

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn blob_daily_quota_exceeded_returns_429() {
    // Allow only 10 bytes per day.
    let (base_url, shutdown_tx) = start_relay(make_config(512 * 1024, 10)).await;

    // First chunk: 5 bytes — fits within quota.
    let (hash1, b64_1, _) = make_chunk(b"hello");
    let s1 = tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        let hash1 = hash1.clone();
        move || post_blob(&base_url, &hash1, &b64_1, Some("sender-abc"))
    })
    .await
    .unwrap();
    assert_eq!(s1, 201, "first chunk should fit in quota");

    // Second chunk: 10 bytes — would push total past the 10-byte limit.
    let (hash2, b64_2, _) = make_chunk(b"0123456789");
    let s2 = tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        move || post_blob(&base_url, &hash2, &b64_2, Some("sender-abc"))
    })
    .await
    .unwrap();
    assert_eq!(s2, 429, "second chunk should exceed daily quota");

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn blob_delete_removes_chunk() {
    let (base_url, shutdown_tx) = start_relay(make_config(512 * 1024, 500 * 1024 * 1024)).await;

    let (hash, b64, _) = make_chunk(b"delete me");

    // Upload
    tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        let hash = hash.clone();
        let b64 = b64.clone();
        move || post_blob(&base_url, &hash, &b64, None)
    })
    .await
    .unwrap();

    // Delete
    let del_status = tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        let hash = hash.clone();
        move || delete_blob(&base_url, &hash)
    })
    .await
    .unwrap();
    assert_eq!(del_status, 204, "delete should return 204 No Content");

    // Confirm gone
    let (get_status, _) = tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        let hash = hash.clone();
        move || get_blob(&base_url, &hash)
    })
    .await
    .unwrap();
    assert_eq!(get_status, 404, "deleted chunk should return 404");

    shutdown_tx.send(()).ok();
}

#[tokio::test]
async fn blob_delete_not_found_returns_404() {
    let (base_url, shutdown_tx) = start_relay(make_config(512 * 1024, 500 * 1024 * 1024)).await;

    let missing = "b".repeat(64);
    let status = tokio::task::spawn_blocking({
        let base_url = base_url.clone();
        move || delete_blob(&base_url, &missing)
    })
    .await
    .unwrap();
    assert_eq!(status, 404, "deleting missing chunk should return 404");

    shutdown_tx.send(()).ok();
}

// ---------------------------------------------------------------------------
// Protocol serialisation round-trip tests
// ---------------------------------------------------------------------------

#[test]
fn attachment_transport_inline_roundtrip() {
    let att = AttachmentRef {
        content_id: ContentId::from_bytes(b"test"),
        content_type: "image/png".to_string(),
        size: 42,
        filename: None,
        data: Some("aGVsbG8=".to_string()),
        transport: None, // None == Inline
    };
    let json = serde_json::to_string(&att).unwrap();
    let roundtrip: AttachmentRef = serde_json::from_str(&json).unwrap();
    assert_eq!(att, roundtrip);
    // transport field should be absent in JSON (skip_serializing_if = None).
    assert!(
        !json.contains("transport"),
        "transport should be absent for inline"
    );
}

#[test]
fn attachment_transport_relay_blob_roundtrip() {
    let att = AttachmentRef {
        content_id: ContentId::from_bytes(b"test"),
        content_type: "video/mp4".to_string(),
        size: 10_000_000,
        filename: Some("movie.mp4".to_string()),
        data: None,
        transport: Some(AttachmentTransport::RelayBlob {
            relay_url: "https://relay.example.com".to_string(),
            chunk_hashes: vec!["abc123".repeat(10), "def456".repeat(10)],
            blob_key: "c29tZWtleWJ5dGVzMTIzNDU2Nzg5MGFiY2Q".to_string(),
        }),
    };
    let json = serde_json::to_string(&att).unwrap();
    let roundtrip: AttachmentRef = serde_json::from_str(&json).unwrap();
    assert_eq!(att, roundtrip);
    assert!(
        json.contains("relay_blob"),
        "should contain relay_blob transport type"
    );
}

#[test]
fn attachment_transport_peer_seeded_roundtrip() {
    let att = AttachmentRef {
        content_id: ContentId::from_bytes(b"test"),
        content_type: "application/zip".to_string(),
        size: 300_000_000,
        filename: None,
        data: None,
        transport: Some(AttachmentTransport::PeerSeeded {
            blob_id: "deadbeef".repeat(8),
            chunk_hashes: vec!["chunk0".to_string(), "chunk1".to_string()],
            seeders: vec!["peer-a".to_string(), "peer-b".to_string()],
            relay_fallback: "https://relay.example.com".to_string(),
            blob_key: "a2V5Ynl0ZXM=".to_string(),
        }),
    };
    let json = serde_json::to_string(&att).unwrap();
    let roundtrip: AttachmentRef = serde_json::from_str(&json).unwrap();
    assert_eq!(att, roundtrip);
    assert!(
        json.contains("peer_seeded"),
        "should contain peer_seeded transport type"
    );
}

#[test]
fn v1_attachment_ref_without_transport_deserialises_as_inline() {
    // A v1 JSON payload (no `transport` field) should deserialise cleanly.
    let v1_json = r#"{
        "content_id": "dGVzdA",
        "content_type": "image/jpeg",
        "size": 1024,
        "data": "aGVsbG8="
    }"#;
    let att: AttachmentRef = serde_json::from_str(v1_json).unwrap();
    assert!(
        att.transport.is_none(),
        "v1 payload should have no transport"
    );
    assert_eq!(att.data, Some("aGVsbG8=".to_string()));
}
