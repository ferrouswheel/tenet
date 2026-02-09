//! tenet-web: Web server binary that acts as a full tenet peer.
//!
//! Serves an embedded SPA, provides REST API + WebSocket for messages,
//! connects to relays, and persists state in SQLite.

#[tokio::main]
async fn main() {
    tenet::web_client::run().await;
}
