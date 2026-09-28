package io.github.faumaray.voelin

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build

/**
 * What the Rust library (crates/voelin-android/src/bridge.rs) calls, from any
 * thread. Keep the names and signatures in step with it.
 */
object Bridge {
    private val app: VoelinApp get() = VoelinApp.instance

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

    /**
     * Capture into mixer input `id` (Native.onAudioInput) what `packageName`
     * plays, or with null everything but us. False without a running screen
     * capture or for a package we cannot see.
     */
    @JvmStatic
    fun startAudioInput(id: Long, packageName: String?): Boolean =
        ScreenCaptureService.startInput(app, id, packageName)

    @JvmStatic
    fun stopAudioInput(id: Long) {
        ScreenCaptureService.stopInput(id)
    }

    /** Launchable apps other than us, as "label<TAB>package" lines sorted by label. */
    @JvmStatic
    fun launchableApps(): String {
        val pm = app.packageManager
        val launcher = Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER)
        val activities = if (Build.VERSION.SDK_INT >= 33) {
            pm.queryIntentActivities(launcher, PackageManager.ResolveInfoFlags.of(0))
        } else {
            @Suppress("DEPRECATION")
            pm.queryIntentActivities(launcher, 0)
        }
        val separators = Regex("[\t\n]")
        return activities
            .map { it.activityInfo.packageName to it.loadLabel(pm).toString() }
            .filter { it.first != app.packageName }
            .distinctBy { it.first }
            .sortedBy { it.second.lowercase() }
            .joinToString("\n") { (packageName, label) -> label.replace(separators, " ") + "\t" + packageName }
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
