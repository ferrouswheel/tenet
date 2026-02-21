package com.example.tenet.data

import android.content.Context
import com.example.tenet.uniffi.FfiConversation
import com.example.tenet.uniffi.FfiFriendRequest
import com.example.tenet.uniffi.FfiGroup
import com.example.tenet.uniffi.FfiGroupInvite
import com.example.tenet.uniffi.FfiGroupMember
import com.example.tenet.uniffi.FfiIdentity
import com.example.tenet.uniffi.FfiMessage
import com.example.tenet.uniffi.FfiNotification
import com.example.tenet.uniffi.FfiPeer
import com.example.tenet.uniffi.FfiProfile
import com.example.tenet.uniffi.FfiReactionSummary
import com.example.tenet.uniffi.FfiSyncResult
import com.example.tenet.uniffi.TenetClient
import com.example.tenet.uniffi.createIdentity as ffiCreateIdentity
import com.example.tenet.uniffi.listIdentities as ffiListIdentities
import com.example.tenet.uniffi.setDefaultIdentity as ffiSetDefaultIdentity
import dagger.hilt.android.qualifiers.ApplicationContext
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import javax.inject.Inject
import javax.inject.Singleton

/**
 * Singleton repository managing [TenetClient] instances — one per identity.
 *
 * Each identity has its own [TenetClient] backed by an independent SQLite
 * database and keypair.  Clients are opened lazily and kept in [clients] so
 * they can all be synced in the background without reopening files on every
 * [SyncWorker] run.
 *
 * UI operations (send, list messages, …) always target the *active* identity
 * returned by [requireClient].  Background sync targets all open identities
 * via [syncAll].
 *
 * Thread safety: [clients] is accessed only under `synchronized(this)`.
 * [TenetClient] itself is Mutex-protected on the Rust side, so concurrent
 * calls to different clients — or concurrent calls to the same client from
 * multiple coroutines — are safe.
 */
