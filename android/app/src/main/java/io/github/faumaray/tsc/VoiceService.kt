package io.github.faumaray.tsc

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
 */
class VoiceService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_DISCONNECT) {
            Native.onDisconnectVoice()
            return START_NOT_STICKY
        }
        val text = intent?.getStringExtra(EXTRA_TEXT) ?: getString(R.string.voice_connected)
        try {
            if (Build.VERSION.SDK_INT >= 30) {
                startForeground(ID, notification(this, text), ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE)
            } else {
                startForeground(ID, notification(this, text))
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
        private const val EXTRA_TEXT = "text"
        private const val ACTION_DISCONNECT = "io.github.faumaray.tsc.DISCONNECT"

        @Volatile
        private var running: VoiceService? = null

        private fun notification(context: Context, text: String): Notification {
            val disconnect = PendingIntent.getService(
                context,
                0,
                Intent(context, VoiceService::class.java).setAction(ACTION_DISCONNECT),
                PendingIntent.FLAG_IMMUTABLE,
            )
            return Notifications.ongoing(
                context,
                Notifications.CHANNEL_VOICE,
                text,
                context.getString(R.string.disconnect),
                disconnect,
            )
        }

        /** Start the service, or update its notification when it runs. */
        fun start(context: Context, text: String) {
            val service = running
            if (service != null) {
                service.getSystemService(NotificationManager::class.java)
                    .notify(ID, notification(service, text))
                return
            }
            try {
                context.startForegroundService(
                    Intent(context, VoiceService::class.java).putExtra(EXTRA_TEXT, text),
                )
            } catch (e: RuntimeException) {
                // Not allowed from the background (Android 12+). Voice is
                // connected from the UI, so this should not happen.
                Log.w(TAG, "cannot start the voice service", e)
            }
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, VoiceService::class.java))
        }
    }
}
