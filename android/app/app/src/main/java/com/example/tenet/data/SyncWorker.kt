package com.example.tenet.data

import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import androidx.core.app.NotificationCompat
import androidx.hilt.work.HiltWorker
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.NetworkType
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import com.example.tenet.MainActivity
import com.example.tenet.TenetApplication
import com.example.tenet.uniffi.FfiNotification
import dagger.assisted.Assisted
import dagger.assisted.AssistedInject
import java.util.concurrent.TimeUnit

/**
 * WorkManager worker that fetches new messages from the relay in the background.
 *
 * Runs every 15 minutes (WorkManager minimum) when the device has a network
 * connection.  After each sync it inspects unread [FfiNotification] records and
 * posts targeted system notifications to the appropriate channel:
 *
 * - [TenetApplication.CHANNEL_MESSAGES] — direct messages and replies
 * - [TenetApplication.CHANNEL_FRIENDS]  — incoming friend requests
 * - [TenetApplication.CHANNEL_GROUPS]   — group invites
 *
 * Each notification taps open the relevant screen via a `tenet://` deep-link
 * URI handled by [MainActivity].
 *
 * Use [schedule] to enqueue the recurring work from [TenetApplication.onCreate].
 */
@HiltWorker
class SyncWorker @AssistedInject constructor(
    @Assisted private val context: Context,
    @Assisted params: WorkerParameters,
    private val repository: TenetRepository,
) : CoroutineWorker(context, params) {

    override suspend fun doWork(): Result {
        if (!repository.isInitialized()) return Result.success()

        return try {
            // Sync every identity and tally new messages across all of them.
            val syncResults = repository.syncAll()
            val totalNewMessages = syncResults.values.sumOf { it.newMessages.toInt() }

            // Collect unread notifications from every identity's database.
            val unread = repository.listAllUnreadNotifications()
            val counts = categorizeNotifications(unread)

            val manager = context.getSystemService(NotificationManager::class.java)

            if (totalNewMessages > 0 || counts.messages > 0) {
                manager.notify(
                    NOTIFICATION_ID_MESSAGES,
                    buildNotification(
                        channel = TenetApplication.CHANNEL_MESSAGES,
                        title = "Tenet",
                        text = pluralMessages(counts.messages.coerceAtLeast(totalNewMessages)),
                        deepLinkUri = "tenet://conversations",
                    ),
                )
            }

            if (counts.friendRequests > 0) {
                manager.notify(
                    NOTIFICATION_ID_FRIENDS,
                    buildNotification(
                        channel = TenetApplication.CHANNEL_FRIENDS,
                        title = "Friend request",
                        text = if (counts.friendRequests == 1) "You have a new friend request"
                               else "You have ${counts.friendRequests} new friend requests",
                        deepLinkUri = "tenet://friends",
                    ),
                )
            } else {
                manager.cancel(NOTIFICATION_ID_FRIENDS)
            }

            if (counts.groupInvites > 0) {
                manager.notify(
                    NOTIFICATION_ID_GROUPS,
                    buildNotification(
                        channel = TenetApplication.CHANNEL_GROUPS,
                        title = "Group invite",
                        text = if (counts.groupInvites == 1) "You have a new group invite"
                               else "You have ${counts.groupInvites} group invites",
                        deepLinkUri = "tenet://groups",
                    ),
                )
            } else {
                manager.cancel(NOTIFICATION_ID_GROUPS)
            }

            Result.success()
        } catch (e: Exception) {
            // Retry on transient failures (network errors, etc.).
            Result.retry()
        }
    }

    private fun buildNotification(
        channel: String,
        title: String,
        text: String,
        deepLinkUri: String,
    ): android.app.Notification {
        val intent = Intent(Intent.ACTION_VIEW, Uri.parse(deepLinkUri), context, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP
        }
        val pendingIntent = PendingIntent.getActivity(
            context,
            deepLinkUri.hashCode(),
            intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        return NotificationCompat.Builder(context, channel)
            .setSmallIcon(android.R.drawable.ic_dialog_email)
            .setContentTitle(title)
            .setContentText(text)
            .setPriority(NotificationCompat.PRIORITY_DEFAULT)
            .setContentIntent(pendingIntent)
            .setAutoCancel(true)
            .build()
    }

    companion object {
        private const val WORK_NAME = "tenet_background_sync"
        private const val NOTIFICATION_ID_MESSAGES = 1001
        private const val NOTIFICATION_ID_FRIENDS  = 1002
        private const val NOTIFICATION_ID_GROUPS   = 1003

        /** Enqueue the periodic sync work.  Safe to call multiple times. */
        fun schedule(context: Context) {
            val request = PeriodicWorkRequestBuilder<SyncWorker>(15, TimeUnit.MINUTES)
                .setConstraints(
                    Constraints.Builder()
                        .setRequiredNetworkType(NetworkType.CONNECTED)
                        .build()
                )
                .build()

            WorkManager.getInstance(context).enqueueUniquePeriodicWork(
                WORK_NAME,
                ExistingPeriodicWorkPolicy.KEEP,
                request,
            )
        }

        /**
         * Counts unread notifications by category.  Extracted as an internal
         * pure function so it can be unit-tested without Android dependencies.
         */
        internal fun categorizeNotifications(notifications: List<FfiNotification>): NotificationCounts {
            var messages = 0
            var friendRequests = 0
            var groupInvites = 0
            for (n in notifications) {
                when (n.notificationType) {
                    "direct_message", "reply", "reaction" -> messages++
                    "friend_request"                       -> friendRequests++
                    "group_invite"                         -> groupInvites++
                }
            }
            return NotificationCounts(messages, friendRequests, groupInvites)
        }

        private fun pluralMessages(count: Int) =
            "$count new message${if (count == 1) "" else "s"}"
    }
}

/** Result of [SyncWorker.categorizeNotifications]. */
data class NotificationCounts(
    val messages: Int,
    val friendRequests: Int,
    val groupInvites: Int,
)
