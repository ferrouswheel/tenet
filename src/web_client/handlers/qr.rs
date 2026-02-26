//! QR code endpoint for peer discovery.
//!
//! Returns an SVG QR code encoding the caller's Tenet peer URI:
//! `tenet://peer/<peer_id>?key=<encryption_public_key_hex>`
//!
//! Co-located peers can scan this with the web-client scanner (or the Android
//! app) to add each other without manually copying 64-character hex keys.

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use qrcode::render::svg;
use qrcode::QrCode;

use crate::web_client::state::SharedState;
use crate::web_client::utils::api_error;

pub async fn qr_handler(State(state): State<SharedState>) -> Response {
    let (peer_id, public_key_hex) = {
        let st = state.lock().await;
        (st.keypair.id.clone(), st.keypair.public_key_hex.clone())
    };

    let uri = format!("tenet://peer/{}?key={}", peer_id, public_key_hex);

    match QrCode::new(uri.as_bytes()) {
        Ok(code) => {
            let svg_str = code
                .render::<svg::Color>()
                .min_dimensions(200, 200)
                .quiet_zone(true)
                .build();
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "image/svg+xml")],
                svg_str,
            )
                .into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
