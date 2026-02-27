//! Attachment upload and download handlers.

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::Multipart;
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::storage::{AttachmentRow, BlobManifestRow};
use crate::web_client::config::{CHUNK_SIZE, INLINE_THRESHOLD, MAX_ATTACHMENT_SIZE};
use crate::web_client::state::SharedState;
use crate::web_client::utils::{api_error, now_secs};

/// Derive a deterministic per-chunk nonce from the blob key and chunk index.
///
/// `nonce = SHA-256(blob_key || chunk_index_le64)[:12]`
///
/// This avoids storing a nonce per chunk while preventing nonce reuse across
/// chunks of the same blob.
fn derive_chunk_nonce(blob_key: &[u8; 32], chunk_index: u64) -> [u8; 12] {
    let mut hasher = Sha256::new();
    hasher.update(blob_key);
    hasher.update(chunk_index.to_le_bytes());
    let digest = hasher.finalize();
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&digest[..12]);
    nonce
}

/// Encrypt a single chunk with the blob key and a per-chunk nonce.
///
/// Returns `(encrypted_bytes, sha256_hex_of_encrypted_bytes)`.
fn encrypt_chunk(
    cipher: &ChaCha20Poly1305,
    blob_key: &[u8; 32],
    chunk_index: u64,
    chunk: &[u8],
) -> Result<(Vec<u8>, String), String> {
    let nonce_bytes = derive_chunk_nonce(blob_key, chunk_index);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let encrypted = cipher
        .encrypt(nonce, chunk)
        .map_err(|e| format!("encryption failed: {e}"))?;
    let hash = hex::encode(Sha256::digest(&encrypted));
    Ok((encrypted, hash))
}

/// Upload a chunk to the relay blob endpoint.
///
/// Returns the chunk hash on success, or an error string on failure.
fn upload_chunk_to_relay(
    relay_url: &str,
    chunk_hash: &str,
    encrypted: &[u8],
    sender_id: Option<&str>,
) -> Result<(), String> {
    let data_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(encrypted);
    let mut body = serde_json::json!({
        "chunk_hash": chunk_hash,
        "data_b64": data_b64,
    });
    if let Some(sid) = sender_id {
        body["sender_id"] = serde_json::Value::String(sid.to_string());
    }
    let url = format!("{}/blobs", relay_url.trim_end_matches('/'));
    let resp = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(|e| format!("relay upload failed: {e}"))?;
    match resp.status() {
        201 | 409 => Ok(()), // 409 = already exists, that's fine (idempotent)
        s => Err(format!("relay returned unexpected status {s}")),
    }
}

