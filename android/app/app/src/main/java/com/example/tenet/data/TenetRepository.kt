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
 * Singleton repository wrapping the [TenetClient] FFI object.
 *
 * All operations are suspend functions dispatched to [Dispatchers.IO] so the
 * JNI/SQLite blocking calls never touch the main thread.  ViewModels call
 * repository functions from their coroutine scopes.
 *
 * The [TenetClient] is created lazily on the first call that needs it; the
 * relay URL is read from [TenetPreferences] (persisted in SharedPreferences).
 *
 * Thread safety: [TenetClient] is itself Mutex-protected on the Rust side, so
 * concurrent coroutine calls are safe.
 */
@Singleton
class TenetRepository @Inject constructor(
    @ApplicationContext private val context: Context,
    private val prefs: TenetPreferences,
) {
    @Volatile
    private var client: TenetClient? = null

    private fun requireClient(): TenetClient {
        return client ?: synchronized(this) {
            client ?: run {
                val dataDir = context.filesDir.absolutePath
                val relayUrl = prefs.relayUrl
                    ?: error("Relay URL not configured — run setup first")
                TenetClient(dataDir, relayUrl, prefs.activeIdentityShortId).also { client = it }
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Setup
    // ---------------------------------------------------------------------------

    /**
     * Initialize the client with a relay URL.  Must be called once (e.g. from
     * the Setup screen) before any other repository method.
     */
    suspend fun initialize(relayUrl: String) = withContext(Dispatchers.IO) {
        val dataDir = context.filesDir.absolutePath
        val newClient = TenetClient(dataDir, relayUrl, prefs.activeIdentityShortId)
        synchronized(this@TenetRepository) { client = newClient }
        prefs.relayUrl = relayUrl
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
     * Create a new identity with [relayUrl] as its relay and return it.
     *
     * The new identity is not automatically switched to; call [switchIdentity]
     * afterwards if the user wants to activate it immediately.
     */
    suspend fun createIdentity(relayUrl: String): FfiIdentity = withContext(Dispatchers.IO) {
        ffiCreateIdentity(context.filesDir.absolutePath, relayUrl)
    }

    /**
     * Switch the active identity to [identity].
     *
     * This updates the default identity in the Rust config, saves it to
     * [TenetPreferences], stores [relayUrl] as a fallback, and recreates
     * [TenetClient] so subsequent calls use the new identity.
     */
    suspend fun switchIdentity(identity: FfiIdentity) = withContext(Dispatchers.IO) {
        val dataDir = context.filesDir.absolutePath
        val relayUrl = identity.relayUrl ?: prefs.relayUrl
            ?: error("No relay URL available for identity ${identity.shortId}")

        ffiSetDefaultIdentity(dataDir, identity.shortId)
        prefs.activeIdentityShortId = identity.shortId
        prefs.relayUrl = relayUrl

        val newClient = TenetClient(dataDir, relayUrl, identity.shortId)
        synchronized(this@TenetRepository) { client = newClient }
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
    // Sync
    // ---------------------------------------------------------------------------

    suspend fun sync(): FfiSyncResult = withContext(Dispatchers.IO) {
        requireClient().sync()
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
    // Notifications
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