@Singleton
class TenetRepository @Inject constructor(
    @ApplicationContext private val context: Context,
    private val prefs: TenetPreferences,
) {
    // All open TenetClient instances, keyed by short_id (first 12 chars of peer ID).
    private val clients = mutableMapOf<String, TenetClient>()

    // ---------------------------------------------------------------------------
    // Internal helpers
    // ---------------------------------------------------------------------------

    /**
     * Returns the [TenetClient] for the currently active identity.
     *
     * Requires that [initialize] has already been called; throws otherwise.
     * Does NOT perform any I/O — only looks up the in-memory map.
     */
    private fun requireClient(): TenetClient = synchronized(this) {
        val shortId = prefs.activeIdentityShortId
        if (shortId != null) {
            clients[shortId]
                ?: error("Client for identity '$shortId' not loaded — call initialize() first")
        } else {
            clients.values.firstOrNull()
                ?: error("No identities loaded — call initialize() first")
        }
    }

    /**
     * Ensures a [TenetClient] is open for [identity], creating one if needed.
     *
     * Safe to call concurrently: uses double-checked locking so only one
     * client is ever created per identity even under concurrent calls.
     * Must be invoked on [Dispatchers.IO] (SQLite open is blocking).
     *
     * Returns the (possibly newly created) client.
     */
    private fun ensureClientOpen(identity: FfiIdentity): TenetClient {
        // Fast path — already open.
        synchronized(this) { clients[identity.shortId] }?.let { return it }

        val relayUrl = identity.relayUrl ?: prefs.relayUrl
            ?: error("No relay URL available for identity '${identity.shortId}'")
        val newClient = TenetClient(context.filesDir.absolutePath, relayUrl, identity.shortId)

        return synchronized(this) {
            // Double-check: another thread may have opened the same client while
            // we were doing the (blocking) TenetClient construction above.
            clients.getOrPut(identity.shortId) { newClient }
        }
    }

    // ---------------------------------------------------------------------------
    // Setup
    // ---------------------------------------------------------------------------

    /**
     * Initialize the repository with a relay URL on first launch.
     *
     * Creates (or loads) the default identity, opens its [TenetClient], and
     * persists the relay URL and active short ID to [TenetPreferences].
     */
    suspend fun initialize(relayUrl: String) = withContext(Dispatchers.IO) {
        val dataDir = context.filesDir.absolutePath
        val newClient = TenetClient(dataDir, relayUrl, prefs.activeIdentityShortId)
        val shortId = newClient.myPeerId().take(12)
        synchronized(this@TenetRepository) { clients[shortId] = newClient }
        prefs.relayUrl = relayUrl
        prefs.activeIdentityShortId = shortId
    }

    fun isInitialized(): Boolean = prefs.relayUrl != null

    fun myPeerId(): String = requireClient().myPeerId()

    fun relayUrl(): String = requireClient().relayUrl()

    // ---------------------------------------------------------------------------
    // Identity management
    // ---------------------------------------------------------------------------

    /**
     * List all identities stored in the app's data directory.
     */
    suspend fun listIdentities(): List<FfiIdentity> = withContext(Dispatchers.IO) {
        ffiListIdentities(context.filesDir.absolutePath)
    }

    /**
     * Create a new identity with [relayUrl] as its relay.
     *
     * Immediately opens a [TenetClient] for it so it participates in the next
     * [syncAll] call without further setup.  The new identity is NOT set as
     * active; call [switchIdentity] if desired.
     */
    suspend fun createIdentity(relayUrl: String): FfiIdentity = withContext(Dispatchers.IO) {
        val identity = ffiCreateIdentity(context.filesDir.absolutePath, relayUrl)
        ensureClientOpen(identity)
        identity
    }

    /**
     * Switch the active identity to [identity].
     *
     * Ensures a client is open for the target identity (lazy-creating it if
     * needed), updates the Rust-layer default in config.toml, and persists the
     * new active short ID and relay URL to [TenetPreferences].
     *
     * Because all clients remain open in [clients], no data is lost on switch —
     * background sync continues to cover every identity.
     */
    suspend fun switchIdentity(identity: FfiIdentity) = withContext(Dispatchers.IO) {
        ensureClientOpen(identity)
        ffiSetDefaultIdentity(context.filesDir.absolutePath, identity.shortId)
        prefs.activeIdentityShortId = identity.shortId
        prefs.relayUrl = identity.relayUrl ?: prefs.relayUrl ?: ""
    }

    // ---------------------------------------------------------------------------
    // Sync
    // ---------------------------------------------------------------------------

    /**
     * Sync the active identity only.
     *
     * Used by foreground polling (e.g. a 30-second timer in a ViewModel) where
     * only the currently visible identity's data needs refreshing.
     */
    suspend fun sync(): FfiSyncResult = withContext(Dispatchers.IO) {
        requireClient().sync()
    }

    /**
     * Sync every known identity and return the per-identity results.
     *
     * Opens a [TenetClient] for any identity that doesn't already have one,
     * then calls [TenetClient.sync] on each.  The relay network call is made
     * outside any lock so identities are synced concurrently-safe (each client
     * holds its own internal Mutex).
     *
     * Used by [SyncWorker] so background sync covers all identities, not just
     * the one currently shown in the UI.
     */
    suspend fun syncAll(): Map<String, FfiSyncResult> = withContext(Dispatchers.IO) {
        val identities = ffiListIdentities(context.filesDir.absolutePath)

        // Open clients for any identity not yet in the map.
        val toOpen = synchronized(this@TenetRepository) {
            identities.filter { !clients.containsKey(it.shortId) }
        }
        for (identity in toOpen) {
            ensureClientOpen(identity)
        }

        // Take a snapshot (to avoid holding the lock during network I/O) and sync.
        val snapshot = synchronized(this@TenetRepository) { clients.toMap() }
        snapshot.mapValues { (_, client) -> client.sync() }
    }

    /**
     * Return all unread [FfiNotification] records across every open identity.
     *
     * Errors from individual clients are swallowed so a single broken identity
     * does not prevent notifications from the others from being reported.
     */
    suspend fun listAllUnreadNotifications(): List<FfiNotification> = withContext(Dispatchers.IO) {
        val snapshot = synchronized(this@TenetRepository) { clients.toMap() }
        snapshot.values.flatMap { client ->
            try { client.listNotifications(true) } catch (_: Exception) { emptyList() }
        }
    }

    // ---------------------------------------------------------------------------
    // Messages
    // ---------------------------------------------------------------------------

    suspend fun getMessage(messageId: String): FfiMessage? = withContext(Dispatchers.IO) {
        requireClient().getMessage(messageId)
    }

    suspend fun listMessages(
        kind: String? = null,
        limit: UInt = 50u,
        beforeTs: Long? = null,
    ): List<FfiMessage> = withContext(Dispatchers.IO) {
        requireClient().listMessages(kind, limit, beforeTs)
    }

    suspend fun markRead(messageId: String) = withContext(Dispatchers.IO) {
        requireClient().markRead(messageId)
    }

    // ---------------------------------------------------------------------------
    // Direct messages
    // ---------------------------------------------------------------------------

    suspend fun sendDirect(recipientId: String, body: String) = withContext(Dispatchers.IO) {
        requireClient().sendDirect(recipientId, body)
    }

    suspend fun listDirectMessages(
        peerId: String,
        limit: UInt = 50u,
        beforeTs: Long? = null,
    ): List<FfiMessage> = withContext(Dispatchers.IO) {
        requireClient().listDirectMessages(peerId, limit, beforeTs)
    }

    // ---------------------------------------------------------------------------
    // Public / group messages
    // ---------------------------------------------------------------------------

    suspend fun sendPublic(body: String) = withContext(Dispatchers.IO) {
        requireClient().sendPublic(body)
    }

    suspend fun sendGroup(groupId: String, body: String) = withContext(Dispatchers.IO) {
        requireClient().sendGroup(groupId, body)
    }

    // ---------------------------------------------------------------------------
    // Replies
    // ---------------------------------------------------------------------------

    suspend fun replyTo(parentMessageId: String, body: String) = withContext(Dispatchers.IO) {
        requireClient().replyTo(parentMessageId, body)
    }

    suspend fun listReplies(
        parentMessageId: String,
        limit: UInt = 50u,
        beforeTs: Long? = null,
    ): List<FfiMessage> = withContext(Dispatchers.IO) {
        requireClient().listReplies(parentMessageId, limit, beforeTs)
    }

    // ---------------------------------------------------------------------------
    // Reactions
    // ---------------------------------------------------------------------------

    suspend fun react(targetMessageId: String, reaction: String): FfiReactionSummary =
        withContext(Dispatchers.IO) {
            requireClient().react(targetMessageId, reaction)
        }

    suspend fun unreact(targetMessageId: String): FfiReactionSummary =
        withContext(Dispatchers.IO) {
            requireClient().unreact(targetMessageId)
        }

    suspend fun getReactions(targetMessageId: String): FfiReactionSummary =
        withContext(Dispatchers.IO) {
            requireClient().getReactions(targetMessageId)
        }

    // ---------------------------------------------------------------------------
    // Attachments
    // ---------------------------------------------------------------------------

    suspend fun uploadAttachment(data: ByteArray, contentType: String): String =
        withContext(Dispatchers.IO) {
            requireClient().uploadAttachment(data, contentType)
        }

    suspend fun downloadAttachment(contentHash: String): ByteArray =
        withContext(Dispatchers.IO) {
            requireClient().downloadAttachment(contentHash)
        }

    // ---------------------------------------------------------------------------
    // Conversations
    // ---------------------------------------------------------------------------

    suspend fun listConversations(): List<FfiConversation> = withContext(Dispatchers.IO) {
        requireClient().listConversations()
    }

    // ---------------------------------------------------------------------------
    // Peers
    // ---------------------------------------------------------------------------

    suspend fun listPeers(): List<FfiPeer> = withContext(Dispatchers.IO) {
        requireClient().listPeers()
    }

    suspend fun getPeer(peerId: String): FfiPeer? = withContext(Dispatchers.IO) {
        requireClient().getPeer(peerId)
    }

    suspend fun addPeer(
        peerId: String,
        displayName: String?,
        signingPublicKeyHex: String,
    ) = withContext(Dispatchers.IO) {
        requireClient().addPeer(peerId, displayName, signingPublicKeyHex)
    }

    suspend fun removePeer(peerId: String) = withContext(Dispatchers.IO) {
        requireClient().removePeer(peerId)
    }

    suspend fun blockPeer(peerId: String) = withContext(Dispatchers.IO) {
        requireClient().blockPeer(peerId)
    }

    suspend fun unblockPeer(peerId: String) = withContext(Dispatchers.IO) {
        requireClient().unblockPeer(peerId)
    }

    suspend fun mutePeer(peerId: String) = withContext(Dispatchers.IO) {
        requireClient().mutePeer(peerId)
    }

    suspend fun unmutePeer(peerId: String) = withContext(Dispatchers.IO) {
        requireClient().unmutePeer(peerId)
    }

    // ---------------------------------------------------------------------------
    // Friends
    // ---------------------------------------------------------------------------

    suspend fun sendFriendRequest(peerId: String, message: String?) = withContext(Dispatchers.IO) {
        requireClient().sendFriendRequest(peerId, message)
    }

    suspend fun listFriendRequests(): List<FfiFriendRequest> = withContext(Dispatchers.IO) {
        requireClient().listFriendRequests()
    }

    suspend fun acceptFriendRequest(requestId: Long) = withContext(Dispatchers.IO) {
        requireClient().acceptFriendRequest(requestId)
    }

    suspend fun ignoreFriendRequest(requestId: Long) = withContext(Dispatchers.IO) {
        requireClient().ignoreFriendRequest(requestId)
    }

    suspend fun blockFriendRequest(requestId: Long) = withContext(Dispatchers.IO) {
        requireClient().blockFriendRequest(requestId)
    }

    // ---------------------------------------------------------------------------
    // Groups
    // ---------------------------------------------------------------------------

    suspend fun listGroups(): List<FfiGroup> = withContext(Dispatchers.IO) {
        requireClient().listGroups()
    }

    suspend fun getGroup(groupId: String): FfiGroup? = withContext(Dispatchers.IO) {
        requireClient().getGroup(groupId)
    }

    suspend fun listGroupMembers(groupId: String): List<FfiGroupMember> =
        withContext(Dispatchers.IO) {
            requireClient().listGroupMembers(groupId)
        }

    suspend fun createGroup(memberIds: List<String>): String = withContext(Dispatchers.IO) {
        requireClient().createGroup(memberIds)
    }

    suspend fun addGroupMember(groupId: String, peerId: String) = withContext(Dispatchers.IO) {
        requireClient().addGroupMember(groupId, peerId)
    }

    suspend fun removeGroupMember(groupId: String, peerId: String) = withContext(Dispatchers.IO) {
        requireClient().removeGroupMember(groupId, peerId)
    }

    suspend fun leaveGroup(groupId: String) = withContext(Dispatchers.IO) {
        requireClient().leaveGroup(groupId)
    }

    suspend fun listGroupInvites(): List<FfiGroupInvite> = withContext(Dispatchers.IO) {
        requireClient().listGroupInvites()
    }

    suspend fun acceptGroupInvite(inviteId: Long) = withContext(Dispatchers.IO) {
        requireClient().acceptGroupInvite(inviteId)
    }

    suspend fun ignoreGroupInvite(inviteId: Long) = withContext(Dispatchers.IO) {
        requireClient().ignoreGroupInvite(inviteId)
    }

    // ---------------------------------------------------------------------------
    // Profiles
    // ---------------------------------------------------------------------------

    suspend fun getOwnProfile(): FfiProfile? = withContext(Dispatchers.IO) {
        requireClient().getOwnProfile()
    }

    suspend fun updateOwnProfile(
        displayName: String?,
        bio: String?,
        avatarHash: String?,
    ) = withContext(Dispatchers.IO) {
        requireClient().updateOwnProfile(displayName, bio, avatarHash)
    }

    suspend fun getPeerProfile(peerId: String): FfiProfile? = withContext(Dispatchers.IO) {
        requireClient().getPeerProfile(peerId)
    }

    // ---------------------------------------------------------------------------
    // Notifications — active identity
    // ---------------------------------------------------------------------------

    suspend fun listNotifications(unreadOnly: Boolean = false): List<FfiNotification> =
        withContext(Dispatchers.IO) {
            requireClient().listNotifications(unreadOnly)
        }

    suspend fun notificationCount(): UInt = withContext(Dispatchers.IO) {
        requireClient().notificationCount()
    }

    suspend fun markNotificationRead(notificationId: Long) = withContext(Dispatchers.IO) {
        requireClient().markNotificationRead(notificationId)
    }

    suspend fun markAllNotificationsRead() = withContext(Dispatchers.IO) {
        requireClient().markAllNotificationsRead()
    }
}
