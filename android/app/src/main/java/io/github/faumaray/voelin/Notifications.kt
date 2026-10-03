package io.github.faumaray.voelin

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

    /** The app's accent (Theme.accent of the UI). */
    private const val ACCENT = 0xFF2D6BFF.toInt()

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

    /** Tapping a notification brings the app back, with `action` for MainActivity. */
    fun openApp(context: Context, action: String? = null): PendingIntent = PendingIntent.getActivity(
        context,
        action.hashCode(),
        Intent(context, MainActivity::class.java)
            .setAction(action)
            .addFlags(Intent.FLAG_ACTIVITY_REORDER_TO_FRONT),
        PendingIntent.FLAG_IMMUTABLE,
    )

    /**
     * An ongoing notification: `title` and `text`, a chronometer from
     * `sinceMs` (Unix ms; 0: none), `actions` (title, intent).
     */
    fun ongoing(
        context: Context,
        channel: String,
        title: String,
        text: String,
        open: PendingIntent,
        sinceMs: Long,
        vararg actions: Pair<String, PendingIntent>,
    ): Notification {
        val builder = Notification.Builder(context, channel)
            .setSmallIcon(R.drawable.ic_notification)
            .setColor(ACCENT)
            .setContentTitle(title)
            .setContentText(text)
            .setContentIntent(open)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setCategory(Notification.CATEGORY_CALL)
        if (sinceMs > 0) {
            builder.setWhen(sinceMs).setShowWhen(true).setUsesChronometer(true)
        } else {
            builder.setShowWhen(false)
        }
        for ((label, intent) in actions) {
            builder.addAction(Notification.Action.Builder(null, label, intent).build())
        }
        return builder.build()
    }
}
