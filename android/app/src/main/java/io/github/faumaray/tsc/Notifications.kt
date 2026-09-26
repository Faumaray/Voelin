package io.github.faumaray.tsc

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent

/** Notification channels and the notifications of the foreground services. */
object Notifications {
    const val CHANNEL_VOICE = "voice"
    const val CHANNEL_SCREEN = "screen"

    fun createChannels(context: Context) {
        val manager = context.getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL_VOICE,
                context.getString(R.string.channel_voice),
                NotificationManager.IMPORTANCE_LOW,
            ),
        )
        manager.createNotificationChannel(
            NotificationChannel(
                CHANNEL_SCREEN,
                context.getString(R.string.channel_screen),
                NotificationManager.IMPORTANCE_LOW,
            ),
        )
    }

    /** Tapping a notification brings the app back. */
    private fun openApp(context: Context): PendingIntent = PendingIntent.getActivity(
        context,
        0,
        Intent(context, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_REORDER_TO_FRONT),
        PendingIntent.FLAG_IMMUTABLE,
    )

    fun ongoing(
        context: Context,
        channel: String,
        text: String,
        actionTitle: String,
        action: PendingIntent,
    ): Notification = Notification.Builder(context, channel)
        .setSmallIcon(R.drawable.ic_notification)
        .setContentTitle(context.getString(R.string.app_name))
        .setContentText(text)
        .setContentIntent(openApp(context))
        .setOngoing(true)
        .setCategory(Notification.CATEGORY_CALL)
        .addAction(Notification.Action.Builder(null, actionTitle, action).build())
        .build()
}
