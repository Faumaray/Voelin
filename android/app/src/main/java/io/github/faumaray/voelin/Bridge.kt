package io.github.faumaray.voelin

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ResolveInfo
import android.graphics.Bitmap
import android.graphics.Canvas
import android.os.Build
import java.io.File

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
     * Run the voice service with this notification (`muted`, `deafened`: the
     * actions offer Unmute, Undeafen; `sinceMs`: in voice since, Unix ms, 0
     * while connecting); a null title stops it.
     */
    @JvmStatic
    fun setVoiceNotification(title: String?, text: String?, muted: Boolean, deafened: Boolean, sinceMs: Long) {
        if (title == null) {
            VoiceService.stop(app)
        } else {
            VoiceService.start(app, VoiceService.Notice(title, text ?: "", muted, deafened, sinceMs))
        }
    }

    /** What the screen-sharing notification says while our stream is live; null: the default. */
    @JvmStatic
    fun setScreenNotification(title: String?, text: String?) {
        ScreenCaptureService.setNotice(title, text)
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

    /**
     * Launchable apps other than us, as "label<TAB>package<TAB>icon" lines
     * sorted by label; the icon is a PNG in our cache (empty if it has none).
     */
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
        val icons = File(app.cacheDir, "app-icons").apply { mkdirs() }
        return activities
            .filter { it.activityInfo.packageName != app.packageName }
            .distinctBy { it.activityInfo.packageName }
            .map { Triple(it, it.activityInfo.packageName, it.loadLabel(pm).toString()) }
            .sortedBy { it.third.lowercase() }
            .joinToString("\n") { (info, packageName, label) ->
                val icon = appIcon(pm, info, packageName, icons)?.path.orEmpty()
                label.replace(separators, " ") + "\t" + packageName + "\t" + icon
            }
    }

    private const val ICON_SIZE = 96

    /** The app's icon as a PNG in `dir`, drawn once per installed version. */
    private fun appIcon(pm: PackageManager, info: ResolveInfo, packageName: String, dir: File): File? {
        val version = info.activityInfo.applicationInfo.sourceDir?.let { File(it).lastModified() } ?: 0L
        val file = File(dir, "$packageName-$version.png")
        if (file.exists()) return file
        return try {
            val bitmap = Bitmap.createBitmap(ICON_SIZE, ICON_SIZE, Bitmap.Config.ARGB_8888)
            info.loadIcon(pm).apply { setBounds(0, 0, ICON_SIZE, ICON_SIZE) }.draw(Canvas(bitmap))
            file.outputStream().use { bitmap.compress(Bitmap.CompressFormat.PNG, 100, it) }
            bitmap.recycle()
            // Icons of earlier versions.
            dir.listFiles { f -> f.name.startsWith("$packageName-") && f.name != file.name }?.forEach { it.delete() }
            file
        } catch (e: Exception) {
            file.delete()
            null
        }
    }

    /** The cameras, as CameraCapture.list describes them. */
    @JvmStatic
    fun cameras(): String = CameraCapture.list(app)

    /**
     * Open camera `cameraId` for feed `id` (Native.onCameraFrame, errors to
     * Native.onCameraError); 0 x 0: its default size.
     */
    @JvmStatic
    fun startCamera(id: Long, cameraId: String, width: Int, height: Int, fps: Int): Boolean =
        CameraCapture.start(app, id, cameraId, width, height, fps)

    @JvmStatic
    fun stopCamera(id: Long) {
        CameraCapture.stop(id)
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
