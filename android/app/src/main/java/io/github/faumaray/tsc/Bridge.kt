package io.github.faumaray.tsc

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build

/**
 * What the Rust library (crates/tsc-android/src/bridge.rs) calls, from any
 * thread. Keep the names and signatures in step with it.
 */
object Bridge {
    private val app: TscApp get() = TscApp.instance

    @JvmStatic
    fun context(): Context = app

    /** Ask for the microphone (voice) and notification (services) permissions. */
    @JvmStatic
    fun requestPermissions() {
        val activity = MainActivity.current?.get() ?: return
        val wanted = buildList {
            add(Manifest.permission.RECORD_AUDIO)
            if (Build.VERSION.SDK_INT >= 33) add(Manifest.permission.POST_NOTIFICATIONS)
        }.filter { activity.checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }
        if (wanted.isNotEmpty()) {
            activity.runOnUiThread { activity.requestPermissions(wanted.toTypedArray(), 1) }
        }
    }

    /**
     * Run the voice service with this notification text (`muted`: the action
     * offers Unmute); null stops it.
     */
    @JvmStatic
    fun setVoiceNotification(text: String?, muted: Boolean) {
        if (text == null) VoiceService.stop(app) else VoiceService.start(app, text, muted)
    }

    /** Ask the user to share the screen; the answer goes to Native.onScreenCaptureResult. */
    @JvmStatic
    fun requestScreenCapture(fps: Int, maxSize: Int) {
        val activity = MainActivity.current?.get()
        if (activity == null) {
            Native.onScreenCaptureResult(false, "the app is not in the foreground")
            return
        }
        activity.runOnUiThread { activity.requestScreenCapture(fps, maxSize) }
    }

    @JvmStatic
    fun stopScreenCapture() {
        ScreenCaptureService.stop(app)
    }

    /** Capture what the device plays; false without a running screen capture. */
    @JvmStatic
    fun startSystemAudio(): Boolean = ScreenCaptureService.startAudio()

    @JvmStatic
    fun stopSystemAudio() {
        ScreenCaptureService.stopAudio()
    }

    @JvmStatic
    fun secretGet(key: String): String? = SecretStore.get(app, key)

    @JvmStatic
    fun secretSet(key: String, value: String) {
        SecretStore.set(app, key, value)
    }

    @JvmStatic
    fun secretDelete(key: String) {
        SecretStore.delete(app, key)
    }
}
