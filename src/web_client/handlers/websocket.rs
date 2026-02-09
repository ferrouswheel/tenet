//! WebSocket upgrade and connection handling.

use std::sync::atomic::Ordering;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tokio::sync::broadcast;

use crate::web_client::config::MAX_WS_CONNECTIONS;
use crate::web_client::state::SharedState;
use crate::web_client::utils::api_error;

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<SharedState>) -> Response {
    // Check connection limit before upgrading
    let ws_count = {
        let st = state.lock().await;
        st.ws_connection_count.clone()
    };

    let current = ws_count.load(Ordering::Relaxed);
    if current >= MAX_WS_CONNECTIONS {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "too many WebSocket connections (max {})",
                MAX_WS_CONNECTIONS
            ),
        );
    }

    ws.on_upgrade(|socket| ws_connection(socket, state))
        .into_response()
}

async fn ws_connection(mut socket: WebSocket, state: SharedState) {
    // Subscribe to the broadcast channel and increment connection count
    let (mut rx, ws_count) = {
        let st = state.lock().await;
        let count = st.ws_connection_count.clone();
        count.fetch_add(1, Ordering::Relaxed);
        (st.ws_tx.subscribe(), count)
    };

    loop {
        tokio::select! {
            // Forward broadcast events to the WebSocket client
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        if let Ok(json) = serde_json::to_string(&event) {
                            if socket.send(WsMessage::Text(json)).await.is_err() {
                                break; // client disconnected
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        crate::tlog!("ws client lagged, skipped {n} events");
                        // Notify client so it can refresh
                        let lag_msg = serde_json::json!({
                            "type": "events_missed",
                            "count": n,
                        });
                        if let Ok(json) = serde_json::to_string(&lag_msg) {
                            if socket.send(WsMessage::Text(json)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // Handle incoming messages from the client (for future use)
            msg = socket.recv() => {
                match msg {
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Ok(WsMessage::Ping(data))) => {
                        let _ = socket.send(WsMessage::Pong(data)).await;
                    }
                    _ => {} // ignore other client messages for now
                }
            }
        }
    }

    // Decrement connection count on disconnect
    ws_count.fetch_sub(1, Ordering::Relaxed);
}