pub async fn upload_attachment_handler(
    State(state): State<SharedState>,
    mut multipart: Multipart,
) -> Response {
    let mut file_data: Option<Vec<u8>> = None;
    let mut content_type: Option<String> = None;
    let mut filename: Option<String> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            content_type = field
                .content_type()
                .map(|ct| ct.to_string())
                .or_else(|| Some("application/octet-stream".to_string()));
            filename = field.file_name().map(|f| f.to_string());
            match field.bytes().await {
                Ok(bytes) => {
                    if bytes.len() as u64 > MAX_ATTACHMENT_SIZE {
                        return api_error(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            format!(
                                "attachment exceeds maximum size of {} bytes",
                                MAX_ATTACHMENT_SIZE
                            ),
                        );
                    }
                    file_data = Some(bytes.to_vec());
                }
                Err(e) => {
                    return api_error(StatusCode::BAD_REQUEST, format!("failed to read file: {e}"))
                }
            }
        }
    }

    let data = match file_data {
        Some(d) if !d.is_empty() => d,
        _ => return api_error(StatusCode::BAD_REQUEST, "no file provided"),
    };

    let mime = content_type.unwrap_or_else(|| "application/octet-stream".to_string());

    // Compute content hash of the **plaintext** file (SHA-256, base64 URL-safe no-pad).
    let digest = Sha256::digest(&data);
    let content_hash = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);

    let now = now_secs();
    let size = data.len() as u64;

    // Determine transport tier based on file size.
    if size < INLINE_THRESHOLD {
        // --- Inline tier (v1 behaviour) ---
        let st = state.lock().await;
        let row = AttachmentRow {
            content_hash: content_hash.clone(),
            content_type: mime.clone(),
            size_bytes: size,
            data,
            created_at: now,
        };
        if let Err(e) = st.storage.insert_attachment(&row) {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
        let json = serde_json::json!({
            "content_hash": content_hash,
            "content_type": mime,
            "size_bytes": size,
            "filename": filename,
            "transport": "inline",
        });
        (StatusCode::CREATED, axum::Json(json)).into_response()
    } else {
        // --- RelayBlob tier ---
        // Require a configured relay URL.
        let relay_url = {
            let st = state.lock().await;
            match st.relay_url.clone() {
                Some(u) => u,
                None => {
                    return api_error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "no relay configured; cannot upload blob",
                    )
                }
            }
        };

        // Generate a random 32-byte blob key.
        let mut blob_key_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut blob_key_bytes);
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&blob_key_bytes));

        // Split plaintext into CHUNK_SIZE chunks, encrypt each, upload to relay.
        let chunks: Vec<&[u8]> = data.chunks(CHUNK_SIZE).collect();
        let mut chunk_hashes: Vec<String> = Vec::with_capacity(chunks.len());

        // Collect sender_id for quota tracking (read from state before spawning blocking tasks).
        let sender_id = {
            let st = state.lock().await;
            Some(st.keypair.id.clone())
        };

        for (i, chunk) in chunks.iter().enumerate() {
            let (encrypted, chunk_hash) =
                match encrypt_chunk(&cipher, &blob_key_bytes, i as u64, chunk) {
                    Ok(r) => r,
                    Err(e) => {
                        return api_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("chunk encryption failed: {e}"),
                        )
                    }
                };

            let relay_url_c = relay_url.clone();
            let chunk_hash_c = chunk_hash.clone();
            let sender_id_c = sender_id.clone();
            let result = tokio::task::spawn_blocking(move || {
                upload_chunk_to_relay(
                    &relay_url_c,
                    &chunk_hash_c,
                    &encrypted,
                    sender_id_c.as_deref(),
                )
            })
            .await;

            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    return api_error(
                        StatusCode::BAD_GATEWAY,
                        format!("relay chunk upload failed: {e}"),
                    )
                }
                Err(e) => {
                    return api_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("internal error: {e}"),
                    )
                }
            }

            chunk_hashes.push(chunk_hash);
        }

        let blob_key_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(blob_key_bytes);

        // Persist the blob manifest and metadata (no inline data for relay blobs).
        let st = state.lock().await;

        // Insert a lightweight metadata row into `attachments` (no inline data).
        let row = AttachmentRow {
            content_hash: content_hash.clone(),
            content_type: mime.clone(),
            size_bytes: size,
            data: Vec::new(), // relay blobs have no local inline data
            created_at: now,
        };
        if let Err(e) = st.storage.insert_attachment(&row) {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }

        // Persist the blob manifest so `build_attachment_refs` can later
        // reconstruct the `AttachmentTransport::RelayBlob` field.
        let manifest = BlobManifestRow {
            content_hash: content_hash.clone(),
            blob_key: blob_key_b64.clone(),
            relay_url: relay_url.clone(),
            chunk_hashes: chunk_hashes.clone(),
            total_size: size,
            created_at: now,
        };
        if let Err(e) = st.storage.insert_blob_manifest(&manifest) {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }

        let json = serde_json::json!({
            "content_hash": content_hash,
            "content_type": mime,
            "size_bytes": size,
            "filename": filename,
            "transport": "relay_blob",
            "relay_url": relay_url,
            "chunk_count": chunk_hashes.len(),
        });
        (StatusCode::CREATED, axum::Json(json)).into_response()
    }
}

pub async fn download_attachment_handler(
    State(state): State<SharedState>,
    Path(content_hash): Path<String>,
) -> Response {
    let st = state.lock().await;
    match st.storage.get_attachment(&content_hash) {
        Ok(Some(att)) => {
            let headers = [
                (header::CONTENT_TYPE, att.content_type.as_str().to_string()),
                (
                    header::CACHE_CONTROL,
                    "public, max-age=31536000, immutable".to_string(),
                ),
            ];
            (StatusCode::OK, headers, att.data).into_response()
        }
        Ok(None) => api_error(StatusCode::NOT_FOUND, "attachment not found"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
