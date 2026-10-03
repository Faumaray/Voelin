package io.github.faumaray.voelin

import android.app.Notification
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log

/**
 * Foreground service while a voice connection exists: Android keeps the
 * process, the network and (type microphone) the microphone of an app in
 * the background only this way. The Rust side starts, updates and stops it.
 * Its notification is the voice bar of the app: the channel, the server and
 * how many are there, the time in voice, and Mute, Deafen and Leave.
 */
class VoiceService : Service() {
    /** What the notification shows (crates/voelin-android/src/foreground.rs). */
    data class Notice(
        val title: String,
        val text: String,
        val muted: Boolean,
        val deafened: Boolean,
        val sinceMs: Long,
    )

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_DISCONNECT -> {
                Native.onDisconnectVoice()
                return START_NOT_STICKY
            }
            ACTION_TOGGLE_MUTE -> {
                Native.onToggleMute()
                return START_NOT_STICKY
            }
            ACTION_TOGGLE_DEAFEN -> {
                Native.onToggleDeafen()
                return START_NOT_STICKY
            }
        }
        val notice = notice ?: Notice(getString(R.string.voice_connected), "", false, false, 0)
        try {
            if (Build.VERSION.SDK_INT >= 30) {
                startForeground(ID, notification(this, notice), ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE)
            } else {
                startForeground(ID, notification(this, notice))
            }
            running = this
        } catch (e: RuntimeException) {
            // E.g. no microphone permission (Android 14 refuses the type):
            // voice then works only while the app is in front.
            Log.w(TAG, "voice service cannot run in the foreground", e)
            stopSelf()
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        if (running === this) running = null
        super.onDestroy()
    }

    companion object {
        private const val TAG = "VoiceService"
        private const val ID = 1
        private const val ACTION_DISCONNECT = "io.github.faumaray.voelin.DISCONNECT"
        private const val ACTION_TOGGLE_MUTE = "io.github.faumaray.voelin.TOGGLE_MUTE"
        private const val ACTION_TOGGLE_DEAFEN = "io.github.faumaray.voelin.TOGGLE_DEAFEN"

        /** MainActivity shows the voice channel for this action. */
        const val ACTION_OPEN_VOICE = "io.github.faumaray.voelin.OPEN_VOICE"

        @Volatile
        private var running: VoiceService? = null

        /** The latest notice, for a service that is still starting. */
        @Volatile
        private var notice: Notice? = null

        private fun action(context: Context, action: String, request: Int): PendingIntent =
            PendingIntent.getService(
                context,
                request,
                Intent(context, VoiceService::class.java).setAction(action),
                PendingIntent.FLAG_IMMUTABLE,
            )

        private fun notification(context: Context, notice: Notice): Notification =
            Notifications.ongoing(
                context,
                Notifications.CHANNEL_VOICE,
                notice.title,
                notice.text,
                Notifications.openApp(context, ACTION_OPEN_VOICE),
                notice.sinceMs,
                context.getString(if (notice.muted) R.string.unmute else R.string.mute) to
                    action(context, ACTION_TOGGLE_MUTE, 1),
                context.getString(if (notice.deafened) R.string.undeafen else R.string.deafen) to
                    action(context, ACTION_TOGGLE_DEAFEN, 2),
                context.getString(R.string.leave) to action(context, ACTION_DISCONNECT, 0),
            )

        /** Start the service, or update its notification when it runs. */
        fun start(context: Context, notice: Notice) {
            this.notice = notice
            val service = running
            if (service != null) {
                service.getSystemService(NotificationManager::class.java)
                    .notify(ID, notification(service, notice))
                return
            }
            try {
                context.startForegroundService(Intent(context, VoiceService::class.java))
            } catch (e: RuntimeException) {
                // Not allowed from the background (Android 12+). Voice is
                // connected from the UI, so this should not happen.
                Log.w(TAG, "cannot start the voice service", e)
            }
        }

        fun stop(context: Context) {
            notice = null
            context.stopService(Intent(context, VoiceService::class.java))
        }
    }
}
