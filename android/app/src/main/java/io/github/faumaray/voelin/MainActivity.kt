package io.github.faumaray.voelin

import android.Manifest
import android.app.NativeActivity
import android.content.Intent
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.OpenableColumns
import android.util.Log
import java.io.File
import java.lang.ref.WeakReference
import kotlin.concurrent.thread

/**
 * The window: a NativeActivity running `android_main` of libvoelin_android.so
 * (the Slint UI). Kotlin only handles what needs an Activity: permission
 * prompts, the screen-capture consent dialog, a tapped notification and
 * what is shared to the app ("Share to Voelin").
 */
class MainActivity : NativeActivity() {
    private var captureFps = 30
    private var captureMaxSize = 1920

    override fun onCreate(savedInstanceState: Bundle?) {
        current = WeakReference(this)
        super.onCreate(savedInstanceState)
        // Not again when the activity is recreated with the same intent.
        if (savedInstanceState == null) handle(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        handle(intent)
    }

    override fun onDestroy() {
        if (current?.get() === this) current = null
        super.onDestroy()
    }

    /**
     * Back leaves the app like Home instead of finishing the activity, so the
     * window (and its state) stays while voice goes on in the background.
     */
    @Deprecated("Deprecated in Java")
    override fun onBackPressed() {
        moveTaskToBack(true)
    }

    /** A tapped voice notification, or something shared to us. */
    private fun handle(intent: Intent?) {
        when (intent?.action) {
            VoiceService.ACTION_OPEN_VOICE -> Native.onOpenVoice()
            Intent.ACTION_SEND, Intent.ACTION_SEND_MULTIPLE -> share(intent)
        }
    }

    /**
     * Text goes into the current chat's composer; files are copied out of the
     * sending app (its content URIs are readable only now) into our cache,
     * then uploaded to the current channel by the Rust side.
     */
    private fun share(intent: Intent) {
        val subject = intent.getStringExtra(Intent.EXTRA_SUBJECT)?.trim().orEmpty()
        val body = intent.getCharSequenceExtra(Intent.EXTRA_TEXT)?.toString()?.trim().orEmpty()
        val text = when {
            subject.isEmpty() -> body
            body.isEmpty() || body.contains(subject) -> body.ifEmpty { subject }
            else -> "$subject\n$body"
        }.ifEmpty { null }
        val uris = sharedUris(intent)
        if (uris.isEmpty()) {
            if (text != null) Native.onShare(text, null)
            return
        }
        val resolver = contentResolver
        val dir = File(cacheDir, "shared/${System.nanoTime()}")
        thread(name = "voelin-share") {
            val paths = uris.mapIndexedNotNull { i, uri ->
                try {
                    val name = displayName(uri) ?: "shared-file-${i + 1}"
                    // Two shared files may have the same name.
                    val file = File(dir, name).takeUnless { it.exists() } ?: File(dir, "${i + 1}-$name")
                    dir.mkdirs()
                    resolver.openInputStream(uri)?.use { input ->
                        file.outputStream().use { input.copyTo(it) }
                    } ?: return@mapIndexedNotNull null
                    file.path
                } catch (e: Exception) {
                    Log.w(TAG, "cannot read the shared $uri", e)
                    null
                }
            }
            Native.onShare(text, paths.joinToString("\n").ifEmpty { null })
        }
    }

    private fun sharedUris(intent: Intent): List<Uri> = if (intent.action == Intent.ACTION_SEND_MULTIPLE) {
        if (Build.VERSION.SDK_INT >= 33) {
            intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM)
        }.orEmpty()
    } else {
        listOfNotNull(
            if (Build.VERSION.SDK_INT >= 33) {
                intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
            } else {
                @Suppress("DEPRECATION")
                intent.getParcelableExtra(Intent.EXTRA_STREAM)
            },
        )
    }

    /** The file name the sending app gives, made safe as one path component. */
    private fun displayName(uri: Uri): String? {
        val name = contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use {
            if (it.moveToFirst()) it.getString(0) else null
        } ?: uri.lastPathSegment
        return name?.replace(Regex("[/\\\\\n\r\u0000]"), "_")?.trim()?.takeIf { it.isNotEmpty() && it != "." && it != ".." }
    }

    /** Ask for the camera (a studio camera source); CameraCapture gets the answer. */
    fun requestCameraPermission() {
        requestPermissions(arrayOf(Manifest.permission.CAMERA), REQUEST_CAMERA)
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == REQUEST_CAMERA) {
            CameraCapture.onPermissionResult(grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED)
        }
    }

    /** Show the system's "start recording or casting?" dialog. */
    fun requestScreenCapture(fps: Int, maxSize: Int) {
        captureFps = fps
        captureMaxSize = maxSize
        val manager = getSystemService(MediaProjectionManager::class.java)
        @Suppress("DEPRECATION")
        startActivityForResult(manager.createScreenCaptureIntent(), REQUEST_SCREEN_CAPTURE)
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        @Suppress("DEPRECATION")
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != REQUEST_SCREEN_CAPTURE) return
        if (resultCode == RESULT_OK && data != null) {
            // The service reports the outcome to Native.onScreenCaptureResult.
            ScreenCaptureService.start(this, resultCode, data, captureFps, captureMaxSize)
        } else {
            Native.onScreenCaptureResult(false, null)
        }
    }

    companion object {
        private const val TAG = "MainActivity"
        private const val REQUEST_SCREEN_CAPTURE = 1001
        private const val REQUEST_CAMERA = 1002

        /** The activity in front, if any. */
        @Volatile
        var current: WeakReference<MainActivity>? = null
            private set
    }
}
