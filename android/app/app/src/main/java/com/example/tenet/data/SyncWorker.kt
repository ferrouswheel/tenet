package com.example.tenet.data

import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
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
import dagger.assisted.Assisted
import dagger.assisted.AssistedInject
import java.util.concurrent.TimeUnit

/**
 * WorkManager worker that fetches new messages from the relay in the background.
 *
 * Runs every 15 minutes (WorkManager minimum) when the device has a network
 * connection.  If new messages arrive, posts a system notification.
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
            val syncResult = repository.sync()
            if (syncResult.newMessages > 0u) {
                postMessageNotification(syncResult.newMessages.toInt())
            }
            Result.success()
        } catch (e: Exception) {
            // Retry on transient failures (network errors, etc.).
            Result.retry()
        }
    }

    private fun postMessageNotification(count: Int) {
        val intent = Intent(context, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK
        }
        val pendingIntent = PendingIntent.getActivity(
            context, 0, intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )

        val notification = NotificationCompat.Builder(context, TenetApplication.CHANNEL_MESSAGES)
            .setSmallIcon(android.R.drawable.ic_dialog_email)
            .setContentTitle("Tenet")
            .setContentText("$count new message${if (count == 1) "" else "s"}")
            .setPriority(NotificationCompat.PRIORITY_DEFAULT)
            .setContentIntent(pendingIntent)
            .setAutoCancel(true)
            .build()

        val manager = context.getSystemService(NotificationManager::class.java)
        manager.notify(NOTIFICATION_ID_MESSAGES, notification)
    }

    companion object {
        private const val WORK_NAME = "tenet_background_sync"
        private const val NOTIFICATION_ID_MESSAGES = 1001

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
    }
}
