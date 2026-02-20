package com.example.tenet

import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.os.Build
import dagger.hilt.android.HiltAndroidApp

/**
 * Application class.  Hilt uses this as the component root.
 * Also creates the Android notification channels on first launch.
 */
@HiltAndroidApp
class TenetApplication : Application() {

    override fun onCreate() {
        super.onCreate()
        createNotificationChannels()
    }

    private fun createNotificationChannels() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return

        val manager = getSystemService(NotificationManager::class.java)

        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL_MESSAGES,
                "Messages",
                NotificationManager.IMPORTANCE_DEFAULT
            ).apply { description = "Direct and group messages" }
        )

        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL_FRIENDS,
                "Friend Requests",
                NotificationManager.IMPORTANCE_DEFAULT
            ).apply { description = "Incoming friend requests and acceptances" }
        )

        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL_GROUPS,
                "Group Invites",
                NotificationManager.IMPORTANCE_DEFAULT
            ).apply { description = "Group invite received" }
        )
    }

    companion object {
        const val CHANNEL_MESSAGES = "tenet_messages"
        const val CHANNEL_FRIENDS  = "tenet_friends"
        const val CHANNEL_GROUPS   = "tenet_groups"
    }
}
