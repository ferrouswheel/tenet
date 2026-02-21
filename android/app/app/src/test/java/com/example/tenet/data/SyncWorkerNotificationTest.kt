package com.example.tenet.data

import com.example.tenet.uniffi.FfiNotification
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * Unit tests for [SyncWorker.categorizeNotifications].
 *
 * The function is a pure helper that maps a list of [FfiNotification] records to a
 * [NotificationCounts] breakdown.  It runs on the JVM without any Android
 * dependencies so standard JUnit 4 tests suffice.
 */
class SyncWorkerNotificationTest {

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    private fun notification(type: String, id: Long = 0): FfiNotification =
        FfiNotification(
            id = id,
            notificationType = type,
            messageId = "msg-$id",
            senderId = "peer-$id",
            createdAt = 1_000_000L,
            isRead = false,
        )

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    @Test
    fun `empty list yields zero counts`() {
        val counts = SyncWorker.categorizeNotifications(emptyList())
        assertEquals(0, counts.messages)
        assertEquals(0, counts.friendRequests)
        assertEquals(0, counts.groupInvites)
    }

    @Test
    fun `direct_message counted as message`() {
        val counts = SyncWorker.categorizeNotifications(
            listOf(notification("direct_message"))
        )
        assertEquals(1, counts.messages)
        assertEquals(0, counts.friendRequests)
        assertEquals(0, counts.groupInvites)
    }

    @Test
    fun `reply counted as message`() {
        val counts = SyncWorker.categorizeNotifications(
            listOf(notification("reply"))
        )
        assertEquals(1, counts.messages)
    }

    @Test
    fun `reaction counted as message`() {
        val counts = SyncWorker.categorizeNotifications(
            listOf(notification("reaction"))
        )
        assertEquals(1, counts.messages)
    }

    @Test
    fun `friend_request counted separately`() {
        val counts = SyncWorker.categorizeNotifications(
            listOf(notification("friend_request"))
        )
        assertEquals(0, counts.messages)
        assertEquals(1, counts.friendRequests)
        assertEquals(0, counts.groupInvites)
    }

    @Test
    fun `group_invite counted separately`() {
        val counts = SyncWorker.categorizeNotifications(
            listOf(notification("group_invite"))
        )
        assertEquals(0, counts.messages)
        assertEquals(0, counts.friendRequests)
        assertEquals(1, counts.groupInvites)
    }

    @Test
    fun `unknown type is ignored`() {
        val counts = SyncWorker.categorizeNotifications(
            listOf(notification("unknown_future_type"))
        )
        assertEquals(0, counts.messages)
        assertEquals(0, counts.friendRequests)
        assertEquals(0, counts.groupInvites)
    }

    @Test
    fun `mixed notifications split into correct buckets`() {
        val notifications = listOf(
            notification("direct_message", 1),
            notification("direct_message", 2),
            notification("reply", 3),
            notification("reaction", 4),
            notification("friend_request", 5),
            notification("friend_request", 6),
            notification("group_invite", 7),
        )
        val counts = SyncWorker.categorizeNotifications(notifications)
        assertEquals(4, counts.messages)        // direct_message×2, reply, reaction
        assertEquals(2, counts.friendRequests)
        assertEquals(1, counts.groupInvites)
    }
}
