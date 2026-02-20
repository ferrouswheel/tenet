package com.example.tenet.data

import android.content.Context
import com.example.tenet.uniffi.FfiMessage
import com.example.tenet.uniffi.FfiPeer
import com.example.tenet.uniffi.FfiSyncResult
import com.example.tenet.uniffi.TenetClient
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
                TenetClient(dataDir, relayUrl).also { client = it }
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
        val newClient = TenetClient(dataDir, relayUrl)
        synchronized(this@TenetRepository) { client = newClient }
        prefs.relayUrl = relayUrl
    }

    fun isInitialized(): Boolean = prefs.relayUrl != null

    fun myPeerId(): String = requireClient().myPeerId()

    fun relayUrl(): String = requireClient().relayUrl()

    // ---------------------------------------------------------------------------
    // Messages
    // ---------------------------------------------------------------------------

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

    suspend fun sendDirect(recipientId: String, body: String) = withContext(Dispatchers.IO) {
        requireClient().sendDirect(recipientId, body)
    }

    suspend fun sendPublic(body: String) = withContext(Dispatchers.IO) {
        requireClient().sendPublic(body)
    }

    // ---------------------------------------------------------------------------
    // Sync
    // ---------------------------------------------------------------------------

    suspend fun sync(): FfiSyncResult = withContext(Dispatchers.IO) {
        requireClient().sync()
    }

    // ---------------------------------------------------------------------------
    // Peers
    // ---------------------------------------------------------------------------

    suspend fun listPeers(): List<FfiPeer> = withContext(Dispatchers.IO) {
        requireClient().listPeers()
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
}
