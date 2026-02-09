//! Axum router construction.

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;

use crate::web_client::config::MAX_ATTACHMENT_SIZE;
use crate::web_client::handlers;
use crate::web_client::state::SharedState;
use crate::web_client::static_files::static_handler;

/// Build the complete Axum router with all API routes and static file serving.
pub fn build_router(state: SharedState) -> Router {
    Router::new()
        // Health
        .route("/api/health", get(handlers::health::health_handler))
        // Messages API
        .route(
            "/api/messages",
            get(handlers::messages::list_messages_handler),
        )
        .route(
            "/api/messages/:message_id",
            get(handlers::messages::get_message_handler),
        )
        .route(
            "/api/messages/direct",
            post(handlers::messages::send_direct_handler),
        )
        .route(
            "/api/messages/public",
            post(handlers::messages::send_public_handler),
        )
        .route(
            "/api/messages/group",
            post(handlers::messages::send_group_handler),
        )
        .route(
            "/api/messages/:message_id/read",
            post(handlers::messages::mark_read_handler),
        )
        // Peers API
        .route(
            "/api/peers",
            get(handlers::peers::list_peers_handler).post(handlers::peers::add_peer_handler),
        )
        .route(
            "/api/peers/:peer_id",
            get(handlers::peers::get_peer_handler).delete(handlers::peers::delete_peer_handler),
        )
        // Groups API
        .route(
            "/api/groups",
            get(handlers::groups::list_groups_handler).post(handlers::groups::create_group_handler),
        )
        .route(
            "/api/groups/:group_id",
            get(handlers::groups::get_group_handler),
        )
        .route(
            "/api/groups/:group_id/members",
            post(handlers::groups::add_group_member_handler),
        )
        .route(
            "/api/groups/:group_id/members/:peer_id",
            axum::routing::delete(handlers::groups::remove_group_member_handler),
        )
        .route(
            "/api/groups/:group_id/leave",
            post(handlers::groups::leave_group_handler),
        )
        // Attachments API
        .route(
            "/api/attachments",
            post(handlers::attachments::upload_attachment_handler)
                .layer(DefaultBodyLimit::max(MAX_ATTACHMENT_SIZE as usize + 4096)),
        )
        .route(
            "/api/attachments/:content_hash",
            get(handlers::attachments::download_attachment_handler),
        )
        // Reactions API
        .route(
            "/api/messages/:message_id/react",
            post(handlers::reactions::react_handler).delete(handlers::reactions::unreact_handler),
        )
        .route(
            "/api/messages/:message_id/reactions",
            get(handlers::reactions::list_reactions_handler),
        )
        // Replies API
        .route(
            "/api/messages/:message_id/replies",
            get(handlers::replies::list_replies_handler),
        )
        .route(
            "/api/messages/:message_id/reply",
            post(handlers::replies::reply_handler),
        )
        // Profiles API
        .route(
            "/api/profile",
            get(handlers::profiles::get_own_profile_handler)
                .put(handlers::profiles::update_own_profile_handler),
        )
        .route(
            "/api/peers/:peer_id/profile",
            get(handlers::profiles::get_peer_profile_handler),
        )
        // Friend Requests API
        .route(
            "/api/friend-requests",
            get(handlers::friends::list_friend_requests_handler)
                .post(handlers::friends::send_friend_request_handler),
        )
        .route(
            "/api/friend-requests/:id/accept",
            post(handlers::friends::accept_friend_request_handler),
        )
        .route(
            "/api/friend-requests/:id/ignore",
            post(handlers::friends::ignore_friend_request_handler),
        )
        .route(
            "/api/friend-requests/:id/block",
            post(handlers::friends::block_friend_request_handler),
        )
        // Conversations API
        .route(
            "/api/conversations",
            get(handlers::conversations::list_conversations_handler),
        )
        .route(
            "/api/conversations/:peer_id",
            get(handlers::conversations::get_conversation_handler),
        )
        // WebSocket
        .route("/api/ws", get(handlers::websocket::ws_handler))
        // Static fallback
        .fallback(get(static_handler))
        .with_state(state)
}
