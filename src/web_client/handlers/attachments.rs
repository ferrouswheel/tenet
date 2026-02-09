//! Attachment upload and download handlers.

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum_extra::extract::Multipart;
use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::storage::AttachmentRow;
use crate::web_client::config::MAX_ATTACHMENT_SIZE;
use crate::web_client::state::SharedState;
use crate::web_client::utils::{api_error, now_secs};

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

    // Compute content hash (SHA256, base64 URL-safe)
    let digest = Sha256::digest(&data);
    let content_hash = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);

    let now = now_secs();
    let size = data.len() as u64;

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
    });
    (StatusCode::CREATED, axum::Json(json)).into_response()
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
